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
use prism_part::generation::Generation;
use prism_part::part::{PartWriter, RowIn};
use prism_quantizer::{CoarseCodebook, PqCodebook};
use prism_types::error::Result;
use prism_types::event::{DeadLetter, Event};
use prism_types::rng::Rng;
use prism_types::Embedder;
use serde::{Deserialize, Serialize};
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

/// Cap on how many vectors are used to train a codebook. Reservoir-sampled, so
/// the sample is spread across everything offered rather than being "the first
/// N rows" — which would bake the first batch's distribution into the meaning
/// of every byte written afterwards.
pub const TRAIN_SAMPLE_MAX: usize = 50_000;

pub fn reservoir_sample(vectors: &[Vec<f32>], max: usize, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut chosen: Vec<usize> = Vec::with_capacity(max.min(vectors.len()));
    for i in 0..vectors.len() {
        if i < max {
            chosen.push(i);
        } else {
            let j = (rng.next_u64() % (i as u64 + 1)) as usize;
            if j < max {
                chosen[j] = i;
            }
        }
    }
    chosen.sort_unstable(); // deterministic output order
    let mut flat = Vec::with_capacity(chosen.len() * vectors.first().map_or(0, |v| v.len()));
    for i in chosen {
        flat.extend_from_slice(&vectors[i]);
    }
    flat
}

impl Engine {
    pub fn ingest(&self, events: Vec<Event>, now_ms: i64) -> Result<IngestReport> {
        let snap = self.snapshot()?;
        let dim = self.store.config.dim;

        let mut dead: Vec<DeadLetter> = Vec::new();
        let mut admitted: Vec<Event> = Vec::new();

        // --- 1. admission ---
        for e in events {
            if let Err(err) = e.validate() {
                dead.push(DeadLetter {
                    reason: err.to_string(),
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

                let sample = reservoir_sample(&vectors, TRAIN_SAMPLE_MAX, self.store.config.seed);
                let n = sample.len() / dim;
                let coarse = CoarseCodebook::train(
                    &sample,
                    n,
                    dim,
                    self.store.config.nlist,
                    self.store.config.seed,
                )?;
                let pq = PqCodebook::train(
                    &sample,
                    n,
                    dim,
                    self.store.config.pq_m,
                    self.store.config.seed,
                )?;
                let g = Generation::new(
                    embedder.model_id(),
                    embedder.model_version(),
                    dim,
                    coarse,
                    pq,
                    &format!("bootstrap: reservoir sample of {n} vectors from the first ingest"),
                )?;
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
        let manifest = PartWriter::write(
            &self.store.parts_dir(),
            snap.next_seq,
            &generation.generation_id,
            &generation.model_id,
            &generation.model_version,
            self.store.config.dim,
            self.store.config.pq_m,
            rows,
            now_ms,
        )?;

        let mut parts = snap.parts.clone();
        parts.push(manifest.part_id.clone());

        let new_snap = self.catalog().commit(
            snap,
            parts,
            snap.next_seq + 1,
            Some(generation.generation_id.clone()),
            now_ms,
        )?;

        Ok(IngestReport {
            admitted,
            dead_lettered: dead.len(),
            part_id: Some(manifest.part_id),
            snapshot_id: new_snap.snapshot_id,
            generation_id: generation.generation_id.clone(),
            trained_generation: trained,
        })
    }

    fn write_dead_letters(&self, dead: &[DeadLetter]) -> Result<()> {
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
                reason: err.to_string(),
                stage: "embedding".to_string(),
                event: e,
            }),
        }
    }
    ((kept_events, kept_vecs), dead)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservoir_sample_is_deterministic_and_bounded() {
        let vectors: Vec<Vec<f32>> = (0..1000).map(|i| vec![i as f32, 0.0]).collect();
        let a = reservoir_sample(&vectors, 100, 7);
        let b = reservoir_sample(&vectors, 100, 7);
        assert_eq!(a, b);
        assert_eq!(a.len(), 200); // 100 vectors x 2 dims
    }

    #[test]
    fn reservoir_sample_spans_the_whole_input_not_just_the_head() {
        let vectors: Vec<Vec<f32>> = (0..10_000).map(|i| vec![i as f32]).collect();
        let s = reservoir_sample(&vectors, 100, 3);
        let max = s.iter().cloned().fold(f32::MIN, f32::max);
        // If it were "the first 100 rows", the max would be 99.
        assert!(max > 1000.0, "sample looks like a head sample: max={max}");
    }

    #[test]
    fn sampling_everything_keeps_everything() {
        let vectors: Vec<Vec<f32>> = (0..10).map(|i| vec![i as f32]).collect();
        let s = reservoir_sample(&vectors, 100, 1);
        assert_eq!(s.len(), 10);
    }
}
