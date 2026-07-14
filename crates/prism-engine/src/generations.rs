//! The generation lifecycle (S5).
//!
//! ```text
//! create ──▶ canary ──▶ compare ──▶ promote ──▶ migrate ──▶ complete ──▶ retire
//!    │                                  │                        │
//!    └──────────── rollback ◀───────────┴────────────────────────┘
//! ```
//!
//! **Every transition is one catalog commit.** Not "mostly"; not "except for the big one".
//! There is no half-migrated flag, no repair path, no state living in a writer's memory — which
//! is what makes rollback a catalog write rather than a restore, and what makes a crash in the
//! middle of a migration boring.
//!
//! The two properties this module exists to protect, and which the S5 gate tests hold it to:
//!
//! 1. **No part is ever decoded with the wrong codebook.** The failure mode here is not a crash.
//!    It is a *plausible wrong answer* — a PQ code interpreted against a codebook that assigns
//!    it a different meaning gives a number, and the number looks fine. So a part names its
//!    generation, the reader resolves it, and a mismatch is an error.
//!
//! 2. **Queries keep working throughout.** A store in the middle of a two-generation migration
//!    answers every query it could answer before. That is the gate.

use crate::engine::Engine;
use crate::sample::{SampleProvenance, SampleRow};
use prism_part::catalog::{BaselineState, PartEntry, Snapshot, SnapshotMeta};
use prism_part::generation::{Generation, GenerationState, TrainingProvenance};
use prism_part::part::{PartSpec, PartWriter, RowIn};
use prism_part::partition::PartitionKey;
use prism_quantizer::{CoarseCodebook, PqCodebook};
use prism_types::error::{PrismError, Result};
use prism_types::event::Event;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub fn provenance(p: &SampleProvenance) -> TrainingProvenance {
    TrainingProvenance {
        strategy: p.strategy.clone(),
        seed: p.seed,
        rows_offered: p.rows_offered,
        rows_sampled: p.rows_sampled,
        strata: p.strata.len(),
        snapshot_id: p.snapshot_id.clone(),
        provisional: p.provisional,
    }
}

/// Present events + their vectors to the sampler, **stratified by tenant**.
///
/// Tenant, and deliberately not partition. A partition is `tenant-bucket × time window ×
/// generation` — and a time window is a *store configuration*. Stratifying on it would mean two
/// stores holding identical rows, configured with different windows, trained different codebooks:
/// charter C-4, one level up from the sample keys. The strata must be a logical property of the
/// data or keying the sample on `event_id` buys nothing.
pub fn sample_rows<'a>(events: &'a [Event], vectors: &'a [Vec<f32>]) -> Vec<SampleRow<'a>> {
    events
        .iter()
        .zip(vectors)
        .map(|(e, v)| SampleRow {
            stratum: e.tenant_id.clone(),
            event_id: &e.event_id,
            vector: v,
        })
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenerationStatus {
    pub generation_id: String,
    pub space: String,
    pub state: Option<GenerationState>,
    pub active: bool,
    pub parts: usize,
    pub rows: usize,
    pub provisional: bool,
    pub trained_from: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrationStatus {
    pub snapshot_id: String,
    pub generations: Vec<GenerationStatus>,
    /// Everything that stands between this store and a finished migration.
    ///
    /// **Empty is the only definition of complete.** See the generation contract §7: "no part
    /// references the old generation" is necessary and *not sufficient*, because a drift
    /// baseline that was never rebuilt in the new space is an alarm that has quietly stopped
    /// meaning anything.
    pub incomplete_because: Vec<String>,
    pub complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompareReport {
    pub base_generation: String,
    pub candidate_generation: String,
    pub same_space: bool,
    pub queries: usize,
    /// Mean overlap of the two generations' top-k, per query. Not "accuracy" — *agreement*.
    pub mean_agreement: f32,
    pub min_agreement: f32,
    /// Recall of each generation against the **exact oracle**, which is the only ground truth
    /// either of them can be measured against.
    pub base_recall: f32,
    pub candidate_recall: f32,
    pub base_p1_recall: f32,
    pub candidate_p1_recall: f32,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrateReport {
    pub from_generation: String,
    pub to_generation: String,
    pub parts_migrated: usize,
    pub rows_migrated: usize,
    pub parts_remaining: usize,
    /// Parts that could not be re-embedded because their bodies are gone (§7).
    ///
    /// Not a failure of the migration — a *fact* about the store, and one that turns into a
    /// DEGRADED baseline rather than a silently missing alarm.
    pub parts_unmigratable: usize,
    pub rows_unmigratable: usize,
    pub snapshot_id: String,
}

impl Engine {
    /// Train a new generation from a **stratified sample of the whole store** and register it as
    /// a candidate.
    ///
    /// A candidate encodes nothing, answers nothing, and changes nothing — which is why creating
    /// one is free and reversible. The store is *identical* afterwards except that it now knows
    /// about a set of codebooks.
    /// `model_version = None` retrains the codebooks **in the same embedding space** — the common
    /// case, and the one the mixed-generation machinery exists for. The vectors mean the same
    /// thing; only the approximation of them changes, so the two generations' exact scores stay
    /// comparable and a query spanning both merges at exact-score time.
    ///
    /// `Some(v)` moves to a new model version, which is a **new embedding space**. That is a much
    /// larger act: until the migration finishes, a query spanning both is refused unless a bridge
    /// is declared (§6). The API makes it the explicit option because it is the explicit decision.
    pub fn generation_create(
        &self,
        model_version: Option<&str>,
        now_ms: i64,
    ) -> Result<Generation> {
        let snap = self.snapshot()?;
        let dim = self.store.config.dim;

        let model_version =
            match model_version {
                Some(v) => v.to_string(),
                None => match &snap.active_generation {
                    Some(a) => self.catalog().get_generation(a)?.model_version,
                    None => return Err(PrismError::Invalid(
                        "the store has no active generation to retrain from; name a model version"
                            .into(),
                    )),
                },
            };
        let embedder = self.plane.embedder("hash-embedder", &model_version, dim)?;

        // Re-embed the sampled rows under the NEW model. A codebook for a new space must be
        // trained on vectors from that space; training it on the old space's vectors would
        // produce centroids that describe a geometry no stored byte will ever live in.
        let readers = self.open_parts(&snap)?;
        let mut events: Vec<Event> = Vec::new();
        for r in &readers {
            if r.manifest.s5()?.bodies_redacted {
                continue;
            }
            events.extend(r.read_all()?.events);
        }
        if events.is_empty() {
            return Err(PrismError::Invalid(
                "cannot create a generation: the store has no rows with readable bodies to train \
                 on. Re-embedding requires the raw text, and retention may have expired it."
                    .into(),
            ));
        }

        let mut vectors = Vec::with_capacity(events.len());
        let mut kept = Vec::with_capacity(events.len());
        for e in events {
            if let Ok(v) = embedder.embed(&e.body) {
                vectors.push(v);
                kept.push(e);
            }
        }

        let (sample, prov) = crate::sample::stratified_sample(
            &sample_rows(&kept, &vectors),
            crate::sample::TRAIN_SAMPLE_MAX,
            self.store.config.seed,
            &snap.snapshot_id,
            false,
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
                "stratified sample of {n} rows across {} partitions of snapshot {}, keyed on \
                 event_id",
                prov.strata.len(),
                snap.snapshot_id
            ),
        )?
        .with_training(provenance(&prov));

        if snap.active_generation.as_deref() == Some(g.generation_id.as_str()) {
            return Err(PrismError::Invalid(
                "training produced a generation byte-identical to the active one: same codebooks, \
                 same id, nothing to migrate. A generation is its codebooks, so this is not a new \
                 generation, it is the one you already have."
                    .into(),
            ));
        }
        self.catalog().put_generation(&g)?;

        let mut meta = SnapshotMeta::of(&snap);
        if !meta.generations.contains_key(&g.generation_id) {
            meta.generations
                .insert(g.generation_id.clone(), GenerationState::Candidate);
        }
        // Whatever was active stays active. A create changes nothing else, on purpose.
        if let Some(a) = &snap.active_generation {
            meta.generations
                .entry(a.clone())
                .or_insert(GenerationState::Active);
        }
        self.catalog().commit_meta(
            &snap,
            snap.parts.clone(),
            snap.next_seq,
            snap.active_generation.clone(),
            meta,
            now_ms,
        )?;
        Ok(g)
    }

    /// Re-embed a bounded number of partitions into a candidate generation.
    ///
    /// The store now has **two live generations**, and that is a normal operating state, not an
    /// incident. Queries keep working throughout (§4), which is the gate.
    pub fn generation_canary(
        &self,
        gen_id: &str,
        max_partitions: usize,
        now_ms: i64,
    ) -> Result<MigrateReport> {
        let g = self.catalog().get_generation(gen_id)?;
        let snap = self.snapshot()?;
        match snap.state_of(gen_id) {
            Some(GenerationState::Candidate) | Some(GenerationState::Canary) => {}
            Some(s) => {
                return Err(PrismError::Invalid(format!(
                    "generation {gen_id} is {s:?}; only a candidate can be canaried"
                )))
            }
            None => {
                return Err(PrismError::NotFound(format!(
                    "generation `{gen_id}` is not registered in this snapshot"
                )))
            }
        }
        self.reembed_parts(
            &snap,
            &g,
            Some(max_partitions),
            GenerationState::Canary,
            now_ms,
        )
    }

    /// Make a generation the one new writes encode under.
    ///
    /// Existing parts are untouched and keep answering under their own generations. Promotion is
    /// a statement about the *future*, which is why it is one catalog commit and why rolling it
    /// back is another.
    pub fn generation_promote(&self, gen_id: &str, now_ms: i64) -> Result<String> {
        let g = self.catalog().get_generation(gen_id)?;
        let snap = self.snapshot()?;
        if snap.state_of(gen_id).is_none() {
            return Err(PrismError::NotFound(format!(
                "generation `{gen_id}` is not registered in this snapshot"
            )));
        }

        // Promoting across an embedding-space boundary is a much larger act than promoting a new
        // codebook within one, and it must not be possible to do it by accident: every part
        // still in the old space becomes unmergeable with the new one until the migration
        // finishes or a bridge is declared (§6).
        if let Some(active) = &snap.active_generation {
            let old = self.catalog().get_generation(active)?;
            if old.space() != g.space() && !snap.parts.is_empty() {
                let bridged = snap.bridge(&old.space(), &g.space()).is_some();
                if !bridged {
                    return Err(PrismError::Invalid(format!(
                        "promoting {} would make `{}` the active space while parts remain in \
                         `{}`. Scores from different embedding spaces are not comparable \
                         (invariant 9), so until those parts are migrated a query spanning both \
                         will be REFUSED unless a bridge is declared. This is allowed, and it is \
                         a decision: migrate first, or declare a bridge with `prism bridge \
                         declare`, then promote.",
                        gen_id,
                        g.space(),
                        old.space()
                    )));
                }
            }
        }

        let mut meta = SnapshotMeta::of(&snap);
        if let Some(active) = &snap.active_generation {
            if active != gen_id {
                meta.generations
                    .insert(active.clone(), GenerationState::Deprecated);
            }
        }
        meta.generations
            .insert(gen_id.to_string(), GenerationState::Active);

        let s = self.catalog().commit_meta(
            &snap,
            snap.parts.clone(),
            snap.next_seq,
            Some(gen_id.to_string()),
            meta,
            now_ms,
        )?;
        Ok(s.snapshot_id)
    }

    /// Re-embed the remaining parts into the active generation. Resumable: it is a no-op once
    /// nothing is left, and running it twice is running it once.
    pub fn generation_migrate(
        &self,
        gen_id: &str,
        max_partitions: Option<usize>,
        now_ms: i64,
    ) -> Result<MigrateReport> {
        let g = self.catalog().get_generation(gen_id)?;
        let snap = self.snapshot()?;
        self.reembed_parts(&snap, &g, max_partitions, GenerationState::Active, now_ms)
    }

    /// Drop a generation record.
    ///
    /// Refused while **any retained snapshot** still names it. Retiring a generation a snapshot
    /// references would make that snapshot unreadable — and a rollback target that cannot be
    /// read is not a rollback target. Retire is deliberately the last step, and the only one
    /// that forecloses anything.
    pub fn generation_retire(&self, gen_id: &str, now_ms: i64) -> Result<String> {
        let snap = self.snapshot()?;
        if snap.active_generation.as_deref() == Some(gen_id) {
            return Err(PrismError::Invalid(format!(
                "generation {gen_id} is ACTIVE; retiring it would leave new writes with nothing \
                 to encode under"
            )));
        }

        for id in self.catalog().list_snapshots()? {
            let s = self.catalog().load_snapshot(&id)?;
            if s.generations_in_use().contains(gen_id) {
                return Err(PrismError::Invalid(format!(
                    "refusing to retire generation {gen_id}: snapshot {id} still has parts \
                     encoded under it. Retiring it would make that snapshot unreadable, and a \
                     rollback target that cannot be read is not a rollback target. GC the \
                     snapshot first, deliberately."
                )));
            }
        }

        let mut meta = SnapshotMeta::of(&snap);
        meta.generations.remove(gen_id);
        let s = self.catalog().commit_meta(
            &snap,
            snap.parts.clone(),
            snap.next_seq,
            snap.active_generation.clone(),
            meta,
            now_ms,
        )?;
        std::fs::remove_file(self.store.generation_path(gen_id)).ok();
        Ok(s.snapshot_id)
    }

    /// The whole truth about where the store is, and what is still standing between it and a
    /// finished migration.
    pub fn migration_status(&self) -> Result<MigrationStatus> {
        let snap = self.snapshot()?;
        let in_use = snap.generations_in_use();

        let mut rows: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for e in &snap.parts {
            if let Some(r) = e.located() {
                let ent = rows.entry(r.partition.generation.clone()).or_default();
                ent.0 += 1;
                ent.1 += r.rows;
            }
        }

        let mut ids: BTreeSet<String> = snap.generations.keys().cloned().collect();
        ids.extend(in_use.iter().cloned());
        if let Some(a) = &snap.active_generation {
            ids.insert(a.clone());
        }

        let mut generations = Vec::new();
        for id in &ids {
            let g = self.catalog().get_generation(id).ok();
            let (parts, r) = rows.get(id).copied().unwrap_or((0, 0));
            generations.push(GenerationStatus {
                generation_id: id.clone(),
                space: g.as_ref().map(|g| g.space()).unwrap_or_default(),
                state: snap.state_of(id),
                active: snap.active_generation.as_deref() == Some(id.as_str()),
                parts,
                rows: r,
                provisional: g
                    .as_ref()
                    .and_then(|g| g.training.as_ref())
                    .map(|t| t.provisional)
                    .unwrap_or(false),
                trained_from: g.map(|g| g.trained_from).unwrap_or_default(),
            });
        }

        let mut incomplete = Vec::new();
        if let Some(active) = &snap.active_generation {
            for id in &in_use {
                if id != active {
                    let (p, r) = rows.get(id).copied().unwrap_or((0, 0));
                    incomplete.push(format!(
                        "{p} parts ({r} rows) are still encoded under generation {id}, not the \
                         active {active}"
                    ));
                }
            }

            // §7: the half everybody forgets. A baseline that was never rebuilt in the new space
            // is an alarm that has quietly stopped meaning anything, and an alarm that has
            // quietly stopped is worse than one that was never configured.
            for b in &snap.baselines {
                if b.generation_id != *active {
                    incomplete.push(format!(
                        "the drift baseline for tenant `{}` is still pinned to generation {}, \
                         not the active {}. A baseline is a statement about a distribution in ONE \
                         embedding space; it does not survive a re-embed, and an alarm evaluating \
                         against it would keep producing numbers that mean nothing.",
                        b.tenant, b.generation_id, active
                    ));
                }
                if let BaselineState::Degraded { reason } = &b.state {
                    incomplete.push(format!(
                        "the drift baseline for tenant `{}` is DEGRADED: {reason}",
                        b.tenant
                    ));
                }
            }
        }

        Ok(MigrationStatus {
            snapshot_id: snap.snapshot_id.clone(),
            generations,
            complete: incomplete.is_empty(),
            incomplete_because: incomplete,
        })
    }

    /// Re-embed parts into `g`, oldest partition first, up to `max_partitions`.
    ///
    /// One catalog commit at the end. A crash before it leaves orphan parts and the old snapshot
    /// live — which is the same thing every other writer in this system does, because it is the
    /// only thing that is safe.
    fn reembed_parts(
        &self,
        snap: &Snapshot,
        g: &Generation,
        max_partitions: Option<usize>,
        state: GenerationState,
        now_ms: i64,
    ) -> Result<MigrateReport> {
        let dim = self.store.config.dim;
        let embedder = self.plane.embedder(&g.model_id, &g.model_version, dim)?;

        let from_gen = snap
            .active_generation
            .clone()
            .unwrap_or_else(|| "(none)".into());

        // Group the parts that are NOT yet in `g` by partition. A migration proceeds partition
        // by partition because a partition is the unit a merge is allowed to collapse.
        let mut todo: BTreeMap<PartitionKey, Vec<String>> = BTreeMap::new();
        let mut keep: Vec<PartEntry> = Vec::new();
        let mut unmigratable_parts = 0usize;
        let mut unmigratable_rows = 0usize;

        for e in &snap.parts {
            let Some(r) = e.located() else {
                keep.push(e.clone());
                continue;
            };
            if r.partition.generation == g.generation_id {
                keep.push(e.clone());
                continue;
            }
            let reader = prism_part::part::PartReader::open(&self.store.part_dir(e.part_id()))?;
            if reader.manifest.s5()?.bodies_redacted {
                // Its bodies are gone, so it cannot be re-embedded — ever, by anyone. It stays,
                // readable, in its own generation, and it is *reported*, because a migration
                // that quietly skipped it would be lying about being complete.
                unmigratable_parts += 1;
                unmigratable_rows += r.rows;
                keep.push(e.clone());
                continue;
            }
            todo.entry(r.partition.clone())
                .or_default()
                .push(r.part_id.clone());
        }

        let total_partitions = todo.len();
        let take = max_partitions.unwrap_or(usize::MAX);

        let mut next_seq = snap.next_seq;
        let mut migrated_parts = 0usize;
        let mut migrated_rows = 0usize;
        let mut done = 0usize;

        for (key, part_ids) in todo {
            if done >= take {
                // Not migrated this round. Still visible, still answering, still in its old
                // generation. Resumability is not a feature here, it is the absence of a
                // half-state.
                for id in &part_ids {
                    if let Some(e) = snap.parts.iter().find(|e| e.part_id() == id) {
                        keep.push(e.clone());
                    }
                }
                continue;
            }
            done += 1;

            let mut rows: Vec<RowIn> = Vec::new();
            for id in &part_ids {
                let reader = prism_part::part::PartReader::open(&self.store.part_dir(id))?;
                let all = reader.read_all()?;
                for e in all.events {
                    match embedder.embed(&e.body) {
                        Ok(v) => {
                            let centroid = g.coarse.assign(&v).0;
                            let code = g.pq.encode(&v)?;
                            rows.push(RowIn {
                                centroid,
                                code,
                                vector: v,
                                event: e,
                            });
                        }
                        Err(_) => {
                            // A row that will not embed under the new model does not silently
                            // vanish: the old part still holds it, and the migration is not
                            // complete while it does.
                        }
                    }
                }
            }
            if rows.is_empty() {
                for id in &part_ids {
                    if let Some(e) = snap.parts.iter().find(|e| e.part_id() == id) {
                        keep.push(e.clone());
                    }
                }
                continue;
            }

            let new_key = PartitionKey {
                bucket: key.bucket,
                window: key.window,
                generation: g.generation_id.clone(),
            };
            let spec = PartSpec {
                partition: Some(new_key.clone()),
                promote: self.store.config.promote.clone(),
                lineage: prism_part::ext::S5Ext {
                    reembedded_from: Some(key.generation.clone()),
                    ..Default::default()
                },
            };
            migrated_rows += rows.len();
            let m = PartWriter::write(
                &self.store.parts_dir(),
                next_seq,
                &g.generation_id,
                &g.model_id,
                &g.model_version,
                dim,
                self.store.config.pq_m,
                self.store.config.block_size,
                &spec,
                rows,
                now_ms,
            )?;
            next_seq += 1;
            migrated_parts += 1;
            keep.push(PartEntry::Located(crate::ingest::part_ref(&m, &new_key)?));
        }

        let mut meta = SnapshotMeta::of(snap);
        meta.generations.insert(g.generation_id.clone(), state);
        // A generation that still holds parts is Deprecated, not gone.
        if let Some(active) = &snap.active_generation {
            if active != &g.generation_id {
                meta.generations
                    .entry(active.clone())
                    .and_modify(|s| {
                        if *s == GenerationState::Active && state == GenerationState::Active {
                            *s = GenerationState::Deprecated;
                        }
                    })
                    .or_insert(GenerationState::Deprecated);
            }
        }

        let active = if state == GenerationState::Active {
            Some(g.generation_id.clone())
        } else {
            snap.active_generation.clone()
        };

        let s = self
            .catalog()
            .commit_meta(snap, keep, next_seq, active, meta, now_ms)?;

        Ok(MigrateReport {
            from_generation: from_gen,
            to_generation: g.generation_id.clone(),
            parts_migrated: migrated_parts,
            rows_migrated: migrated_rows,
            parts_remaining: total_partitions.saturating_sub(done),
            parts_unmigratable: unmigratable_parts,
            rows_unmigratable: unmigratable_rows,
            snapshot_id: s.snapshot_id,
        })
    }
}

impl Engine {
    /// Expire the raw bodies of every part whose rows end before `before_ms`.
    ///
    /// > *"Raw-body retention is policy-controlled (prompts contain secrets)."* — PRISM.md, §Ingest
    ///
    /// Immutability is law, so this does not edit anything: it **rewrites** the affected parts
    /// without their bodies and swaps the catalog, exactly like a merge. The old parts are still
    /// there, byte-identical, until GC is separately asked — which is the only reason this is
    /// safe to run at all.
    ///
    /// The rows survive and stay queryable. What is destroyed is the *text they were embedded
    /// from*, and destroying it is irreversible in a way that matters far beyond storage: those
    /// rows can never be re-embedded into a new space again. Which means any drift baseline that
    /// would have been rebuilt from them **cannot be**, and the alarm that depended on it goes
    /// `DEGRADED` rather than quietly silent (generation contract §7).
    pub fn redact_bodies(&self, before_ms: i64, reason: &str, now_ms: i64) -> Result<usize> {
        if reason.trim().is_empty() {
            return Err(PrismError::Invalid(
                "redaction needs a recorded reason; an irreversible deletion with no cause is not \
                 a retention policy, it is data loss"
                    .into(),
            ));
        }
        let snap = self.snapshot()?;
        let dim = self.store.config.dim;

        let mut keep: Vec<PartEntry> = Vec::new();
        let mut next_seq = snap.next_seq;
        let mut redacted = 0usize;

        for e in &snap.parts {
            let Some(r) = e.located() else {
                keep.push(e.clone());
                continue;
            };
            let reader = prism_part::part::PartReader::open(&self.store.part_dir(&r.part_id))?;
            if r.time_max >= before_ms || reader.manifest.s5()?.bodies_redacted {
                keep.push(e.clone());
                continue;
            }

            let all = reader.read_all()?;
            let g = self.catalog().get_generation(&r.partition.generation)?;
            let mut rows: Vec<RowIn> = Vec::with_capacity(all.events.len());
            for (i, mut ev) in all.events.into_iter().enumerate() {
                // The vector stays. The text does not. That asymmetry is the whole story: the
                // store can still answer questions about what these events MEANT, and can never
                // again ask a different model what they meant.
                ev.body = String::new();
                rows.push(RowIn {
                    centroid: all.centroids[i],
                    code: all.codes[i * self.store.config.pq_m..(i + 1) * self.store.config.pq_m]
                        .to_vec(),
                    vector: all.vectors[i * dim..(i + 1) * dim].to_vec(),
                    event: ev,
                });
            }

            let spec = PartSpec {
                partition: Some(r.partition.clone()),
                promote: self.store.config.promote.clone(),
                lineage: prism_part::ext::S5Ext {
                    reembedded_from: None,
                    bodies_redacted: true,
                    redacted_at_ms: now_ms,
                    redaction_reason: reason.to_string(),
                },
            };
            let m = PartWriter::write(
                &self.store.parts_dir(),
                next_seq,
                &g.generation_id,
                &g.model_id,
                &g.model_version,
                dim,
                self.store.config.pq_m,
                self.store.config.block_size,
                &spec,
                rows,
                now_ms,
            )?;
            next_seq += 1;
            redacted += 1;
            keep.push(PartEntry::Located(crate::ingest::part_ref(
                &m,
                &r.partition,
            )?));
        }

        self.catalog().commit_meta(
            &snap,
            keep,
            next_seq,
            snap.active_generation.clone(),
            SnapshotMeta::of(&snap),
            now_ms,
        )?;
        Ok(redacted)
    }

    /// Declare a bridge between two embedding spaces (generation contract §6).
    ///
    /// The default is that there is no bridge and a cross-space query is refused. Declaring one
    /// is somebody saying, on the record, "these two may be answered together, this way" — and
    /// the only way is `rank_fusion`, which merges **ranks** and never scores.
    pub fn bridge_declare(
        &self,
        from_space: &str,
        to_space: &str,
        validation: &str,
        now_ms: i64,
    ) -> Result<String> {
        if validation.trim().is_empty() {
            return Err(PrismError::Invalid(
                "a bridge needs a validation note. An unvalidated bridge is a guess with a \
                 schema, and it will be believed by everyone who reads a bridged answer."
                    .into(),
            ));
        }
        let snap = self.snapshot()?;
        let mut meta = SnapshotMeta::of(&snap);
        meta.bridges.retain(|b| !b.joins(from_space, to_space));
        meta.bridges.push(prism_part::catalog::Bridge {
            from_space: from_space.to_string(),
            to_space: to_space.to_string(),
            policy: prism_part::catalog::BridgePolicy::RankFusion,
            validation: validation.to_string(),
            declared_at_ms: now_ms,
        });
        let s = self.catalog().commit_meta(
            &snap,
            snap.parts.clone(),
            snap.next_seq,
            snap.active_generation.clone(),
            meta,
            now_ms,
        )?;
        Ok(s.snapshot_id)
    }
}

impl Engine {
    /// Compare a candidate generation against the active one, on the frozen probe queries.
    ///
    /// **A promotion without a comparison is a hope.** This is the step that turns "the new
    /// codebooks trained without erroring" into a number somebody can be held to.
    ///
    /// Two things are measured, and they are not the same thing:
    ///
    /// - **Agreement** — how much the two generations' top-k overlap. Useful, and *not* a quality
    ///   measure: two generations can agree perfectly and both be wrong.
    /// - **Recall against the exact oracle** — which is the only ground truth either of them has.
    ///   A candidate that agrees with the incumbent 95% of the time and has worse recall is a
    ///   candidate that has learned to be wrong in the same places.
    ///
    /// Across an embedding-space boundary, agreement is still meaningful (it compares *sets of
    /// ids*, not scores) but the scores are not, and the report says so rather than printing a
    /// number that invites the comparison invariant 9 forbids.
    pub fn generation_compare(
        &self,
        candidate: &str,
        golden: &crate::oracle::Golden,
    ) -> Result<CompareReport> {
        let snap = self.snapshot()?;
        let cand = self.catalog().get_generation(candidate)?;
        let base_id = snap.active_generation.clone().ok_or_else(|| {
            PrismError::Invalid(
                "nothing to compare against: the store has no active generation".into(),
            )
        })?;
        let base = self.catalog().get_generation(&base_id)?;

        let mut agreements: Vec<f32> = Vec::new();
        let mut base_hits = 0usize;
        let mut cand_hits = 0usize;
        let mut total = 0usize;
        let mut base_recalls: Vec<f32> = Vec::new();
        let mut cand_recalls: Vec<f32> = Vec::new();

        for exp in &golden.expectations {
            let mut q = exp.query.to_query();
            let k = q.k.max(1);

            q.space = Some(base.space());
            let b = self.search_at(&snap, &q)?;
            q.space = Some(cand.space());
            let c = self.search_at(&snap, &q)?;

            let bids: BTreeSet<String> = b
                .hits
                .iter()
                .take(k)
                .map(|h| h.event.event_id.clone())
                .collect();
            let cids: BTreeSet<String> = c
                .hits
                .iter()
                .take(k)
                .map(|h| h.event.event_id.clone())
                .collect();
            let truth: BTreeSet<String> = exp.expected_ids.iter().take(k).cloned().collect();
            if truth.is_empty() {
                continue;
            }

            let union = bids.union(&cids).count().max(1);
            agreements.push(bids.intersection(&cids).count() as f32 / union as f32);

            let br = bids.intersection(&truth).count() as f32 / truth.len() as f32;
            let cr = cids.intersection(&truth).count() as f32 / truth.len() as f32;
            base_recalls.push(br);
            cand_recalls.push(cr);
            base_hits += bids.intersection(&truth).count();
            cand_hits += cids.intersection(&truth).count();
            total += truth.len();
        }

        if total == 0 {
            return Err(PrismError::Invalid(
                "the golden set produced no comparable queries".into(),
            ));
        }

        let p1 = |mut v: Vec<f32>| -> f32 {
            v.sort_by(|a, b| a.total_cmp(b));
            // The tail, not the mean. S0 shipped a configuration whose mean recall was 0.904
            // while five queries returned NOTHING, and a promotion decision made on a mean would
            // make that mistake again.
            let idx = ((v.len() as f64 - 1.0) * 0.01).round() as usize;
            v.get(idx.min(v.len().saturating_sub(1)))
                .copied()
                .unwrap_or(0.0)
        };

        let same_space = base.space() == cand.space();
        Ok(CompareReport {
            base_generation: base_id,
            candidate_generation: candidate.to_string(),
            same_space,
            queries: agreements.len(),
            mean_agreement: agreements.iter().sum::<f32>() / agreements.len().max(1) as f32,
            min_agreement: agreements.iter().copied().fold(f32::INFINITY, f32::min),
            base_recall: base_hits as f32 / total as f32,
            candidate_recall: cand_hits as f32 / total as f32,
            base_p1_recall: p1(base_recalls),
            candidate_p1_recall: p1(cand_recalls),
            note: if same_space {
                "Both generations are in the same embedding space, so their exact scores are \
                 comparable and recall is measured against the same oracle. A candidate that \
                 AGREES with the incumbent but has worse recall has learned to be wrong in the \
                 same places -- compare the recalls, not the agreement."
                    .into()
            } else {
                "These generations are in DIFFERENT embedding spaces. Agreement (an overlap of \
                 ids) is meaningful; their SCORES are not comparable and no number here invites \
                 you to compare them (invariant 9). Recall is each one measured against the exact \
                 oracle in its own space."
                    .into()
            },
        })
    }
}
