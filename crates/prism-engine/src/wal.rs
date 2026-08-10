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

use crate::keys::{DekCache, KeyProvider};
use crate::storage::object::{cas_publish, CasOutcome, ObjectStore};
use prism_part::crypto::{generate_dek, BlockCipher, AEAD_XCHACHA20_POLY1305};
use prism_part::io;
use prism_types::error::{PrismError, Result};
use prism_types::event::Event;
use prism_types::hash::{crc32, hex, sha256};
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

// --- envelope encryption (S14, [D-095](../../../docs/DECISIONS.md)) -----------------------------
//
// The WAL is the one place an admitted event is durable and **not yet a part**. Between the ack and
// the catalog commit the event exists nowhere else, so an encrypted store whose WAL is plaintext
// has a window — bounded by publication latency, unbounded by a crash — in which its rows sit on
// disk and in the authoritative bucket in the clear. Encryption follows the data, so it follows it
// here ([contract §5](../../../docs/ENCRYPTION-CONTRACT.md)).

/// Domain separation for admission-log payloads. NUL-prefixed, exactly as key wrapping and the
/// backup receipt are, so a sealed WAL record can never be substituted for a data block or a
/// wrapped DEK, whatever an attacker gets to choose.
const WAL_DOMAIN: &str = "\u{0}prism-admission-wal";

/// The AAD slot that binds a payload to **its own record id**.
///
/// Carried in the string slot rather than the block index because a record id is a `u64` and a
/// block index is a `usize`: on a 32-bit target the index would truncate, and two records whose ids
/// differ only above 2^32 would become substitutable for each other. A string cannot truncate.
fn wal_column(record_id: u64) -> String {
    format!("\u{0}wal-record-{record_id}")
}

/// The version of the sealed-record envelope. Present in every sealed frame, and **its presence is
/// what distinguishes a sealed frame from a plaintext one** — a plaintext record has no such field,
/// and a sealed record has no `events` field, so neither can be read as the other by accident.
const SEALED_WAL_VERSION: u16 = 1;

/// What a sealed record says about the key that opens it. No key material: only the *wrapped* DEK.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalEnvelope {
    pub algorithm: u16,
    /// The **explicit** id of the wrapping key. Never inferred, never defaulted.
    pub wrapping_key_id: String,
    pub dek_epoch: u64,
    pub wrapped_dek: Vec<u8>,
}

/// The wire form of a sealed record.
///
/// `record_id` stays in the clear because it is exactly what the log must read *before* it can
/// unwrap anything: the floor comparison that decides what recovery replays, the compaction filter,
/// and the immutable remote key are all functions of the id alone. Everything else — the events,
/// the source, the offset — is ciphertext.
#[derive(Debug, Serialize, Deserialize)]
struct SealedWalRecord {
    sealed_wal: u16,
    record_id: u64,
    algorithm: u16,
    wrapping_key_id: String,
    dek_epoch: u64,
    wrapped_dek: Vec<u8>,
    ciphertext: Vec<u8>,
}

/// The key material an encrypted admission log seals under, and the cache that opens what it finds.
///
/// **One DEK per log instance, not one per record.** The ack path is latency-critical and a wrap is
/// a call to the key service; minting once at open costs one wrap per process rather than one per
/// batch. That is safe here precisely because the nonce is random and 192 bits wide
/// ([contract §4](../../../docs/ENCRYPTION-CONTRACT.md)): sealing many records under one DEK needs
/// no counter and no coordination between the writers that successive ownership epochs create.
pub struct WalCrypto {
    keys: Arc<dyn KeyProvider>,
    cache: DekCache,
    sealing: Arc<BlockCipher>,
    envelope: WalEnvelope,
}

impl std::fmt::Debug for WalCrypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalCrypto")
            .field("wrapping_key_id", &self.envelope.wrapping_key_id)
            .field("dek_epoch", &self.envelope.dek_epoch)
            .finish()
    }
}

/// How many distinct WAL DEKs stay resident. Recovery may meet records from several predecessor
/// processes, each with its own DEK; bounded because an unbounded key cache is an unbounded amount
/// of key material in memory.
const WAL_DEK_CACHE_ENTRIES: usize = 16;

impl WalCrypto {
    /// Mint this log's DEK and wrap it under the key service's active key.
    pub fn new(keys: Arc<dyn KeyProvider>) -> Result<Self> {
        let dek = generate_dek()?;
        let (wrapping_key_id, wrapped_dek) = keys.wrap(&dek)?;
        let sealing = Arc::new(BlockCipher::new(dek, wrapping_key_id.clone()));
        Ok(Self {
            keys,
            cache: DekCache::new(WAL_DEK_CACHE_ENTRIES),
            sealing,
            envelope: WalEnvelope {
                algorithm: AEAD_XCHACHA20_POLY1305,
                wrapping_key_id,
                dek_epoch: 1,
                wrapped_dek,
            },
        })
    }

    /// The cipher that opens a record sealed under `envelope`.
    ///
    /// Keyed on the wrapped DEK's digest as well as the key id, because a log written across
    /// several process lifetimes holds several DEKs wrapped under the *same* key id — caching on
    /// the id alone would open every one of them with the first DEK it saw.
    fn open_cipher(&self, envelope: &WalEnvelope) -> Result<Arc<BlockCipher>> {
        if envelope.algorithm != AEAD_XCHACHA20_POLY1305 {
            return Err(PrismError::Corrupt(format!(
                "admission log record was sealed with AEAD id {}, which this build does not \
                 implement",
                envelope.algorithm
            )));
        }
        let cache_key = format!(
            "{}#{}#{}",
            envelope.wrapping_key_id,
            envelope.dek_epoch,
            hex(&sha256(&envelope.wrapped_dek))
        );
        self.cache.get_or_open(&cache_key, || {
            let dek = self
                .keys
                .unwrap(&envelope.wrapping_key_id, &envelope.wrapped_dek)?;
            Ok(BlockCipher::new(dek, envelope.wrapping_key_id.clone()))
        })
    }
}

/// Serialize one record for the log: plaintext when there is no key service, sealed when there is.
///
/// A store with no key service produces **byte-identical** frames to the ones it produced before
/// encryption existed — the plaintext branch is the original line, untouched.
fn encode_record(rec: &WalRecord, crypto: Option<&WalCrypto>) -> Result<Vec<u8>> {
    let Some(crypto) = crypto else {
        return Ok(serde_json::to_vec(rec)?);
    };
    let ciphertext = crypto.sealing.seal(
        WAL_DOMAIN,
        &wal_column(rec.record_id),
        0,
        &serde_json::to_vec(rec)?,
    )?;
    Ok(serde_json::to_vec(&SealedWalRecord {
        sealed_wal: SEALED_WAL_VERSION,
        record_id: rec.record_id,
        algorithm: crypto.envelope.algorithm,
        wrapping_key_id: crypto.envelope.wrapping_key_id.clone(),
        dek_epoch: crypto.envelope.dek_epoch,
        wrapped_dek: crypto.envelope.wrapped_dek.clone(),
        ciphertext,
    })?)
}

/// Read one record back, opening it when it is sealed.
///
/// A sealed record met with no key service is a **named refusal**, never a skip and never an empty
/// batch: silently dropping an acknowledged record is the one thing a WAL exists to prevent.
fn decode_record(bytes: &[u8], crypto: Option<&WalCrypto>) -> Result<WalRecord> {
    // A sealed frame carries `sealed_wal` and no `events`; a plaintext frame carries `events` and
    // no `sealed_wal`. Neither parses as the other, so the discrimination cannot go wrong quietly.
    let Ok(sealed) = serde_json::from_slice::<SealedWalRecord>(bytes) else {
        return Ok(serde_json::from_slice::<WalRecord>(bytes)?);
    };
    if sealed.sealed_wal != SEALED_WAL_VERSION {
        return Err(PrismError::Corrupt(format!(
            "admission log record {} declares sealed-envelope version {}, which this build does \
             not implement",
            sealed.record_id, sealed.sealed_wal
        )));
    }
    let crypto = crypto.ok_or_else(|| {
        PrismError::Policy(format!(
            "admission log record {} is sealed under key `{}` but this store has no key service \
             configured; an acknowledged record cannot be read, and must not be skipped",
            sealed.record_id, sealed.wrapping_key_id
        ))
    })?;
    let envelope = WalEnvelope {
        algorithm: sealed.algorithm,
        wrapping_key_id: sealed.wrapping_key_id,
        dek_epoch: sealed.dek_epoch,
        wrapped_dek: sealed.wrapped_dek,
    };
    let cipher = crypto.open_cipher(&envelope)?;
    let plain = cipher.open(
        WAL_DOMAIN,
        &wal_column(sealed.record_id),
        0,
        &sealed.ciphertext,
    )?;
    let rec: WalRecord = serde_json::from_slice(&plain).map_err(|e| {
        PrismError::Corrupt(format!(
            "admission log record {} decrypted but will not parse: {e}",
            sealed.record_id
        ))
    })?;
    // The AAD already binds the payload to this id; this catches the same substitution at the
    // application layer, where the error can say which two ids were involved.
    if rec.record_id != sealed.record_id {
        return Err(PrismError::Corrupt(format!(
            "admission log record sealed as {} decrypted to record {}",
            sealed.record_id, rec.record_id
        )));
    }
    Ok(rec)
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
    /// Present exactly when the store is encrypted. Explicit by construction — there is no ambient
    /// default and nothing is inferred from the bytes already on disk.
    crypto: Option<Arc<WalCrypto>>,
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
            crypto: None,
        })
    }

    /// Seal every record this log writes from now on.
    pub fn with_crypto(mut self, crypto: Arc<WalCrypto>) -> Self {
        self.crypto = Some(crypto);
        self
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
        let json = encode_record(rec, self.crypto.as_deref())?;

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
            match decode_record(json, self.crypto.as_deref()) {
                Ok(r) => out.push(r),
                // A sealed record we cannot open is a refusal in its own right and must reach the
                // caller as itself; only a genuine parse failure becomes a byte-named corruption.
                Err(e @ (PrismError::Policy(_) | PrismError::Corrupt(_))) => return Err(e),
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

    /// Every wrapping key some record in this log still needs, **without opening one**.
    ///
    /// A sealed frame names its wrapping key in the clear precisely so this question is answerable
    /// without the key it is asking about — which is what lets the retire guard refuse *before* an
    /// operator has made the log unreadable, rather than discovering it afterwards.
    pub fn wrapping_key_ids(&self) -> Result<std::collections::BTreeSet<String>> {
        let mut out = std::collections::BTreeSet::new();
        if !self.path.exists() {
            return Ok(out);
        }
        let bytes = std::fs::read(&self.path)?;
        let mut pos = 0usize;
        while pos + FRAME_HEADER <= bytes.len() {
            let len =
                u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
                    as usize;
            let start = pos + FRAME_HEADER;
            let end = match start.checked_add(len) {
                Some(e) if e <= bytes.len() => e,
                _ => break,
            };
            if let Ok(sealed) = serde_json::from_slice::<SealedWalRecord>(&bytes[start..end]) {
                out.insert(sealed.wrapping_key_id);
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
            // Re-encode through the same path an append takes. Compaction rewrites the whole log,
            // so a plain `to_vec` here would quietly turn every surviving sealed record back into
            // plaintext on disk — encryption undone by a maintenance operation nobody was watching.
            let json = encode_record(r, self.crypto.as_deref())?;
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
    crypto: Option<Arc<WalCrypto>>,
}

impl RemoteWal {
    pub fn new(store: Arc<dyn ObjectStore>, shard_id: usize) -> Self {
        Self {
            store,
            prefix: format!("wal/shard-{shard_id}/records/"),
            crypto: None,
        }
    }

    /// Seal every payload this log makes durable.
    ///
    /// The remote log is the one the contract names outright: its objects live in the shared
    /// authoritative bucket, so a plaintext payload here is readable by anyone with bucket access
    /// and no node-local disk at all.
    pub fn with_crypto(mut self, crypto: Arc<WalCrypto>) -> Self {
        self.crypto = Some(crypto);
        self
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
        let bytes = encode_record(record, self.crypto.as_deref())?;
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
            let record = decode_record(&self.store.get(&key)?, self.crypto.as_deref())?;
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
