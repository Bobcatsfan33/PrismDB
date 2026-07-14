//! The immutable part: PrismDB's unit of storage, pruning, and scanning.
//!
//! Physical layout (Part III §9). Rows inside a part are ordered by
//! `(centroid_id, event_time, event_id)`. That inner order is the entire reason
//! the scan is cheap: every centroid's rows are *contiguous*, so probing a
//! centroid is a byte range, not a gather. The manifest persists those ranges as
//! marks, so a reader knows the exact `(offset, len)` to fetch before it opens a
//! single column file.
//!
//! Two tiers, deliberately separate files:
//!
//! * hot — `pq.codes` (m bytes/row) and the scalar columns. What the scan reads.
//! * cold — `rerank.vec`. What rerank reads, for survivors only, within a
//!   declared budget. ~32x larger; never on the scan path. Its *encoding* is a
//!   declared, versioned choice (D-003-resolved), so the reader dispatches on it
//!   rather than assuming float32 forever.
//!
//! Two formats live here. v2 is written: a binary manifest with an explicit
//! header, and column files framed into checksummed blocks. v1 — S0's JSON
//! manifest over unframed columns — is still *read*, because a compatibility
//! corpus that no longer opens is just a directory of dead bytes.

use crate::faults;
use crate::format::{
    self, BlockRef, Header, RerankDescriptor, BLOCK_SIZE, CODEC_RAW, FEATURE_BLOCK_FRAMING,
    FORMAT_VERSION, FRAME_HEADER_BYTES,
};
use crate::io;
use crate::legacy_v1;
use prism_types::error::{PrismError, Result};
use prism_types::event::Event;
use prism_types::hash::{content_id, crc32};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

/// v2 writes this. v1 wrote `manifest.json`; the presence of one or the other is
/// how a part announces its format, before a single byte of it is trusted.
pub const MANIFEST_FILE: &str = "manifest.bin";
pub const LEGACY_MANIFEST_FILE: &str = "manifest.json";

/// One persisted centroid mark: where this centroid's rows live, in rows and in
/// bytes, in both tiers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CentroidRange {
    pub centroid: u32,
    pub first_row: usize,
    pub row_count: usize,
    pub pq_offset: u64,
    pub pq_len: usize,
    /// Byte range in the rerank tier. Sized by the *declared* encoding, not by
    /// an assumption about float32.
    pub rerank_offset: u64,
    pub rerank_len: usize,
    /// Zone map scoped to this range, so a probe can be skipped on time alone.
    pub time_min: i64,
    pub time_max: i64,
}

/// How a column's bytes are laid out in its file.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum ColumnStorage {
    /// v1: the file *is* the logical stream, with one checksum over all of it.
    /// One flipped byte condemns the whole column.
    Unframed { bytes: usize, crc32: u32 },
    /// v2: checksummed blocks. One flipped byte condemns one block, and the
    /// error names it.
    Framed {
        logical_bytes: u64,
        blocks: Vec<BlockRef>,
    },
}

impl ColumnStorage {
    pub fn logical_bytes(&self) -> u64 {
        match self {
            ColumnStorage::Unframed { bytes, .. } => *bytes as u64,
            ColumnStorage::Framed { logical_bytes, .. } => *logical_bytes,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ColumnMeta {
    pub name: String,
    pub file: String,
    /// Which codec decodes this column. An id, not a boolean, because S4 adds
    /// dictionary and delta encodings and an old reader must refuse them rather
    /// than misread them.
    pub codec_id: u16,
    pub storage: ColumnStorage,
}

/// Self-contained and self-describing: a part can be validated, and read,
/// knowing nothing but its own directory and its generation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PartManifest {
    pub format_version: u32,
    pub part_id: String,
    /// The generation whose codebooks give these bytes their meaning.
    pub generation_id: String,
    pub model_id: String,
    pub model_version: String,

    pub row_count: usize,
    pub dim: usize,
    pub pq_m: usize,

    /// D-003-resolved. What encoding the exact-rerank tier uses, and what
    /// accuracy contract that encoding owes a caller. float32/exact is the only
    /// one implemented; the descriptor exists so that changing it later is a
    /// generation migration and never a format break.
    pub rerank: RerankDescriptor,

    // --- pruning metadata: everything needed to eliminate this part unopened ---
    pub time_min: i64,
    pub time_max: i64,
    pub tenants: Vec<String>,
    pub cost_min: f64,
    pub cost_max: f64,
    pub has_error: bool,
    pub has_success: bool,

    pub centroid_ranges: Vec<CentroidRange>,
    pub columns: Vec<ColumnMeta>,
    pub created_at_ms: i64,
}

impl PartManifest {
    pub fn column(&self, name: &str) -> Result<&ColumnMeta> {
        self.columns.iter().find(|c| c.name == name).ok_or_else(|| {
            PrismError::Corrupt(format!("part {} has no column `{name}`", self.part_id))
        })
    }

    /// Can this part possibly contain a row matching these predicates?
    ///
    /// Conservative by construction: it may say yes and contribute nothing, but
    /// it must never say no to a part that holds a match. Pruning that can lose
    /// a row is not pruning, it is sampling.
    pub fn may_match(&self, tenant: Option<&str>, from: Option<i64>, to: Option<i64>) -> bool {
        if let Some(t) = tenant {
            if !self.tenants.iter().any(|x| x == t) {
                return false;
            }
        }
        if let Some(f) = from {
            if self.time_max < f {
                return false;
            }
        }
        if let Some(t) = to {
            if self.time_min > t {
                return false;
            }
        }
        true
    }

    pub fn bytes_per_vector(&self) -> Result<usize> {
        self.rerank.bytes_per_vector(self.dim)
    }

    // --- binary encoding (v2) ---

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut b = format::Writer::new();
        b.string(&self.part_id);
        b.string(&self.generation_id);
        b.string(&self.model_id);
        b.string(&self.model_version);
        b.u64(self.row_count as u64);
        b.u32(self.dim as u32);
        b.u32(self.pq_m as u32);
        b.u16(self.rerank.encoding_id);
        b.u16(self.rerank.accuracy_contract_id);
        b.i64(self.time_min);
        b.i64(self.time_max);
        b.f64(self.cost_min);
        b.f64(self.cost_max);
        b.u8(u8::from(self.has_error) | (u8::from(self.has_success) << 1));
        b.i64(self.created_at_ms);

        b.len(self.tenants.len());
        for t in &self.tenants {
            b.string(t);
        }

        b.len(self.centroid_ranges.len());
        for r in &self.centroid_ranges {
            b.u32(r.centroid);
            b.u64(r.first_row as u64);
            b.u64(r.row_count as u64);
            b.u64(r.pq_offset);
            b.u64(r.pq_len as u64);
            b.u64(r.rerank_offset);
            b.u64(r.rerank_len as u64);
            b.i64(r.time_min);
            b.i64(r.time_max);
        }

        b.len(self.columns.len());
        for c in &self.columns {
            b.string(&c.name);
            b.string(&c.file);
            b.u16(c.codec_id);
            match &c.storage {
                ColumnStorage::Framed {
                    logical_bytes,
                    blocks,
                } => {
                    b.u64(*logical_bytes);
                    b.len(blocks.len());
                    for blk in blocks {
                        b.u64(blk.file_offset);
                        b.u32(blk.payload_len);
                        b.u32(blk.crc32);
                    }
                }
                ColumnStorage::Unframed { .. } => {
                    return Err(PrismError::Invariant(
                        "refusing to write an unframed column: v2 parts are always framed".into(),
                    ));
                }
            }
        }

        let body = b.buf;
        let header = Header {
            format_version: FORMAT_VERSION,
            byte_order: format::BYTE_ORDER_LITTLE,
            feature_flags: FEATURE_BLOCK_FRAMING,
            body_len: body.len() as u32,
            body_crc32: crc32(&body),
        };
        let mut out = header.encode();
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Decode a v2 manifest. Every length is checked against the bytes present
    /// before anything is reserved (see `format::Cursor`).
    pub fn decode(bytes: &[u8]) -> Result<PartManifest> {
        let (_header, body) = format::split_manifest(bytes)?;
        let mut c = format::Cursor::new(body);

        let part_id = c.string()?;
        let generation_id = c.string()?;
        let model_id = c.string()?;
        let model_version = c.string()?;
        let row_count = c.u64()? as usize;
        let dim = c.u32()? as usize;
        let pq_m = c.u32()? as usize;
        let rerank = RerankDescriptor {
            encoding_id: c.u16()?,
            accuracy_contract_id: c.u16()?,
        };
        let time_min = c.i64()?;
        let time_max = c.i64()?;
        let cost_min = c.f64()?;
        let cost_max = c.f64()?;
        let flags = c.u8()?;
        let created_at_ms = c.i64()?;

        // A tenant id is at least a 4-byte length prefix.
        let n_tenants = c.read_len(4, "tenants")?;
        let mut tenants = Vec::with_capacity(n_tenants);
        for _ in 0..n_tenants {
            tenants.push(c.string()?);
        }

        // A centroid range is 4+8+8+8+8+8+8+8+8 = 68 bytes on the wire.
        let n_ranges = c.read_len(68, "centroid ranges")?;
        let mut centroid_ranges = Vec::with_capacity(n_ranges);
        for _ in 0..n_ranges {
            centroid_ranges.push(CentroidRange {
                centroid: c.u32()?,
                first_row: c.u64()? as usize,
                row_count: c.u64()? as usize,
                pq_offset: c.u64()?,
                pq_len: c.u64()? as usize,
                rerank_offset: c.u64()?,
                rerank_len: c.u64()? as usize,
                time_min: c.i64()?,
                time_max: c.i64()?,
            });
        }

        // A column is at least two length-prefixed strings + codec + logical + count.
        let n_cols = c.read_len(4 + 4 + 2 + 8 + 4, "columns")?;
        let mut columns = Vec::with_capacity(n_cols);
        for _ in 0..n_cols {
            let name = c.string()?;
            let file = c.string()?;
            let codec_id = c.u16()?;
            let logical_bytes = c.u64()?;
            // A block ref is 8+4+4 = 16 bytes.
            let n_blocks = c.read_len(16, "blocks")?;
            let mut blocks = Vec::with_capacity(n_blocks);
            for _ in 0..n_blocks {
                blocks.push(BlockRef {
                    file_offset: c.u64()?,
                    payload_len: c.u32()?,
                    crc32: c.u32()?,
                });
            }
            columns.push(ColumnMeta {
                name,
                file,
                codec_id,
                storage: ColumnStorage::Framed {
                    logical_bytes,
                    blocks,
                },
            });
        }

        Ok(PartManifest {
            format_version: FORMAT_VERSION,
            part_id,
            generation_id,
            model_id,
            model_version,
            row_count,
            dim,
            pq_m,
            rerank,
            time_min,
            time_max,
            tenants,
            cost_min,
            cost_max,
            has_error: flags & 1 != 0,
            has_success: flags & 2 != 0,
            centroid_ranges,
            columns,
            created_at_ms,
        })
    }

    /// Everything a manifest must say about itself that is checkable without
    /// reading a single column byte.
    ///
    /// This runs on every open. It is cheap, and it is what stops a corrupt
    /// manifest from steering a reader into a bad index or a giant allocation
    /// later, at a point where the error would be much harder to attribute.
    pub fn validate_structure(&self) -> Result<()> {
        if self.dim == 0 || self.pq_m == 0 || self.dim % self.pq_m != 0 {
            return Err(PrismError::Corrupt(format!(
                "part {} declares dim={} pq_m={}, which is not a valid product quantization",
                self.part_id, self.dim, self.pq_m
            )));
        }
        self.rerank.validate()?;

        for c in &self.columns {
            if c.codec_id != CODEC_RAW {
                return Err(PrismError::Corrupt(format!(
                    "part {} column `{}` uses codec id {} ({}), which this build cannot decode",
                    self.part_id,
                    c.name,
                    c.codec_id,
                    format::codec_name(c.codec_id)
                )));
            }
            if let ColumnStorage::Framed {
                logical_bytes,
                blocks,
            } = &c.storage
            {
                // The block directory must actually cover the logical stream —
                // no gaps, no overlaps, no lies about how much data is here.
                let covered: u64 = blocks.iter().map(|b| b.payload_len as u64).sum();
                if covered != *logical_bytes {
                    return Err(PrismError::Corrupt(format!(
                        "part {} column `{}`: blocks cover {covered} bytes but the column \
                         declares {logical_bytes}",
                        self.part_id, c.name
                    )));
                }
                let expected_blocks = if *logical_bytes == 0 {
                    1
                } else {
                    logical_bytes.div_ceil(BLOCK_SIZE as u64) as usize
                };
                if blocks.len() != expected_blocks {
                    return Err(PrismError::Corrupt(format!(
                        "part {} column `{}`: {} blocks for {logical_bytes} logical bytes, \
                         expected {expected_blocks}",
                        self.part_id,
                        c.name,
                        blocks.len()
                    )));
                }
                for (i, b) in blocks.iter().enumerate() {
                    let is_last = i + 1 == blocks.len();
                    if !is_last && b.payload_len != BLOCK_SIZE {
                        return Err(PrismError::Corrupt(format!(
                            "part {} column `{}` block {i} is {} bytes; only the last block \
                             may be short",
                            self.part_id, c.name, b.payload_len
                        )));
                    }
                    if b.payload_len > BLOCK_SIZE {
                        return Err(PrismError::Corrupt(format!(
                            "part {} column `{}` block {i} claims {} bytes, over the {BLOCK_SIZE} \
                             block size",
                            self.part_id, c.name, b.payload_len
                        )));
                    }
                }
            }
        }

        // Column sizes must be exactly what the row count implies. A manifest
        // that disagrees with itself is corrupt however good its checksums are.
        let bpv = self.bytes_per_vector()?;
        let expect = |name: &str, want: u64| -> Result<()> {
            let c = self.column(name)?;
            let got = c.storage.logical_bytes();
            if got != want {
                return Err(PrismError::Corrupt(format!(
                    "part {} column `{name}` holds {got} bytes, but {} rows imply {want}",
                    self.part_id, self.row_count
                )));
            }
            Ok(())
        };
        let rows = self.row_count as u64;
        let checked = |a: u64, b: u64, what: &str| -> Result<u64> {
            a.checked_mul(b).ok_or_else(|| {
                PrismError::Corrupt(format!(
                    "part {} row count {} overflows when sizing {what}",
                    self.part_id, self.row_count
                ))
            })
        };
        expect("pq_codes", checked(rows, self.pq_m as u64, "pq codes")?)?;
        expect(
            "rerank_vectors",
            checked(rows, bpv as u64, "rerank vectors")?,
        )?;
        expect("centroid", checked(rows, 4, "centroids")?)?;
        expect("event_time", checked(rows, 8, "event times")?)?;
        expect("cost", checked(rows, 8, "costs")?)?;
        expect("error", rows)?;

        // The centroid marks must tile the rows exactly, in order. This is the
        // invariant the whole scan path leans on: a probe is a byte range only
        // because a centroid's rows are contiguous.
        let mut next_row = 0usize;
        let mut total = 0usize;
        for (i, r) in self.centroid_ranges.iter().enumerate() {
            if r.first_row != next_row {
                return Err(PrismError::Corrupt(format!(
                    "part {} centroid mark {i} starts at row {}, expected {next_row}: the marks \
                     do not tile the rows",
                    self.part_id, r.first_row
                )));
            }
            let end = r.first_row.checked_add(r.row_count).ok_or_else(|| {
                PrismError::Corrupt(format!("part {} centroid mark {i} overflows", self.part_id))
            })?;
            if end > self.row_count {
                return Err(PrismError::Corrupt(format!(
                    "part {} centroid mark {i} runs to row {end}, past the {} rows in the part",
                    self.part_id, self.row_count
                )));
            }
            if r.pq_offset != (r.first_row * self.pq_m) as u64
                || r.pq_len != r.row_count * self.pq_m
            {
                return Err(PrismError::Corrupt(format!(
                    "part {} centroid mark {i} has pq byte range ({}, {}) that disagrees with its \
                     rows",
                    self.part_id, r.pq_offset, r.pq_len
                )));
            }
            if r.rerank_offset != (r.first_row * bpv) as u64 || r.rerank_len != r.row_count * bpv {
                return Err(PrismError::Corrupt(format!(
                    "part {} centroid mark {i} has a rerank byte range that disagrees with its rows",
                    self.part_id
                )));
            }
            if i > 0 && r.centroid <= self.centroid_ranges[i - 1].centroid {
                return Err(PrismError::Corrupt(format!(
                    "part {} centroid marks are not strictly ordered by centroid id",
                    self.part_id
                )));
            }
            next_row = end;
            total += r.row_count;
        }
        if total != self.row_count {
            return Err(PrismError::Corrupt(format!(
                "part {} centroid marks cover {total} rows but the part has {}",
                self.part_id, self.row_count
            )));
        }
        Ok(())
    }
}

/// The scalar columns a filter mask evaluates against, borrowed rather than
/// materialized.
pub struct Scalars {
    pub times: Vec<i64>,
    tenant_data: Vec<u8>,
    tenant_offs: Vec<i64>,
    row_count: usize,
}

impl Scalars {
    /// Does this row belong to `want`? Allocation-free: the comparison is made
    /// against the bytes in place.
    pub fn tenant_is(&self, row: usize, want: &str) -> bool {
        io::string_at(&self.tenant_data, &self.tenant_offs, row, self.row_count)
            .map(|t| t == want)
            .unwrap_or(false)
    }
}

/// The decoded, in-memory rows of a part.
#[derive(Clone, Debug)]
pub struct PartRows {
    pub events: Vec<Event>,
    pub centroids: Vec<u32>,
    pub codes: Vec<u8>,
    pub vectors: Vec<f32>,
}

pub struct PartWriter;

/// One row on its way into a part.
pub struct RowIn {
    pub event: Event,
    pub centroid: u32,
    pub code: Vec<u8>,
    pub vector: Vec<f32>,
}

impl PartWriter {
    /// Write an immutable v2 part and make it visible with a single rename.
    ///
    /// Publication order, which is the whole ballgame:
    ///   1. every column file written and fsynced inside `<id>.tmp/`
    ///   2. the temp directory itself fsynced
    ///   3. one `rename` into `<id>/`, then the parts directory fsynced
    ///
    /// A crash before (3) leaves a `.tmp` directory that no snapshot names and
    /// no reader can see. A crash after (3) leaves a complete, checksum-valid
    /// part that no snapshot names *yet* — still invisible, because visibility
    /// belongs to the catalog, not to the filesystem. Either way: an orphan, and
    /// orphans are GC's problem, not a reader's.
    #[allow(clippy::too_many_arguments)]
    pub fn write(
        parts_dir: &Path,
        seq: u64,
        generation_id: &str,
        model_id: &str,
        model_version: &str,
        dim: usize,
        pq_m: usize,
        mut rows: Vec<RowIn>,
        now_ms: i64,
    ) -> Result<PartManifest> {
        if rows.is_empty() {
            return Err(PrismError::Invalid(
                "refusing to write an empty part".into(),
            ));
        }
        let rerank = RerankDescriptor::float32_exact();
        let bpv = rerank.bytes_per_vector(dim)?;

        for r in &rows {
            if r.vector.len() != dim {
                return Err(PrismError::Invariant(format!(
                    "row {} has a {}-dim vector in a {dim}-dim part",
                    r.event.event_id,
                    r.vector.len()
                )));
            }
            if r.code.len() != pq_m {
                return Err(PrismError::Invariant(format!(
                    "row {} has a {}-byte code in an m={pq_m} part",
                    r.event.event_id,
                    r.code.len()
                )));
            }
        }

        // The inner order. Everything downstream depends on it.
        rows.sort_by(|a, b| {
            a.centroid
                .cmp(&b.centroid)
                .then(a.event.event_time.cmp(&b.event.event_time))
                .then(a.event.event_id.cmp(&b.event.event_id))
        });

        let row_count = rows.len();

        let mut event_ids = Vec::with_capacity(row_count);
        let mut tenant_ids = Vec::with_capacity(row_count);
        let mut event_names = Vec::with_capacity(row_count);
        let mut bodies = Vec::with_capacity(row_count);
        let mut times = Vec::with_capacity(row_count);
        let mut costs = Vec::with_capacity(row_count);
        let mut errors: Vec<u8> = Vec::with_capacity(row_count);
        let mut centroids: Vec<u32> = Vec::with_capacity(row_count);
        let mut codes: Vec<u8> = Vec::with_capacity(row_count * pq_m);
        let mut rerank_bytes: Vec<u8> = Vec::with_capacity(row_count * bpv);

        for r in &rows {
            event_ids.push(r.event.event_id.clone());
            tenant_ids.push(r.event.tenant_id.clone());
            event_names.push(r.event.event_name.clone());
            bodies.push(r.event.body.clone());
            times.push(r.event.event_time);
            costs.push(r.event.cost);
            errors.push(u8::from(r.event.error));
            centroids.push(r.centroid);
            codes.extend_from_slice(&r.code);
            rerank_bytes.extend_from_slice(&rerank.encode_vector(&r.vector)?);
        }

        // --- persisted centroid marks ---
        let mut ranges: Vec<CentroidRange> = Vec::new();
        let mut i = 0usize;
        while i < row_count {
            let c = centroids[i];
            let start = i;
            let mut tmin = i64::MAX;
            let mut tmax = i64::MIN;
            while i < row_count && centroids[i] == c {
                tmin = tmin.min(times[i]);
                tmax = tmax.max(times[i]);
                i += 1;
            }
            let n = i - start;
            ranges.push(CentroidRange {
                centroid: c,
                first_row: start,
                row_count: n,
                pq_offset: (start * pq_m) as u64,
                pq_len: n * pq_m,
                rerank_offset: (start * bpv) as u64,
                rerank_len: n * bpv,
                time_min: tmin,
                time_max: tmax,
            });
        }

        // --- zone maps / membership ---
        let time_min = *times.iter().min().unwrap();
        let time_max = *times.iter().max().unwrap();
        let tenants: Vec<String> = tenant_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let cost_min = costs.iter().cloned().fold(f64::INFINITY, f64::min);
        let cost_max = costs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let has_error = errors.contains(&1);
        let has_success = errors.contains(&0);

        // --- serialize columns to their logical byte streams ---
        let (eid_dat, eid_off) = io::encode_strings(&event_ids);
        let (tid_dat, tid_off) = io::encode_strings(&tenant_ids);
        let (nam_dat, nam_off) = io::encode_strings(&event_names);
        let (bod_dat, bod_off) = io::encode_strings(&bodies);

        let logical: Vec<(&str, &str, Vec<u8>)> = vec![
            ("pq_codes", "pq.codes", codes),
            ("rerank_vectors", "rerank.vec", rerank_bytes),
            ("centroid", "centroid.u32", io::encode_u32(&centroids)),
            ("event_time", "event_time.i64", io::encode_i64(&times)),
            ("cost", "cost.f64", io::encode_f64(&costs)),
            ("error", "error.u8", errors),
            ("event_id.data", "event_id.dat", eid_dat),
            ("event_id.offsets", "event_id.off", eid_off),
            ("tenant_id.data", "tenant_id.dat", tid_dat),
            ("tenant_id.offsets", "tenant_id.off", tid_off),
            ("event_name.data", "event_name.dat", nam_dat),
            ("event_name.offsets", "event_name.off", nam_off),
            ("body.data", "body.dat", bod_dat),
            ("body.offsets", "body.off", bod_off),
        ];

        // --- frame each column into checksummed blocks ---
        let mut framed: Vec<(&str, Vec<u8>)> = Vec::with_capacity(logical.len());
        let mut columns: Vec<ColumnMeta> = Vec::with_capacity(logical.len());
        for (name, file, bytes) in &logical {
            let (file_bytes, blocks) = format::frame_column(bytes);
            columns.push(ColumnMeta {
                name: (*name).to_string(),
                file: (*file).to_string(),
                codec_id: CODEC_RAW,
                storage: ColumnStorage::Framed {
                    logical_bytes: bytes.len() as u64,
                    blocks,
                },
            });
            framed.push((file, file_bytes));
        }

        // The part id is derived from what is *in* the part, so writing the same
        // rows under the same generation twice yields the same name. The
        // sequence prefix keeps parts sortable by publication order and keeps
        // two distinct batches with identical content from colliding.
        let fingerprint: Vec<u8> = columns
            .iter()
            .flat_map(|c| {
                let mut v = c.name.as_bytes().to_vec();
                if let ColumnStorage::Framed { blocks, .. } = &c.storage {
                    for b in blocks {
                        v.extend_from_slice(&b.crc32.to_le_bytes());
                        v.extend_from_slice(&b.payload_len.to_le_bytes());
                    }
                }
                v
            })
            .chain(generation_id.as_bytes().iter().copied())
            .collect();
        let part_id = format!("p{:08}-{}", seq, content_id(&fingerprint));

        let manifest = PartManifest {
            format_version: FORMAT_VERSION,
            part_id: part_id.clone(),
            generation_id: generation_id.to_string(),
            model_id: model_id.to_string(),
            model_version: model_version.to_string(),
            row_count,
            dim,
            pq_m,
            rerank,
            time_min,
            time_max,
            tenants,
            cost_min,
            cost_max,
            has_error,
            has_success,
            centroid_ranges: ranges,
            columns,
            created_at_ms: now_ms,
        };

        // We just built it; if it does not validate, that is our bug, and it must
        // not reach the disk where it becomes someone else's corruption.
        manifest.validate_structure()?;
        let manifest_bytes = manifest.encode()?;

        // --- durable write, then one rename ---
        let final_dir = parts_dir.join(&part_id);
        let tmp_dir = io::tmp_path(&final_dir);
        if tmp_dir.exists() {
            fs::remove_dir_all(&tmp_dir)?;
        }
        io::ensure_dir(&tmp_dir)?;

        use std::io::Write;
        for (file, bytes) in &framed {
            let path = tmp_dir.join(file);
            let mut f = File::create(&path)?;
            f.write_all(bytes)?;
            faults::maybe_kill("part.after_write_before_fsync");
            f.sync_all()?;
        }
        {
            let path = tmp_dir.join(MANIFEST_FILE);
            let mut f = File::create(&path)?;
            f.write_all(&manifest_bytes)?;
            f.sync_all()?;
        }
        io::fsync_dir(&tmp_dir)?;
        faults::maybe_kill("part.after_fsync_before_rename");

        if final_dir.exists() {
            // Idempotent republication of byte-identical content. Immutability
            // means we must not overwrite it; the existing part already is it.
            fs::remove_dir_all(&tmp_dir)?;
        } else {
            fs::rename(&tmp_dir, &final_dir)?;
            io::fsync_dir(parts_dir)?;
        }
        faults::maybe_kill("part.after_rename_before_snapshot");

        Ok(manifest)
    }
}

/// Reads a part, in either format.
///
/// Opens column files lazily so that a part eliminated by pruning costs exactly
/// one manifest read — and so `parts_opened` in the counters means what it says.
pub struct PartReader {
    pub manifest: PartManifest,
    dir: PathBuf,
}

impl PartReader {
    /// Load only the manifest, and validate everything about it that does not
    /// require reading a column. This is what pruning uses.
    pub fn open(dir: &Path) -> Result<Self> {
        let binary = dir.join(MANIFEST_FILE);
        let legacy = dir.join(LEGACY_MANIFEST_FILE);

        // A part announces its format by which manifest it carries. We never
        // guess: no manifest at all is not "probably fine", it is not a part.
        let manifest = if binary.exists() {
            let bytes = io::read_file(&binary)?;
            PartManifest::decode(&bytes)?
        } else if legacy.exists() {
            let bytes = io::read_file(&legacy)?;
            legacy_v1::decode(&bytes)?
        } else {
            return Err(PrismError::Corrupt(format!(
                "{} holds no part manifest ({MANIFEST_FILE} or {LEGACY_MANIFEST_FILE})",
                dir.display()
            )));
        };

        manifest.validate_structure()?;

        Ok(PartReader {
            manifest,
            dir: dir.to_path_buf(),
        })
    }

    pub fn is_legacy(&self) -> bool {
        self.manifest.format_version != FORMAT_VERSION
    }

    fn column_file(&self, column: &str) -> Result<(File, &ColumnMeta)> {
        let c = self.manifest.column(column)?;
        let f = File::open(self.dir.join(&c.file))?;
        Ok((f, c))
    }

    /// Read a logical byte range from a column, verifying every block it touches.
    ///
    /// This is the one road into column bytes. Framed (v2) columns fetch only the
    /// blocks that overlap the range — which is what makes "we read only the
    /// ranges we selected" a fact about the syscalls. Unframed (v1) columns have
    /// no block directory, so the whole file is read and checked; that asymmetry
    /// *is* the upgrade, and it is why the merge that rewrites a v1 part into a
    /// v2 part is worth doing.
    pub fn read_range(&self, column: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        let (f, c) = self.column_file(column)?;
        let logical_bytes = c.storage.logical_bytes();

        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| PrismError::Corrupt("byte range overflows".into()))?;
        if end > logical_bytes {
            return Err(PrismError::Corrupt(format!(
                "part {} column `{column}`: range {offset}..{end} runs past the {logical_bytes} \
                 bytes in the column",
                self.manifest.part_id
            )));
        }

        match &c.storage {
            ColumnStorage::Unframed { bytes, crc32: want } => {
                let all = io::read_file(&self.dir.join(&c.file))?;
                if all.len() != *bytes {
                    return Err(PrismError::Corrupt(format!(
                        "part {} column `{column}` is {} bytes on disk, manifest says {bytes}",
                        self.manifest.part_id,
                        all.len()
                    )));
                }
                let actual = crc32(&all);
                if actual != *want {
                    return Err(PrismError::Corrupt(format!(
                        "part {} column `{column}` failed checksum: expected {want:#010x}, got \
                         {actual:#010x}",
                        self.manifest.part_id
                    )));
                }
                Ok(all[offset as usize..end as usize].to_vec())
            }
            ColumnStorage::Framed { blocks, .. } => {
                let (first, last) = format::blocks_for_range(offset, len);
                if last >= blocks.len() {
                    return Err(PrismError::Corrupt(format!(
                        "part {} column `{column}`: range needs block {last}, but the column has {}",
                        self.manifest.part_id,
                        blocks.len()
                    )));
                }

                // A truncated file must name itself. Letting `read_exact_at` fail
                // would surface "failed to fill whole buffer" — a generic io
                // error that tells an operator nothing about which bytes are
                // missing, and which is exactly what the S1 gate forbids.
                let on_disk = f.metadata()?.len();

                let mut out = Vec::with_capacity(len);
                for (i, b) in blocks.iter().enumerate().take(last + 1).skip(first) {
                    let need = b
                        .file_offset
                        .checked_add((FRAME_HEADER_BYTES + b.payload_len as usize) as u64)
                        .ok_or_else(|| {
                            PrismError::Corrupt(format!(
                                "part {} column `{column}` block {i}: byte range overflows",
                                self.manifest.part_id
                            ))
                        })?;
                    if need > on_disk {
                        return Err(PrismError::Corrupt(format!(
                            "part {} column `{column}` is truncated: block {i} needs bytes \
                             {}..{need} of `{}`, but the file is only {on_disk} bytes",
                            self.manifest.part_id, b.file_offset, c.file
                        )));
                    }

                    let raw = io::read_range(
                        &f,
                        b.file_offset,
                        FRAME_HEADER_BYTES + b.payload_len as usize,
                    )?;
                    let payload = format::read_block(&raw, i, b, column, &self.manifest.part_id)?;

                    // Slice the requested window out of this block.
                    let block_start = (i as u64) * BLOCK_SIZE as u64;
                    let from = offset.saturating_sub(block_start) as usize;
                    let to = ((end - block_start) as usize).min(payload.len());
                    if from < payload.len() {
                        out.extend_from_slice(&payload[from..to]);
                    }
                }
                Ok(out)
            }
        }
    }

    /// Read a whole column, verifying every block.
    pub fn read_column_checked(&self, column: &str) -> Result<Vec<u8>> {
        let c = self.manifest.column(column)?;
        let n = c.storage.logical_bytes();
        self.read_range(column, 0, n as usize)
    }

    /// Fetch exactly the PQ bytes for one centroid range. Nothing else is read.
    pub fn read_pq_range(&self, r: &CentroidRange) -> Result<Vec<u8>> {
        self.read_range("pq_codes", r.pq_offset, r.pq_len)
    }

    /// Fetch the exact vectors for specific rows — the rerank tier. Bounded by
    /// the caller's declared budget, never by "however many candidates there
    /// were". Decoded through the part's declared rerank encoding, never by
    /// assuming what those bytes are.
    pub fn read_vectors_for_rows(&self, rows: &[usize]) -> Result<Vec<Vec<f32>>> {
        let dim = self.manifest.dim;
        let bpv = self.manifest.bytes_per_vector()?;
        let mut out = Vec::with_capacity(rows.len());
        for &row in rows {
            if row >= self.manifest.row_count {
                return Err(PrismError::Corrupt(format!(
                    "row {row} is out of range for part {} ({} rows)",
                    self.manifest.part_id, self.manifest.row_count
                )));
            }
            let bytes = self.read_range("rerank_vectors", (row * bpv) as u64, bpv)?;
            out.push(self.manifest.rerank.decode_vector(&bytes, dim)?);
        }
        Ok(out)
    }

    /// Just the `event_id`s of specific rows.
    ///
    /// Ranking ties are broken on `event_id`, so the ranker needs identities
    /// without paying for bodies — and without the order of a result depending
    /// on which part a row physically happens to live in.
    pub fn read_event_ids_for_rows(&self, rows: &[usize]) -> Result<Vec<String>> {
        let n = self.manifest.row_count;
        let data = self.read_column_checked("event_id.data")?;
        let offs = io::string_offsets(&self.read_column_checked("event_id.offsets")?, n)?;
        rows.iter()
            .map(|&r| Ok(io::string_at(&data, &offs, r, n)?.to_string()))
            .collect()
    }

    /// The scalar columns the fused filter mask needs, without touching text and
    /// without allocating a `String` per row.
    ///
    /// The mask runs once per *scanned row*, so it must not allocate. Decoding a
    /// million tenant ids into a million `String`s to compare each against one
    /// literal would cost more than the scan it is filtering.
    pub fn read_scalars(&self) -> Result<Scalars> {
        let n = self.manifest.row_count;
        Ok(Scalars {
            times: io::decode_i64(&self.read_column_checked("event_time")?),
            tenant_data: self.read_column_checked("tenant_id.data")?,
            tenant_offs: io::string_offsets(&self.read_column_checked("tenant_id.offsets")?, n)?,
            row_count: n,
        })
    }

    /// Materialize specific rows as events. Only survivors pay for their text.
    ///
    /// Emphatically *not* "decode the part and index into it". A top-10 over a
    /// million-row part must cost ten rows of text, not a million — otherwise
    /// every query is secretly a full scan of the widest column in the store, and
    /// all the pruning upstream of it was for nothing.
    pub fn read_events_for_rows(&self, rows: &[usize]) -> Result<Vec<Event>> {
        let n = self.manifest.row_count;

        let times = io::decode_i64(&self.read_column_checked("event_time")?);
        let costs = io::decode_f64(&self.read_column_checked("cost")?);
        let errors = self.read_column_checked("error")?;

        // The offset arrays are small (8 bytes/row); the blobs are borrowed and
        // only the requested rows are ever turned into a String.
        let load = |base: &str| -> Result<(Vec<u8>, Vec<i64>)> {
            let d = self.read_column_checked(&format!("{base}.data"))?;
            let o = io::string_offsets(&self.read_column_checked(&format!("{base}.offsets"))?, n)?;
            Ok((d, o))
        };
        let (eid_d, eid_o) = load("event_id")?;
        let (tid_d, tid_o) = load("tenant_id")?;
        let (nam_d, nam_o) = load("event_name")?;
        let (bod_d, bod_o) = load("body")?;

        let mut out = Vec::with_capacity(rows.len());
        for &r in rows {
            if r >= n {
                return Err(PrismError::Corrupt(format!(
                    "row {r} is out of range for part {} ({n} rows)",
                    self.manifest.part_id
                )));
            }
            out.push(Event {
                event_id: io::string_at(&eid_d, &eid_o, r, n)?.to_string(),
                tenant_id: io::string_at(&tid_d, &tid_o, r, n)?.to_string(),
                event_time: times[r],
                event_name: io::string_at(&nam_d, &nam_o, r, n)?.to_string(),
                cost: costs[r],
                error: errors[r] == 1,
                body: io::string_at(&bod_d, &bod_o, r, n)?.to_string(),
            });
        }
        Ok(out)
    }

    /// Decode the entire part, verifying every checksum. Used by merge,
    /// re-embed, `verify`, `fsck`, and the exact oracle.
    pub fn read_all(&self) -> Result<PartRows> {
        let m = &self.manifest;
        let n = m.row_count;

        let codes = self.read_column_checked("pq_codes")?;
        let centroids = io::decode_u32(&self.read_column_checked("centroid")?);
        let times = io::decode_i64(&self.read_column_checked("event_time")?);
        let costs = io::decode_f64(&self.read_column_checked("cost")?);
        let errors = self.read_column_checked("error")?;

        let bpv = m.bytes_per_vector()?;
        let raw = self.read_column_checked("rerank_vectors")?;
        let mut vectors = Vec::with_capacity(n * m.dim);
        for i in 0..n {
            vectors.extend_from_slice(
                &m.rerank
                    .decode_vector(&raw[i * bpv..(i + 1) * bpv], m.dim)?,
            );
        }

        let load_str = |base: &str| -> Result<Vec<String>> {
            let d = self.read_column_checked(&format!("{base}.data"))?;
            let o = self.read_column_checked(&format!("{base}.offsets"))?;
            io::decode_strings(&d, &o, n)
        };
        let event_ids = load_str("event_id")?;
        let tenant_ids = load_str("tenant_id")?;
        let event_names = load_str("event_name")?;
        let bodies = load_str("body")?;

        let events = (0..n)
            .map(|i| Event {
                event_id: event_ids[i].clone(),
                tenant_id: tenant_ids[i].clone(),
                event_time: times[i],
                event_name: event_names[i].clone(),
                cost: costs[i],
                error: errors[i] == 1,
                body: bodies[i].clone(),
            })
            .collect();

        Ok(PartRows {
            events,
            centroids,
            codes,
            vectors,
        })
    }

    /// The integrity audit: every stored byte, *and* every stored structure.
    ///
    /// Checksums alone are not enough, and the `bad-offsets` compat fixture is
    /// why. A string-offset array can be perfectly checksum-valid and still claim
    /// a row starts 1 TiB into a 4 KiB blob — the CRC only proves the bytes are
    /// the bytes we wrote, not that they mean anything. So the audit decodes the
    /// part as well as checksumming it, which forces every untrusted length
    /// through the bounds checks in `decode_strings`.
    ///
    /// `open` deliberately does none of this: paying a full decode on every part
    /// at query time would make pruning pointless.
    pub fn verify(&self) -> Result<()> {
        self.manifest.validate_structure()?;
        for c in &self.manifest.columns {
            self.read_column_checked(&c.name)?;
        }
        self.read_all()?;
        Ok(())
    }
}
