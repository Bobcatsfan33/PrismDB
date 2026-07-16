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
