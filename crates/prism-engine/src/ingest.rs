//! Ingest (Part III §10).
//!
//! validate → embed → normalize → assign centroid → PQ-encode under the pinned
//! generation → sort by the inner key → write an immutable part → one atomic
//! catalog commit.
//!
//! The rule that shapes everything: **an event is never silently stored without
//! the semantic columns it asked for.** If it cannot be validated or cannot be
//! embedded, it goes to the dead-letter log where someone can see it. It does
//! not get a null vector, it does not get dropped, and it does not get stored
//! as an event that will never match a semantic query for reasons no one can
//! reconstruct later.

use crate::engine::Engine;
use prism_part::catalog::PartEntry;
use prism_part::generation::Generation;
use prism_part::part::{PartManifest, PartSpec, PartWriter, RowIn};
use prism_part::partition::{PartRef, PartitionKey};
use prism_quantizer::{CoarseCodebook, PqCodebook};
use prism_types::error::Result;
use prism_types::event::{DeadLetter, Event};
use prism_types::{Embedder, EmbeddingInput, EmbeddingPurpose};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;

/// A trained-but-not-yet-committed bootstrap generation and the embeddings it was trained from —
/// the output of [`Engine::train_generation`], reused by the single-store bootstrap (which writes
/// the parts) and the cluster (which discards the embeddings and installs the generation).
pub struct Trained {
    pub generation: Generation,
    pub events: Vec<Event>,
    pub vectors: Vec<Vec<f32>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IngestReport {
    pub admitted: usize,
    pub dead_lettered: usize,
    pub part_id: Option<String>,
    pub snapshot_id: String,
    pub generation_id: String,
    /// True when this ingest had to bootstrap the first generation by training
    /// codebooks. Worth knowing: those codebooks saw only this data.
    pub trained_generation: bool,
}

/// What the catalog records about a freshly written part — enough to prune it *without
/// opening it*, which is the whole S4 isolation property.
pub fn part_ref(m: &PartManifest, key: &PartitionKey) -> Result<PartRef> {
    Ok(PartRef {
        part_id: m.part_id.clone(),
        partition: key.clone(),
        rows: m.row_count,
        tenants: m.tenants.clone(),
        time_min: m.time_min,
        time_max: m.time_max,
    })
}

/// Cap on how many vectors train a codebook: see `crate::sample::TRAIN_SAMPLE_MAX`.
///
/// The position-keyed reservoir that used to live here is gone. It sampled by *index into a
/// vector built by reading parts in catalog order*, so the same rows, laid out differently,
/// trained a different codebook -- and a codebook is the meaning of every byte encoded under it.
/// Charter C-4 forbids the class; `crate::sample` is the replacement, keyed on `event_id`.
pub use crate::sample::TRAIN_SAMPLE_MAX;

impl Engine {
    /// Ingest a batch (S0 loader path): no WAL record, so the applied-progress marker is inherited
    /// unchanged from the parent snapshot.
    pub fn ingest(&self, events: Vec<Event>, now_ms: i64) -> Result<IngestReport> {
        self.ingest_inner(events, now_ms, None, true)
    }

    /// Ingest a batch **as the publication of a WAL record** ([D-077](../../../docs/DECISIONS.md)):
    /// the resulting snapshot records `record_id` as its `applied_wal_record`, so publication and
    /// applied-progress-marking are one atomic commit and recovery can never double-publish it.
    pub fn ingest_wal(
        &self,
        events: Vec<Event>,
        now_ms: i64,
        record_id: u64,
    ) -> Result<IngestReport> {
        self.ingest_inner(events, now_ms, Some(record_id), false)
    }

    fn ingest_inner(
        &self,
        events: Vec<Event>,
        now_ms: i64,
        applied_wal_record: Option<u64>,
        preflight_model: bool,
    ) -> Result<IngestReport> {
        let snap = self.snapshot()?;
        let dim = self.store.config.dim;

        let mut dead: Vec<DeadLetter> = Vec::new();
        let mut admitted: Vec<Event> = Vec::new();

        // --- 1. admission ---
        for e in events {
            if let Err((reason, detail)) = e.validate() {
                dead.push(DeadLetter {
                    reason: reason.to_string(),
                    detail,
                    stage: "admission".to_string(),
                    event: e,
                });
                continue;
            }
            admitted.push(e);
        }
        if preflight_model {
            let (allowed, denied) = self.model_preflight_ingest_for_snapshot(&snap, admitted)?;
            admitted = allowed;
            dead.extend(denied);
            if admitted.is_empty() {
                return self.finish_empty(&snap, dead, now_ms);
            }
        }

        // --- 2. resolve or bootstrap the generation, then embed under it ---
        let (generation, trained) = match &snap.active_generation {
            Some(g) => (self.catalog().get_generation(g)?, false),
            None => {
                // The bootstrap generation: the one honest exception to "never the first
                // batch", because the first batch is all there is. Training is factored out
                // (`train_generation`) so a cluster can train it the SAME way over a cluster-wide
                // sample and install it on every shard — a codebook fit to one shard's tenants is
                // the D-072 mistake ([D-072](../../../docs/DECISIONS.md)).
                let (trained, failed) = self.train_generation(&snap.snapshot_id, admitted)?;
                dead.extend(failed);
                let Some(t) = trained else {
                    return self.finish_empty(&snap, dead, now_ms);
                };
                self.catalog().put_generation(&t.generation)?;
                return self.write_and_commit(
                    &snap,
                    &t.generation,
                    t.events,
                    t.vectors,
                    dead,
                    true,
                    now_ms,
                    applied_wal_record,
                );
            }
        };

        let embedder = self.plane.embedder(
            &generation.model_id,
            &generation.model_version,
            dim,
            generation.model_artifacts.as_ref(),
        )?;
        let (kept, failed) = embed_all(&*embedder, admitted);
        dead.extend(failed);

        self.write_and_commit(
            &snap,
            &generation,
            kept.0,
            kept.1,
            dead,
            trained,
            now_ms,
            applied_wal_record,
        )
    }

    pub(crate) fn model_preflight_ingest(
        &self,
        events: Vec<Event>,
    ) -> Result<(Vec<Event>, Vec<DeadLetter>)> {
        let snap = self.snapshot()?;
        self.model_preflight_ingest_for_snapshot(&snap, events)
    }

    fn model_preflight_ingest_for_snapshot(
        &self,
        snap: &prism_part::catalog::Snapshot,
        events: Vec<Event>,
    ) -> Result<(Vec<Event>, Vec<DeadLetter>)> {
        if events.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let dim = self.store.config.dim;
        let (model_id, model_version) = match &snap.active_generation {
            Some(generation_id) => {
                let generation = self.catalog().get_generation(generation_id)?;
                (generation.model_id, generation.model_version)
            }
            None => {
                let embedder = self.plane.default_embedder(dim)?;
                (
                    embedder.model_id().to_string(),
                    embedder.model_version().to_string(),
                )
            }
        };
        let inputs: Vec<EmbeddingInput<'_>> = events
            .iter()
            .map(|event| EmbeddingInput {
                tenant_id: Some(&event.tenant_id),
                purpose: EmbeddingPurpose::Ingest,
                text: &event.body,
            })
            .collect();
        let decisions = self.plane.preflight(&model_id, &model_version, &inputs);
        if decisions.len() != events.len() {
            return Err(prism_types::PrismError::Invariant(format!(
                "model policy returned {} preflight decisions for {} events",
                decisions.len(),
                events.len()
            )));
        }
        let mut allowed = Vec::new();
        let mut denied = Vec::new();
        for (event, decision) in events.into_iter().zip(decisions) {
            match decision {
                Ok(()) => allowed.push(event),
                Err(prism_types::PrismError::Policy(detail)) => denied.push(DeadLetter {
                    reason: prism_types::RejectReason::ModelPolicyDenied.to_string(),
                    detail,
                    stage: "model_policy".to_string(),
                    event,
                }),
                Err(error) => return Err(error),
            }
        }
        Ok((allowed, denied))
    }

    /// Train a bootstrap generation from a batch — the shared trainer the single-store bootstrap and
    /// the cluster both use. Deterministic in `(events, seed, config, snapshot_id)`: the stratified
    /// sample is seeded on `snapshot_id`, and the codebooks on `config.seed`, so training the same
    /// events with the same seed yields a **byte-identical, content-addressed** generation — which is
    /// exactly what lets a cluster train one codebook over a cluster-wide sample and install it on
    /// every shard ([D-072](../../../docs/DECISIONS.md)). Does **not** commit; the caller installs it.
    /// Returns `None` when nothing embeds (the caller finishes empty), plus the dead-letters either way.
    pub fn train_generation(
        &self,
        snapshot_id: &str,
        admitted: Vec<Event>,
    ) -> Result<(Option<Trained>, Vec<DeadLetter>)> {
        let dim = self.store.config.dim;
        let embedder = self.plane.default_embedder(dim)?;
        let (kept, dead) = embed_all(&*embedder, admitted);
        let (events, vectors) = kept;
        if vectors.is_empty() {
            return Ok((None, dead));
        }
        let (sample, prov) = crate::sample::stratified_sample(
            &crate::generations::sample_rows(&events, &vectors),
            crate::sample::TRAIN_SAMPLE_MAX,
            self.store.config.seed,
            snapshot_id,
            true,
        )?;
        let n = prov.rows_sampled;
        let coarse = CoarseCodebook::train_restarts(
            &sample,
            n,
            dim,
            self.store.config.nlist,
            self.store.config.seed,
            self.store.config.kmeans_restarts,
        )?;
        let pq = PqCodebook::train_restarts(
            &sample,
            n,
            dim,
            self.store.config.pq_m,
            self.store.config.seed,
            self.store.config.kmeans_restarts,
        )?;
        let trained_from = format!(
            "bootstrap (PROVISIONAL): stratified sample of {n} vectors from the first ingest, \
             keyed on event_id"
        );
        let generation = match embedder.artifacts() {
            Some(artifacts) => Generation::new_registered(
                embedder.model_id(),
                embedder.model_version(),
                artifacts.clone(),
                dim,
                coarse,
                pq,
                &trained_from,
            )?,
            None => Generation::new(
                embedder.model_id(),
                embedder.model_version(),
                dim,
                coarse,
                pq,
                &trained_from,
            )?,
        }
        .with_training(crate::generations::provenance(&prov));
        Ok((
            Some(Trained {
                generation,
                events,
                vectors,
            }),
            dead,
        ))
    }

    fn finish_empty(
        &self,
        snap: &prism_part::catalog::Snapshot,
        dead: Vec<DeadLetter>,
        _now_ms: i64,
    ) -> Result<IngestReport> {
        let n = dead.len();
        self.write_dead_letters(&dead)?;
        Ok(IngestReport {
            admitted: 0,
            dead_lettered: n,
            part_id: None,
            snapshot_id: snap.snapshot_id.clone(),
            generation_id: snap.active_generation.clone().unwrap_or_default(),
            trained_generation: false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn write_and_commit(
        &self,
        snap: &prism_part::catalog::Snapshot,
        generation: &Generation,
        events: Vec<Event>,
        vectors: Vec<Vec<f32>>,
        dead: Vec<DeadLetter>,
        trained: bool,
        now_ms: i64,
        applied_wal_record: Option<u64>,
    ) -> Result<IngestReport> {
        // Dead letters are durable *before* the commit. An operator must never
        // be able to see the rows that made it in without being able to see the
        // rows that did not.
        self.write_dead_letters(&dead)?;

        if events.is_empty() {
            return Ok(IngestReport {
                admitted: 0,
                dead_lettered: dead.len(),
                part_id: None,
                snapshot_id: snap.snapshot_id.clone(),
                generation_id: generation.generation_id.clone(),
                trained_generation: trained,
            });
        }

        let rows: Vec<RowIn> = events
            .into_iter()
            .zip(vectors)
            .map(|(event, vector)| {
                let (centroid, _) = generation.coarse.assign(&vector);
                let code = generation.pq.encode(&vector)?;
                Ok(RowIn {
                    event,
                    centroid,
                    code,
                    vector,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let admitted = rows.len();

        // --- buffer by outer partition (S4) ---
        //
        // `tenant-bucket x event-time window x generation`. One part per partition, so a part
        // never spans two buckets and a query for one tenant never has a reason to open a part
        // belonging to another. Keyed on event_time -- always -- because agent telemetry is
        // late by nature and keying on arrival would smear one trace across partitions.
        let scheme = &self.store.config.partitions;
        let mut by_partition: BTreeMap<PartitionKey, Vec<RowIn>> = BTreeMap::new();
        for r in rows {
            let key = PartitionKey {
                bucket: scheme.bucket_of(&r.event.tenant_id),
                window: scheme.window_of(r.event.event_time),
                generation: generation.generation_id.clone(),
            };
            by_partition.entry(key).or_default().push(r);
        }

        // **The crash that matters most.** The batch is acked (it is in the WAL),
        // the embedding has already cost GPU time, and these events exist nowhere
        // durable but the log. Recovery must bring them back -- exactly once, with
        // their semantic columns.
        prism_part::faults::maybe_kill("ingest.after_embed_before_part");

        let mut parts = snap.parts.clone();
        let mut seq = snap.next_seq;
        let mut first_part: Option<String> = None;
        let mut new_ids: Vec<String> = Vec::new();

        for (key, rows) in by_partition {
            // A fresh DEK per part; `None` on a plaintext store, which is the default.
            let bucket_ordinal =
                prism_part::partition::bucket_ordinal(&self.store.config.partitions, &key.bucket);
            let spec = PartSpec {
                tenant_tokenizer: self.tenant_tokenizer()?,
                partition: Some(key.clone()),
                promote: self.store.config.promote.clone(),
                lineage: Default::default(),
                encryption: self.part_encryption_for(bucket_ordinal, 1)?,
            };
            let manifest = PartWriter::write(
                &self.store.parts_dir(),
                seq,
                &generation.generation_id,
                &generation.model_id,
                &generation.model_version,
                self.store.config.dim,
                self.store.config.pq_m,
                self.store.config.block_size,
                &spec,
                rows,
                now_ms,
            )?;
            seq += 1;
            first_part.get_or_insert(manifest.part_id.clone());
            new_ids.push(manifest.part_id.clone());
            parts.push(PartEntry::Located(part_ref(&manifest, &key)?));
        }

        // **Remote-durable publication (invariant 2, extended).** Every new part's cold tier is
        // uploaded and verified on the object store *before* the catalog references it — so a
        // snapshot can never name a part whose exact-vector bytes are not durable and complete on
        // the backend. The two kill points inside bracket the boundary; a crash there leaves the
        // catalog at the old snapshot and the bytes an orphan, never a dangling reference.
        for id in &new_ids {
            self.publish_part_cold(id)?;
        }

        // **Fence the write path (D-076).** A writer that acquired ownership must still hold it at
        // commit: if a restart acquired a higher epoch while this writer was mid-publication, refuse
        // by name here — before any durable commit — so the superseded writer publishes nothing (no
        // torn catalog, no duplicate parts). The pause point freezes a writer exactly here, after its
        // parts are written but before the commit, so the zombie gate can drive a real takeover.
        // A no-op for the single-writer path (an engine that never acquired ownership).
        prism_part::faults::maybe_pause("publish.before_commit_fence");
        self.assert_write_owner()?;

        // **Catalog mirror convergence (D-069).** Before committing the new snapshot, bring the
        // mirror up to the parent — healing any earlier crash between a `CURRENT` rename and its
        // mirror write. Safe because the mirror never leads; idempotent when already caught up.
        self.mirror_snapshot(snap)?;

        // The WAL applied-progress marker rides *inside* this atomic commit (D-077): a WAL publish
        // sets it to the record being published, an S0 write inherits the parent's. So the batch
        // becoming visible and the log recording it as applied are the same rename — a crash between
        // them is impossible, and recovery replays only records above the committed marker.
        let mut meta = prism_part::catalog::SnapshotMeta::of(snap);
        if applied_wal_record.is_some() {
            meta.applied_wal_record = applied_wal_record;
        }
        let new_snap = self.catalog().commit_meta(
            snap,
            parts,
            seq,
            Some(generation.generation_id.clone()),
            meta,
            now_ms,
        )?;

        // **The catalog mirror (D-069).** Local `CURRENT` has been renamed — the commit is live. Now
        // CAS-write the snapshot to the object-store mirror, which lags the local truth and never
        // leads it. A crash at this kill point leaves the mirror one snapshot behind; the next write
        // (which re-mirrors the parent above) or an explicit recovery converges it — old-or-new holds
        // because the local rename already decided the outcome.
        prism_part::faults::maybe_kill("mirror.after_rename_before_mirror");
        self.mirror_snapshot(&new_snap)?;

        Ok(IngestReport {
            admitted,
            dead_lettered: dead.len(),
            part_id: first_part,
            snapshot_id: new_snap.snapshot_id,
            generation_id: generation.generation_id.clone(),
            trained_generation: trained,
        })
    }

    pub fn write_dead_letters(&self, dead: &[DeadLetter]) -> Result<()> {
        if dead.is_empty() {
            return Ok(());
        }
        let path = self.store.deadletter_path();
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        for d in dead {
            f.write_all(serde_json::to_string(d)?.as_bytes())?;
            f.write_all(b"\n")?;
        }
        f.sync_all()?;
        Ok(())
    }
}

type Embedded = (Vec<Event>, Vec<Vec<f32>>);

/// Embed a batch, splitting it into what survived and what must be dead-lettered.
fn embed_all(embedder: &dyn Embedder, events: Vec<Event>) -> (Embedded, Vec<DeadLetter>) {
    let inputs: Vec<EmbeddingInput<'_>> = events
        .iter()
        .map(|event| EmbeddingInput {
            tenant_id: Some(&event.tenant_id),
            purpose: EmbeddingPurpose::Ingest,
            text: &event.body,
        })
        .collect();
    let results = embedder.embed_batch_scoped(&inputs);
    if results.len() != events.len() {
        let detail = format!(
            "embedder returned {} results for {} events; refusing the entire batch",
            results.len(),
            events.len()
        );
        let dead = events
            .into_iter()
            .map(|event| DeadLetter {
                reason: prism_types::RejectReason::EmbeddingFailed.to_string(),
                detail: detail.clone(),
                stage: "embedding".to_string(),
                event,
            })
            .collect();
        return ((Vec::new(), Vec::new()), dead);
    }
    let mut kept_events = Vec::with_capacity(events.len());
    let mut kept_vecs = Vec::with_capacity(events.len());
    let mut dead = Vec::new();

    for (e, result) in events.into_iter().zip(results) {
        match result {
            Ok(v) => {
                kept_events.push(e);
                kept_vecs.push(v);
            }
            Err(err) => {
                let reason = if matches!(&err, prism_types::PrismError::Policy(_)) {
                    prism_types::RejectReason::ModelPolicyDenied
                } else {
                    prism_types::RejectReason::EmbeddingFailed
                };
                dead.push(DeadLetter {
                    reason: reason.to_string(),
                    detail: err.to_string(),
                    stage: "embedding".to_string(),
                    event: e,
                });
            }
        }
    }
    ((kept_events, kept_vecs), dead)
}

// The sampler's own tests moved to `crate::sample` along with the sampler. The three that used
// to live here tested a reservoir keyed on *position*, which is exactly the thing charter C-4
// forbids -- they asserted the old behaviour was deterministic, and it was, and that was the bug.

#[cfg(test)]
mod tests {
    use super::*;
    use prism_types::error::{PrismError, Result};

    struct ShortBatchEmbedder;

    impl Embedder for ShortBatchEmbedder {
        fn model_id(&self) -> &str {
            "broken-test-model"
        }

        fn model_version(&self) -> &str {
            "1"
        }

        fn dim(&self) -> usize {
            8
        }

        fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            Err(PrismError::Invariant("single path should not run".into()))
        }

        fn embed_batch(&self, _texts: &[&str]) -> Vec<Result<Vec<f32>>> {
            Vec::new()
        }
    }

    struct PolicyDenyEmbedder;

    impl Embedder for PolicyDenyEmbedder {
        fn model_id(&self) -> &str {
            "governed-test-model"
        }

        fn model_version(&self) -> &str {
            "1"
        }

        fn dim(&self) -> usize {
            8
        }

        fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            unreachable!("scoped path must run")
        }

        fn embed_batch_scoped(
            &self,
            inputs: &[prism_types::EmbeddingInput<'_>],
        ) -> Vec<Result<Vec<f32>>> {
            inputs
                .iter()
                .map(|_| Err(PrismError::Policy("test denial".into())))
                .collect()
        }
    }

    #[test]
    fn a_batch_cardinality_violation_dead_letters_every_event() {
        let events = crate::corpus::generate(crate::corpus::Kind::Uniform, 4, 17);
        let ((kept, vectors), dead) = embed_all(&ShortBatchEmbedder, events);
        assert!(kept.is_empty());
        assert!(vectors.is_empty());
        assert_eq!(dead.len(), 4);
        assert!(dead
            .iter()
            .all(|letter| letter.detail.contains("0 results for 4 events")));
    }

    #[test]
    fn a_policy_denial_has_a_stable_dead_letter_reason() {
        let events = crate::corpus::generate(crate::corpus::Kind::Uniform, 2, 19);
        let ((kept, vectors), dead) = embed_all(&PolicyDenyEmbedder, events);
        assert!(kept.is_empty());
        assert!(vectors.is_empty());
        assert_eq!(dead.len(), 2);
        assert!(dead
            .iter()
            .all(|letter| letter.reason == "model_policy_denied"));
    }
}
