//! Merge, re-embed, rollback (Part III §13).
//!
//! One mechanism does compaction, deduplication, and model migration, because
//! they are the same operation: read immutable parts, write a new immutable
//! part, swap the catalog. Nothing is edited. Nothing is deleted. The old parts
//! are still sitting there, byte-identical, until GC is *separately* asked to
//! reclaim them — which is why rollback is a catalog write and not a restore.
//!
//! S0 merges everything into one part per generation. Size-tiered selection
//! with I/O and write-amplification budgets is S10; what matters now is that
//! the *shape* is right, so S10 changes the policy and not the invariants.

use crate::engine::Engine;
use prism_part::catalog::PartEntry;
use prism_part::generation::Generation;
use prism_part::part::{PartSpec, PartWriter, RowIn};
use prism_part::partition::PartitionKey;
use prism_quantizer::{CoarseCodebook, PqCodebook};
use prism_types::error::{PrismError, Result};
use prism_types::event::Event;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeReport {
    pub parts_in: usize,
    pub parts_out: usize,
    pub rows_in: usize,
    pub rows_out: usize,
    /// Rows dropped because a newer row carried the same `event_id`.
    pub duplicates_reconciled: usize,
    /// Parts that were rewritten out of an older storage format. A merge is how
    /// a store is migrated forward; nothing is ever rewritten in place.
    pub parts_migrated: usize,
    pub bytes_read: usize,
    pub bytes_written: usize,
    /// bytes written / bytes read. The number that decides whether merge
    /// capacity can stay ahead of ingest, which is budgeted, never assumed.
    pub write_amplification: f64,
    pub snapshot_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReembedReport {
    pub old_generation: String,
    pub new_generation: String,
    pub old_model: String,
    pub new_model: String,
    pub rows: usize,
    pub parts_written: usize,
    pub snapshot_id: String,
    /// The previous snapshot. Rolling back to it is a catalog write; no data is
    /// rewritten, and no byte of the old parts was ever touched.
    pub rollback_to: String,
}

/// The duplicate policy, stated once so it is never re-decided by accident:
/// **last write wins by `event_time`; ties go to the later part.** Documented,
/// deterministic, and tested — the S2 gate demands duplicate behavior be
/// *documented*, not that duplicates be impossible.
fn supersedes(new: &Event, old: &Event) -> bool {
    new.event_time >= old.event_time
}

impl Engine {
    pub fn merge(&self, now_ms: i64) -> Result<MergeReport> {
        let snap = self.snapshot()?;
        let readers = self.open_parts(&snap)?;

        // A part written in an older format is always worth rewriting, even when
        // there is nothing to compact: the merge is how a store is *migrated*
        // forward, and a single-part v1 store must not be stranded on a format
        // that only the compatibility path can read. Everything the newer format
        // buys — block-local damage, an explicit feature bitset, a declared
        // rerank encoding — is only real once the bytes are actually rewritten.
        let has_legacy = readers.iter().any(|r| r.is_legacy());

        if snap.parts.len() < 2 && !has_legacy {
            return Ok(MergeReport {
                parts_in: snap.parts.len(),
                parts_out: snap.parts.len(),
                rows_in: 0,
                rows_out: 0,
                duplicates_reconciled: 0,
                parts_migrated: 0,
                bytes_read: 0,
                bytes_written: 0,
                write_amplification: 0.0,
                snapshot_id: snap.snapshot_id,
            });
        }

        let migrated = readers.iter().filter(|r| r.is_legacy()).count();
        let dim = self.store.config.dim;

        let mut bytes_read = 0usize;
        let mut rows_in = 0usize;

        // Group by **partition**, not merely by generation. A merge that combined two
        // partitions would undo the isolation the partitioning exists to create: a part
        // spanning two buckets is a part a query for one tenant has a reason to open on behalf
        // of another. The generation is part of the partition key, so this subsumes the old
        // grouping.
        let scheme = self.store.config.partitions.clone();
        let mut by_partition: BTreeMap<PartitionKey, BTreeMap<String, RowIn>> = BTreeMap::new();
        let mut duplicates = 0usize;

        for r in &readers {
            bytes_read += r
                .manifest
                .columns
                .iter()
                .map(|c| c.storage.logical_bytes() as usize)
                .sum::<usize>();
            let rows = r.read_all()?;
            rows_in += rows.events.len();
            let m = r.manifest.pq_m;
            for i in 0..rows.events.len() {
                let ev = rows.events[i].clone();
                let key = PartitionKey {
                    bucket: scheme.bucket_of(&ev.tenant_id),
                    window: scheme.window_of(ev.event_time),
                    generation: r.manifest.generation_id.clone(),
                };
                let bucket = by_partition.entry(key).or_default();
                let row = RowIn {
                    centroid: rows.centroids[i],
                    code: rows.codes[i * m..(i + 1) * m].to_vec(),
                    vector: rows.vectors[i * dim..(i + 1) * dim].to_vec(),
                    event: ev,
                };
                match bucket.get(&row.event.event_id) {
                    Some(existing) if !supersedes(&row.event, &existing.event) => {
                        duplicates += 1;
                    }
                    Some(_) => {
                        duplicates += 1;
                        bucket.insert(row.event.event_id.clone(), row);
                    }
                    None => {
                        bucket.insert(row.event.event_id.clone(), row);
                    }
                }
            }
        }

        let mut new_parts = Vec::new();
        let mut next_seq = snap.next_seq;
        let mut rows_out = 0usize;
        let mut bytes_written = 0usize;

        for (key, bucket) in by_partition {
            let g = self.catalog().get_generation(&key.generation)?;
            let rows: Vec<RowIn> = bucket.into_values().collect();
            rows_out += rows.len();

            // A merge is also how a part is migrated onto a NEW promotion scheme: it rewrites
            // rows into a fresh part under the store's current `promote` list. Existing parts
            // are never touched, so both representations coexist -- which is exactly what the
            // dual-door equivalence test exercises.
            let spec = PartSpec {
                partition: Some(key.clone()),
                promote: self.store.config.promote.clone(),
            };
            let manifest = PartWriter::write(
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
            bytes_written += manifest
                .columns
                .iter()
                .map(|c| c.storage.logical_bytes() as usize)
                .sum::<usize>();
            new_parts.push(PartEntry::Located(crate::ingest::part_ref(
                &manifest, &key,
            )?));
            next_seq += 1;
        }

        prism_part::faults::maybe_kill("merge.after_part_before_commit");

        let parts_in = snap.parts.len();
        let parts_out = new_parts.len();
        let new_snap = self.catalog().commit(
            &snap,
            new_parts,
            next_seq,
            snap.active_generation.clone(),
            now_ms,
        )?;

        Ok(MergeReport {
            parts_in,
            parts_out,
            rows_in,
            rows_out,
            duplicates_reconciled: duplicates,
            parts_migrated: migrated,
            bytes_read,
            bytes_written,
            write_amplification: if bytes_read == 0 {
                0.0
            } else {
                bytes_written as f64 / bytes_read as f64
            },
            snapshot_id: new_snap.snapshot_id,
        })
    }

    /// Re-embed every row into a new generation and swap the catalog.
    ///
    /// The old parts are untouched and still on disk. If the new generation is
    /// worse, `rollback` puts the old snapshot back by writing one file. That is
    /// the payoff for never mutating anything.
    pub fn reembed(&self, new_version: &str, now_ms: i64) -> Result<ReembedReport> {
        let snap = self.snapshot()?;
        let old_gen_id = snap
            .active_generation
            .clone()
            .ok_or_else(|| PrismError::Invalid("nothing to re-embed: store is empty".into()))?;
        let old_gen = self.catalog().get_generation(&old_gen_id)?;

        let dim = self.store.config.dim;
        let embedder = self.plane.embedder("hash-embedder", new_version, dim)?;

        // 1. Re-embed every row under the new model.
        let readers = self.open_parts(&snap)?;
        let mut events: Vec<Event> = Vec::new();
        for r in &readers {
            events.extend(r.read_all()?.events);
        }
        if events.is_empty() {
            return Err(PrismError::Invalid("nothing to re-embed: no rows".into()));
        }

        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(events.len());
        let mut kept: Vec<Event> = Vec::with_capacity(events.len());
        for e in events {
            match embedder.embed(&e.body) {
                Ok(v) => {
                    vectors.push(v);
                    kept.push(e);
                }
                Err(err) => {
                    // A row that was embeddable under the old model but not the
                    // new one must not vanish. Fail the migration loudly rather
                    // than complete it with a hole in it.
                    return Err(PrismError::Invariant(format!(
                        "re-embedding `{}` failed under model version {new_version}: {err}. \
                         Aborting the migration; the current generation is untouched.",
                        e.event_id
                    )));
                }
            }
        }

        // 2. Train new codebooks over a reservoir sample of the whole store —
        //    not the first batch, and not the old codebooks reused in a new
        //    space, which would be meaningless.
        let sample = crate::ingest::reservoir_sample(
            &vectors,
            crate::ingest::TRAIN_SAMPLE_MAX,
            self.store.config.seed,
        );
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
        let new_gen = Generation::new(
            embedder.model_id(),
            embedder.model_version(),
            dim,
            coarse,
            pq,
            &format!("re-embed from generation {old_gen_id}: reservoir sample of {n} vectors"),
        )?;

        if new_gen.generation_id == old_gen_id {
            return Err(PrismError::Invalid(
                "re-embedding produced an identical generation; nothing to migrate".into(),
            ));
        }
        self.catalog().put_generation(&new_gen)?;

        // 3. Write the new parts.
        let rows: Vec<RowIn> = kept
            .into_iter()
            .zip(vectors)
            .map(|(event, vector)| {
                let (centroid, _) = new_gen.coarse.assign(&vector);
                let code = new_gen.pq.encode(&vector)?;
                Ok(RowIn {
                    event,
                    centroid,
                    code,
                    vector,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let count = rows.len();

        // The generation is part of the partition key, so a re-embed necessarily writes into
        // NEW partitions -- which is exactly right: a part is pinned to one generation, and a
        // partition that spanned two would be a partition whose parts disagree about what their
        // bytes mean.
        let scheme = self.store.config.partitions.clone();
        let mut by_partition: BTreeMap<PartitionKey, Vec<RowIn>> = BTreeMap::new();
        for r in rows {
            let key = PartitionKey {
                bucket: scheme.bucket_of(&r.event.tenant_id),
                window: scheme.window_of(r.event.event_time),
                generation: new_gen.generation_id.clone(),
            };
            by_partition.entry(key).or_default().push(r);
        }

        let mut new_parts: Vec<PartEntry> = Vec::new();
        let mut seq = snap.next_seq;
        for (key, rows) in by_partition {
            let spec = PartSpec {
                partition: Some(key.clone()),
                promote: self.store.config.promote.clone(),
            };
            let manifest = PartWriter::write(
                &self.store.parts_dir(),
                seq,
                &new_gen.generation_id,
                &new_gen.model_id,
                &new_gen.model_version,
                dim,
                self.store.config.pq_m,
                self.store.config.block_size,
                &spec,
                rows,
                now_ms,
            )?;
            seq += 1;
            new_parts.push(PartEntry::Located(crate::ingest::part_ref(
                &manifest, &key,
            )?));
        }
        let parts_written = new_parts.len();

        // 4. One atomic swap. Before it, every query sees the old generation;
        //    after it, every query sees the new one. Never a mixture of the two
        //    in a single answer.
        let new_snap = self.catalog().commit(
            &snap,
            new_parts,
            seq,
            Some(new_gen.generation_id.clone()),
            now_ms,
        )?;

        Ok(ReembedReport {
            old_generation: old_gen_id,
            new_generation: new_gen.generation_id,
            old_model: format!("{}:{}", old_gen.model_id, old_gen.model_version),
            new_model: format!("{}:{}", embedder.model_id(), embedder.model_version()),
            rows: count,
            parts_written,
            snapshot_id: new_snap.snapshot_id,
            rollback_to: snap.snapshot_id,
        })
    }

    /// Roll back to an earlier snapshot. A catalog reference change; not a data
    /// rewrite, not a restore, not a copy. The parts it names must still exist —
    /// if GC has already reclaimed them, we say so instead of committing a
    /// snapshot that would not open (invariant 2).
    pub fn rollback(&self, to: &str, now_ms: i64) -> Result<String> {
        let target = self.catalog().load_snapshot(to)?;
        let current = self.snapshot()?;
        let next_seq = current.next_seq.max(target.next_seq);
        let snap = self.catalog().commit(
            &current,
            target.parts.clone(),
            next_seq,
            target.active_generation.clone(),
            now_ms,
        )?;
        Ok(snap.snapshot_id)
    }
}
