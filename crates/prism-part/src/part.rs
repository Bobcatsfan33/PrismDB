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
//! * cold — `vectors.f32` (dim*4 bytes/row). What rerank reads, for survivors
//!   only, within a declared budget. 32x larger; never on the scan path.

use crate::faults;
use crate::io;
use prism_types::error::{PrismError, Result};
use prism_types::event::Event;
use prism_types::hash::{content_id, crc32};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

/// One persisted centroid mark: where this centroid's rows live, in rows and in
/// bytes, in both tiers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CentroidRange {
    pub centroid: u32,
    pub first_row: usize,
    pub row_count: usize,
    pub pq_offset: u64,
    pub pq_len: usize,
    pub vec_offset: u64,
    pub vec_len: usize,
    /// Zone map scoped to this range, so a probe can be skipped on time alone.
    pub time_min: i64,
    pub time_max: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ColumnMeta {
    pub name: String,
    pub file: String,
    pub bytes: usize,
    pub crc32: u32,
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
    /// Declared, not assumed. A future big-endian reader must byte-swap.
    pub byte_order: String,

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
}

/// The decoded, in-memory rows of a part. Used by merge, re-embed, and the
/// exact-scan oracle — not by the query scan path, which reads ranges.
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
    /// Write an immutable part and make it visible with a single rename.
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
        let mut vectors: Vec<f32> = Vec::with_capacity(row_count * dim);

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
            vectors.extend_from_slice(&r.vector);
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
                vec_offset: (start * dim * 4) as u64,
                vec_len: n * dim * 4,
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

        // --- serialize columns ---
        let (eid_dat, eid_off) = io::encode_strings(&event_ids);
        let (tid_dat, tid_off) = io::encode_strings(&tenant_ids);
        let (nam_dat, nam_off) = io::encode_strings(&event_names);
        let (bod_dat, bod_off) = io::encode_strings(&bodies);

        let files: Vec<(&str, &str, Vec<u8>)> = vec![
            ("pq_codes", "pq.codes", codes),
            ("vectors", "vectors.f32", io::encode_f32(&vectors)),
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

        let columns: Vec<ColumnMeta> = files
            .iter()
            .map(|(name, file, bytes)| ColumnMeta {
                name: (*name).to_string(),
                file: (*file).to_string(),
                bytes: bytes.len(),
                crc32: crc32(bytes),
            })
            .collect();

        // The part id is derived from what is *in* the part, so writing the same
        // rows under the same generation twice yields the same name. The
        // sequence prefix keeps parts sortable by publication order and keeps
        // two distinct batches with identical content from colliding.
        let fingerprint: Vec<u8> = columns
            .iter()
            .flat_map(|c| {
                let mut v = c.name.as_bytes().to_vec();
                v.extend_from_slice(&c.crc32.to_le_bytes());
                v.extend_from_slice(&(c.bytes as u64).to_le_bytes());
                v
            })
            .chain(generation_id.as_bytes().iter().copied())
            .collect();
        let part_id = format!("p{:08}-{}", seq, content_id(&fingerprint));

        let manifest = PartManifest {
            format_version: crate::store::FORMAT_VERSION,
            part_id: part_id.clone(),
            generation_id: generation_id.to_string(),
            model_id: model_id.to_string(),
            model_version: model_version.to_string(),
            row_count,
            dim,
            pq_m,
            byte_order: "little".to_string(),
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

        // --- durable write, then one rename ---
        let final_dir = parts_dir.join(&part_id);
        let tmp_dir = io::tmp_path(&final_dir);
        if tmp_dir.exists() {
            fs::remove_dir_all(&tmp_dir)?;
        }
        io::ensure_dir(&tmp_dir)?;

        use std::io::Write;
        for (_, file, bytes) in &files {
            let path = tmp_dir.join(file);
            let mut f = File::create(&path)?;
            f.write_all(bytes)?;
            faults::maybe_kill("part.after_write_before_fsync");
            f.sync_all()?;
        }
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        {
            let path = tmp_dir.join("manifest.json");
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

/// Reads a part. Opens column files lazily so that a part eliminated by
/// pruning costs exactly one manifest read — and so `parts_opened` in the
/// counters means what it says.
pub struct PartReader {
    pub manifest: PartManifest,
    dir: PathBuf,
}

impl PartReader {
    /// Load only the manifest. This is what pruning uses.
    pub fn open(dir: &Path) -> Result<Self> {
        let manifest_path = dir.join("manifest.json");
        let bytes = io::read_file(&manifest_path).map_err(|e| {
            PrismError::Corrupt(format!(
                "part at {} has no readable manifest: {e}",
                dir.display()
            ))
        })?;
        let manifest: PartManifest = serde_json::from_slice(&bytes).map_err(|e| {
            PrismError::Corrupt(format!(
                "part at {} has an unparseable manifest: {e}",
                dir.display()
            ))
        })?;

        if manifest.format_version != crate::store::FORMAT_VERSION {
            return Err(PrismError::Corrupt(format!(
                "part {} is format version {}, this build reads version {}",
                manifest.part_id,
                manifest.format_version,
                crate::store::FORMAT_VERSION
            )));
        }
        if manifest.byte_order != "little" {
            return Err(PrismError::Corrupt(format!(
                "part {} declares byte order `{}`, which this build cannot read",
                manifest.part_id, manifest.byte_order
            )));
        }

        // Cheap structural check on every open: declared sizes must match the
        // rows the manifest claims. Full checksum verification is `verify`.
        let expect = |name: &str, want: usize| -> Result<()> {
            let c = manifest.column(name)?;
            if c.bytes != want {
                return Err(PrismError::Corrupt(format!(
                    "part {} column `{name}` is {} bytes, manifest implies {want}",
                    manifest.part_id, c.bytes
                )));
            }
            Ok(())
        };
        expect("pq_codes", manifest.row_count * manifest.pq_m)?;
        expect("vectors", manifest.row_count * manifest.dim * 4)?;
        expect("centroid", manifest.row_count * 4)?;
        expect("event_time", manifest.row_count * 8)?;
        expect("cost", manifest.row_count * 8)?;
        expect("error", manifest.row_count)?;

        let total: usize = manifest.centroid_ranges.iter().map(|r| r.row_count).sum();
        if total != manifest.row_count {
            return Err(PrismError::Corrupt(format!(
                "part {} centroid marks cover {total} rows but the part has {}",
                manifest.part_id, manifest.row_count
            )));
        }

        Ok(PartReader {
            manifest,
            dir: dir.to_path_buf(),
        })
    }

    fn open_file(&self, column: &str) -> Result<File> {
        let c = self.manifest.column(column)?;
        let path = self.dir.join(&c.file);
        Ok(File::open(&path)?)
    }

    /// Read a whole column and check it against the checksum in the manifest.
    pub fn read_column_checked(&self, column: &str) -> Result<Vec<u8>> {
        let c = self.manifest.column(column)?;
        let bytes = io::read_file(&self.dir.join(&c.file))?;
        if bytes.len() != c.bytes {
            return Err(PrismError::Corrupt(format!(
                "part {} column `{column}` is {} bytes on disk, manifest says {}",
                self.manifest.part_id,
                bytes.len(),
                c.bytes
            )));
        }
        let actual = crc32(&bytes);
        if actual != c.crc32 {
            return Err(PrismError::Corrupt(format!(
                "part {} column `{column}` failed checksum: expected {:#010x}, got {actual:#010x}",
                self.manifest.part_id, c.crc32
            )));
        }
        Ok(bytes)
    }

    /// Fetch exactly the PQ bytes for one centroid range. Nothing else is read.
    pub fn read_pq_range(&self, r: &CentroidRange) -> Result<Vec<u8>> {
        let f = self.open_file("pq_codes")?;
        io::read_range(&f, r.pq_offset, r.pq_len)
    }

    /// Fetch the exact vectors for specific rows — the rerank tier. Bounded by
    /// the caller's declared budget, never by "however many candidates there
    /// were".
    pub fn read_vectors_for_rows(&self, rows: &[usize]) -> Result<Vec<Vec<f32>>> {
        let f = self.open_file("vectors")?;
        let dim = self.manifest.dim;
        let mut out = Vec::with_capacity(rows.len());
        for &row in rows {
            if row >= self.manifest.row_count {
                return Err(PrismError::Corrupt(format!(
                    "row {row} is out of range for part {} ({} rows)",
                    self.manifest.part_id, self.manifest.row_count
                )));
            }
            let bytes = io::read_range(&f, (row * dim * 4) as u64, dim * 4)?;
            out.push(io::decode_f32(&bytes));
        }
        Ok(out)
    }

    /// Just the `event_id`s of specific rows.
    ///
    /// Ranking ties are broken on `event_id`, so the ranker needs identities
    /// without paying for bodies — and without the order of a result depending
    /// on which part a row physically happens to live in.
    pub fn read_event_ids_for_rows(&self, rows: &[usize]) -> Result<Vec<String>> {
        let data = self.read_column_checked("event_id.data")?;
        let offs = self.read_column_checked("event_id.offsets")?;
        let ids = io::decode_strings(&data, &offs, self.manifest.row_count)?;
        rows.iter()
            .map(|&r| {
                ids.get(r).cloned().ok_or_else(|| {
                    PrismError::Corrupt(format!(
                        "row {r} is out of range for part {}",
                        self.manifest.part_id
                    ))
                })
            })
            .collect()
    }

    /// The scalar columns needed to evaluate a filter, without touching text.
    pub fn read_scalars(&self) -> Result<(Vec<i64>, Vec<String>)> {
        let times = io::decode_i64(&self.read_column_checked("event_time")?);
        let tdat = self.read_column_checked("tenant_id.data")?;
        let toff = self.read_column_checked("tenant_id.offsets")?;
        let tenants = io::decode_strings(&tdat, &toff, self.manifest.row_count)?;
        Ok((times, tenants))
    }

    /// Materialize specific rows as events. Only survivors pay for their text.
    pub fn read_events_for_rows(&self, rows: &[usize]) -> Result<Vec<Event>> {
        let all = self.read_all()?;
        let mut out = Vec::with_capacity(rows.len());
        for &r in rows {
            if r >= all.events.len() {
                return Err(PrismError::Corrupt(format!(
                    "row {r} is out of range for part {}",
                    self.manifest.part_id
                )));
            }
            out.push(all.events[r].clone());
        }
        Ok(out)
    }

    /// Decode the entire part, verifying every checksum. Used by merge,
    /// re-embed, `verify`, and the exact oracle.
    pub fn read_all(&self) -> Result<PartRows> {
        let m = &self.manifest;
        let n = m.row_count;

        let codes = self.read_column_checked("pq_codes")?;
        let vectors = io::decode_f32(&self.read_column_checked("vectors")?);
        let centroids = io::decode_u32(&self.read_column_checked("centroid")?);
        let times = io::decode_i64(&self.read_column_checked("event_time")?);
        let costs = io::decode_f64(&self.read_column_checked("cost")?);
        let errors = self.read_column_checked("error")?;

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
    /// why. A string-offset array can be perfectly checksum-valid and still
    /// claim a row starts 1 TiB into a 4 KiB blob — the CRC only proves the
    /// bytes are the bytes we wrote, not that they mean anything. So the audit
    /// decodes the part as well as checksumming it, which is what forces every
    /// untrusted length through the bounds checks in `decode_strings`.
    ///
    /// `open` deliberately does none of this: paying a full CRC and a full
    /// decode on every part at query time would make pruning pointless.
    pub fn verify(&self) -> Result<()> {
        for c in &self.manifest.columns {
            self.read_column_checked(&c.name)?;
        }
        self.read_all()?;
        Ok(())
    }
}
