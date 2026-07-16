//! Drift baselines and novelty (S5) — and the state that says *"this alarm is not running"*.
//!
//! S9 owns novelty and drift as a *feature*. S5 owns the thing that will silently break it: a
//! re-embed changes the embedding space, and **a baseline is a statement about a distribution in
//! one space**. When the space changes underneath it, the baseline is not stale — it is
//! meaningless, and invariant 9 forbids comparing across it.
//!
//! So the rules here are small and absolute:
//!
//! - An event of generation `G` is scored **only** against a baseline of generation `G`.
//! - A migration is **not complete** until every baseline has been rebuilt in the new space.
//! - If a baseline **cannot** be rebuilt — the rows are still there, but their raw bodies have
//!   expired under retention, so they can never be re-embedded — the alarm goes **`DEGRADED`**
//!   and says so on every evaluation.
//!
//! That last one is the point of the whole module. An alarm that quietly stops firing is worse
//! than one that was never configured, because a configured alarm is *trusted*. `DEGRADED` does
//! not return zero, does not return the old numbers, and does not return nothing.

use crate::engine::Engine;
use prism_part::baseline::Baseline;
use prism_part::catalog::{BaselineRef, BaselineState, SnapshotMeta};
use prism_types::error::{PrismError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How many clusters describe "normal".
///
/// **Policy** (C-1): a baseline is a summary, and this is how coarse a summary. Too few and
/// every cluster is a blur that nothing looks novel against; too many and the baseline memorizes
/// the window and nothing is ever novel again. Sixteen is a deliberate middle, and S9 — which
/// owns drift as a feature — is where it earns a receipt.
pub const BASELINE_CLUSTERS: usize = 16;

/// The novelty percentile of the baseline's own window that becomes its alarm threshold.
///
/// **Policy** (C-1). A threshold picked by hand is a number somebody liked; a threshold
/// calibrated from the data is a statement about the data. At the 99th percentile, roughly one
/// event in a hundred of *normal* traffic is "novel" — which is what makes an alarm that fires
/// on 20% of a window mean something.
///
/// An integer, so it can live in the constant ledger like everything else. A constant that
/// cannot be registered because of its *type* would be a constant that escaped C-1 on a
/// technicality.
pub const BASELINE_QUANTILE_PCT: usize = 99;

/// How many times the calibrated novelty rate must be exceeded before an alarm fires.
///
/// **Policy** (C-1). The baseline is calibrated so that ~1% of *normal* traffic is novel. Five
/// times the expected rate is not noise; it is a different distribution. Firing at 2× would page
/// somebody every time a tenant deployed a new prompt.
pub const DRIFT_FIRE_MULTIPLE: usize = 5;

fn quantile() -> f64 {
    BASELINE_QUANTILE_PCT as f64 / 100.0
}

/// The fraction of *normal* traffic the baseline calibrates as novel (`1 − quantile`), i.e. the
/// expected false-positive rate. An alarm fires at [`DRIFT_FIRE_MULTIPLE`] times this. Exposed so
/// the S9 novelty primitive shares the exact firing rule, not a copy of it.
pub fn quantile_fraction() -> f64 {
    1.0 - quantile()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DriftAlarm {
    pub tenant: String,
    pub generation_id: String,
    pub baseline_id: String,
    pub events: usize,
    /// Events whose novelty exceeded the baseline's calibrated threshold.
    pub novel: usize,
    pub novel_fraction: f64,
    pub mean_novelty: f32,
    pub max_novelty: f32,
    pub threshold: f32,
    pub fired: bool,
}

/// The result of asking "is this tenant drifting?".
///
/// One variant per generation the tenant has live rows in — **because during a migration there
/// are two**, each with its own baseline, and each answering for itself. Never merged: a novelty
/// score is a score, and scores from different spaces are not comparable.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DriftReport {
    pub tenant: String,
    pub snapshot_id: String,
    pub alarms: Vec<DriftAlarm>,
    /// **The reason this report is not silent.**
    ///
    /// A generation whose baseline could not be built or rebuilt. The alarm is not running, and
    /// this says so — with the generation, the reason, and the row count that is going
    /// unwatched. An operator who reads `degraded: []` knows their alarms are live. An operator
    /// who reads nothing at all learns nothing at all, which is how a drift alarm silently stops
    /// mattering.
    pub degraded: Vec<DegradedAlarm>,
    pub fired: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DegradedAlarm {
    pub tenant: String,
    pub generation_id: String,
    pub rows_unwatched: usize,
    pub reason: String,
}

impl DriftReport {
    /// Degraded is not "fine". A caller that ignores this is a caller that believes an alarm is
    /// running when it is not.
    pub fn is_degraded(&self) -> bool {
        !self.degraded.is_empty()
    }
}

impl Engine {
    /// Build (or rebuild) a tenant's drift baseline **in the generation its rows are encoded
    /// under**.
    ///
    /// One baseline per (tenant, generation). During a migration a tenant legitimately has two,
    /// and each is used only for its own generation's events.
    pub fn baseline_build(
        &self,
        tenant: &str,
        generation_id: &str,
        now_ms: i64,
    ) -> Result<Baseline> {
        let snap = self.snapshot()?;
        let g = self.catalog().get_generation(generation_id)?;
        let dim = self.store.config.dim;

        // Only this generation's parts, and only this tenant's rows. Both halves matter: the
        // first because a vector from another space would poison the geometry, the second
        // because "normal" is a property of a tenant, not of a bucket they happen to share.
        let mut vectors: Vec<f32> = Vec::new();
        let mut rows = 0usize;
        for e in &snap.parts {
            let Some(r) = e.located() else { continue };
            if r.partition.generation != generation_id {
                continue;
            }
            if !r.tenants.iter().any(|t| t == tenant) {
                continue;
            }
            let reader = prism_part::part::PartReader::open(&self.store.part_dir(&r.part_id))?;
            let all = reader.read_all()?;
            for (i, ev) in all.events.iter().enumerate() {
                if ev.tenant_id != tenant {
                    continue;
                }
                vectors.extend_from_slice(&all.vectors[i * dim..(i + 1) * dim]);
                rows += 1;
            }
        }

        if rows == 0 {
            return Err(PrismError::Invalid(format!(
                "cannot build a baseline for tenant `{tenant}` in generation {generation_id}: it \
                 has no rows there"
            )));
        }

        let nlist = BASELINE_CLUSTERS.min(rows);
        let centroids =
            prism_quantizer::kmeans(&vectors, rows, dim, nlist, 25, self.store.config.seed)?;

        // Calibrate the threshold against the baseline window's own novelty distribution. A
        // number derived from the data, not a number somebody liked.
        let provisional = Baseline::new(
            tenant,
            generation_id,
            dim,
            centroids.clone(),
            nlist,
            f32::MAX,
            "uncalibrated",
            rows,
            &snap.snapshot_id,
            now_ms,
        )?;
        let mut novelties: Vec<f32> = Vec::with_capacity(rows);
        for i in 0..rows {
            novelties.push(provisional.novelty(&vectors[i * dim..(i + 1) * dim])?);
        }
        novelties.sort_by(|a, b| a.total_cmp(b));
        let idx = (((rows - 1) as f64) * quantile()).round() as usize;
        let threshold = novelties[idx.min(rows - 1)];

        let b = Baseline::new(
            tenant,
            generation_id,
            dim,
            centroids,
            nlist,
            threshold,
            &format!(
                "the p{BASELINE_QUANTILE_PCT} novelty of the {rows} rows this baseline was built \
                 from, in space {}",
                g.space()
            ),
            rows,
            &snap.snapshot_id,
            now_ms,
        )?;
        self.catalog().put_baseline(&b)?;

        let mut meta = SnapshotMeta::of(&snap);
        meta.baselines
            .retain(|r| !(r.tenant == tenant && r.generation_id == generation_id));
        meta.baselines.push(BaselineRef {
            baseline_id: b.baseline_id.clone(),
            tenant: tenant.to_string(),
            generation_id: generation_id.to_string(),
            state: BaselineState::Ready,
        });
        meta.baselines.sort_by(|a, b| {
            a.tenant
                .cmp(&b.tenant)
                .then(a.generation_id.cmp(&b.generation_id))
        });

        self.catalog().commit_meta(
            &snap,
            snap.parts.clone(),
            snap.next_seq,
            snap.active_generation.clone(),
            meta,
            now_ms,
        )?;
        Ok(b)
    }

    /// Rebuild every baseline that the current generations need, and mark **`DEGRADED`** the ones
    /// that cannot be rebuilt.
    ///
    /// This is the step a migration is not complete without (generation contract §7). It is not
    /// extra work — its input is exactly what the migration already produced.
    pub fn baselines_refresh(&self, now_ms: i64) -> Result<Vec<BaselineRef>> {
        let snap = self.snapshot()?;

        // Which (tenant, generation) pairs have live rows, and how many of them sit in parts
        // whose bodies are gone?
        let mut live: BTreeMap<(String, String), usize> = BTreeMap::new();
        let mut redacted: BTreeMap<(String, String), usize> = BTreeMap::new();
        for e in &snap.parts {
            let Some(r) = e.located() else { continue };
            let reader = prism_part::part::PartReader::open(&self.store.part_dir(&r.part_id))?;
            let gone = reader.manifest.s5()?.bodies_redacted;
            for t in &r.tenants {
                let key = (t.clone(), r.partition.generation.clone());
                *live.entry(key.clone()).or_default() += r.rows;
                if gone {
                    *redacted.entry(key).or_default() += r.rows;
                }
            }
        }

        // Which tenants are being watched at all? A tenant with no baseline has not asked for an
        // alarm, and we do not invent alarms for people.
        let watched: std::collections::BTreeSet<String> =
            snap.baselines.iter().map(|b| b.tenant.clone()).collect();

        let mut out: Vec<BaselineRef> = Vec::new();
        for ((tenant, gen), rows) in &live {
            if !watched.contains(tenant) {
                continue;
            }
            if snap.baselines.iter().any(|b| {
                &b.tenant == tenant && &b.generation_id == gen && b.state == BaselineState::Ready
            }) {
                continue; // already good in this space
            }

            // **The whole point of §7, and it took a failing test to state it correctly.**
            //
            // The naive rule -- "degraded if THIS generation's rows are redacted" -- is wrong, and
            // wrong in the direction that stays silent. Redaction does not invalidate an existing
            // baseline: the vectors are still there and the space has not moved, so the OLD
            // generation's baseline is still perfectly good for the old generation's rows.
            //
            // What is broken is the NEW one. A baseline is a description of a tenant's normal, and
            // "normal" is defined by their whole history. Rows whose raw bodies expired can never
            // be re-embedded into the new space -- not by this migration, not by any future one --
            // so a baseline built for the new generation would be calibrated on whatever *happened*
            // to survive. That is not a baseline. It is a biased subset wearing a threshold, and it
            // would go on producing plausible numbers forever.
            //
            // So: this generation's baseline is DEGRADED if any of the tenant's history is stranded
            // in a generation it can never leave.
            let gone: usize = redacted
                .iter()
                .filter(|((t, g), _)| t == tenant && g != gen)
                .map(|(_, n)| *n)
                .sum();
            if gone > 0 {
                out.push(BaselineRef {
                    baseline_id: String::new(),
                    tenant: tenant.clone(),
                    generation_id: gen.clone(),
                    state: BaselineState::Degraded {
                        reason: format!(
                            "{gone} rows of tenant `{tenant}`'s history are stranded in parts \
                             whose raw bodies expired under retention. They can never be \
                             re-embedded into generation {gen} — not by this migration, not by \
                             any future one — so a baseline for this space could only be \
                             calibrated on the {rows} rows that happened to survive. That is not \
                             a baseline; it is a biased subset wearing a threshold, and it would \
                             go on producing plausible numbers forever. **This alarm is NOT \
                             RUNNING.** It will not fire. Rebuild it from a fully retained window, \
                             or accept that this generation is unwatched — but do not mistake \
                             this for quiet."
                        ),
                    },
                });
                continue;
            }

            match self.baseline_build(tenant, gen, now_ms) {
                Ok(b) => out.push(BaselineRef {
                    baseline_id: b.baseline_id,
                    tenant: tenant.clone(),
                    generation_id: gen.clone(),
                    state: BaselineState::Ready,
                }),
                Err(e) => out.push(BaselineRef {
                    baseline_id: String::new(),
                    tenant: tenant.clone(),
                    generation_id: gen.clone(),
                    state: BaselineState::Degraded {
                        reason: format!("baseline could not be built: {e}"),
                    },
                }),
            }
        }

        // Rebuilding commits per baseline; re-read and write the final set in one commit, so the
        // snapshot never claims a baseline is ready before it is.
        let snap = self.snapshot()?;
        let mut meta = SnapshotMeta::of(&snap);
        for r in &out {
            meta.baselines
                .retain(|b| !(b.tenant == r.tenant && b.generation_id == r.generation_id));
            meta.baselines.push(r.clone());
        }
        // A baseline pinned to a generation that no longer holds any rows is not degraded — it
        // is finished. Drop it, or `migration_status` would call the migration incomplete
        // forever on the strength of a baseline nothing needs.
        meta.baselines
            .retain(|b| live.contains_key(&(b.tenant.clone(), b.generation_id.clone())));
        meta.baselines.sort_by(|a, b| {
            a.tenant
                .cmp(&b.tenant)
                .then(a.generation_id.cmp(&b.generation_id))
        });

        let final_set = meta.baselines.clone();
        self.catalog().commit_meta(
            &snap,
            snap.parts.clone(),
            snap.next_seq,
            snap.active_generation.clone(),
            meta,
            now_ms,
        )?;
        Ok(final_set)
    }

    /// Evaluate drift for a tenant over a window — **once per generation its rows live in**.
    ///
    /// During a migration that is two evaluations against two baselines, and they are never
    /// merged. A novelty score is a score, and scores from different embedding spaces are not
    /// comparable (invariant 9). Reporting "the drift" as one number across a migration would be
    /// averaging two different units and printing the result.
    pub fn drift_check(
        &self,
        tenant: &str,
        from: Option<i64>,
        to: Option<i64>,
    ) -> Result<DriftReport> {
        let snap = self.snapshot()?;
        let dim = self.store.config.dim;

        let mut by_gen: BTreeMap<String, Vec<f32>> = BTreeMap::new();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();

        for e in &snap.parts {
            let Some(r) = e.located() else { continue };
            if !r.may_match(tenant, from, to) {
                continue;
            }
            let reader = prism_part::part::PartReader::open(&self.store.part_dir(&r.part_id))?;
            let all = reader.read_all()?;
            for (i, ev) in all.events.iter().enumerate() {
                if ev.tenant_id != tenant {
                    continue;
                }
                if from.is_some_and(|f| ev.event_time < f) || to.is_some_and(|t| ev.event_time > t)
                {
                    continue;
                }
                let g = r.partition.generation.clone();
                by_gen
                    .entry(g.clone())
                    .or_default()
                    .extend_from_slice(&all.vectors[i * dim..(i + 1) * dim]);
                *counts.entry(g).or_default() += 1;
            }
        }

        let mut alarms = Vec::new();
        let mut degraded = Vec::new();

        for (gen, vectors) in &by_gen {
            let n = counts[gen];
            let Some(bref) = snap.baseline_for(tenant, gen) else {
                degraded.push(DegradedAlarm {
                    tenant: tenant.to_string(),
                    generation_id: gen.clone(),
                    rows_unwatched: n,
                    reason: format!(
                        "no baseline exists for tenant `{tenant}` in generation {gen}. These \
                         {n} events are NOT being watched. A baseline from another generation \
                         cannot be used: it describes a different embedding space, and comparing \
                         across it is forbidden (invariant 9)."
                    ),
                });
                continue;
            };

            if let BaselineState::Degraded { reason } = &bref.state {
                degraded.push(DegradedAlarm {
                    tenant: tenant.to_string(),
                    generation_id: gen.clone(),
                    rows_unwatched: n,
                    reason: reason.clone(),
                });
                continue;
            }

            let b = self.catalog().get_baseline(&bref.baseline_id)?;
            // Belt and braces. The catalog says this baseline is for this generation; the
            // baseline itself says so too; if they ever disagreed, we would be one line away
            // from scoring a vector against a yardstick from another universe.
            if b.generation_id != *gen {
                return Err(PrismError::Invariant(format!(
                    "baseline {} claims generation {} but the catalog filed it under {gen}",
                    b.baseline_id, b.generation_id
                )));
            }

            let mut novel = 0usize;
            let mut sum = 0.0f32;
            let mut max = f32::MIN;
            for i in 0..n {
                let v = &vectors[i * dim..(i + 1) * dim];
                let nov = b.novelty(v)?;
                sum += nov;
                if nov > max {
                    max = nov;
                }
                if nov > b.threshold {
                    novel += 1;
                }
            }

            let frac = novel as f64 / n as f64;
            alarms.push(DriftAlarm {
                tenant: tenant.to_string(),
                generation_id: gen.clone(),
                baseline_id: b.baseline_id.clone(),
                events: n,
                novel,
                novel_fraction: frac,
                mean_novelty: sum / n as f32,
                max_novelty: max,
                threshold: b.threshold,
                // The baseline is calibrated so ~1% of *normal* traffic is novel. Several times
                // that is not noise; it is a different distribution.
                fired: frac > DRIFT_FIRE_MULTIPLE as f64 * (1.0 - quantile()),
            });
        }

        alarms.sort_by(|a, b| a.generation_id.cmp(&b.generation_id));
        degraded.sort_by(|a, b| a.generation_id.cmp(&b.generation_id));

        Ok(DriftReport {
            tenant: tenant.to_string(),
            snapshot_id: snap.snapshot_id.clone(),
            fired: alarms.iter().any(|a| a.fired),
            alarms,
            degraded,
        })
    }
}
