//! The cold-tier read path (S11) — exact rerank vectors fetched through the object store + cache.
//!
//! Every exact-vector fetch the rerank path makes goes through here, so the bytes come from the
//! `CachedObjectStore` ([`crate::engine::Engine::cold`]) rather than straight off the local mmap.
//! The framing is the same the local reader uses — per-block CRC-32, the S1 named-byte discipline —
//! and it is applied to the bytes the object store returns, so a truncated remote block names
//! itself exactly as a truncated local one does.
//!
//! Two guarantees the answer-invariance gate rests on ([storage contract §3/§4](../../../../docs/STORAGE-CONTRACT.md)):
//! a **cache state never changes the answer** (the bytes are content-verified either way), and a
//! transient remote fault produces **the correct answer via a bounded retry, or the named remote
//! condition — never a silently short result set**.

use super::COLD_FETCH_MAX_RETRIES;
use crate::engine::Engine;
use prism_part::catalog::GC_HORIZON_MS;
use prism_part::format::{self, FRAME_HEADER_BYTES};
use prism_part::part::{ColumnStorage, PartReader};
use prism_types::error::{PrismError, Result};

/// The remote key of a part's cold tier (`parts/<id>/rerank.vec`), or a catalog mirror snapshot
/// (`catalog/SNAPSHOT-<snapshot_id>`, the scheme [D-069](../../../../docs/DECISIONS.md)'s mirror
/// writes). What reconciliation reclaims against the live set.
const COLD_TIER_FILE: &str = "rerank.vec";
const MIRROR_SNAPSHOT_PREFIX: &str = "catalog/SNAPSHOT-";

/// What a remote-orphan reconciliation pass did.
#[derive(Clone, Debug, Default)]
pub struct ReconcileReport {
    pub dry_run: bool,
    /// Keys reclaimed (absent from the live set and older than the reader-lease horizon).
    pub removed: Vec<String>,
    /// Live cold-tier objects a retained snapshot references — left untouched.
    pub protected_parts: usize,
    /// Live catalog mirror snapshots within the recovery depth — left untouched.
    pub protected_mirrors: usize,
    /// Objects absent from the live set but younger than the horizon (an in-flight publication):
    /// graced, not swept.
    pub too_young: usize,
    /// Incomplete multipart uploads aborted (a crashed large-object publication's server-side parts).
    pub aborted_uploads: Vec<String>,
}

/// The part id of a `parts/<id>/rerank.vec` key, if the key is a cold-tier object.
fn cold_tier_part_of(key: &str) -> Option<&str> {
    let rest = key.strip_prefix("parts/")?;
    let (id, tail) = rest.split_once('/')?;
    (tail == COLD_TIER_FILE).then_some(id)
}

/// The snapshot id a `catalog/SNAPSHOT-<id>` mirror key names, if the key is a mirror snapshot.
fn mirror_snapshot_of(key: &str) -> Option<&str> {
    key.strip_prefix(MIRROR_SNAPSHOT_PREFIX)
}

impl Engine {
    /// Fetch the exact vectors for `rows` of `reader`'s part through the cold-tier object store and
    /// cache. Byte-identical to [`PartReader::read_vectors_for_rows`] on the same bytes — it only
    /// changes *where the bytes come from*, never *what they are* (storage §3).
    pub fn cold_read_vectors(&self, reader: &PartReader, rows: &[usize]) -> Result<Vec<Vec<f32>>> {
        let m = &reader.manifest;
        let dim = m.dim;
        let bpv = m.bytes_per_vector()?;
        let col = m.column("rerank_vectors")?;
        let (blocks, block_size) = match &col.storage {
            ColumnStorage::Framed {
                blocks, block_size, ..
            } => (blocks, *block_size),
            // A v1 unframed cold tier predates the object-store path; read it locally.
            ColumnStorage::Unframed { .. } => return reader.read_vectors_for_rows(rows),
        };
        let key = format!("parts/{}/{}", m.part_id, col.file);

        let mut out = Vec::with_capacity(rows.len());
        for &row in rows {
            if row >= m.row_count {
                return Err(PrismError::Corrupt(format!(
                    "row {row} is out of range for part {} ({} rows)",
                    m.part_id, m.row_count
                )));
            }
            let offset = (row * bpv) as u64;
            let end = offset + bpv as u64;
            let (first, last) = format::blocks_for_range(offset, bpv, block_size);
            if last >= blocks.len() {
                return Err(PrismError::Corrupt(format!(
                    "part {} rerank range needs block {last}, but the column has {}",
                    m.part_id,
                    blocks.len()
                )));
            }
            let mut buf: Vec<u8> = Vec::with_capacity(bpv);
            for (i, b) in blocks.iter().enumerate().take(last + 1).skip(first) {
                let block_len = FRAME_HEADER_BYTES + b.payload_len as usize;
                let raw = self.fetch_cold_block(&key, b.file_offset, block_len)?;
                let payload =
                    format::read_block(&raw, i, b, "rerank_vectors", &m.part_id, block_size)?;
                let block_start = (i as u64) * block_size as u64;
                let from = offset.saturating_sub(block_start) as usize;
                let to = ((end - block_start) as usize).min(payload.len());
                if from < payload.len() {
                    buf.extend_from_slice(&payload[from..to]);
                }
            }
            out.push(m.rerank.decode_vector(&buf, dim)?);
        }
        Ok(out)
    }

    /// Upload every live part's cold tier (`rerank.vec`) to a destination object store, keyed
    /// `parts/<id>/rerank.vec` — the key the cold read expects. This is the cold-tier half of
    /// remote-durable publication (storage §2); the answer-invariance-through-MinIO gate uses it to
    /// put the cold tier on the remote before querying it. Returns the number of parts uploaded.
    pub fn upload_cold_tier(&self, dst: &dyn super::object::ObjectStore) -> Result<usize> {
        let snap = self.snapshot()?;
        let mut n = 0;
        for e in &snap.parts {
            let id = e.part_id();
            let path = self.store.part_dir(id).join("rerank.vec");
            if path.exists() {
                let bytes = std::fs::read(&path)?;
                dst.put(&format!("parts/{id}/rerank.vec"), &bytes)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Reconcile the object-store listing against the live set — the **remote analogue of local GC**
    /// ([storage §2](../../../../docs/STORAGE-CONTRACT.md)). It reclaims a remote object only when it
    /// is BOTH absent from the live set (no retained snapshot references the part, or the mirror
    /// snapshot is past the recovery depth) AND older than the reader-lease horizon
    /// ([`GC_HORIZON_MS`]) — so a just-uploaded cold tier whose publication commit has not yet landed
    /// is graced, never swept, and a catalog mirror snapshot the recovery path could need is
    /// protected exactly as a reader's snapshot is (invariant 6, by construction). An object under
    /// neither recognised scheme is never touched: reconciliation reclaims only what it positively
    /// knows to be an orphan.
    ///
    /// Publication never deletes as it goes (a retried upload could race a live object); reclaiming
    /// orphans is *only* this pass's job, exactly as local GC — never the writer — reclaims a `.tmp`.
    pub fn reconcile_remote_orphans(
        &self,
        retain_snapshots: usize,
        now_ms: i64,
        dry_run: bool,
    ) -> Result<ReconcileReport> {
        let live = self.catalog().retained(retain_snapshots, now_ms)?;
        let backend = self.cold.backend();

        let mut objects = backend.list_meta("parts/")?;
        objects.extend(backend.list_meta("catalog/")?);

        let mut report = ReconcileReport {
            dry_run,
            ..Default::default()
        };
        for obj in objects {
            let protected = if let Some(id) = cold_tier_part_of(&obj.key) {
                let live_part = live.parts.contains(id);
                report.protected_parts += live_part as usize;
                live_part
            } else if let Some(sid) = mirror_snapshot_of(&obj.key) {
                let live_mirror = live.snapshots.contains(sid);
                report.protected_mirrors += live_mirror as usize;
                live_mirror
            } else {
                // Neither a cold tier nor a mirror snapshot: never sweep the unrecognised.
                true
            };
            if protected {
                continue;
            }

            // Absent from the live set — but an object younger than the horizon may be an in-flight
            // publication whose commit is about to reference it. Grace it, exactly as GC would not
            // reclaim a snapshot within a reader's lease.
            if now_ms.saturating_sub(obj.last_modified_ms) <= GC_HORIZON_MS {
                report.too_young += 1;
                continue;
            }

            report.removed.push(obj.key.clone());
            if !dry_run {
                backend.delete(&obj.key)?;
            }
        }
        report.removed.sort();

        // **Sweep incomplete multipart uploads.** A large-object publication that crashed mid-upload
        // leaves server-side parts that no object references and no listing shows — only a multipart
        // enumeration does. Abort those older than the horizon (a crashed upload), graced within it
        // (an upload still in flight for a live publication). A backend without multipart lists none.
        for mp in backend.list_multipart("parts/")? {
            if now_ms.saturating_sub(mp.initiated_ms) <= GC_HORIZON_MS {
                report.too_young += 1;
                continue;
            }
            report.aborted_uploads.push(mp.key.clone());
            if !dry_run {
                backend.abort_multipart(&mp.key, &mp.upload_id)?;
            }
        }
        report.aborted_uploads.sort();
        Ok(report)
    }

    /// Publish one part's cold tier to the object-store backend **durably, before the catalog
    /// references it** ([storage contract §2](../../../../docs/STORAGE-CONTRACT.md)) — invariant 2
    /// extended to the remote: a snapshot may name a part only once its exact-vector bytes are
    /// durable and complete on the object store, not merely on local disk.
    ///
    /// The `rerank.vec` is uploaded only if the backend does not already hold it at the right size.
    /// The default local backend is rooted at the store, so the part write already put it there and
    /// this is a single HEAD; a remote backend receives the bytes here. A part with no cold tier (an
    /// unframed v1 column, or an empty part) has nothing to publish and returns cleanly.
    ///
    /// Two kill points bracket the boundary so the fault harness proves old-or-new-never-hybrid: a
    /// crash after the upload but before the verify, and after the verify but before the catalog
    /// commit that references the part, must each leave the catalog at the OLD snapshot with the
    /// uploaded bytes an orphan on the backend (reclaimed later by remote GC) — never a reference to
    /// an object that was not confirmed durable.
    pub fn publish_part_cold(&self, part_id: &str) -> Result<()> {
        let path = self.store.part_dir(part_id).join("rerank.vec");
        if !path.exists() {
            return Ok(()); // no cold tier to publish
        }
        let bytes = std::fs::read(&path)?;
        let want = bytes.len() as u64;
        let key = format!("parts/{part_id}/rerank.vec");
        let backend = self.cold.backend();

        // Upload only if the backend does not already hold the object at the right size — idempotent
        // by construction, since the cold tier is content-addressed. The local backend already has
        // it (the part write wrote straight into the object key's path); a remote backend gets it.
        if backend.head(&key)? != Some(want) {
            backend.put(&key, &bytes)?;
        }

        // A crash here leaves the bytes on the backend but no catalog reference: an orphan.
        prism_part::faults::maybe_kill("publish.after_upload_before_verify");

        // Verify the object is durable and complete before the catalog may reference it: a PUT that
        // returned OK but landed short or absent is caught here, named, and never referenced. Byte
        // integrity is then re-checked per block on every read (the CRC-32 framing, storage §4), so
        // this HEAD is the publication-time "it landed, whole" gate.
        match backend.head(&key)? {
            Some(len) if len == want => {}
            Some(len) => {
                return Err(PrismError::Invariant(format!(
                    "cold-tier publish of part `{part_id}` did not verify: the backend holds {len} \
                     bytes at `{key}`, but the part's cold tier is {want} bytes"
                )));
            }
            None => {
                return Err(PrismError::Invariant(format!(
                    "cold-tier publish of part `{part_id}` did not verify: the backend has no \
                     object at `{key}` after the upload"
                )));
            }
        }

        // A crash here leaves a verified object the catalog does not yet reference: still an orphan,
        // still old-or-new — the commit that would reference it has not run.
        prism_part::faults::maybe_kill("publish.after_verify_before_reference");
        Ok(())
    }

    /// One cold block, with a **bounded retry** on transient remote faults. A truncated/corrupt read
    /// is a named byte error at once (never retried); a persistent outage is named after the budget
    /// is spent. It never returns fewer bytes than asked — the query gets the block or a named error.
    fn fetch_cold_block(&self, key: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut last: Option<PrismError> = None;
        for _ in 0..=COLD_FETCH_MAX_RETRIES {
            match self.cold.get_range_cached(key, offset, len) {
                Ok(bytes) => return Ok(bytes),
                // A named byte error (truncation / corruption) is not a transient blip; surface it.
                Err(e @ PrismError::Corrupt(_)) => return Err(e),
                // A transient remote fault (5xx, drop): retry within budget.
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| {
            PrismError::Io("remote unavailable: cold fetch failed after retries".into())
        }))
    }
}
