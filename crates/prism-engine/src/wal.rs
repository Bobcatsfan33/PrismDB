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

use crate::storage::object::{cas_publish, CasOutcome, ObjectStore};
use prism_part::io;
use prism_types::error::{PrismError, Result};
use prism_types::event::Event;
use prism_types::hash::crc32;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One acknowledged, not-yet-published batch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
const REMOTE_RECORD_SEQUENCE_BITS: u32 = 32;
const REMOTE_RECORD_SEQUENCE_MASK: u64 = u32::MAX as u64;

pub struct Wal {
    path: PathBuf,
    applied_path: PathBuf,
}

/// The record-id **allocator** — a monotonic counter that survives compaction (deriving the next id
/// from the log would reuse ids after the log is compacted below them). This is **not** a record of
/// what is applied; applied progress lives *inside the snapshot* now ([D-077](../../../docs/DECISIONS.md)),
/// so there is one source of truth, not two. An older store's `applied.json` also carried an `ids`
/// list; serde ignores it on read, and it is never written again.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Allocator {
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

    fn allocator(&self) -> Result<Allocator> {
        if !self.applied_path.exists() {
            return Ok(Allocator::default());
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
        let record_id = self.allocator()?.next_id;
        let rec = WalRecord {
            record_id,
            events,
            source,
            source_offset,
            created_at_ms: now_ms,
        };
        self.append_record(&rec)?;
        Ok(record_id)
    }

    /// Append a caller-assigned record ID and fsync it.
    ///
    /// Replicated mode assigns IDs from the ownership epoch so two successive nodes can never
    /// generate the same record key. IDs may skip, but they may never move backwards.
    pub fn append_record(&self, rec: &WalRecord) -> Result<()> {
        let mut alloc = self.allocator()?;
        if rec.record_id < alloc.next_id {
            return Err(PrismError::Invariant(format!(
                "admission WAL record {} is older than allocator next_id {}",
                rec.record_id, alloc.next_id
            )));
        }
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

        alloc.next_id = rec.record_id.checked_add(1).ok_or_else(|| {
            PrismError::Invariant("admission WAL record id space is exhausted".into())
        })?;
        io::write_atomic(&self.applied_path, &serde_json::to_vec(&alloc)?)?;
        Ok(())
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

    /// Records not yet reflected in the committed snapshot — those with id **greater than the
    /// snapshot's `applied_wal_record` floor**. **This is what recovery replays** ([D-077](../../../docs/DECISIONS.md)):
    /// because the floor is set atomically with publication, a record at or below it is already
    /// visible and must not be replayed, and a record above it is genuinely unpublished. This is the
    /// whole reason the ack can precede the commit without ever double-publishing.
    pub fn outstanding_after(&self, floor: Option<u64>) -> Result<Vec<WalRecord>> {
        Ok(self
            .read_all()?
            .into_iter()
            .filter(|r| floor.map_or(true, |f| r.record_id > f))
            .collect())
    }

    /// Drop records the snapshot has already applied (id ≤ `floor`) from the log.
    ///
    /// Compaction, not deletion-in-place: the log is rewritten to a temp file and renamed, because a
    /// WAL that can be corrupted by its own truncation is worse than no WAL. The allocator's `next_id`
    /// is left untouched, so a compacted-empty log never reuses an id.
    pub fn compact_through(&self, floor: Option<u64>) -> Result<usize> {
        let Some(floor) = floor else {
            return Ok(0); // nothing applied yet; nothing to drop.
        };
        let all = self.read_all()?;
        let keep: Vec<&WalRecord> = all.iter().filter(|r| r.record_id > floor).collect();
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
        Ok(dropped)
    }
}

/// A create-only admission log in the shard's authoritative object store.
///
/// Every object is immutable and named by its record ID. `append` does not return until the CAS is
/// resolved and the exact bytes are read back, so a successful return is a remote durability fact,
/// not an optimistic network acknowledgement.
pub struct RemoteWal {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl RemoteWal {
    pub fn new(store: Arc<dyn ObjectStore>, shard_id: usize) -> Self {
        Self {
            store,
            prefix: format!("wal/shard-{shard_id}/records/"),
        }
    }

    fn key(&self, record_id: u64) -> String {
        format!("{}{record_id:020}.json", self.prefix)
    }

    /// Allocate the next record ID inside `epoch`.
    ///
    /// The high 32 bits are the monotonic ownership epoch and the low 32 bits are the sequence
    /// within that ownership. A takeover therefore orders every new record after the old writer,
    /// preserving the snapshot's single monotonic `applied_wal_record` floor.
    pub fn next_record_id(&self, epoch: u64, local: &[WalRecord]) -> Result<u64> {
        if epoch == 0 || epoch > u32::MAX as u64 {
            return Err(PrismError::Invariant(format!(
                "replicated WAL ownership epoch {epoch} is outside 1..={}",
                u32::MAX
            )));
        }
        let base = epoch << REMOTE_RECORD_SEQUENCE_BITS;
        let mut next = 0u64;
        for record in self.read_all()?.iter().chain(local.iter()) {
            let record_epoch = record.record_id >> REMOTE_RECORD_SEQUENCE_BITS;
            if record_epoch == epoch {
                next = next.max((record.record_id & REMOTE_RECORD_SEQUENCE_MASK) + 1);
            } else if record.record_id >= base {
                return Err(PrismError::Invariant(format!(
                    "replicated WAL found record {} at or above current ownership epoch {epoch}",
                    record.record_id
                )));
            }
        }
        if next > REMOTE_RECORD_SEQUENCE_MASK {
            return Err(PrismError::Invariant(format!(
                "replicated WAL exhausted the record sequence for ownership epoch {epoch}"
            )));
        }
        Ok(base | next)
    }

    pub fn append(&self, record: &WalRecord) -> Result<()> {
        let key = self.key(record.record_id);
        let bytes = serde_json::to_vec(record)?;
        match cas_publish(self.store.as_ref(), &key, &bytes)? {
            CasOutcome::Created | CasOutcome::AlreadyOurs => {}
            CasOutcome::Conflict => {
                return Err(PrismError::Invariant(format!(
                    "replicated admission WAL conflict at immutable key `{key}`"
                )))
            }
        }
        let durable = self.store.get(&key)?;
        if durable != bytes {
            return Err(PrismError::Corrupt(format!(
                "replicated admission WAL read-back differs at `{key}`"
            )));
        }
        Ok(())
    }

    pub fn read_all(&self) -> Result<Vec<WalRecord>> {
        let mut records = BTreeMap::new();
        for key in self.store.list(&self.prefix)? {
            let suffix = key.strip_prefix(&self.prefix).ok_or_else(|| {
                PrismError::Corrupt(format!(
                    "replicated admission WAL listed key outside `{}`: `{key}`",
                    self.prefix
                ))
            })?;
            let id_text = suffix.strip_suffix(".json").ok_or_else(|| {
                PrismError::Corrupt(format!(
                    "replicated admission WAL key has an invalid suffix: `{key}`"
                ))
            })?;
            if id_text.len() != 20 || !id_text.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(PrismError::Corrupt(format!(
                    "replicated admission WAL key has an invalid record id: `{key}`"
                )));
            }
            let key_id = id_text.parse::<u64>().map_err(|error| {
                PrismError::Corrupt(format!(
                    "replicated admission WAL key `{key}` will not parse: {error}"
                ))
            })?;
            let record: WalRecord = serde_json::from_slice(&self.store.get(&key)?)?;
            if record.record_id != key_id {
                return Err(PrismError::Corrupt(format!(
                    "replicated admission WAL key id {key_id} does not match body id {}",
                    record.record_id
                )));
            }
            if records.insert(key_id, record).is_some() {
                return Err(PrismError::Corrupt(format!(
                    "replicated admission WAL contains duplicate record id {key_id}"
                )));
            }
        }
        Ok(records.into_values().collect())
    }
}

/// Does this store have a durable admission log at all?
pub fn exists(dir: &Path) -> bool {
    dir.join("admission.wal").exists()
}
