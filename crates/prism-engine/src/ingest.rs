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
use prism_types::Embedder;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;

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
    pub fn ingest(&self, events: Vec<Event>, now_ms: i64) -> Result<IngestReport> {
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

        // --- 2. resolve or bootstrap the generation, then embed under it ---
        let (generation, trained) = match &snap.active_generation {
            Some(g) => (self.catalog().get_generation(g)?, false),
            None => {
                let embedder = self.plane.default_embedder(dim);
                let (kept, failed) = embed_all(&*embedder, admitted);
                admitted = kept.0;
                let vectors = kept.1;
                dead.extend(failed);

                if vectors.is_empty() {
                    return self.finish_empty(&snap, dead, now_ms);
                }

                // The bootstrap generation: the one honest exception to "never the first
                // batch", because the first batch is all there is. It is stratified, keyed on
                // event_id, and recorded as PROVISIONAL -- which is exactly what `prism
                // generation create` exists to replace. A provisional generation is not a bug;
                // a provisional generation nobody told you about is.
                let (sample, prov) = crate::sample::stratified_sample(
                    &crate::generations::sample_rows(&admitted, &vectors),
                    crate::sample::TRAIN_SAMPLE_MAX,
                    self.store.config.seed,
                    &snap.snapshot_id,
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
                let g = Generation::new(
                    embedder.model_id(),
                    embedder.model_version(),
                    dim,
                    coarse,
                    pq,
                    &format!(
                        "bootstrap (PROVISIONAL): stratified sample of {n} vectors from the \
                         first ingest, keyed on event_id"
                    ),
                )?
                .with_training(crate::generations::provenance(&prov));
                self.catalog().put_generation(&g)?;

                return self.write_and_commit(&snap, &g, admitted, vectors, dead, true, now_ms);
            }
        };

        let embedder = self
            .plane
            .embedder(&generation.model_id, &generation.model_version, dim)?;
        let (kept, failed) = embed_all(&*embedder, admitted);
        dead.extend(failed);

        self.write_and_commit(&snap, &generation, kept.0, kept.1, dead, trained, now_ms)
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
    fn write_and_commit(
        &self,
        snap: &prism_part::catalog::Snapshot,
        generation: &Generation,
        events: Vec<Event>,
        vectors: Vec<Vec<f32>>,
        dead: Vec<DeadLetter>,
        trained: bool,
        now_ms: i64,
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

        for (key, rows) in by_partition {
            let spec = PartSpec {
                partition: Some(key.clone()),
                promote: self.store.config.promote.clone(),
                lineage: Default::default(),
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
            parts.push(PartEntry::Located(part_ref(&manifest, &key)?));
        }

        let new_snap = self.catalog().commit(
            snap,
            parts,
            seq,
            Some(generation.generation_id.clone()),
            now_ms,
        )?;

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
    let mut kept_events = Vec::with_capacity(events.len());
    let mut kept_vecs = Vec::with_capacity(events.len());
    let mut dead = Vec::new();

    for e in events {
        match embedder.embed(&e.body) {
            Ok(v) => {
                kept_events.push(e);
                kept_vecs.push(v);
            }
            Err(err) => dead.push(DeadLetter {
                reason: prism_types::RejectReason::EmbeddingFailed.to_string(),
                detail: err.to_string(),
                stage: "embedding".to_string(),
                event: e,
            }),
        }
    }
    ((kept_events, kept_vecs), dead)
}

// The sampler's own tests moved to `crate::sample` along with the sampler. The three that used
// to live here tested a reservoir keyed on *position*, which is exactly the thing charter C-4
// forbids -- they asserted the old behaviour was deterministic, and it was, and that was the bug.
