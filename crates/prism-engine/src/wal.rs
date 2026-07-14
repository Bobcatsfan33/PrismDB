//! The durable admission log (S2).
//!
//! **An ack means durable. It does not mean visible.**
//!
//! A batch is acknowledged once it is appended here and `fsync`ed. At that instant
//! the events are guaranteed to become queryable — even if the process dies
//! immediately afterwards, because recovery replays this log.
//!
//! That guarantee is what makes the crash that matters survivable: the one
//! **between embedding and the part write**. At that moment the event has been
//! acked, it has already cost GPU time, and it exists nowhere durable except here.
//! Without a WAL the only honest options are to ack *after* the catalog commit (and
//! pay the full latency) or to lose the event. With one, we can do both.
//!
//! ```text
//!   poll source                         offset = 100
//!     → admission checks
//!     → WAL append + fsync         ←──  ACK to producer
//!     → embed
//!     → write immutable part
//!     → catalog commit             ←──  events are now VISIBLE
//!     → mark WAL record applied
//!     → advance source offset           offset = 200
//! ```
//!
//! A crash anywhere before the commit leaves an unapplied WAL record. Recovery
//! finds it, re-embeds, writes the part, commits. The idempotency index — which is
//! only written *at publication* — has no record of these events, so they are not
//! mistaken for replays and suppressed. Exactly once.

use prism_part::io;
use prism_types::error::{PrismError, Result};
use prism_types::event::Event;
use prism_types::hash::crc32;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// One acknowledged, not-yet-published batch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalRecord {
    pub record_id: u64,
    pub events: Vec<Event>,
    /// Where these events came from, so the source offset can be advanced only
    /// after they are published — and so recovery knows what to advance it *to*.
    pub source: Option<String>,
    pub source_offset: Option<u64>,
    pub created_at_ms: i64,
}

/// The on-disk frame. A record that is torn — half-written when the power went —
/// must be *ignored*, not half-applied, and a checksum plus a length prefix is
/// what makes that decidable.
///
/// Layout: `len:u32 | crc32:u32 | json[len]`
const FRAME_HEADER: usize = 8;

pub struct Wal {
    path: PathBuf,
    applied_path: PathBuf,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Applied {
    /// Records already published. Everything else in the log is outstanding.
    ids: Vec<u64>,
    next_id: u64,
}

impl Wal {
    pub fn open(dir: &Path) -> Result<Self> {
        io::ensure_dir(dir)?;
        Ok(Wal {
            path: dir.join("admission.wal"),
            applied_path: dir.join("applied.json"),
        })
    }

    fn applied(&self) -> Result<Applied> {
        if !self.applied_path.exists() {
            return Ok(Applied::default());
        }
        Ok(serde_json::from_slice(&std::fs::read(&self.applied_path)?)?)
    }

    /// Append a batch and make it durable. **This is the ack point.**
    ///
    /// Returns the record id. The `fsync` is not optional and is not batched away:
    /// an ack that outruns the disk is a lie, and it is the specific lie that loses
    /// data in exactly the situation a WAL exists to survive.
    pub fn append(
        &self,
        events: Vec<Event>,
        source: Option<String>,
        source_offset: Option<u64>,
        now_ms: i64,
    ) -> Result<u64> {
        let mut applied = self.applied()?;
        let record_id = applied.next_id;

        let rec = WalRecord {
            record_id,
            events,
            source,
            source_offset,
            created_at_ms: now_ms,
        };
        let json = serde_json::to_vec(&rec)?;

        let mut frame = Vec::with_capacity(FRAME_HEADER + json.len());
        frame.extend_from_slice(&(json.len() as u32).to_le_bytes());
        frame.extend_from_slice(&crc32(&json).to_le_bytes());
        frame.extend_from_slice(&json);

        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(&frame)?;
        prism_part::faults::maybe_kill("wal.after_append_before_fsync");
        f.sync_all()?; // <-- the ack point. Everything before this is a promise we cannot keep.

        applied.next_id += 1;
        io::write_atomic(&self.applied_path, &serde_json::to_vec(&applied)?)?;

        Ok(record_id)
    }

    /// Every record in the log, skipping any torn tail.
    pub fn read_all(&self) -> Result<Vec<WalRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let bytes = std::fs::read(&self.path)?;
        let mut out = Vec::new();
        let mut pos = 0usize;

        while pos + FRAME_HEADER <= bytes.len() {
            let len =
                u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
                    as usize;
            let want_crc = u32::from_le_bytes([
                bytes[pos + 4],
                bytes[pos + 5],
                bytes[pos + 6],
                bytes[pos + 7],
            ]);
            let start = pos + FRAME_HEADER;
            let end = match start.checked_add(len) {
                Some(e) if e <= bytes.len() => e,
                // A torn tail: the last append was interrupted mid-frame. It was
                // never fsynced, so it was never acked, so no producer is waiting on
                // it. Stop here — do NOT try to salvage it. A half-record is not
                // data, it is debris.
                _ => break,
            };
            let json = &bytes[start..end];
            if crc32(json) != want_crc {
                break;
            }
            match serde_json::from_slice::<WalRecord>(json) {
                Ok(r) => out.push(r),
                Err(e) => {
                    return Err(PrismError::Corrupt(format!(
                        "admission log record at byte {pos} will not parse: {e}"
                    )))
                }
            }
            pos = end;
        }
        Ok(out)
    }

    /// Records that were acknowledged but never published. **This is what recovery
    /// replays**, and it is the whole reason the ack can precede the commit.
    pub fn outstanding(&self) -> Result<Vec<WalRecord>> {
        let applied = self.applied()?;
        Ok(self
            .read_all()?
            .into_iter()
            .filter(|r| !applied.ids.contains(&r.record_id))
            .collect())
    }

    /// Mark a record published. Called **after** the catalog commit, never before.
    pub fn mark_applied(&self, record_id: u64) -> Result<()> {
        let mut applied = self.applied()?;
        if !applied.ids.contains(&record_id) {
            applied.ids.push(record_id);
        }
        applied.next_id = applied.next_id.max(record_id + 1);
        io::write_atomic(&self.applied_path, &serde_json::to_vec(&applied)?)?;
        Ok(())
    }

    /// Drop applied records from the log.
    ///
    /// Compaction, not deletion-in-place: the log is rewritten to a temp file and
    /// renamed, because a WAL that can be corrupted by its own truncation is worse
    /// than no WAL.
    pub fn compact(&self) -> Result<usize> {
        let applied = self.applied()?;
        let all = self.read_all()?;
        let keep: Vec<&WalRecord> = all
            .iter()
            .filter(|r| !applied.ids.contains(&r.record_id))
            .collect();
        let dropped = all.len() - keep.len();
        if dropped == 0 {
            return Ok(0);
        }

        let mut buf = Vec::new();
        for r in &keep {
            let json = serde_json::to_vec(r)?;
            buf.extend_from_slice(&(json.len() as u32).to_le_bytes());
            buf.extend_from_slice(&crc32(&json).to_le_bytes());
            buf.extend_from_slice(&json);
        }
        io::write_atomic(&self.path, &buf)?;

        let mut a = applied;
        a.ids.retain(|id| keep.iter().any(|r| r.record_id == *id));
        io::write_atomic(&self.applied_path, &serde_json::to_vec(&a)?)?;
        Ok(dropped)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.outstanding()?.is_empty())
    }
}

/// Does this store have a durable admission log at all?
pub fn exists(dir: &Path) -> bool {
    dir.join("admission.wal").exists()
}
