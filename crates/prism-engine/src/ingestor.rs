//! The S2 ingestion path: admission → WAL → embed → part → commit → offset.
//!
//! This file is where the ingestion contract stops being a document and starts
//! being a sequence of `?`s in a specific, load-bearing order. The order is:
//!
//! ```text
//!   poll source                         offset stays at 100
//!     → admission (fairness, caps, cardinality, skew, quota)
//!     → idempotency (new? replay? conflict?)
//!     → WAL append + fsync         ←──  ACK. The events are now guaranteed.
//!     → embed
//!     → write immutable part
//!     → catalog commit             ←──  the events are now VISIBLE
//!     → record idempotency keys         (invariant 7: with-or-after publication)
//!     → mark the WAL record applied
//!     → advance the source offset       offset = 200
//! ```
//!
//! Every arrow is a place we can die, and the contract says what each death costs.
//! Three of them are named kill points and are driven in CI.

use crate::admission::{self, KeyDictionary, QuotaEnforcer};
use crate::engine::Engine;
use crate::idempotency::{IdempotencyIndex, Verdict};
use crate::source::Source;
use crate::wal::{RemoteWal, Wal, WalRecord};
use prism_types::error::{PrismError, Result};
use prism_types::event::{DeadLetter, Event};
use prism_types::limits::RejectReason;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IngestReport2 {
    pub offered: usize,
    /// Admitted, embedded, published, queryable.
    pub published: usize,
    /// Replays: same key, same content. **Acknowledged, not stored again.** The
    /// producer did the right thing and must not be punished for it.
    pub duplicates_suppressed: usize,
    /// Rejected, with a named reason, visible in the dead-letter log.
    pub dead_lettered: usize,
    pub by_reason: std::collections::BTreeMap<String, usize>,

    pub part_id: Option<String>,
    pub snapshot_id: String,
    pub wal_record: Option<u64>,
    pub source: Option<String>,
    pub source_offset_before: Option<u64>,
    pub source_offset_after: Option<u64>,

    /// Set when this batch came out of the WAL rather than off the wire.
    pub recovered: bool,
}

/// The durable admission state that lives alongside a store.
pub struct Ingestor {
    pub engine: Arc<Engine>,
    pub wal: Wal,
    pub quotas: QuotaEnforcer,
    dict_path: PathBuf,
    idem_path: PathBuf,
    remote_wal: Option<RemoteWal>,
}

impl Ingestor {
    pub fn open(engine: Engine) -> Result<Self> {
        Self::open_shared(Arc::new(engine))
    }

    fn open_shared(engine: Arc<Engine>) -> Result<Self> {
        Self::open_shared_with(engine, None)
    }

    /// The admission log's key material, minted once per ingestor when the store is encrypted.
    ///
    /// Both logs share one [`WalCrypto`], so a record is sealed under the same DEK locally and
    /// remotely. That is what lets the replacement node in the D-094 drill open the remote log it
    /// inherited from a predecessor it never shared a process with.
    fn wal_crypto(engine: &Engine) -> Result<Option<Arc<crate::wal::WalCrypto>>> {
        match engine.keys() {
            None => Ok(None),
            Some(keys) => Ok(Some(Arc::new(crate::wal::WalCrypto::new(Arc::clone(
                keys,
            ))?))),
        }
    }

    fn open_shared_with(engine: Arc<Engine>, remote_wal: Option<RemoteWal>) -> Result<Self> {
        let root = engine.store.root.clone();
        let crypto = Self::wal_crypto(&engine)?;
        let mut wal = Wal::open(&root.join("wal"))?;
        let mut remote_wal = remote_wal;
        if let Some(crypto) = &crypto {
            wal = wal.with_crypto(Arc::clone(crypto));
            remote_wal = remote_wal.map(|r| r.with_crypto(Arc::clone(crypto)));
        }
        prism_part::io::ensure_dir(&root.join("admission"))?;
        Ok(Ingestor {
            wal,
            quotas: QuotaEnforcer::new(),
            dict_path: root.join("admission/key-dictionary.json"),
            idem_path: root.join("admission/idempotency.json"),
            remote_wal,
            engine,
        })
    }

    /// Open the cross-node write path.
    ///
    /// Ownership is acquired before the remote log is used. The ownership epoch becomes part of
    /// every WAL record ID, and every accepted batch must be durable both locally and in the
    /// authoritative object store before the write path proceeds past its acknowledgement point.
    pub fn open_replicated(engine: Engine, shard_id: usize) -> Result<Self> {
        let engine = Arc::new(engine);
        engine.acquire_ownership()?;
        let remote_wal = RemoteWal::new(Arc::clone(engine.cold.backend()), shard_id);
        // Built here rather than assigned afterwards: the remote log must be handed the same
        // WalCrypto the local one got, and a field assigned after construction is exactly how it
        // would end up sealing locally and publishing plaintext remotely.
        Self::open_shared_with(engine, Some(remote_wal))
    }

    pub fn key_dictionary(&self) -> Result<KeyDictionary> {
        if !self.dict_path.exists() {
            return Ok(KeyDictionary::new());
        }
        Ok(serde_json::from_slice(&std::fs::read(&self.dict_path)?)?)
    }

    pub fn idempotency(&self) -> Result<IdempotencyIndex> {
        IdempotencyIndex::load(&self.idem_path)
    }

    /// **Recovery.** Replay every acknowledged-but-unpublished WAL record.
    ///
    /// Runs before anything else touches the store. These events were acked: a
    /// producer has been told they are safe, and they are not queryable yet. That
    /// promise is the only thing standing between us and silent data loss, and this
    /// is where it is kept.
    ///
    /// They are *not* re-admitted. Admission already ran and already passed — and
    /// re-running quotas against them could reject an event we have already
    /// promised to keep, which would turn a crash into a data-loss event.
    pub fn recover(&mut self, now_ms: i64) -> Result<Vec<IngestReport2>> {
        // The applied floor is the committed snapshot's marker ([D-077](../../../docs/DECISIONS.md)) —
        // the single source of truth for what is published, set atomically with the publication. A
        // record above it is genuinely unpublished and is replayed; a record at or below it is already
        // visible and must NOT be replayed (that was the double-publish bug), but the crash may have
        // landed between its commit and its client-facing idempotency record or its source-offset
        // commit, so it is reconciled instead.
        let floor = self.engine.snapshot()?.applied_wal_record;
        let mut reports = Vec::new();

        for rec in self.admission_records()? {
            let applied = matches!(floor, Some(f) if rec.record_id <= f);
            if applied {
                self.reconcile_applied(&rec)?;
            } else {
                let mut r = self.publish_wal_record(&rec, now_ms)?;
                r.recovered = true;
                reports.push(r);
            }
        }

        // Compact everything now at or below the (possibly advanced) marker; `next_id` is untouched.
        let floor = self.engine.snapshot()?.applied_wal_record;
        self.wal.compact_through(floor)?;
        Ok(reports)
    }

    fn admission_records(&self) -> Result<Vec<WalRecord>> {
        let mut records = BTreeMap::new();
        for record in self.wal.read_all()? {
            records.insert(record.record_id, record);
        }
        if let Some(remote) = &self.remote_wal {
            for record in remote.read_all()? {
                match records.get(&record.record_id) {
                    Some(local) if local != &record => {
                        return Err(PrismError::Corrupt(format!(
                            "local and replicated admission WAL differ for record {}",
                            record.record_id
                        )))
                    }
                    Some(_) => {}
                    None => {
                        records.insert(record.record_id, record);
                    }
                }
            }
        }
        Ok(records.into_values().collect())
    }

    /// Reconcile the client-facing state of a WAL record the snapshot has **already applied** but a
    /// crash may have left half-finished after the atomic commit ([D-077](../../../docs/DECISIONS.md)):
    /// record its idempotency keys (so a resubmission after recovery is recognised as a duplicate
    /// through the front door, not published a second time) and advance its source offset (so the
    /// source is not re-polled for events that are already visible). Both are idempotent.
    fn reconcile_applied(&self, rec: &WalRecord) -> Result<()> {
        let mut idem = self.idempotency()?;
        for e in &rec.events {
            let hash = e.content_hash();
            let (t, k) = e.dedup_key();
            idem.record(t, k, &hash, e.event_time);
        }
        idem.save(&self.idem_path)?;
        if let (Some(name), Some(off)) = (&rec.source, rec.source_offset) {
            crate::source::commit_offset(&self.engine.store.root.join("sources"), name, off)?;
        }
        Ok(())
    }

    /// Take a batch from the wire (or from a source) all the way to visible.
    pub fn ingest(
        &mut self,
        events: Vec<Event>,
        source: Option<&dyn Source>,
        next_offset: Option<u64>,
        now_ms: i64,
    ) -> Result<IngestReport2> {
        let offered = events.len();
        let mut report = IngestReport2 {
            offered,
            source: source.map(|s| s.name().to_string()),
            source_offset_before: match source {
                Some(s) => Some(s.committed_offset()?),
                None => None,
            },
            ..Default::default()
        };

        // --- 1. admission: fairness, caps, cardinality, skew, quota ---
        let mut dict = self.key_dictionary()?;
        let admitted = admission::admit(events, &mut dict, &mut self.quotas, now_ms);

        let mut dead: Vec<DeadLetter> = admitted.rejected;

        // --- 2. idempotency: new, replay, or conflict? ---
        let mut idem = self.idempotency()?;
        let mut accepted: Vec<Event> = Vec::with_capacity(admitted.accepted.len());

        for e in admitted.accepted {
            let hash = e.content_hash();
            let (tenant, key) = e.dedup_key();
            let (tenant, key) = (tenant.to_string(), key.to_string());

            match idem.check(&tenant, &key, &hash) {
                Verdict::New => accepted.push(e),
                Verdict::Replay => {
                    // The producer retried after an ack they never saw, or a source
                    // re-delivered after our crash. Both are correct behaviour. We
                    // acknowledge and store nothing.
                    report.duplicates_suppressed += 1;
                }
                Verdict::Conflict { stored_hash } => {
                    dead.push(DeadLetter {
                        reason: RejectReason::IdempotencyConflict.to_string(),
                        detail: format!(
                            "key ({tenant}, {key}) was already accepted with content hash \
                             {stored_hash}, and this event hashes to {hash}. An id that was \
                             supposed to identify one event now identifies two. Refusing rather \
                             than silently rewriting history under a reused id; both are kept — \
                             the stored one, and this one, here."
                        ),
                        stage: "idempotency".to_string(),
                        event: e,
                    });
                }
            }
        }

        // Model authorization and its local GPU usage reservation happen
        // before the durable ACK. Recovery does not consume the rate budget a
        // second time; the governed embedder still rechecks the current exact
        // grant before it spends GPU work.
        let (model_allowed, model_denied) = self.engine.model_preflight_ingest(accepted)?;
        accepted = model_allowed;
        dead.extend(model_denied);

        // Dead letters are durable *before* anything is acked. An operator must
        // never be able to see what got in without being able to see what did not.
        self.engine.write_dead_letters(&dead)?;
        report.dead_lettered = dead.len();
        for d in &dead {
            *report.by_reason.entry(d.reason.clone()).or_default() += 1;
        }

        if accepted.is_empty() {
            // Nothing to publish. But a batch of pure replays still advances the
            // source offset: those events *are* published, they were published
            // before, and refusing to advance would replay them forever.
            let snap = self.engine.snapshot()?;
            report.snapshot_id = snap.snapshot_id;
            if let (Some(s), Some(off)) = (source, next_offset) {
                s.commit(off)?;
                report.source_offset_after = Some(off);
            }
            return Ok(report);
        }

        // --- 3. the durable admission log. THIS IS THE ACK POINT. ---
        let source_name = source.map(|s| s.name().to_string());
        let record_id = if let Some(remote) = &self.remote_wal {
            self.engine.assert_write_owner()?;
            let local = self.wal.read_all()?;
            let record_id = remote.next_record_id(self.engine.ownership_epoch(), &local)?;
            let record = WalRecord {
                record_id,
                events: accepted.clone(),
                source: source_name.clone(),
                source_offset: next_offset,
                created_at_ms: now_ms,
            };
            self.wal.append_record(&record)?;
            remote.append(&record)?;
            // A replacement may have acquired the shard during the remote round trip. Such a
            // request is not acknowledged by this node; the replacement will recover the durable
            // record, while this stale writer is forbidden from publishing it.
            self.engine.assert_write_owner()?;
            record_id
        } else {
            self.wal
                .append(accepted.clone(), source_name.clone(), next_offset, now_ms)?
        };
        report.wal_record = Some(record_id);

        // From here on the events are *guaranteed*. If we die, recovery replays them.

        // --- 4..8. embed, write, commit, record, advance ---
        let rec = WalRecord {
            record_id,
            events: accepted,
            source: source_name,
            source_offset: next_offset,
            created_at_ms: now_ms,
        };
        let published = self.publish_wal_record(&rec, now_ms)?;

        report.published = published.published;
        report.part_id = published.part_id;
        report.snapshot_id = published.snapshot_id;
        report.source_offset_after = published.source_offset_after;

        // Persist the widened key dictionary. Only the keys of events that actually
        // made it in — a rejected event never widens it.
        prism_part::io::write_atomic(&self.dict_path, &serde_json::to_vec(&dict)?)?;

        // Housekeeping, well away from the critical path. Compact everything the committed snapshot's
        // marker now covers ([D-077](../../../docs/DECISIONS.md)).
        idem.prune(now_ms);
        let floor = self.engine.snapshot()?.applied_wal_record;
        self.wal.compact_through(floor)?;

        Ok(report)
    }

    /// Everything after the ack: embed → part → catalog → idempotency → offset.
    ///
    /// Shared by the live path and by recovery, on purpose. A recovery path that is
    /// a *different* code path from the live path is a recovery path that is only
    /// exercised during disasters.
    fn publish_wal_record(&self, rec: &WalRecord, now_ms: i64) -> Result<IngestReport2> {
        let mut report = IngestReport2 {
            offered: rec.events.len(),
            wal_record: Some(rec.record_id),
            source: rec.source.clone(),
            ..Default::default()
        };

        // --- embed + write the part + commit the catalog ---
        //
        // `Engine::ingest_wal` owns the generation bootstrap, the embedding, the dead-lettering of
        // unembeddable rows, the part write, and the atomic commit — and the commit stamps this
        // record's id as the snapshot's `applied_wal_record` marker ([D-077](../../../docs/DECISIONS.md)),
        // so publication and applied-progress-marking are one rename. The kill point between embedding
        // and the part write lives inside it.
        let inner = self
            .engine
            .ingest_wal(rec.events.clone(), now_ms, rec.record_id)?;

        report.published = inner.admitted;
        report.dead_lettered += inner.dead_lettered;
        report.part_id = inner.part_id;
        report.snapshot_id = inner.snapshot_id;

        // --- the catalog is committed; the events are VISIBLE, and the log already records them as
        // applied (the marker rode the commit). ---
        //
        // Only now may an idempotency record advance (invariant 7). Recording a key *before*
        // publication would mean that a crash here leaves the retry suppressed as a "replay" of an
        // event that exists nowhere — silently, permanently lost, by the very index that was supposed
        // to protect it. A crash *after* the commit but before this is reconciled on recovery.
        let mut idem = self.idempotency()?;
        for e in &rec.events {
            let hash = e.content_hash();
            let (t, k) = e.dedup_key();
            idem.record(t, k, &hash, e.event_time);
        }
        idem.save(&self.idem_path)?;

        // --- and only now, the source offset. ---
        prism_part::faults::maybe_kill("ingest.after_publish_before_offset_commit");
        if let (Some(name), Some(off)) = (&rec.source, rec.source_offset) {
            crate::source::commit_offset(&self.engine.store.root.join("sources"), name, off)?;
            report.source_offset_after = Some(off);
        }

        Ok(report)
    }

    /// Poll a source and drive one batch all the way through.
    pub fn poll_and_ingest(
        &mut self,
        source: &dyn Source,
        max: usize,
        now_ms: i64,
    ) -> Result<IngestReport2> {
        let batch = source.poll(max)?;
        if batch.events.is_empty() {
            let snap = self.engine.snapshot()?;
            return Ok(IngestReport2 {
                snapshot_id: snap.snapshot_id,
                source: Some(source.name().to_string()),
                source_offset_before: Some(source.committed_offset()?),
                source_offset_after: Some(source.committed_offset()?),
                ..Default::default()
            });
        }
        self.ingest(batch.events, Some(source), Some(batch.next_offset), now_ms)
    }
}
