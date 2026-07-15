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
    self, BlockRef, Extension, Header, RerankDescriptor, BLOCK_SIZE, CODEC_RAW, FEATURE_ATTRIBUTES,
    FEATURE_BLOCK_FRAMING, FEATURE_TRACE_CONTEXT, FORMAT_VERSION, FRAME_HEADER_BYTES,
    RESERVED_WORDS, SUPPORTED_EXTENSIONS,
};
use crate::io;
use crate::legacy_v1;
use prism_types::attributes::Attributes;
use prism_types::error::{PrismError, Result};
use prism_types::event::Event;
use prism_types::hash::{content_id, crc32};
use prism_types::limits::MAX_ATTRIBUTE_KEY_CARDINALITY;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// v2+: checksummed blocks. One flipped byte condemns one block, and the
    /// error names it.
    ///
    /// `block_size` is stored per column, not assumed from a global constant. That
    /// is what makes the constant a *default* rather than a law: a part written at
    /// one block size stays readable forever, whatever the default later becomes —
    /// and it is what let the block size be *derived from measurement* at all
    /// (charter amendment C-1), because a store could actually be built at each
    /// candidate size and queried.
    Framed {
        logical_bytes: u64,
        block_size: u32,
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

    pub fn block_size(&self) -> u32 {
        match self {
            ColumnStorage::Unframed { .. } => BLOCK_SIZE,
            ColumnStorage::Framed { block_size, .. } => *block_size,
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

    /// The features this part actually uses. Carried in the header, mirrored here
    /// so a reader above the format layer can ask what a part has without
    /// re-parsing bytes.
    #[serde(default)]
    pub feature_flags: u64,

    /// **The bounded attribute key dictionary** (S2, directive 1).
    ///
    /// Keys are dictionary-encoded, and the dictionary lives in the *manifest* —
    /// not in a column — for one reason: it makes "does this part contain any row
    /// with key X?" answerable without opening a single column file. That is the
    /// same trick the tenant list and the zone maps play, and it is why attribute
    /// pruning will be free when S4 comes to build it.
    ///
    /// Bounded at [`MAX_ATTRIBUTE_KEY_CARDINALITY`]. A part that exceeds it is
    /// refused: an unbounded dictionary carried in every manifest is exactly how a
    /// columnar format dies.
    #[serde(default)]
    pub attribute_keys: Vec<String>,

    /// TLV extensions (directive 2). The mechanism ships before its first user so
    /// that the first user is not also a format break.
    #[serde(default)]
    pub extensions: Vec<Extension>,

    /// Reserved fixed words. Must be zero. A future fixed-width field lands here
    /// guarded by a feature bit, without a version bump.
    #[serde(default)]
    pub reserved: [u64; RESERVED_WORDS],
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

    /// Does this part carry this column at all?
    ///
    /// A v1 or v2 part has no `observed_time`, no trace context and no attributes,
    /// because those columns did not exist when it was written. That is not
    /// corruption — it is history, and a reader that cannot tell the difference
    /// will condemn perfectly good data.
    pub fn has_column(&self, name: &str) -> bool {
        self.columns.iter().any(|c| c.name == name)
    }

    pub fn has_attributes(&self) -> bool {
        self.feature_flags & FEATURE_ATTRIBUTES != 0 && self.has_column("attributes.data")
    }

    /// The S4 extension: partition key, per-tenant scoped stats, promoted columns.
    ///
    /// Decoded on demand rather than stored, because most readers never need it and the ones
    /// that do need it once.
    /// Lineage + retention (S5). Absent means: an ordinary part, bodies intact.
    pub fn s5(&self) -> Result<crate::ext::S5Ext> {
        match self
            .extensions
            .iter()
            .find(|e| e.id == crate::ext::EXT_S5_LINEAGE)
        {
            Some(e) => crate::ext::S5Ext::decode(&e.bytes),
            None => Ok(crate::ext::S5Ext::default()),
        }
    }

    pub fn s4(&self) -> Result<crate::ext::S4Ext> {
        match self
            .extensions
            .iter()
            .find(|e| e.id == crate::ext::EXT_S4_PARTITION)
        {
            Some(e) => crate::ext::S4Ext::decode(&e.bytes),
            None => Ok(crate::ext::S4Ext::default()),
        }
    }

    pub fn has_trace_context(&self) -> bool {
        self.feature_flags & FEATURE_TRACE_CONTEXT != 0 && self.has_column("trace_id.data")
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
                    block_size,
                    blocks,
                } => {
                    b.u64(*logical_bytes);
                    b.u32(*block_size);
                    b.len(blocks.len());
                    for blk in blocks {
                        b.u64(blk.file_offset);
                        b.u32(blk.payload_len);
                        b.u32(blk.crc32);
                    }
                }
                ColumnStorage::Unframed { .. } => {
                    return Err(PrismError::Invariant(
                        "refusing to write an unframed column: binary parts are always framed"
                            .into(),
                    ));
                }
            }
        }

        // --- v3 additions ---
        b.len(self.attribute_keys.len());
        for k in &self.attribute_keys {
            b.string(k);
        }
        for w in &self.reserved {
            b.u64(*w);
        }
        b.len(self.extensions.len());
        for e in &self.extensions {
            b.u16(e.id);
            b.len(e.bytes.len());
            b.buf.extend_from_slice(&e.bytes);
        }

        let body = b.buf;
        let header = Header {
            format_version: FORMAT_VERSION,
            byte_order: format::BYTE_ORDER_LITTLE,
            feature_flags: self.feature_flags,
            body_len: body.len() as u32,
            body_crc32: crc32(&body),
        };
        let mut out = header.encode();
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Decode a binary manifest, dispatching on the declared version.
    ///
    /// Every length is checked against the bytes present before anything is
    /// reserved (see `format::Cursor`). v2 and v3 share a header and diverge in the
    /// body — which is precisely why the header is parsed, and its version and
    /// feature bits believed, *before* a single body byte is.
    pub fn decode(bytes: &[u8]) -> Result<PartManifest> {
        let (header, body) = format::split_manifest(bytes)?;
        Self::decode_body(&header, body)
    }

    fn decode_body(header: &Header, body: &[u8]) -> Result<PartManifest> {
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
            // v2 had no per-column block size: every v2 column is BLOCK_SIZE.
            let block_size = if header.format_version >= 3 {
                c.u32()?
            } else {
                BLOCK_SIZE
            };
            if block_size == 0 || !block_size.is_power_of_two() {
                return Err(PrismError::Corrupt(format!(
                    "column `{name}` declares a block size of {block_size}, which is not a \
                     positive power of two"
                )));
            }
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
                    block_size,
                    blocks,
                },
            });
        }

        // --- v3 additions. A v2 part simply does not carry them. ---
        let mut attribute_keys = Vec::new();
        let mut reserved = [0u64; RESERVED_WORDS];
        let mut extensions: Vec<Extension> = Vec::new();

        if header.format_version >= 3 {
            let n_keys = c.read_len(4, "attribute keys")?;
            if n_keys > MAX_ATTRIBUTE_KEY_CARDINALITY {
                return Err(PrismError::Corrupt(format!(
                    "part declares {n_keys} attribute keys, over the {MAX_ATTRIBUTE_KEY_CARDINALITY} \
                     cardinality bound; an unbounded key dictionary in every manifest is how a \
                     columnar format dies"
                )));
            }
            attribute_keys = Vec::with_capacity(n_keys);
            for _ in 0..n_keys {
                attribute_keys.push(c.string()?);
            }

            for w in reserved.iter_mut() {
                *w = c.u64()?;
            }
            // A non-zero reserved word means a writer put something here that this
            // build cannot interpret. Refuse rather than ignore: ignoring is how a
            // reader silently drops a field that changed what the data means.
            if reserved.iter().any(|w| *w != 0) {
                return Err(PrismError::Corrupt(
                    "part sets a reserved manifest word this build does not understand".into(),
                ));
            }

            let n_ext = c.read_len(6, "extensions")?;
            for _ in 0..n_ext {
                let id = c.u16()?;
                let len = c.read_len(1, "extension bytes")?;
                let mut bytes = vec![0u8; 0];
                bytes.reserve_exact(len);
                for _ in 0..len {
                    bytes.push(c.u8()?);
                }
                let ext = Extension { id, bytes };

                // The required bit is the whole point of the TLV scheme: a reader
                // that does not know a *required* extension must refuse the part,
                // and may safely skip an optional one.
                if ext.is_required() && !SUPPORTED_EXTENSIONS.contains(&id) {
                    return Err(PrismError::Corrupt(format!(
                        "part carries required extension {id:#06x}, which this build does not \
                         implement; refusing rather than reading it as if the extension were not \
                         there"
                    )));
                }
                extensions.push(ext);
            }
        }

        Ok(PartManifest {
            format_version: header.format_version,
            feature_flags: header.feature_flags,
            attribute_keys,
            extensions,
            reserved,
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
                block_size,
                blocks,
            } = &c.storage
            {
                let bs = *block_size;
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
                    logical_bytes.div_ceil(bs as u64) as usize
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
                    if !is_last && b.payload_len != bs {
                        return Err(PrismError::Corrupt(format!(
                            "part {} column `{}` block {i} is {} bytes; only the last block \
                             may be short",
                            self.part_id, c.name, b.payload_len
                        )));
                    }
                    if b.payload_len > bs {
                        return Err(PrismError::Corrupt(format!(
                            "part {} column `{}` block {i} claims {} bytes, over the {bs}-byte \
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
        if self.attribute_keys.len() > MAX_ATTRIBUTE_KEY_CARDINALITY {
            return Err(PrismError::Corrupt(format!(
                "part {} carries {} attribute keys, over the {MAX_ATTRIBUTE_KEY_CARDINALITY} bound",
                self.part_id,
                self.attribute_keys.len()
            )));
        }
        if self.has_column("observed_time") {
            expect("observed_time", checked(rows, 8, "observed times")?)?;
        }
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
    id_data: Vec<u8>,
    id_offs: Vec<i64>,
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

    /// This row's event id, borrowed in place.
    ///
    /// The scan needs it, not just the rerank, and that is not an optimization detail — it is
    /// the ordering contract. `(score DESC, event_id ASC)` is a total order on the *data*, and
    /// a bounded candidate heap that breaks distance ties on physical position decides *which
    /// tied rows are allowed to be answers* by where they happen to be stored. Two stores with
    /// identical rows would then answer the same query differently. See D-033.
    pub fn event_id_at(&self, row: usize) -> &str {
        io::string_at(&self.id_data, &self.id_offs, row, self.row_count).unwrap_or("")
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

/// Everything about *where* and *how* a part is written, beyond its rows.
#[derive(Clone, Debug, Default)]
pub struct PartSpec {
    /// The outer partition this part belongs to (S4).
    pub partition: Option<crate::partition::PartitionKey>,
    /// Attribute keys to promote to typed columns.
    ///
    /// A promoted key is **removed from the attribute map** in this part. Storing it twice
    /// would make promotion cost storage rather than save it, and leave two sources of truth
    /// for one value.
    pub promote: Vec<String>,
    /// Lineage and retention (S5). Default = a part written by an ordinary ingest, from bodies
    /// that are still here.
    pub lineage: crate::ext::S5Ext,
}

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
        block_size: u32,
        spec: &PartSpec,
        mut rows: Vec<RowIn>,
        now_ms: i64,
    ) -> Result<PartManifest> {
        if block_size == 0 || !block_size.is_power_of_two() {
            return Err(PrismError::Invalid(format!(
                "block size {block_size} is not a positive power of two"
            )));
        }
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
        let mut observed = Vec::with_capacity(row_count);
        let mut trace_ids = Vec::with_capacity(row_count);
        let mut span_ids = Vec::with_capacity(row_count);
        let mut idem_keys = Vec::with_capacity(row_count);
        let mut costs = Vec::with_capacity(row_count);
        let mut errors: Vec<u8> = Vec::with_capacity(row_count);
        let mut centroids: Vec<u32> = Vec::with_capacity(row_count);
        let mut codes: Vec<u8> = Vec::with_capacity(row_count * pq_m);
        let mut rerank_bytes: Vec<u8> = Vec::with_capacity(row_count * bpv);
        let mut attr_rows: Vec<Attributes> = Vec::with_capacity(row_count);

        for r in &rows {
            event_ids.push(r.event.event_id.clone());
            tenant_ids.push(r.event.tenant_id.clone());
            event_names.push(r.event.event_name.clone());
            bodies.push(r.event.body.clone());
            times.push(r.event.event_time);
            observed.push(r.event.observed_time);
            trace_ids.push(r.event.trace_id.clone());
            span_ids.push(r.event.span_id.clone());
            idem_keys.push(r.event.idempotency_key.clone().unwrap_or_default());
            costs.push(r.event.cost);
            errors.push(u8::from(r.event.error));
            centroids.push(r.centroid);
            codes.extend_from_slice(&r.code);
            rerank_bytes.extend_from_slice(&rerank.encode_vector(&r.vector)?);
            attr_rows.push(r.event.attributes.clone());
        }

        // --- the bounded attribute key dictionary (directive 1) ---
        //
        // Built from the rows actually present, so a part's dictionary is exactly
        // the keys it uses and nothing else. The cardinality bound is enforced at
        // admission, long before here; this is the backstop that makes it a
        // property of the *format* and not merely of the ingest path, because a
        // part written by any other code path must obey it too.
        // --- promotion (issue #2, directive 4) ---
        //
        // A promoted key is REMOVED from the attribute map and written as its own typed
        // column. The two representations therefore coexist across parts of different ages --
        // an old part has the key in its map, a new one has it in a column -- and every reader
        // must dispatch on which. That dispatch is what the S4 gate test hammers: the same
        // query over a promoted key must return identical rows and identical logical counters
        // whichever representation it lands on.
        let mut promoted: Vec<crate::ext::PromotedColumn> = Vec::new();
        let mut promoted_values: Vec<Vec<Option<prism_types::AttrValue>>> = Vec::new();
        for key in &spec.promote {
            let mut col: Vec<Option<prism_types::AttrValue>> = Vec::with_capacity(row_count);
            let mut tag: Option<u8> = None;
            let mut any = false;
            for a in attr_rows.iter_mut() {
                match a.remove(key) {
                    Some(v) => {
                        // A promoted column is TYPED. If a key is used with two different types
                        // across rows, it is not a column -- it is a map entry pretending to be
                        // one, and promoting it would silently coerce or drop values.
                        match tag {
                            None => tag = Some(v.type_tag()),
                            Some(t) if t != v.type_tag() => {
                                return Err(PrismError::Invariant(format!(
                                    "cannot promote attribute `{key}`: it is used with more than one \
                                     value type in this part, so it has no column type"
                                )));
                            }
                            _ => {}
                        }
                        any = true;
                        col.push(Some(v));
                    }
                    None => col.push(None),
                }
            }
            if !any {
                continue; // nothing to promote in this part
            }
            promoted.push(crate::ext::PromotedColumn {
                key: key.clone(),
                column: crate::ext::PromotedColumn::column_for(key),
                type_tag: tag.unwrap_or(prism_types::attributes::ATTR_TYPE_STR),
            });
            promoted_values.push(col);
        }

        let key_set: BTreeSet<String> = attr_rows.iter().flat_map(|a| a.keys().cloned()).collect();
        if key_set.len() > MAX_ATTRIBUTE_KEY_CARDINALITY {
            return Err(PrismError::Invariant(format!(
                "part would carry {} distinct attribute keys, over the \
                 {MAX_ATTRIBUTE_KEY_CARDINALITY} cardinality bound",
                key_set.len()
            )));
        }
        let attribute_keys: Vec<String> = key_set.into_iter().collect();
        let dict: BTreeMap<String, u32> = attribute_keys
            .iter()
            .enumerate()
            .map(|(i, k)| (k.clone(), i as u32))
            .collect();

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

        // --- per-tenant scoped statistics (directive 3) ---
        //
        // In a SHARED bucket, part-level metadata describes the bucket, not the tenant: one
        // time range, one cost range, one union key dictionary. Every one of those tells tenant
        // A something about tenant B. So the metadata a query can observe is scoped per tenant,
        // and a query only ever reads its own section.
        let mut per_tenant: BTreeMap<String, crate::ext::TenantStats> = BTreeMap::new();
        for (i, r) in rows.iter().enumerate() {
            let e = &r.event;
            let st =
                per_tenant
                    .entry(e.tenant_id.clone())
                    .or_insert_with(|| crate::ext::TenantStats {
                        tenant: e.tenant_id.clone(),
                        rows: 0,
                        time_min: i64::MAX,
                        time_max: i64::MIN,
                        cost_min: f64::INFINITY,
                        cost_max: f64::NEG_INFINITY,
                        has_error: false,
                        has_success: false,
                        attribute_keys: Vec::new(),
                    });
            st.rows += 1;
            st.time_min = st.time_min.min(e.event_time);
            st.time_max = st.time_max.max(e.event_time);
            st.cost_min = st.cost_min.min(e.cost);
            st.cost_max = st.cost_max.max(e.cost);
            st.has_error |= e.error;
            st.has_success |= !e.error;
            for k in attr_rows[i].keys() {
                if !st.attribute_keys.iter().any(|x| x == k) {
                    st.attribute_keys.push(k.clone());
                }
            }
            // A promoted key still belongs to the tenant that used it -- otherwise promoting a
            // key would make it invisible to "does this part hold key X for me?".
            for (pi, p) in promoted.iter().enumerate() {
                if promoted_values[pi][i].is_some() && !st.attribute_keys.contains(&p.key) {
                    st.attribute_keys.push(p.key.clone());
                }
            }
        }
        for st in per_tenant.values_mut() {
            st.attribute_keys.sort();
        }

        let s4 = crate::ext::S4Ext {
            partition: spec.partition.clone(),
            tenant_stats: per_tenant.into_values().collect(),
            promoted: promoted.clone(),
        };

        let (tr_dat, tr_off) = io::encode_strings(&trace_ids);
        let (sp_dat, sp_off) = io::encode_strings(&span_ids);
        let (ik_dat, ik_off) = io::encode_strings(&idem_keys);
        let (attr_dat, attr_off) = io::encode_attributes(&attr_rows, &dict)?;

        let logical: Vec<(&str, &str, Vec<u8>)> = vec![
            ("pq_codes", "pq.codes", codes),
            ("rerank_vectors", "rerank.vec", rerank_bytes),
            ("centroid", "centroid.u32", io::encode_u32(&centroids)),
            ("event_time", "event_time.i64", io::encode_i64(&times)),
            (
                "observed_time",
                "observed_time.i64",
                io::encode_i64(&observed),
            ),
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
            ("trace_id.data", "trace_id.dat", tr_dat),
            ("trace_id.offsets", "trace_id.off", tr_off),
            ("span_id.data", "span_id.dat", sp_dat),
            ("span_id.offsets", "span_id.off", sp_off),
            ("idempotency_key.data", "idem.dat", ik_dat),
            ("idempotency_key.offsets", "idem.off", ik_off),
            ("attributes.data", "attrs.dat", attr_dat),
            ("attributes.offsets", "attrs.off", attr_off),
        ];

        // Promoted columns: a null map plus a typed value column, so `absent` stays distinct
        // from `zero` -- an attribute a row does not have is Null, and Null is not false.
        let mut promoted_files: Vec<(String, String, Vec<u8>)> = Vec::new();
        for (pi, p) in promoted.iter().enumerate() {
            let vals = &promoted_values[pi];
            let nulls: Vec<u8> = vals.iter().map(|v| u8::from(v.is_some())).collect();
            let mut data: Vec<u8> = Vec::new();
            let mut offs: Vec<i64> = Vec::with_capacity(row_count + 1);
            offs.push(0);
            for v in vals {
                if let Some(v) = v {
                    match v {
                        prism_types::AttrValue::Str(x) => data.extend_from_slice(x.as_bytes()),
                        prism_types::AttrValue::Int(x) => data.extend_from_slice(&x.to_le_bytes()),
                        prism_types::AttrValue::Double(x) => {
                            data.extend_from_slice(&x.to_bits().to_le_bytes())
                        }
                        prism_types::AttrValue::Bool(x) => data.push(u8::from(*x)),
                    }
                }
                offs.push(data.len() as i64);
            }
            let safe = p.key.replace(['/', '\\'], "_");
            promoted_files.push((
                format!("{}.nulls", p.column),
                format!("pc_{safe}.null"),
                nulls,
            ));
            promoted_files.push((format!("{}.data", p.column), format!("pc_{safe}.dat"), data));
            promoted_files.push((
                format!("{}.offsets", p.column),
                format!("pc_{safe}.off"),
                io::encode_i64(&offs),
            ));
        }
        let logical: Vec<(&str, &str, Vec<u8>)> = logical
            .into_iter()
            .chain(
                promoted_files
                    .iter()
                    .map(|(n, f, b)| (n.as_str(), f.as_str(), b.clone())),
            )
            .collect();

        // --- frame each column into checksummed blocks ---
        let mut framed: Vec<(&str, Vec<u8>)> = Vec::with_capacity(logical.len());
        let mut columns: Vec<ColumnMeta> = Vec::with_capacity(logical.len());
        for (name, file, bytes) in &logical {
            let (file_bytes, blocks) = format::frame_column(bytes, block_size);
            columns.push(ColumnMeta {
                name: (*name).to_string(),
                file: (*file).to_string(),
                codec_id: CODEC_RAW,
                storage: ColumnStorage::Framed {
                    logical_bytes: bytes.len() as u64,
                    block_size,
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
            feature_flags: FEATURE_BLOCK_FRAMING
                | FEATURE_ATTRIBUTES
                | FEATURE_TRACE_CONTEXT
                | if spec.partition.is_some() {
                    crate::format::FEATURE_PARTITION_META
                } else {
                    0
                }
                | if promoted.is_empty() {
                    0
                } else {
                    crate::format::FEATURE_PROMOTED_COLUMNS
                },
            attribute_keys,
            extensions: {
                let mut ext = Vec::new();
                if spec.partition.is_some() || !promoted.is_empty() {
                    ext.push(Extension {
                        id: crate::ext::EXT_S4_PARTITION,
                        bytes: s4.encode(),
                    });
                }
                // Only written when it says something. A part with nothing to declare declares
                // nothing -- an extension that is always present is a header field wearing a
                // costume, and it would break every pre-S5 compatibility fixture for no gain.
                if !spec.lineage.is_default() {
                    ext.push(Extension {
                        id: crate::ext::EXT_S5_LINEAGE,
                        bytes: spec.lineage.encode(),
                    });
                }
                ext
            },
            reserved: [0u64; RESERVED_WORDS],
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
    /// Bytes actually pulled off the disk by this reader.
    ///
    /// Distinct from the *logical* bytes a plan asked for, and the gap between them
    /// is the block layer's over-read: a 300-byte centroid range living inside a
    /// 64 KiB block costs 64 KiB. That gap is invisible to every logical counter,
    /// it is what the disk actually charges, and it is what decides the block size.
    io_bytes: std::cell::Cell<usize>,
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
            io_bytes: std::cell::Cell::new(0),
        })
    }

    /// Bytes this reader has physically read. Reset per part-open, and parts are
    /// opened per query, so summing this across a query's readers gives the query's
    /// true I/O.
    pub fn io_bytes(&self) -> usize {
        self.io_bytes.get()
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
                self.io_bytes.set(self.io_bytes.get() + all.len());
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
            ColumnStorage::Framed {
                blocks, block_size, ..
            } => {
                let bs = *block_size;
                let (first, last) = format::blocks_for_range(offset, len, bs);
                if last >= blocks.len() {
                    return Err(PrismError::Corrupt(format!(
                        "part {} column `{column}`: range needs block {last}, but the column has {}",
                        self.manifest.part_id,
                        blocks.len()
                    )));
                }

                // Map the column file read-only (S6). Parts are immutable, so a read-only mapping
                // is safe, and a truncated file must still name itself — letting an out-of-range
                // access `SIGBUS` would be a process death an operator cannot act on, and letting
                // `read_exact_at` fail would surface "failed to fill whole buffer", a generic io
                // error, both of which the S1 gate forbids. `Mmap::slice` bounds-checks against the
                // file's real length and returns the named truncation error before any byte past
                // the end is touched (see crates/prism-part/src/mmap.rs).
                let map = crate::mmap::Mmap::open(&f)?;

                let mut out = Vec::with_capacity(len);
                for (i, b) in blocks.iter().enumerate().take(last + 1).skip(first) {
                    let block_len = FRAME_HEADER_BYTES + b.payload_len as usize;
                    let part_id = &self.manifest.part_id;
                    let file = &c.file;
                    // The whole block is sliced, header and all, whatever window of it the caller
                    // wanted. This is the over-read, and this is where it becomes a number.
                    let raw = map.slice(b.file_offset as usize, block_len, &|| {
                        format!("part {part_id} column `{column}` block {i} of `{file}`")
                    })?;
                    self.io_bytes.set(self.io_bytes.get() + raw.len());
                    let payload =
                        format::read_block(raw, i, b, column, &self.manifest.part_id, bs)?;

                    // Slice the requested window out of this block.
                    //
                    // `bs`, not the global default. This line read `BLOCK_SIZE` until
                    // the block size became a per-column value, and it was silently
                    // correct only for as long as every column happened to be 64 KiB.
                    // The first store built at any other size returned whole blocks
                    // where a caller had asked for 256 bytes.
                    let block_start = (i as u64) * bs as u64;
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

    /// Load a **promoted** attribute column, once.
    ///
    /// The dispatch that makes promotion safe. A part that promoted `gen_ai.system` no longer
    /// has it in its attribute map; a part written before the promotion still does. The caller
    /// asks for a *key* and gets the same answer either way.
    ///
    /// Loaded **once per part**, not once per row. The first version of this re-read all three
    /// column files on every single row — which made promotion read *more* bytes than the map it
    /// was supposed to replace, and the equivalence test caught it by asserting the win rather
    /// than assuming it.
    pub fn promoted_column(&self, p: &crate::ext::PromotedColumn) -> Result<PromotedCol> {
        let n = self.manifest.row_count;
        Ok(PromotedCol {
            type_tag: p.type_tag,
            column: p.column.clone(),
            nulls: self.read_column_checked(&format!("{}.nulls", p.column))?,
            data: self.read_column_checked(&format!("{}.data", p.column))?,
            offs: io::string_offsets(
                &self.read_column_checked(&format!("{}.offsets", p.column))?,
                n,
            )?,
            row_count: n,
        })
    }
}

/// One promoted column, loaded.
pub struct PromotedCol {
    type_tag: u8,
    column: String,
    nulls: Vec<u8>,
    data: Vec<u8>,
    offs: Vec<i64>,
    row_count: usize,
}

impl PromotedCol {
    pub fn value(&self, row: usize) -> Result<Option<prism_types::AttrValue>> {
        use prism_types::attributes::*;

        let n = self.row_count;
        let p = self;
        if row >= n {
            return Err(PrismError::Corrupt(format!(
                "row {row} is out of range for promoted column `{}` ({n} rows)",
                p.column
            )));
        }
        if p.nulls.get(row).copied().unwrap_or(0) == 0 {
            // Absent, not zero. An attribute a row does not have is Null, and Null is not false.
            return Ok(None);
        }
        let (data, offs) = (&p.data, &p.offs);
        let (a, b) = (offs[row], offs[row + 1]);
        if a < 0 || b < a || b as usize > data.len() {
            return Err(PrismError::Corrupt(format!(
                "promoted column `{}` offset pair ({a}, {b}) is outside a {}-byte blob",
                p.column,
                data.len()
            )));
        }
        let raw = &data[a as usize..b as usize];

        Ok(Some(match p.type_tag {
            ATTR_TYPE_STR => AttrValue::Str(
                std::str::from_utf8(raw)
                    .map_err(|e| {
                        PrismError::Corrupt(format!(
                            "promoted column `{}` is not utf-8: {e}",
                            p.column
                        ))
                    })?
                    .to_string(),
            ),
            ATTR_TYPE_INT => {
                if raw.len() != 8 {
                    return Err(PrismError::Corrupt(format!(
                        "promoted int column `{}` row {row} is {} bytes, not 8",
                        p.column,
                        raw.len()
                    )));
                }
                AttrValue::Int(i64::from_le_bytes(raw.try_into().unwrap()))
            }
            ATTR_TYPE_DOUBLE => {
                if raw.len() != 8 {
                    return Err(PrismError::Corrupt(format!(
                        "promoted double column `{}` row {row} is {} bytes, not 8",
                        p.column,
                        raw.len()
                    )));
                }
                let d = f64::from_bits(u64::from_le_bytes(raw.try_into().unwrap()));
                if !d.is_finite() {
                    return Err(PrismError::Corrupt(format!(
                        "promoted column `{}` row {row} is not a finite number",
                        p.column
                    )));
                }
                AttrValue::Double(d)
            }
            ATTR_TYPE_BOOL => AttrValue::Bool(raw.first().copied().unwrap_or(0) != 0),
            other => {
                return Err(PrismError::Corrupt(format!(
                    "promoted column `{}` has type tag {other}, which this build cannot decode",
                    p.column
                )))
            }
        }))
    }
}

impl PartReader {
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
            id_data: self.read_column_checked("event_id.data")?,
            id_offs: io::string_offsets(&self.read_column_checked("event_id.offsets")?, n)?,
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

        // --- columns that only exist from v3 onward ---
        //
        // A v1/v2 part does not have these. It is not corrupt; it is old. Its
        // observed_time is unknowable, so we say so by mirroring event_time rather
        // than inventing a number — and its attributes are empty because it had
        // none, not because we dropped them.
        let observed = if self.manifest.has_column("observed_time") {
            io::decode_i64(&self.read_column_checked("observed_time")?)
        } else {
            times.clone()
        };
        let trace = if self.manifest.has_trace_context() {
            Some((load("trace_id")?, load("span_id")?))
        } else {
            None
        };
        let idem = if self.manifest.has_column("idempotency_key.data") {
            Some(load("idempotency_key")?)
        } else {
            None
        };
        let attrs = if self.manifest.has_attributes() {
            let d = self.read_column_checked("attributes.data")?;
            let o = io::string_offsets(&self.read_column_checked("attributes.offsets")?, n)?;
            Some((d, o))
        } else {
            None
        };
        let promoted: Vec<(String, PromotedCol)> = self
            .manifest
            .s4()?
            .promoted
            .iter()
            .map(|p| Ok((p.key.clone(), self.promoted_column(p)?)))
            .collect::<Result<Vec<_>>>()?;

        let mut out = Vec::with_capacity(rows.len());
        for &r in rows {
            if r >= n {
                return Err(PrismError::Corrupt(format!(
                    "row {r} is out of range for part {} ({n} rows)",
                    self.manifest.part_id
                )));
            }
            let (trace_id, span_id) = match &trace {
                Some(((td, to), (sd, so))) => (
                    io::string_at(td, to, r, n)?.to_string(),
                    io::string_at(sd, so, r, n)?.to_string(),
                ),
                None => (String::new(), String::new()),
            };
            let idempotency_key = match &idem {
                Some((d, o)) => {
                    let k = io::string_at(d, o, r, n)?;
                    if k.is_empty() {
                        None
                    } else {
                        Some(k.to_string())
                    }
                }
                None => None,
            };
            let mut attributes = match &attrs {
                Some((d, o)) => {
                    io::decode_attributes_at(d, o, r, n, &self.manifest.attribute_keys)?
                }
                None => Attributes::new(),
            };
            // Re-attach promoted keys. **A promoted key is still an attribute of the event.**
            // Promotion is a storage decision, not a schema change -- an event read back out of
            // a part that promoted a key must be byte-identical to the same event read out of a
            // part that did not, or every equivalence in the system quietly stops holding.
            for (key, col) in &promoted {
                if let Some(v) = col.value(r)? {
                    attributes.insert(key.clone(), v);
                }
            }

            out.push(Event {
                event_id: io::string_at(&eid_d, &eid_o, r, n)?.to_string(),
                tenant_id: io::string_at(&tid_d, &tid_o, r, n)?.to_string(),
                event_time: times[r],
                observed_time: observed[r],
                event_name: io::string_at(&nam_d, &nam_o, r, n)?.to_string(),
                cost: costs[r],
                error: errors[r] == 1,
                body: io::string_at(&bod_d, &bod_o, r, n)?.to_string(),
                trace_id,
                span_id,
                attributes,
                idempotency_key,
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

        let bpv = m.bytes_per_vector()?;
        let raw = self.read_column_checked("rerank_vectors")?;
        let mut vectors = Vec::with_capacity(n * m.dim);
        for i in 0..n {
            vectors.extend_from_slice(
                &m.rerank
                    .decode_vector(&raw[i * bpv..(i + 1) * bpv], m.dim)?,
            );
        }

        // One materialization path, so the legacy fallbacks (a v1/v2 part has no
        // observed_time, no trace context, no attributes) are written once and
        // cannot drift between the two readers.
        let all_rows: Vec<usize> = (0..n).collect();
        let events = self.read_events_for_rows(&all_rows)?;

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
