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
use prism_part::format::{self, FRAME_HEADER_BYTES};
use prism_part::part::{ColumnStorage, PartReader};
use prism_types::error::{PrismError, Result};

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
