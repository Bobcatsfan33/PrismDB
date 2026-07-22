//! The query path (Part III §11).
//!
//! ```text
//!   prune parts on metadata          (no column file is opened)
//!     → score the resident centroids  (tiny; this is the whole index)
//!     → read the selected centroid ranges as contiguous byte ranges
//!     → ADC-scan compressed codes, scalar mask fused into the loop
//!     → bounded candidate heap        (candidate width)
//!     → fetch exact vectors for survivors only, within the declared budget
//!     → exact rerank                  (rerank width)
//!     → optional semantic grouping with real exemplar events
//! ```
//!
//! Four controls, deliberately separate — `k`, `nprobe`, candidate width,
//! rerank width — because they trade different things, and collapsing them into
//! one "quality" knob is how systems end up unable to explain their own recall.
//!
//! Approximate distances decide *who gets reranked*. They never decide the
//! answer, and they never reach the surface: every score returned is an exact
//! cosine against a stored float32 vector.

use crate::engine::Engine;
use prism_part::part::PartReader;
use prism_quantizer::AdcTable;
use prism_types::error::{PrismError, Result};
use prism_types::event::Event;
use prism_types::vector::{cosine_from_l2_sq, dot, l2_sq};
use prism_types::{ClusterSummary, Counters, Hit, Query, SearchResult};
use std::collections::{BTreeMap, BTreeSet};

/// A candidate that survived into the rerank set and has an exact score.
pub(crate) struct Scored {
    pub(crate) score: f32,
    pub(crate) part: usize,
    pub(crate) row: usize,
    pub(crate) vector: Vec<f32>,
    pub(crate) event_id: String,
}

/// A round-1 candidate a shard hands the coordinator: its PQ distance, its `event_id` (the tie the
/// global merge breaks on, C-4), and the `(part_id, row)` handle the coordinator returns in round 2.
pub(crate) struct ShardCandidate {
    pub(crate) dist: f32,
    pub(crate) event_id: String,
    pub(crate) part_id: String,
    pub(crate) row: usize,
}

/// A round-2 exact score from a shard: the exact rerank score, the `event_id`, the `(part_id, row)`
/// handle, and the exact vector (which the coordinator needs only for a semantic `GROUP BY`).
pub(crate) struct ShardScored {
    pub(crate) score: f32,
    pub(crate) event_id: String,
    pub(crate) part_id: String,
    pub(crate) row: usize,
    pub(crate) vector: Vec<f32>,
}

/// Everything a query needs from one generation, computed once.
struct SpaceContext {
    query_vector: Vec<f32>,
    adc: AdcTable,
    /// Centroid ids ranked nearest-first.
    ranked: Vec<u32>,
    /// Their distances, in the same order — kept so adaptive probing can read the margin between
    /// the base boundary and the next centroid (S6).
    ranked_dists: Vec<f32>,
}

/// The outcome of the candidate phase: either the query is **done here** (an empty set, a bridge
/// fusion, or a refusal — the unhappy paths) or it produced a candidate set to **rerank**.
enum CandidatePhase {
    Done(SearchResult),
    Rerank(CandidateSet),
}

/// The candidate set plus the context the rerank phase needs. Owns its readers so it can cross the
/// phase boundary; `eligible` indexes into `readers` (in scan order), and each candidate's `part`
/// indexes into `eligible` — the same indirection the scan used, preserved so the rerank is identical.
struct CandidateSet {
    readers: Vec<PartReader>,
    eligible: Vec<usize>,
    ctxs: BTreeMap<String, SpaceContext>,
    candidates: Vec<crate::topk::Candidate>,
    gen_ids: BTreeSet<String>,
    plan_choice: crate::plan::PlanChoice,
    counters: Counters,
}

impl Engine {
    /// Search the live snapshot.
    pub fn search(&self, q: &Query) -> Result<SearchResult> {
        let snap = self.snapshot()?;
        self.search_at(&snap, q)
    }

    /// Choose the physical execution strategy for a query (S8), or honour a forced one.
    ///
    /// The choice is a pure cost decision, and it turns on one estimate: **selectivity** — the
    /// fraction of probed rows the predicate admits. A selective predicate favours *scalar-first*
    /// (distance only the survivors); a permissive one favours *semantic-first* (the distance does
    /// the narrowing). The selectivity estimate is deliberately **crude** (directive 7: choose
    /// among three strategies well, do not research cardinality estimation) and carries a receipt
    /// saying so; S16's benchmarks will show where estimation actually hurts.
    pub(crate) fn choose_plan(
        &self,
        q: &Query,
        eligible: &[&prism_part::part::PartReader],
        forced: Option<crate::plan::Strategy>,
    ) -> crate::plan::PlanChoice {
        use crate::plan::{estimate_cost, PlanChoice, Strategy};

        if let Some(s) = forced {
            return PlanChoice {
                strategy: s,
                reason: "forced by the caller (test / advanced)".into(),
                estimated_selectivity: f64::NAN,
            };
        }

        // No predicate: there is nothing to filter, so filter-order is moot and interleaved (the
        // fused default) is the honest choice.
        if q.predicate.is_none()
            && q.tenant.is_none()
            && q.time_from.is_none()
            && q.time_to.is_none()
        {
            return PlanChoice {
                strategy: Strategy::Interleaved,
                reason: "no predicate to filter; interleaved".into(),
                estimated_selectivity: 1.0,
            };
        }

        let sel = self.estimate_selectivity(q, eligible);
        // Estimate the probed row count: nprobe centroids' worth of rows across eligible parts. A
        // rough upper bound is fine -- the DECISION turns on selectivity, and the row count scales
        // all three costs together.
        let probed_rows: usize = eligible
            .iter()
            .flat_map(|r| r.manifest.centroid_ranges.iter())
            .map(|cr| cr.row_count)
            .take(q.nprobe.max(1) * eligible.len().max(1))
            .sum::<usize>()
            .max(1);

        let (best, _) = Strategy::ALL
            .iter()
            .map(|&s| (s, estimate_cost(s, probed_rows, sel, q.candidates)))
            .min_by_key(|&(_, cost)| cost)
            .unwrap();

        PlanChoice {
            strategy: best,
            reason: format!("{}: est. selectivity {sel:.3}", best.name()),
            estimated_selectivity: sel,
        }
    }

    /// Estimate the fraction of probed rows the predicate admits. **Crude on purpose** (directive
    /// 7), and honest about it: a tenant-scoped query in a shared bucket is estimated from the
    /// tenant's row share; a general predicate gets a fixed prior. `testing/evidence/cost-model.json`
    /// records that this is a placeholder, and the calibration harness tracks its error so S16
    /// knows whether it is worth improving.
    fn estimate_selectivity(&self, q: &Query, eligible: &[&prism_part::part::PartReader]) -> f64 {
        let mut sel = 1.0f64;

        // A tenant predicate in a shared bucket: the tenant's share of the bucket's rows. This one
        // we can actually estimate from the per-tenant stats already in the manifest (S4).
        if let Some(tenant) = &q.tenant {
            let mut tenant_rows = 0usize;
            let mut total_rows = 0usize;
            for r in eligible {
                total_rows += r.manifest.row_count;
                if let Ok(s4) = r.manifest.s4() {
                    if let Some(st) = s4.stats_for(tenant) {
                        tenant_rows += st.rows;
                    }
                }
            }
            if total_rows > 0 {
                sel *= (tenant_rows as f64 / total_rows as f64).clamp(0.0, 1.0);
            }
        }

        // A general predicate: estimate the pass rate from a BOUNDED SAMPLE. Not a histogram (that
        // is cardinality-estimation research, out of scope -- directive 7), just a cheap sample of
        // real rows: read at most `PLAN_SAMPLE_ROWS` evenly spaced rows from the first eligible
        // part, evaluate the predicate, and use the pass rate. Crude, but grounded in the data
        // rather than a fixed prior -- which is what the regret gate needs. `cost-model.json`
        // records the sample size and that this is a placeholder for real statistics.
        if let Some(p) = &q.predicate {
            const PLAN_SAMPLE_ROWS: usize = 512;
            if let Some(r) = eligible.first() {
                if let Ok(view) = crate::rowsource::PartRows::new(r, Some(p)) {
                    let n = r.manifest.row_count;
                    if n > 0 {
                        let sample = PLAN_SAMPLE_ROWS.min(n);
                        let step = (n / sample).max(1);
                        let mut seen = 0usize;
                        let mut passed = 0usize;
                        let mut row = 0usize;
                        while row < n && seen < sample {
                            if prism_types::predicate::eval(p, &view, row).unwrap_or(false) {
                                passed += 1;
                            }
                            seen += 1;
                            row += step;
                        }
                        if seen > 0 {
                            sel *= (passed as f64 / seen as f64).clamp(0.0, 1.0);
                        }
                    }
                }
            }
        }

        sel.clamp(0.0001, 1.0)
    }

    /// Build one query embedding + ADC table per generation present in `eligible` — the per-space
    /// context the scan and the rerank share ("different codebook generations within one embedding
    /// space are fine: each gets its own table, and they merge at exact-score time"). Extracted so a
    /// cluster's round two can rebuild it for the parts it re-opens, without a second copy.
    fn build_space_contexts(
        &self,
        eligible: &[&prism_part::part::PartReader],
        q: &Query,
        c: &mut Counters,
    ) -> Result<(BTreeMap<String, SpaceContext>, BTreeSet<String>)> {
        let dim = self.store.config.dim;
        let gen_ids: BTreeSet<String> = eligible
            .iter()
            .map(|r| r.manifest.generation_id.clone())
            .collect();
        let mut ctxs: BTreeMap<String, SpaceContext> = BTreeMap::new();
        for gid in &gen_ids {
            let g = self.catalog().get_generation(gid)?;
            let embedder = self.plane.embedder(&g.model_id, &g.model_version, dim)?;
            let qv = embedder.embed(&q.text)?;
            let adc = g.pq.adc_table(&qv)?;
            let scored = g.coarse.rank(&qv);
            let ranked: Vec<u32> = scored.iter().map(|(id, _)| *id).collect();
            let ranked_dists: Vec<f32> = scored.iter().map(|(_, d)| *d).collect();
            c.centroids_scored += ranked.len();
            ctxs.insert(
                gid.clone(),
                SpaceContext {
                    query_vector: qv,
                    adc,
                    ranked,
                    ranked_dists,
                },
            );
        }
        Ok((ctxs, gen_ids))
    }

    /// Search a **specific** snapshot. Single-store search is the **degenerate one-shard case** of
    /// the phased path ([query §20](../../../docs/QUERY-CONTRACT.md)): the candidate phase, then the
    /// rerank phase — the same two units a distributed query fans out and merges over. One
    /// implementation, so a single node and a cluster cannot diverge.
    ///
    /// This is what makes pagination correct. A paginated query is a query whose lifetime
    /// spans several requests, and invariant 4 says a reader pins a snapshot for the
    /// lifetime of a query. Parts are immutable and a snapshot is a fixed set of them, so
    /// the answer to a query against a given snapshot is fixed forever — ingest publishes a
    /// new snapshot without touching the old one, and merge writes new parts without
    /// touching the old ones. Pagination needed no new invariant; it needed the ones we
    /// already had to be true.
    pub fn search_at(
        &self,
        snap: &prism_part::catalog::Snapshot,
        q: &Query,
    ) -> Result<SearchResult> {
        match self.candidate_phase(snap, q)? {
            CandidatePhase::Done(result) => Ok(result),
            CandidatePhase::Rerank(cs) => {
                let eligible: Vec<&PartReader> =
                    cs.eligible.iter().map(|i| &cs.readers[*i]).collect();
                self.rerank_phase(
                    snap,
                    q,
                    &eligible,
                    &cs.ctxs,
                    &cs.gen_ids,
                    &cs.plan_choice,
                    cs.candidates,
                    cs.counters,
                )
            }
        }
    }

    /// The **candidate phase**: validate, prune partitions, open eligible parts, embed the query,
    /// scan the compressed codes under the chosen strategy, and bound a candidate set by PQ distance.
    /// This is round 1 of a distributed query (each shard runs it) and the front half of single-store
    /// search. It returns the candidates plus the context the rerank phase needs
    /// ([`CandidatePhase::Rerank`]), or a **finished result** for a path that terminates here — an
    /// empty eligible set, a bridge fusion, or a refusal ([`CandidatePhase::Done`]) — so the unhappy
    /// paths fork in exactly one place, provable against the monolith oracle.
    fn candidate_phase(
        &self,
        snap: &prism_part::catalog::Snapshot,
        q: &Query,
    ) -> Result<CandidatePhase> {
        if q.k == 0 {
            return Err(PrismError::Invalid("k must be positive".into()));
        }
        if q.nprobe == 0 {
            return Err(PrismError::Invalid("nprobe must be positive".into()));
        }
        if q.text.trim().is_empty() {
            return Err(PrismError::Invalid("query text is empty".into()));
        }

        // Time bounds from the query AND, conservatively, from the predicate's top-level
        // conjunction. `time_bounds` never narrows across an OR: pruning that can lose a row is
        // not pruning, it is sampling.
        let (pred_from, pred_to) = match &q.predicate {
            Some(p) => prism_types::predicate::time_bounds(p),
            None => (None, None),
        };
        let from = match (q.time_from, pred_from) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let to = match (q.time_to, pred_to) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };

        // --- 0. PARTITION PRUNING, IN THE CATALOG, BEFORE ANY PART IS OPENED (S4) ---
        //
        // This is the line that makes "cross-tenant reads are physically impossible" an I/O
        // property rather than a slogan. A part outside this query's partitions is never
        // opened, never checksummed, never read -- so another tenant's partitions could be
        // filled with unreadable garbage and this query would still answer correctly, because
        // it never looked.
        let (readers, catalog_pruned) =
            self.open_candidates(snap, q.tenant.as_deref(), from, to)?;

        let mut c = Counters {
            parts_total: snap.parts.len(),
            parts_pruned: catalog_pruned,
            ..Default::default()
        };

        // --- 1. per-tenant zone maps, inside the parts we did open ---
        //
        // A shared bucket's part-level zone map describes the BUCKET, not the tenant. Using it
        // would both leak (tenant A learns tenant B's time range) and under-prune (A cannot skip
        // a part whose range is wide only because of B). So a query consults its OWN tenant's
        // section (directive 3).
        let mut eligible: Vec<&PartReader> = Vec::new();
        // `eligible_idx[i]` is the index into `readers` of `eligible[i]` — carried so the candidate
        // set can own `readers` and cross the phase boundary without a self-referential borrow.
        let mut eligible_idx: Vec<usize> = Vec::new();
        for (ridx, r) in readers.iter().enumerate() {
            // **No part is ever decoded with the wrong codebook.** The catalog says which
            // generation this part is in; the part itself says so too. If they ever disagree,
            // one of them is lying, and the cost of guessing which is not a crash -- it is a
            // PLAUSIBLE WRONG ANSWER, because a PQ code read against the wrong codebook still
            // produces a number, and the number looks fine. So: refuse.
            if let Some(pr) = snap
                .parts
                .iter()
                .find_map(|e| e.located().filter(|pr| pr.part_id == r.manifest.part_id))
            {
                if pr.partition.generation != r.manifest.generation_id {
                    return Err(PrismError::Corrupt(format!(
                        "part {} is filed in the catalog under generation {} but its manifest \
                         says it was encoded under {}. Decoding it with either codebook would \
                         produce a number, and the number would look fine. Refusing.",
                        r.manifest.part_id, pr.partition.generation, r.manifest.generation_id
                    )));
                }
            }
            let keep = match (
                q.tenant.as_deref(),
                r.manifest.s4()?.stats_for_owned(q.tenant.as_deref()),
            ) {
                (Some(_), Some(st)) => st.may_match(from, to),
                _ => r.manifest.may_match(q.tenant.as_deref(), from, to),
            };
            if keep {
                eligible.push(r);
                eligible_idx.push(ridx);
            } else {
                c.parts_pruned += 1;
            }
        }

        // --- 2. embedding-space check (invariant 9) ---
        let mut spaces: BTreeSet<String> = BTreeSet::new();
        for r in &eligible {
            spaces.insert(format!(
                "{}:{}",
                r.manifest.model_id, r.manifest.model_version
            ));
        }
        if spaces.len() > 1 {
            match &q.space {
                Some(want) => {
                    if !spaces.contains(want) {
                        return Err(PrismError::Invalid(format!(
                            "no eligible parts are in embedding space `{want}`; \
                             present spaces: {:?}",
                            spaces
                        )));
                    }
                    let before = eligible.len();
                    // Filter `eligible` and `eligible_idx` in lockstep so the index mapping the
                    // candidate set carries stays valid after a space narrows the set.
                    let mut kept_eligible = Vec::new();
                    let mut kept_idx = Vec::new();
                    for (r, idx) in eligible.iter().zip(eligible_idx.iter()) {
                        if format!("{}:{}", r.manifest.model_id, r.manifest.model_version) == *want
                        {
                            kept_eligible.push(*r);
                            kept_idx.push(*idx);
                        }
                    }
                    eligible = kept_eligible;
                    eligible_idx = kept_idx;
                    // Parts dropped by the space filter are PRUNED, and must be counted as such.
                    // Naming a space narrows the query, and a narrowing a caller cannot see in
                    // the counters is a narrowing they cannot audit.
                    c.parts_pruned += before - eligible.len();
                }
                None => {
                    // A bridge, if somebody declared one, is the ONLY way across (generation
                    // contract §6). It does not merge scores -- it fuses ranks, which are
                    // unitless. Obeying invariant 9 rather than working around it.
                    let list: Vec<&String> = spaces.iter().collect();
                    if list.len() == 2 {
                        if let Some(b) = snap.bridge(list[0], list[1]) {
                            return Ok(CandidatePhase::Done(
                                self.search_bridged(snap, q, &spaces, b)?,
                            ));
                        }
                    }
                    // The error is written to TEACH (docs/QUERY-CONTRACT.md §13): this is invariant
                    // 9 surfacing where a SQL user first meets it, and a terse "spaces differ" would
                    // leave them guessing why their perfectly reasonable query was refused.
                    let mut names: Vec<&str> = spaces.iter().map(|s| s.as_str()).collect();
                    names.sort();
                    let (a, b) = (names[0], names.get(1).copied().unwrap_or(""));
                    return Err(PrismError::Invariant(format!(
                        "this query spans two embedding spaces — {a} and {b} — whose scores are \
                         not comparable (a cosine of 0.8 in one is not a cosine of 0.8 in the \
                         other). PrismDB will not merge them into one ranking. Either name one \
                         space with `USING SPACE '{b}'`, declare a bridge to fuse their ranks \
                         (`prism bridge declare`), or finish the re-embed migration so a single \
                         space remains."
                    )));
                }
            }
        }

        c.rows_eligible = eligible.iter().map(|r| r.manifest.row_count).sum();

        if eligible.is_empty() {
            return Ok(CandidatePhase::Done(SearchResult {
                hits: Vec::new(),
                clusters: None,
                counters: c,
                generations: Vec::new(),
                bridge: None,
                explain: None,
                snapshot_id: snap.snapshot_id.clone(),
            }));
        }

        // --- 3. one query embedding + one ADC table per generation ---
        let (ctxs, gen_ids) = self.build_space_contexts(&eligible, q, &mut c)?;

        // --- 4. scan the selected centroid ranges ---

        // Pick the kernel once, for the whole query, and record it. Every kernel returns
        // bit-identical distances (docs/DETERMINISM-CONTRACT.md §1), so this changes the query's
        // speed and never its answer.
        let isa = prism_quantizer::kernel::best();
        c.scan_isa = isa.name().to_string();

        // Pre-load every eligible part's scalar columns, once, and keep them alive for the whole
        // scan. The top-k's tie-break borrows event ids out of these instead of owning copies, so
        // a row entering the top-k allocates nothing (§4). These columns are read anyway -- the
        // fused mask needs tenant and time -- so this only moves the read earlier, it does not add
        // one.
        let part_scalars: Vec<prism_part::part::Scalars> = eligible
            .iter()
            .map(|r| r.read_scalars())
            .collect::<Result<Vec<_>>>()?;
        let id_of =
            |part: u32, row: u32| -> &str { part_scalars[part as usize].event_id_at(row as usize) };
        let mut topk = crate::topk::TopK::new(q.candidates, &id_of);

        // One distance buffer, sized once to the largest range any eligible part holds, and
        // reused for every range of every part. The centroid ranges are in the manifest, so this
        // needs no I/O -- and it is what makes the hot loop allocate nothing (§4).
        let max_range_rows = eligible
            .iter()
            .flat_map(|r| r.manifest.centroid_ranges.iter())
            .map(|cr| cr.row_count)
            .max()
            .unwrap_or(0);
        let mut dists = vec![0.0f32; max_range_rows];

        // --- choose the physical strategy (S8) ---
        //
        // The strategy is invisible to the answer (plan-invariance, docs/QUERY-CONTRACT.md §9), so
        // the optimizer chooses on cost alone. A forced plan (test override, or a per-query hint)
        // wins, so the plan-invariance gate can prove every strategy answers identically.
        let plan_choice = {
            let forced = crate::plan::forced_plan_override()
                .or_else(|| q.plan.as_deref().and_then(crate::plan::Strategy::parse));
            self.choose_plan(q, &eligible, forced)
        };
        let strategy = plan_choice.strategy;
        c.plan = strategy.name().to_string();

        for (pi, r) in eligible.iter().enumerate() {
            c.parts_opened += 1;
            let ctx = ctxs.get(&r.manifest.generation_id).ok_or_else(|| {
                PrismError::Corrupt("part references an absent generation".into())
            })?;

            let scalars = &part_scalars[pi];
            let times = &scalars.times;

            // The row predicate, if any. Columns load lazily and only if the predicate
            // actually names them — a filter that never mentions `body` must not cost a
            // `body` decode.
            let rows_view = match &q.predicate {
                Some(p) => Some(crate::rowsource::PartRows::new(r, Some(p))?),
                None => None,
            };

            let by_centroid: BTreeMap<u32, &prism_part::part::CentroidRange> = r
                .manifest
                .centroid_ranges
                .iter()
                .map(|cr| (cr.centroid, cr))
                .collect();

            // Adaptive probing (S6): a boundary query may probe ABOVE the base nprobe, never
            // below it, so recall can only improve and the receipts stay valid as floors. On a
            // query deep inside a cluster the margin is not met and this is exactly q.nprobe.
            let eff_nprobe = if q.adaptive {
                prism_types::query::adaptive_nprobe(
                    &ctx.ranked_dists,
                    q.nprobe,
                    q.adaptive_margin
                        .unwrap_or(prism_types::query::ADAPTIVE_MARGIN),
                    prism_types::query::ADAPTIVE_MAX_NPROBE,
                )
            } else {
                q.nprobe.min(ctx.ranked.len())
            };
            c.probes_taken += eff_nprobe;
            c.probes_widened += eff_nprobe.saturating_sub(q.nprobe.min(ctx.ranked.len()));

            let probes = ctx.ranked.iter().take(eff_nprobe);
            for &cid in probes {
                let Some(range) = by_centroid.get(&cid) else {
                    // This part has no rows in that centroid. The probe costs
                    // nothing: no range, no read.
                    continue;
                };

                // Range-level zone map: a probe whose whole time span is outside
                // the predicate is skipped without a read.
                if let Some(f) = q.time_from {
                    if range.time_max < f {
                        continue;
                    }
                }
                if let Some(t) = q.time_to {
                    if range.time_min > t {
                        continue;
                    }
                }

                let codes = r.read_pq_range(range)?;
                c.ranges_scanned += 1;
                c.pq_bytes_scanned += codes.len();
                c.rows_scanned_pq += range.row_count;

                let m = r.manifest.pq_m;

                // The scalar mask, one closure so all three strategies apply the *same* predicate
                // to the *same* rows. `pe` counts general-predicate evaluations (the expensive
                // part; tenant and time are cheap columnar checks).
                let mask = |row: usize, pe: &mut usize| -> Result<bool> {
                    if let Some(t) = &q.tenant {
                        if !scalars.tenant_is(row, t) {
                            return Ok(false);
                        }
                    }
                    if let Some(f) = q.time_from {
                        if times[row] < f {
                            return Ok(false);
                        }
                    }
                    if let Some(t) = q.time_to {
                        if times[row] > t {
                            return Ok(false);
                        }
                    }
                    if let (Some(p), Some(view)) = (&q.predicate, &rows_view) {
                        *pe += 1;
                        if !prism_types::predicate::eval(p, view, row)? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                };

                let mut pe = 0usize; // predicate evals this range
                let mut dc = 0usize; // distances computed this range

                // **Three strategies, one candidate set (docs/QUERY-CONTRACT.md §9).** Every branch
                // offers exactly the predicate-passing rows, with their PQ distance, to the same
                // bounded top-k. They differ only in when the predicate runs relative to the
                // distance -- which changes the work, never the set. Plan-invariance is therefore
                // by construction, and the gate proves it.
                match strategy {
                    crate::plan::Strategy::Interleaved => {
                        // Distance every probed row (batched SIMD), filter inline.
                        prism_quantizer::kernel::adc_scan(
                            isa,
                            ctx.adc.table(),
                            m,
                            &codes,
                            &mut dists[..range.row_count],
                        );
                        dc += range.row_count;
                        for (i, &dist) in dists[..range.row_count].iter().enumerate() {
                            let row = range.first_row + i;
                            if !mask(row, &mut pe)? {
                                continue;
                            }
                            c.rows_passing_filter += 1;
                            topk.offer(crate::topk::Candidate {
                                dist,
                                part: pi as u32,
                                row: row as u32,
                            });
                        }
                    }
                    crate::plan::Strategy::ScalarFirst => {
                        // Filter first; compute a distance ONLY for survivors. When the predicate is
                        // selective, most distances are never computed.
                        for i in 0..range.row_count {
                            let row = range.first_row + i;
                            if !mask(row, &mut pe)? {
                                continue;
                            }
                            c.rows_passing_filter += 1;
                            let dist = ctx.adc.distance(&codes[i * m..(i + 1) * m]);
                            dc += 1;
                            topk.offer(crate::topk::Candidate {
                                dist,
                                part: pi as u32,
                                row: row as u32,
                            });
                        }
                    }
                    crate::plan::Strategy::SemanticFirst => {
                        // Distance first; evaluate the predicate ONLY for rows near enough to enter
                        // the selection. When the distance already narrows hard, the predicate is
                        // barely consulted. `would_admit` is conservative -- it never skips a row
                        // that could enter -- so the offered set is identical.
                        prism_quantizer::kernel::adc_scan(
                            isa,
                            ctx.adc.table(),
                            m,
                            &codes,
                            &mut dists[..range.row_count],
                        );
                        dc += range.row_count;
                        for (i, &dist) in dists[..range.row_count].iter().enumerate() {
                            if !topk.would_admit(dist) {
                                continue;
                            }
                            let row = range.first_row + i;
                            if !mask(row, &mut pe)? {
                                continue;
                            }
                            c.rows_passing_filter += 1;
                            topk.offer(crate::topk::Candidate {
                                dist,
                                part: pi as u32,
                                row: row as u32,
                            });
                        }
                    }
                }
                c.predicate_evals += pe;
                c.distances_computed += dc;
            }
        }

        let candidates: Vec<crate::topk::Candidate> = topk.into_sorted(); // nearest first
        c.candidates_considered = candidates.len();

        // The candidate phase ends here. Drop the borrowed views (`eligible`, `part_scalars`) so the
        // owned `readers` can move into the candidate set; `eligible_idx` preserves the mapping.
        drop(eligible);
        drop(part_scalars);
        Ok(CandidatePhase::Rerank(CandidateSet {
            readers,
            eligible: eligible_idx,
            ctxs,
            candidates,
            gen_ids,
            plan_choice,
            counters: c,
        }))
    }

    /// The **rerank phase**: exact-score a bounded PQ candidate set, apply the fetch budget and the
    /// similarity threshold, take the top-`k` with the C-4 `event_id` tie-break, materialize, and
    /// group. Factored out of [`search_at`](Self::search_at) so a distributed query reranks a
    /// *global* candidate set with the **same code** a single node runs on a local one — one
    /// implementation, no divergence ([query §20](../../../docs/QUERY-CONTRACT.md)). Single-store
    /// search is the degenerate one-shard case: `search_at` = candidate phase then this.
    /// The rerank stage of single-store search: bound the candidate set (rerank width, then the
    /// declared byte budget — storage §6), exact-score it, and finalize. In a cluster the bounding is
    /// the COORDINATOR's over the *global* candidate set and the scoring/finalizing fan out per shard,
    /// but it is the **same two methods** ([`rerank_scores`](Self::rerank_scores) /
    /// [`finalize`](Self::finalize)) — one implementation, no divergence ([D-073](../../../docs/DECISIONS.md)).
    #[allow(clippy::too_many_arguments)]
    fn rerank_phase(
        &self,
        snap: &prism_part::catalog::Snapshot,
        q: &Query,
        eligible: &[&prism_part::part::PartReader],
        ctxs: &BTreeMap<String, SpaceContext>,
        gen_ids: &BTreeSet<String>,
        plan_choice: &crate::plan::PlanChoice,
        candidates: Vec<crate::topk::Candidate>,
        mut c: Counters,
    ) -> Result<SearchResult> {
        let dim = self.store.config.dim;
        let mut candidates = candidates;
        candidates.truncate(q.rerank);
        // The fetch budget is a **byte** ceiling on the cold tier (storage §6): a plan declares how
        // many bytes of exact vectors it will pull, and execution is bounded by it. Exhausting it
        // reranks only the most-promising candidates that fit (already in PQ order) and **flags** the
        // result — never a silent over-fetch or a silent short answer. In a cluster this exact rule
        // runs once, at the coordinator, over the global set: the budget holds for the query, not
        // per-shard × N.
        let bytes_per_vector = dim * 4;
        if let Some(budget) = q.fetch_budget_bytes {
            let max_vectors = budget / bytes_per_vector.max(1);
            if candidates.len() > max_vectors {
                candidates.truncate(max_vectors);
                c.fetch_budget_exhausted = true;
            }
        }
        c.rerank_width = candidates.len();

        let scored = self.rerank_scores(q, eligible, ctxs, &candidates, &mut c)?;

        // Single-node materialization: the survivors' bodies and centroids come from the local
        // readers. The cluster passes a materializer that routes to the owning shards instead — the
        // one place the two paths differ, and only in *where* the survivor bodies come from (D-073).
        let materialize =
            |needed: &BTreeSet<(usize, usize)>| -> Result<BTreeMap<(usize, usize), (Event, u32)>> {
                let mut per_part: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
                for (p, r) in needed {
                    per_part.entry(*p).or_default().push(*r);
                }
                let mut out: BTreeMap<(usize, usize), (Event, u32)> = BTreeMap::new();
                for (pi, rows) in &per_part {
                    let r = eligible[*pi];
                    let evs = r.read_events_for_rows(rows)?;
                    for (row, ev) in rows.iter().zip(evs) {
                        let centroid = r
                            .manifest
                            .centroid_ranges
                            .iter()
                            .find(|cr| *row >= cr.first_row && *row < cr.first_row + cr.row_count)
                            .map(|cr| cr.centroid)
                            .ok_or_else(|| {
                                PrismError::Corrupt(format!(
                                    "row {row} of part {} is in no centroid range",
                                    r.manifest.part_id
                                ))
                            })?;
                        out.insert((*pi, *row), (ev, centroid));
                    }
                }
                Ok(out)
            };

        // `physical_bytes_read` is read AFTER materialization, because materializing the survivor
        // bodies is itself disk the query moved — computing it before would undercount.
        let tombstones: BTreeSet<String> = snap.tombstones.iter().cloned().collect();
        self.finalize(
            &tombstones,
            &snap.snapshot_id,
            q,
            scored,
            gen_ids,
            plan_choice,
            c,
            materialize,
            || eligible.iter().map(|r| r.io_bytes()).sum(),
        )
    }

    /// **Exact-score a bounded candidate set** — fetch each candidate's exact vector and score it on
    /// the chosen route, degrading to CPU on a device fault. Shared by the single node and, per shard,
    /// by a cluster's round two ([D-073](../../../docs/DECISIONS.md)). The candidates are already
    /// bounded (by the node, or by the coordinator's global budget), so this never re-bounds them.
    fn rerank_scores(
        &self,
        q: &Query,
        eligible: &[&prism_part::part::PartReader],
        ctxs: &BTreeMap<String, SpaceContext>,
        candidates: &[crate::topk::Candidate],
        c: &mut Counters,
    ) -> Result<Vec<Scored>> {
        let dim = self.store.config.dim;
        let mut by_part: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for cand in candidates {
            by_part
                .entry(cand.part as usize)
                .or_default()
                .push(cand.row as usize);
        }

        // Fetch every candidate's exact vector and id FIRST, separating I/O from the rerank compute:
        // a device-route degradation is then a pure recompute on already-fetched vectors.
        struct Fetched {
            part: usize,
            row: usize,
            vector: Vec<f32>,
            event_id: String,
            query_vector: Vec<f32>,
        }
        let mut fetched: Vec<Fetched> = Vec::with_capacity(candidates.len());
        for (pi, rows) in &by_part {
            let r = eligible[*pi];
            let ctx = ctxs.get(&r.manifest.generation_id).unwrap();
            // The cold tier goes through the object store + cache (S11): a cache state may not change
            // the answer, and a transient remote fault is a bounded retry or a named condition.
            let vectors = self.cold_read_vectors(r, rows)?;
            let ids = r.read_event_ids_for_rows(rows)?;
            c.exact_vectors_fetched += vectors.len();
            c.exact_bytes_fetched += vectors.len() * dim * 4;
            // One cold-tier object request per part touched (S11 EXPLAIN economics, §6).
            c.object_requests += 1;
            for ((row, v), event_id) in rows.iter().zip(vectors).zip(ids) {
                fetched.push(Fetched {
                    part: *pi,
                    row: *row,
                    vector: v,
                    event_id,
                    query_vector: ctx.query_vector.clone(),
                });
            }
        }

        // Route the rerank (S7): the route is invisible to the answer, so the planner chooses on cost;
        // a per-query force (a cursor pins one, to keep pagination on one route) wins over the global
        // override, which wins over the cost model.
        let forced = q
            .force_route
            .as_deref()
            .map(|s| match s {
                "gpu-reference" => crate::gpu::Route::GpuReference,
                "cuda" => crate::gpu::Route::Cuda,
                _ => crate::gpu::Route::Cpu,
            })
            .or_else(crate::gpu::forced_route_override);
        let mut plan = crate::gpu::plan_route(fetched.len(), forced);

        // Per-tenant device admission: a tenant over its device share is DEGRADED to CPU, not failed.
        let _reservation = if plan.route.is_device() {
            let bytes = fetched.len() * dim * 4;
            let tenant = q.tenant.as_deref().unwrap_or("(none)");
            match crate::gpu::admission().try_reserve(tenant, bytes) {
                Some(res) => Some(res),
                None => {
                    plan = crate::gpu::RoutePlan {
                        route: crate::gpu::Route::Cpu,
                        reason: "device admission refused this tenant's footprint; degraded".into(),
                    };
                    None
                }
            }
        } else {
            None
        };

        let score_all =
            |route: crate::gpu::Route| -> std::result::Result<Vec<Scored>, crate::gpu::DeviceFault> {
                let fault = if route.is_device() {
                    crate::gpu::injected_fault()
                } else {
                    None
                };
                let mut out = Vec::with_capacity(fetched.len());
                for f in &fetched {
                    // Exact cosine on the stored float32 vector. This is the answer; the PQ distance
                    // was only ever a way to avoid computing this for everything.
                    let score = crate::gpu::rerank_score(route, &f.query_vector, &f.vector, fault)?;
                    out.push(Scored {
                        score,
                        part: f.part,
                        row: f.row,
                        vector: f.vector.clone(),
                        event_id: f.event_id.clone(),
                    });
                }
                Ok(out)
            };

        let scored: Vec<Scored> = match score_all(plan.route) {
            Ok(s) => {
                c.rerank_route = plan.route.name().to_string();
                s
            }
            Err(fault) => {
                eprintln!(
                    "prism: device route `{}` faulted at {} ({}); degraded to CPU for tenant {:?}",
                    plan.route.name(),
                    fault.phase.name(),
                    fault.reason,
                    q.tenant.as_deref().unwrap_or("(none)")
                );
                c.route_degraded = true;
                c.rerank_route = crate::gpu::Route::Cpu.name().to_string();
                score_all(crate::gpu::Route::Cpu).expect("the CPU route cannot fault")
            }
        };
        drop(_reservation);
        Ok(scored)
    }

    /// **Finalize the rerank**: order by exact score (C-4 `event_id` tie-break), apply the similarity
    /// threshold, take the top-`k`, materialize the survivors (via `materialize` — local readers on a
    /// single node, the owning shards in a cluster, D-073), filter tombstones, and group. Shared by
    /// both paths, so a single node and a cluster resolve ties, thresholds, and limits identically.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finalize(
        &self,
        tombstones: &BTreeSet<String>,
        snapshot_id: &str,
        q: &Query,
        mut scored: Vec<Scored>,
        gen_ids: &BTreeSet<String>,
        plan_choice: &crate::plan::PlanChoice,
        mut c: Counters,
        materialize: impl Fn(
            &BTreeSet<(usize, usize)>,
        ) -> Result<BTreeMap<(usize, usize), (Event, u32)>>,
        physical_bytes_read: impl Fn() -> usize,
    ) -> Result<SearchResult> {
        let dim = self.store.config.dim;

        // Descending score, ties broken on `event_id` (C-4) — never on (part, row): order must be a
        // function of the data, not the layout, so a merge that moves rows between parts (or shards)
        // cannot change it.
        scored.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then(a.event_id.cmp(&b.event_id))
        });

        // Similarity threshold (§12): keep rows whose EXACT score clears the bar, THEN apply `k`.
        // Fewer than `k` clearing it is the honest count, not an error.
        if let Some(tau) = q.threshold {
            scored.retain(|s| s.score >= tau);
        }

        // Materialize only what we return (bodies are the most expensive column): the survivors for a
        // grouped query are all rerank rows; for a plain query, the top-`k`.
        let take = if q.group_k.is_some() {
            scored.len()
        } else {
            q.k
        };
        let needed: BTreeSet<(usize, usize)> =
            scored.iter().take(take).map(|s| (s.part, s.row)).collect();
        let materialized = materialize(&needed)?;
        // What the disk actually moved — read after materialization, since the survivor bodies count.
        c.physical_bytes_read = physical_bytes_read();

        let mut hits: Vec<Hit> = scored
            .iter()
            .take(q.k)
            .map(|s| {
                let (event, centroid) = materialized.get(&(s.part, s.row)).ok_or_else(|| {
                    PrismError::Corrupt(format!("survivor {} was not materialized", s.event_id))
                })?;
                Ok(Hit {
                    event: event.clone(),
                    score: s.score,
                    centroid: *centroid,
                })
            })
            .collect::<Result<Vec<Hit>>>()?;

        // A tombstoned row is logically deleted as of this snapshot — filtered from the answer even
        // while still physically present, until a merge reconciles it away (merge §6). In a cluster
        // the set is the union of the shards' tombstones (each keyed by event_id).
        if !tombstones.is_empty() {
            hits.retain(|h| !tombstones.contains(&h.event.event_id));
        }

        // Semantic grouping of the rerank survivors (§7): clusters the survivor vectors, with the
        // survivor events for exemplars.
        let events: BTreeMap<(usize, usize), Event> = materialized
            .iter()
            .map(|(k, (e, _))| (*k, e.clone()))
            .collect();
        let clusters = match q.group_k {
            Some(gk) if gk > 0 && !scored.is_empty() => {
                Some(group(&scored, &events, gk, dim, self.store.config.seed)?)
            }
            _ => None,
        };

        let explain = if q.explain {
            let actual_selectivity = if c.predicate_evals > 0 {
                c.rows_passing_filter as f64 / c.predicate_evals as f64
            } else if c.rows_scanned_pq > 0 {
                c.rows_passing_filter as f64 / c.rows_scanned_pq as f64
            } else {
                1.0
            };
            Some(prism_types::Explain {
                chosen_plan: c.plan.clone(),
                plan_reason: plan_choice.reason.clone(),
                chosen_route: c.rerank_route.clone(),
                estimated_selectivity: plan_choice.estimated_selectivity,
                actual_selectivity,
                estimated_nprobe: q.nprobe,
                actual_nprobe: c.probes_taken,
                actual_candidates: c.candidates_considered,
                actual_rerank: c.rerank_width,
                actual_k: hits.len(),
                actual_parts_opened: c.parts_opened,
                actual_ranges_scanned: c.ranges_scanned,
                actual_bytes_read: c.physical_bytes_read,
                object_requests: c.object_requests,
                retrieved_bytes: c.exact_bytes_fetched,
                estimated_cost_micros: crate::storage::estimated_cost_micros(
                    c.object_requests,
                    c.exact_bytes_fetched,
                ),
                declared_fetch_budget_bytes: q.fetch_budget_bytes,
                fetch_budget_exhausted: c.fetch_budget_exhausted,
            })
        } else {
            None
        };

        Ok(SearchResult {
            hits,
            clusters,
            counters: c,
            generations: gen_ids.iter().cloned().collect(),
            bridge: None,
            explain,
            snapshot_id: snapshot_id.to_string(),
        })
    }

    // --- the cluster's three round-trips (S12, [D-073](../../../docs/DECISIONS.md)) ------------
    //
    // A shard exposes exactly what the coordinator needs and nothing about its parts: round-1
    // candidates, round-2 exact scores for the coordinator's chosen rows, and materialization of the
    // final survivors. The coordinator never holds a reader.

    /// **Round 1.** Run the candidate phase and return its candidates as a serializable list — each a
    /// PQ distance, an `event_id` (the C-4 tie the coordinator merges on), and the `(part_id, row)`
    /// handle it sends back in round 2. An empty eligible set yields no candidates; a multi-space
    /// query (a bridge) is refused for now — cross-shard rank-fusion is not in this increment.
    pub(crate) fn search_candidates(
        &self,
        snap: &prism_part::catalog::Snapshot,
        q: &Query,
    ) -> Result<Vec<ShardCandidate>> {
        match self.candidate_phase(snap, q)? {
            CandidatePhase::Done(r) => {
                if r.hits.is_empty() && r.bridge.is_none() {
                    Ok(Vec::new())
                } else {
                    Err(PrismError::Invalid(
                        "a cross-tenant cluster query that spans two embedding spaces (a bridge) is \
                         not supported yet — rank-fusion across shards is a later increment"
                            .into(),
                    ))
                }
            }
            CandidatePhase::Rerank(cs) => {
                let eligible: Vec<&prism_part::part::PartReader> =
                    cs.eligible.iter().map(|i| &cs.readers[*i]).collect();
                // Read event ids in one pass per part.
                let mut by_part: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
                for cand in &cs.candidates {
                    by_part
                        .entry(cand.part as usize)
                        .or_default()
                        .push(cand.row as usize);
                }
                let mut eid: BTreeMap<(usize, usize), String> = BTreeMap::new();
                for (pi, rows) in &by_part {
                    let ids = eligible[*pi].read_event_ids_for_rows(rows)?;
                    for (row, id) in rows.iter().zip(ids) {
                        eid.insert((*pi, *row), id);
                    }
                }
                Ok(cs
                    .candidates
                    .iter()
                    .map(|cand| ShardCandidate {
                        dist: cand.dist,
                        event_id: eid[&(cand.part as usize, cand.row as usize)].clone(),
                        part_id: eligible[cand.part as usize].manifest.part_id.clone(),
                        row: cand.row as usize,
                    })
                    .collect())
            }
        }
    }

    /// Open the named parts (deduplicated) and return them plus a `part_id → index` map — the readers
    /// round 2 and materialization score/read against.
    fn open_selected(
        &self,
        selected: &[(String, usize)],
    ) -> Result<(
        Vec<prism_part::part::PartReader>,
        std::collections::HashMap<String, usize>,
    )> {
        let mut order: Vec<String> = Vec::new();
        let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (pid, _) in selected {
            if !index.contains_key(pid) {
                index.insert(pid.clone(), order.len());
                order.push(pid.clone());
            }
        }
        let readers = order
            .iter()
            .map(|pid| prism_part::part::PartReader::open(&self.store.part_dir(pid)))
            .collect::<Result<Vec<_>>>()?;
        Ok((readers, index))
    }

    /// **Round 2.** Exact-score exactly the rows the coordinator chose from the global candidate set —
    /// no more, so the total exact-vector fetches across all shards stay within the one global budget
    /// ([D-073](../../../docs/DECISIONS.md)). Re-opens the parts and rebuilds the per-space context
    /// (shared with the candidate phase, `build_space_contexts`), then runs the one shared
    /// `rerank_scores`.
    pub(crate) fn search_rerank_selected(
        &self,
        q: &Query,
        selected: &[(String, usize)],
    ) -> Result<Vec<ShardScored>> {
        if selected.is_empty() {
            return Ok(Vec::new());
        }
        let (readers, index) = self.open_selected(selected)?;
        let eligible: Vec<&prism_part::part::PartReader> = readers.iter().collect();
        let mut c = Counters::default();
        let (ctxs, _gen_ids) = self.build_space_contexts(&eligible, q, &mut c)?;
        let candidates: Vec<crate::topk::Candidate> = selected
            .iter()
            .map(|(pid, row)| crate::topk::Candidate {
                dist: 0.0,
                part: index[pid] as u32,
                row: *row as u32,
            })
            .collect();
        let scored = self.rerank_scores(q, &eligible, &ctxs, &candidates, &mut c)?;
        Ok(scored
            .into_iter()
            .map(|s| ShardScored {
                score: s.score,
                event_id: s.event_id,
                part_id: eligible[s.part].manifest.part_id.clone(),
                row: s.row,
                vector: s.vector,
            })
            .collect())
    }

    /// **Materialize** the final survivors the coordinator selected: their `Event` body and the
    /// centroid each lives in, in the order asked, so payload is bounded by `k`.
    pub(crate) fn search_materialize(
        &self,
        selected: &[(String, usize)],
    ) -> Result<Vec<(Event, u32)>> {
        if selected.is_empty() {
            return Ok(Vec::new());
        }
        let (readers, index) = self.open_selected(selected)?;
        let mut out = Vec::with_capacity(selected.len());
        for (pid, row) in selected {
            let r = &readers[index[pid]];
            let ev = r.read_events_for_rows(&[*row])?.pop().ok_or_else(|| {
                PrismError::Corrupt(format!("row {row} of part {pid} is missing"))
            })?;
            let centroid = r
                .manifest
                .centroid_ranges
                .iter()
                .find(|cr| *row >= cr.first_row && *row < cr.first_row + cr.row_count)
                .map(|cr| cr.centroid)
                .ok_or_else(|| {
                    PrismError::Corrupt(format!("row {row} of part {pid} is in no centroid range"))
                })?;
            out.push((ev, centroid));
        }
        Ok(out)
    }

    /// Answer a query that spans two embedding spaces, through a **declared bridge**.
    ///
    /// > *"Scores from different embedding spaces are never merged without an explicit,
    /// > validated bridge policy."* — PRISM.md, invariant 9
    ///
    /// **This does not merge scores. It merges ranks.** Each space answers the query natively —
    /// its own query embedding, its own codebooks, its own cosines, entirely inside its own
    /// geometry — and then the two *rankings* are fused. A rank is unitless, which is exactly why
    /// this obeys the invariant rather than sneaking around it. A cosine of 0.83 in one model's
    /// space and 0.83 in another's are two different numbers that happen to be printed the same
    /// way, and averaging them would be a category error with a plausible-looking result.
    ///
    /// Reciprocal-rank fusion: a row's fused score is the sum of `1 / (K + rank)` over the spaces
    /// that returned it. The score in the output is the **fusion score, not a cosine**, and
    /// `bridge` is set so nobody can mistake it for one.
    /// **TEST ORACLE (S12 inc2) — deleted at the exit criterion.** The frozen, self-contained
    /// monolithic search, kept only so the phased path can be proven identical to it — same rows,
    /// same errors, same named conditions, same degraded behavior, same counters — before it is
    /// removed and the phased path becomes the sole implementation ([query §20](../../../docs/QUERY-CONTRACT.md)).
    #[doc(hidden)]
    pub fn search_at_monolith(
        &self,
        snap: &prism_part::catalog::Snapshot,
        q: &Query,
    ) -> Result<SearchResult> {
        if q.k == 0 {
            return Err(PrismError::Invalid("k must be positive".into()));
        }
        if q.nprobe == 0 {
            return Err(PrismError::Invalid("nprobe must be positive".into()));
        }
        if q.text.trim().is_empty() {
            return Err(PrismError::Invalid("query text is empty".into()));
        }

        let dim = self.store.config.dim;

        // Time bounds from the query AND, conservatively, from the predicate's top-level
        // conjunction. `time_bounds` never narrows across an OR: pruning that can lose a row is
        // not pruning, it is sampling.
        let (pred_from, pred_to) = match &q.predicate {
            Some(p) => prism_types::predicate::time_bounds(p),
            None => (None, None),
        };
        let from = match (q.time_from, pred_from) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let to = match (q.time_to, pred_to) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };

        // --- 0. PARTITION PRUNING, IN THE CATALOG, BEFORE ANY PART IS OPENED (S4) ---
        //
        // This is the line that makes "cross-tenant reads are physically impossible" an I/O
        // property rather than a slogan. A part outside this query's partitions is never
        // opened, never checksummed, never read -- so another tenant's partitions could be
        // filled with unreadable garbage and this query would still answer correctly, because
        // it never looked.
        let (readers, catalog_pruned) =
            self.open_candidates(snap, q.tenant.as_deref(), from, to)?;

        let mut c = Counters {
            parts_total: snap.parts.len(),
            parts_pruned: catalog_pruned,
            ..Default::default()
        };

        // --- 1. per-tenant zone maps, inside the parts we did open ---
        //
        // A shared bucket's part-level zone map describes the BUCKET, not the tenant. Using it
        // would both leak (tenant A learns tenant B's time range) and under-prune (A cannot skip
        // a part whose range is wide only because of B). So a query consults its OWN tenant's
        // section (directive 3).
        let mut eligible: Vec<&PartReader> = Vec::new();
        for r in readers.iter() {
            // **No part is ever decoded with the wrong codebook.** The catalog says which
            // generation this part is in; the part itself says so too. If they ever disagree,
            // one of them is lying, and the cost of guessing which is not a crash -- it is a
            // PLAUSIBLE WRONG ANSWER, because a PQ code read against the wrong codebook still
            // produces a number, and the number looks fine. So: refuse.
            if let Some(pr) = snap
                .parts
                .iter()
                .find_map(|e| e.located().filter(|pr| pr.part_id == r.manifest.part_id))
            {
                if pr.partition.generation != r.manifest.generation_id {
                    return Err(PrismError::Corrupt(format!(
                        "part {} is filed in the catalog under generation {} but its manifest \
                         says it was encoded under {}. Decoding it with either codebook would \
                         produce a number, and the number would look fine. Refusing.",
                        r.manifest.part_id, pr.partition.generation, r.manifest.generation_id
                    )));
                }
            }
            let keep = match (
                q.tenant.as_deref(),
                r.manifest.s4()?.stats_for_owned(q.tenant.as_deref()),
            ) {
                (Some(_), Some(st)) => st.may_match(from, to),
                _ => r.manifest.may_match(q.tenant.as_deref(), from, to),
            };
            if keep {
                eligible.push(r);
            } else {
                c.parts_pruned += 1;
            }
        }

        // --- 2. embedding-space check (invariant 9) ---
        let mut spaces: BTreeSet<String> = BTreeSet::new();
        for r in &eligible {
            spaces.insert(format!(
                "{}:{}",
                r.manifest.model_id, r.manifest.model_version
            ));
        }
        if spaces.len() > 1 {
            match &q.space {
                Some(want) => {
                    if !spaces.contains(want) {
                        return Err(PrismError::Invalid(format!(
                            "no eligible parts are in embedding space `{want}`; \
                             present spaces: {:?}",
                            spaces
                        )));
                    }
                    let before = eligible.len();
                    eligible.retain(|r| {
                        format!("{}:{}", r.manifest.model_id, r.manifest.model_version) == *want
                    });
                    // Parts dropped by the space filter are PRUNED, and must be counted as such.
                    // Naming a space narrows the query, and a narrowing a caller cannot see in
                    // the counters is a narrowing they cannot audit.
                    c.parts_pruned += before - eligible.len();
                }
                None => {
                    // A bridge, if somebody declared one, is the ONLY way across (generation
                    // contract §6). It does not merge scores -- it fuses ranks, which are
                    // unitless. Obeying invariant 9 rather than working around it.
                    let list: Vec<&String> = spaces.iter().collect();
                    if list.len() == 2 {
                        if let Some(b) = snap.bridge(list[0], list[1]) {
                            return self.search_bridged(snap, q, &spaces, b);
                        }
                    }
                    // The error is written to TEACH (docs/QUERY-CONTRACT.md §13): this is invariant
                    // 9 surfacing where a SQL user first meets it, and a terse "spaces differ" would
                    // leave them guessing why their perfectly reasonable query was refused.
                    let mut names: Vec<&str> = spaces.iter().map(|s| s.as_str()).collect();
                    names.sort();
                    let (a, b) = (names[0], names.get(1).copied().unwrap_or(""));
                    return Err(PrismError::Invariant(format!(
                        "this query spans two embedding spaces — {a} and {b} — whose scores are \
                         not comparable (a cosine of 0.8 in one is not a cosine of 0.8 in the \
                         other). PrismDB will not merge them into one ranking. Either name one \
                         space with `USING SPACE '{b}'`, declare a bridge to fuse their ranks \
                         (`prism bridge declare`), or finish the re-embed migration so a single \
                         space remains."
                    )));
                }
            }
        }

        c.rows_eligible = eligible.iter().map(|r| r.manifest.row_count).sum();

        if eligible.is_empty() {
            return Ok(SearchResult {
                hits: Vec::new(),
                clusters: None,
                counters: c,
                generations: Vec::new(),
                bridge: None,
                explain: None,
                snapshot_id: snap.snapshot_id.clone(),
            });
        }

        // --- 3. one query embedding + one ADC table per generation ---
        let (ctxs, gen_ids) = self.build_space_contexts(&eligible, q, &mut c)?;

        // --- 4. scan the selected centroid ranges ---

        // Pick the kernel once, for the whole query, and record it. Every kernel returns
        // bit-identical distances (docs/DETERMINISM-CONTRACT.md §1), so this changes the query's
        // speed and never its answer.
        let isa = prism_quantizer::kernel::best();
        c.scan_isa = isa.name().to_string();

        // Pre-load every eligible part's scalar columns, once, and keep them alive for the whole
        // scan. The top-k's tie-break borrows event ids out of these instead of owning copies, so
        // a row entering the top-k allocates nothing (§4). These columns are read anyway -- the
        // fused mask needs tenant and time -- so this only moves the read earlier, it does not add
        // one.
        let part_scalars: Vec<prism_part::part::Scalars> = eligible
            .iter()
            .map(|r| r.read_scalars())
            .collect::<Result<Vec<_>>>()?;
        let id_of =
            |part: u32, row: u32| -> &str { part_scalars[part as usize].event_id_at(row as usize) };
        let mut topk = crate::topk::TopK::new(q.candidates, &id_of);

        // One distance buffer, sized once to the largest range any eligible part holds, and
        // reused for every range of every part. The centroid ranges are in the manifest, so this
        // needs no I/O -- and it is what makes the hot loop allocate nothing (§4).
        let max_range_rows = eligible
            .iter()
            .flat_map(|r| r.manifest.centroid_ranges.iter())
            .map(|cr| cr.row_count)
            .max()
            .unwrap_or(0);
        let mut dists = vec![0.0f32; max_range_rows];

        // --- choose the physical strategy (S8) ---
        //
        // The strategy is invisible to the answer (plan-invariance, docs/QUERY-CONTRACT.md §9), so
        // the optimizer chooses on cost alone. A forced plan (test override, or a per-query hint)
        // wins, so the plan-invariance gate can prove every strategy answers identically.
        let plan_choice = {
            let forced = crate::plan::forced_plan_override()
                .or_else(|| q.plan.as_deref().and_then(crate::plan::Strategy::parse));
            self.choose_plan(q, &eligible, forced)
        };
        let strategy = plan_choice.strategy;
        c.plan = strategy.name().to_string();

        for (pi, r) in eligible.iter().enumerate() {
            c.parts_opened += 1;
            let ctx = ctxs.get(&r.manifest.generation_id).ok_or_else(|| {
                PrismError::Corrupt("part references an absent generation".into())
            })?;

            let scalars = &part_scalars[pi];
            let times = &scalars.times;

            // The row predicate, if any. Columns load lazily and only if the predicate
            // actually names them — a filter that never mentions `body` must not cost a
            // `body` decode.
            let rows_view = match &q.predicate {
                Some(p) => Some(crate::rowsource::PartRows::new(r, Some(p))?),
                None => None,
            };

            let by_centroid: BTreeMap<u32, &prism_part::part::CentroidRange> = r
                .manifest
                .centroid_ranges
                .iter()
                .map(|cr| (cr.centroid, cr))
                .collect();

            // Adaptive probing (S6): a boundary query may probe ABOVE the base nprobe, never
            // below it, so recall can only improve and the receipts stay valid as floors. On a
            // query deep inside a cluster the margin is not met and this is exactly q.nprobe.
            let eff_nprobe = if q.adaptive {
                prism_types::query::adaptive_nprobe(
                    &ctx.ranked_dists,
                    q.nprobe,
                    q.adaptive_margin
                        .unwrap_or(prism_types::query::ADAPTIVE_MARGIN),
                    prism_types::query::ADAPTIVE_MAX_NPROBE,
                )
            } else {
                q.nprobe.min(ctx.ranked.len())
            };
            c.probes_taken += eff_nprobe;
            c.probes_widened += eff_nprobe.saturating_sub(q.nprobe.min(ctx.ranked.len()));

            let probes = ctx.ranked.iter().take(eff_nprobe);
            for &cid in probes {
                let Some(range) = by_centroid.get(&cid) else {
                    // This part has no rows in that centroid. The probe costs
                    // nothing: no range, no read.
                    continue;
                };

                // Range-level zone map: a probe whose whole time span is outside
                // the predicate is skipped without a read.
                if let Some(f) = q.time_from {
                    if range.time_max < f {
                        continue;
                    }
                }
                if let Some(t) = q.time_to {
                    if range.time_min > t {
                        continue;
                    }
                }

                let codes = r.read_pq_range(range)?;
                c.ranges_scanned += 1;
                c.pq_bytes_scanned += codes.len();
                c.rows_scanned_pq += range.row_count;

                let m = r.manifest.pq_m;

                // The scalar mask, one closure so all three strategies apply the *same* predicate
                // to the *same* rows. `pe` counts general-predicate evaluations (the expensive
                // part; tenant and time are cheap columnar checks).
                let mask = |row: usize, pe: &mut usize| -> Result<bool> {
                    if let Some(t) = &q.tenant {
                        if !scalars.tenant_is(row, t) {
                            return Ok(false);
                        }
                    }
                    if let Some(f) = q.time_from {
                        if times[row] < f {
                            return Ok(false);
                        }
                    }
                    if let Some(t) = q.time_to {
                        if times[row] > t {
                            return Ok(false);
                        }
                    }
                    if let (Some(p), Some(view)) = (&q.predicate, &rows_view) {
                        *pe += 1;
                        if !prism_types::predicate::eval(p, view, row)? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                };

                let mut pe = 0usize; // predicate evals this range
                let mut dc = 0usize; // distances computed this range

                // **Three strategies, one candidate set (docs/QUERY-CONTRACT.md §9).** Every branch
                // offers exactly the predicate-passing rows, with their PQ distance, to the same
                // bounded top-k. They differ only in when the predicate runs relative to the
                // distance -- which changes the work, never the set. Plan-invariance is therefore
                // by construction, and the gate proves it.
                match strategy {
                    crate::plan::Strategy::Interleaved => {
                        // Distance every probed row (batched SIMD), filter inline.
                        prism_quantizer::kernel::adc_scan(
                            isa,
                            ctx.adc.table(),
                            m,
                            &codes,
                            &mut dists[..range.row_count],
                        );
                        dc += range.row_count;
                        for (i, &dist) in dists[..range.row_count].iter().enumerate() {
                            let row = range.first_row + i;
                            if !mask(row, &mut pe)? {
                                continue;
                            }
                            c.rows_passing_filter += 1;
                            topk.offer(crate::topk::Candidate {
                                dist,
                                part: pi as u32,
                                row: row as u32,
                            });
                        }
                    }
                    crate::plan::Strategy::ScalarFirst => {
                        // Filter first; compute a distance ONLY for survivors. When the predicate is
                        // selective, most distances are never computed.
                        for i in 0..range.row_count {
                            let row = range.first_row + i;
                            if !mask(row, &mut pe)? {
                                continue;
                            }
                            c.rows_passing_filter += 1;
                            let dist = ctx.adc.distance(&codes[i * m..(i + 1) * m]);
                            dc += 1;
                            topk.offer(crate::topk::Candidate {
                                dist,
                                part: pi as u32,
                                row: row as u32,
                            });
                        }
                    }
                    crate::plan::Strategy::SemanticFirst => {
                        // Distance first; evaluate the predicate ONLY for rows near enough to enter
                        // the selection. When the distance already narrows hard, the predicate is
                        // barely consulted. `would_admit` is conservative -- it never skips a row
                        // that could enter -- so the offered set is identical.
                        prism_quantizer::kernel::adc_scan(
                            isa,
                            ctx.adc.table(),
                            m,
                            &codes,
                            &mut dists[..range.row_count],
                        );
                        dc += range.row_count;
                        for (i, &dist) in dists[..range.row_count].iter().enumerate() {
                            if !topk.would_admit(dist) {
                                continue;
                            }
                            let row = range.first_row + i;
                            if !mask(row, &mut pe)? {
                                continue;
                            }
                            c.rows_passing_filter += 1;
                            topk.offer(crate::topk::Candidate {
                                dist,
                                part: pi as u32,
                                row: row as u32,
                            });
                        }
                    }
                }
                c.predicate_evals += pe;
                c.distances_computed += dc;
            }
        }

        c.candidates_considered = topk.len();

        // --- 5. exact rerank, within the declared fetch budget ---
        let mut candidates: Vec<crate::topk::Candidate> = topk.into_sorted(); // nearest first
        candidates.truncate(q.rerank);
        // The fetch budget is a **byte** ceiling on the cold tier (storage contract §6): a plan may
        // declare how many bytes of exact vectors it is willing to pull, and execution is bounded by
        // it — not an unbounded fetch. Exhausting it reranks only the most-promising candidates that
        // fit (they are already in PQ-distance order, so this keeps the best) and **flags** the
        // result as budget-limited rather than silently over-fetching or silently under-answering.
        let bytes_per_vector = dim * 4;
        if let Some(budget) = q.fetch_budget_bytes {
            let max_vectors = budget / bytes_per_vector.max(1);
            if candidates.len() > max_vectors {
                candidates.truncate(max_vectors);
                c.fetch_budget_exhausted = true;
            }
        }
        c.rerank_width = candidates.len();

        let mut by_part: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for cand in &candidates {
            by_part
                .entry(cand.part as usize)
                .or_default()
                .push(cand.row as usize);
        }

        // Fetch every candidate's exact vector and id FIRST, separating I/O from the rerank
        // compute. This is what makes a device-route degradation a pure recompute: if the GPU
        // route faults, we re-score the already-fetched vectors on the CPU without touching disk.
        struct Fetched {
            part: usize,
            row: usize,
            vector: Vec<f32>,
            event_id: String,
            query_vector: Vec<f32>,
        }
        let mut fetched: Vec<Fetched> = Vec::with_capacity(candidates.len());
        for (pi, rows) in &by_part {
            let r = eligible[*pi];
            let ctx = ctxs.get(&r.manifest.generation_id).unwrap();
            // The cold tier goes through the object store + cache (S11): a cache state is a physical
            // layout and may not change the answer, and a transient remote fault is a bounded retry
            // or a named condition, never a silently short fetch (storage contract §3/§4).
            let vectors = self.cold_read_vectors(r, rows)?;
            let ids = r.read_event_ids_for_rows(rows)?;
            c.exact_vectors_fetched += vectors.len();
            c.exact_bytes_fetched += vectors.len() * dim * 4;
            // One cold-tier object request per part touched (S11 EXPLAIN economics, §6). Coalesced
            // ranged reads within a part are one logical request against the object.
            c.object_requests += 1;
            for ((row, v), event_id) in rows.iter().zip(vectors).zip(ids) {
                fetched.push(Fetched {
                    part: *pi,
                    row: *row,
                    vector: v,
                    event_id,
                    query_vector: ctx.query_vector.clone(),
                });
            }
        }

        // --- route the rerank (S7) ---
        //
        // The route is invisible to the answer (selection-identity, determinism contract §9), so
        // the planner chooses on cost alone. With the GPU off this is always CPU; a test may force
        // a device route to prove the answer survives the route change.
        // Precedence: an explicit per-query route (which a cursor sets, to pin a paginated query
        // to one route) wins over the global test override, which wins over the cost model. The
        // cursor's route MUST win, or a route flip between pages would corrupt pagination.
        let forced = q
            .force_route
            .as_deref()
            .map(|s| match s {
                "gpu-reference" => crate::gpu::Route::GpuReference,
                "cuda" => crate::gpu::Route::Cuda,
                _ => crate::gpu::Route::Cpu,
            })
            .or_else(crate::gpu::forced_route_override);
        let mut plan = crate::gpu::plan_route(fetched.len(), forced);

        // Per-tenant device admission: a device route reserves its footprint before running, and a
        // tenant over its share is DEGRADED to CPU rather than allowed to fail another tenant
        // (determinism contract §11). The reservation releases on drop.
        let _reservation = if plan.route.is_device() {
            let bytes = fetched.len() * dim * 4;
            let tenant = q.tenant.as_deref().unwrap_or("(none)");
            match crate::gpu::admission().try_reserve(tenant, bytes) {
                Some(res) => Some(res),
                None => {
                    // Not enough of this tenant's device share; degrade, do not fail.
                    plan = crate::gpu::RoutePlan {
                        route: crate::gpu::Route::Cpu,
                        reason: "device admission refused this tenant's footprint; degraded".into(),
                    };
                    None
                }
            }
        } else {
            None
        };

        // Rerank on the chosen route, degrading to CPU on a device fault. A device is an
        // accelerator, not a dependency: a query answerable on the CPU is always answered.
        let score_all = |route: crate::gpu::Route| -> std::result::Result<Vec<Scored>, crate::gpu::DeviceFault> {
            let fault = if route.is_device() {
                crate::gpu::injected_fault()
            } else {
                None
            };
            let mut out = Vec::with_capacity(fetched.len());
            for f in &fetched {
                // Exact cosine on the stored float32 vector. This is the answer; the PQ distance
                // was only ever a way to avoid computing this for everything.
                let score = crate::gpu::rerank_score(route, &f.query_vector, &f.vector, fault)?;
                out.push(Scored {
                    score,
                    part: f.part,
                    row: f.row,
                    vector: f.vector.clone(),
                    event_id: f.event_id.clone(),
                });
            }
            Ok(out)
        };

        let scored: Vec<Scored> = match score_all(plan.route) {
            Ok(s) => {
                c.rerank_route = plan.route.name().to_string();
                s
            }
            Err(fault) => {
                // Degrade to CPU, logged and observable. Never a failed query. The log makes a
                // degradation loud -- a GPU that quietly stopped being used is one you are paying
                // for and not getting -- and `route_degraded` carries it in the counters too.
                eprintln!(
                    "prism: device route `{}` faulted at {} ({}); degraded to CPU for tenant {:?}",
                    plan.route.name(),
                    fault.phase.name(),
                    fault.reason,
                    q.tenant.as_deref().unwrap_or("(none)")
                );
                c.route_degraded = true;
                c.rerank_route = crate::gpu::Route::Cpu.name().to_string();
                score_all(crate::gpu::Route::Cpu).expect("the CPU route cannot fault")
            }
        };
        drop(_reservation);
        let mut scored = scored;

        // Descending score, ties broken on `event_id`.
        //
        // Not on (part, row). Bodies repeat in real telemetry, so exact score
        // ties are common, and a tie-break on physical position means the same
        // query returns a different order after a merge moves rows between
        // parts. Order must be a function of the data, not of the layout — the
        // exact oracle breaks ties the same way, so the two paths agree on tied
        // results and recall stays measurable.
        scored.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then(a.event_id.cmp(&b.event_id))
        });

        // Similarity threshold (docs/QUERY-CONTRACT.md §12): keep only rows whose EXACT rerank
        // score clears the bar, THEN apply `k`. Threshold first, LIMIT second. Applied to the
        // exact score, never the approximate PQ distance -- a threshold on an approximate score
        // would admit rows the exact score rejects. Fewer than `k` clearing the bar is the honest
        // count, not an error.
        if let Some(tau) = q.threshold {
            scored.retain(|s| s.score >= tau);
        }

        // --- 6. materialize only what we return ---
        //
        // Bodies are the most expensive column and the least useful one to a
        // scan. Only the rows a caller will actually see pay for theirs.
        let take = if q.group_k.is_some() {
            scored.len()
        } else {
            q.k
        };
        let needed: BTreeSet<(usize, usize)> =
            scored.iter().take(take).map(|s| (s.part, s.row)).collect();

        let mut per_part: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (p, r) in &needed {
            per_part.entry(*p).or_default().push(*r);
        }
        let mut events: BTreeMap<(usize, usize), Event> = BTreeMap::new();
        for (pi, rows) in &per_part {
            let evs = eligible[*pi].read_events_for_rows(rows)?;
            for (row, ev) in rows.iter().zip(evs) {
                events.insert((*pi, *row), ev);
            }
        }

        // Attach the centroid each hit actually lives in, so the physical
        // clustering is inspectable from a result and not only from the counters.
        let mut hits: Vec<Hit> = scored
            .iter()
            .take(q.k)
            .map(|s| {
                let r = eligible[s.part];
                let centroid = r
                    .manifest
                    .centroid_ranges
                    .iter()
                    .find(|cr| s.row >= cr.first_row && s.row < cr.first_row + cr.row_count)
                    .map(|cr| cr.centroid)
                    .ok_or_else(|| {
                        PrismError::Corrupt(format!(
                            "row {} of part {} is in no centroid range",
                            s.row, r.manifest.part_id
                        ))
                    })?;
                Ok(Hit {
                    event: events.get(&(s.part, s.row)).unwrap().clone(),
                    score: s.score,
                    centroid,
                })
            })
            .collect::<Result<Vec<Hit>>>()?;

        // A tombstoned row is logically deleted as of this snapshot — it is filtered from the
        // answer even while it is still physically present, until a merge reconciles it away
        // (merge contract §6). Filtering the final hits can return fewer than `k`, which is the
        // honest count of surviving matches, exactly as a threshold can (§12).
        if !snap.tombstones.is_empty() {
            hits.retain(|h| !snap.is_tombstoned(&h.event.event_id));
        }

        // --- 7. semantic grouping of the rerank survivors ---
        let clusters = match q.group_k {
            Some(gk) if gk > 0 && !scored.is_empty() => {
                Some(group(&scored, &events, gk, dim, self.store.config.seed)?)
            }
            _ => None,
        };

        // What the disk actually moved, as opposed to what the plan asked for.
        c.physical_bytes_read = eligible.iter().map(|r| r.io_bytes()).sum();

        // EXPLAIN (S8, §14): estimates alongside actuals, so cost-model drift is a visible number.
        let explain = if q.explain {
            // Pass rate among rows the predicate was ACTUALLY evaluated on. For interleaved and
            // scalar-first that is every scanned row, so it is the true selectivity; semantic-first
            // evaluates a biased near subset, so its number is only a lower bound -- EXPLAIN says
            // which plan produced it, so the reader knows.
            let actual_selectivity = if c.predicate_evals > 0 {
                c.rows_passing_filter as f64 / c.predicate_evals as f64
            } else if c.rows_scanned_pq > 0 {
                // No general predicate: selectivity is the tenant/time pass rate over scanned rows.
                c.rows_passing_filter as f64 / c.rows_scanned_pq as f64
            } else {
                1.0
            };
            Some(prism_types::Explain {
                chosen_plan: c.plan.clone(),
                plan_reason: plan_choice.reason.clone(),
                chosen_route: c.rerank_route.clone(),
                estimated_selectivity: plan_choice.estimated_selectivity,
                actual_selectivity,
                estimated_nprobe: q.nprobe,
                actual_nprobe: c.probes_taken,
                actual_candidates: c.candidates_considered,
                actual_rerank: c.rerank_width,
                actual_k: hits.len(),
                actual_parts_opened: c.parts_opened,
                actual_ranges_scanned: c.ranges_scanned,
                actual_bytes_read: c.physical_bytes_read,
                object_requests: c.object_requests,
                retrieved_bytes: c.exact_bytes_fetched,
                estimated_cost_micros: crate::storage::estimated_cost_micros(
                    c.object_requests,
                    c.exact_bytes_fetched,
                ),
                declared_fetch_budget_bytes: q.fetch_budget_bytes,
                fetch_budget_exhausted: c.fetch_budget_exhausted,
            })
        } else {
            None
        };

        Ok(SearchResult {
            hits,
            clusters,
            counters: c,
            generations: gen_ids.into_iter().collect(),
            bridge: None,
            explain,
            snapshot_id: snap.snapshot_id.clone(),
        })
    }

    fn search_bridged(
        &self,
        snap: &prism_part::catalog::Snapshot,
        q: &Query,
        spaces: &BTreeSet<String>,
        bridge: &prism_part::catalog::Bridge,
    ) -> Result<SearchResult> {
        /// The RRF damping constant. **Policy**: it decides how steeply rank 1 outweighs rank 10.
        /// 60 is the value the fusion literature settled on; we have no measurement of our own
        /// that would justify a different one, and inventing a number here would be worse than
        /// borrowing one honestly.
        const RRF_K: f32 = 60.0;

        let mut fused: BTreeMap<String, (f32, Hit)> = BTreeMap::new();
        let mut counters = Counters::default();
        let mut generations: Vec<String> = Vec::new();

        for space in spaces {
            let mut sub = q.clone();
            sub.space = Some(space.clone());
            // Each space answers natively, in its own geometry, on its own terms.
            let r = self.search_at(snap, &sub)?;

            for (i, hit) in r.hits.iter().enumerate() {
                let e = fused
                    .entry(hit.event.event_id.clone())
                    .or_insert_with(|| (0.0, hit.clone()));
                e.0 += 1.0 / (RRF_K + (i + 1) as f32);
            }

            counters.parts_total = r.counters.parts_total;
            counters.parts_pruned += r.counters.parts_pruned;
            counters.parts_opened += r.counters.parts_opened;
            counters.rows_scanned_pq += r.counters.rows_scanned_pq;
            counters.rows_passing_filter += r.counters.rows_passing_filter;
            counters.candidates_considered += r.counters.candidates_considered;
            counters.exact_vectors_fetched += r.counters.exact_vectors_fetched;
            counters.exact_bytes_fetched += r.counters.exact_bytes_fetched;
            counters.physical_bytes_read += r.counters.physical_bytes_read;
            counters.rows_eligible += r.counters.rows_eligible;
            generations.extend(r.generations);
        }

        let mut rows: Vec<(f32, Hit)> = fused.into_values().collect();
        // Fused score descending, ties on event_id — the same total order as everywhere else
        // (C-4). The *selection* below is bounded, so the tie-break decides which rows are
        // allowed to be answers, not merely how they are printed.
        rows.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then(a.1.event.event_id.cmp(&b.1.event.event_id))
        });
        rows.truncate(q.k);

        let hits = rows
            .into_iter()
            .map(|(score, mut h)| {
                // The score is a FUSION SCORE, not a cosine. Leaving the native cosine here
                // would be handing back a number from one geometry and calling it the answer for
                // two.
                h.score = score;
                h
            })
            .collect();

        generations.sort();
        generations.dedup();

        Ok(SearchResult {
            hits,
            clusters: None,
            counters,
            generations,
            bridge: Some(format!(
                "{:?} across {} <-> {} ({})",
                bridge.policy, bridge.from_space, bridge.to_space, bridge.validation
            )),
            explain: None,
            snapshot_id: snap.snapshot_id.clone(),
        })
    }

    /// Brute-force exact search over every eligible row. No centroids, no PQ,
    /// no candidate list — the ground truth the approximate path is measured
    /// against (Part II §7.3). It is the oracle, and it is deliberately slow.
    pub fn exact_search(&self, q: &Query) -> Result<Vec<Hit>> {
        let snap = self.snapshot()?;
        let readers = self.open_parts(&snap)?;
        let dim = self.store.config.dim;

        let mut all: Vec<(f32, Event, u32)> = Vec::new();

        for r in &readers {
            if !r
                .manifest
                .may_match(q.tenant.as_deref(), q.time_from, q.time_to)
            {
                continue;
            }
            let g = self.catalog().get_generation(&r.manifest.generation_id)?;
            let embedder = self.plane.embedder(&g.model_id, &g.model_version, dim)?;
            let qv = embedder.embed(&q.text)?;

            let rows = r.read_all()?;
            for i in 0..rows.events.len() {
                let e = &rows.events[i];
                if let Some(t) = &q.tenant {
                    if &e.tenant_id != t {
                        continue;
                    }
                }
                if let Some(f) = q.time_from {
                    if e.event_time < f {
                        continue;
                    }
                }
                if let Some(t) = q.time_to {
                    if e.event_time > t {
                        continue;
                    }
                }
                if let Some(p) = &q.predicate {
                    let view = crate::rowsource::EventRow {
                        event: e,
                        score: 0.0,
                    };
                    if !prism_types::predicate::eval(p, &view, 0)? {
                        continue;
                    }
                }
                let v = &rows.vectors[i * dim..(i + 1) * dim];
                all.push((dot(&qv, v), e.clone(), rows.centroids[i]));
            }
        }

        all.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.event_id.cmp(&b.1.event_id)));
        Ok(all
            .into_iter()
            .take(q.k)
            .map(|(score, event, centroid)| Hit {
                event,
                score,
                centroid,
            })
            .collect())
    }
}

/// Cluster the rerank survivors by meaning and summarize each group.
///
/// This is the shape of the flagship aggregate at S0 scale: groups, per-group
/// scalar aggregates, and — the part that makes it a *product* rather than a
/// number — an exemplar that is a real event someone can go read. Doing this
/// over an arbitrarily large *filtered set* rather than over the survivors of a
/// top-k is S9; the surface is the same, the execution is not.
fn group(
    scored: &[Scored],
    events: &BTreeMap<(usize, usize), Event>,
    group_k: usize,
    dim: usize,
    seed: u64,
) -> Result<Vec<ClusterSummary>> {
    let n = scored.len();
    let k = group_k.min(n);

    let mut flat: Vec<f32> = Vec::with_capacity(n * dim);
    for s in scored {
        flat.extend_from_slice(&s.vector);
    }

    let centroids = prism_quantizer::kmeans(&flat, n, dim, k, 25, seed)?;

    let mut members: Vec<Vec<usize>> = vec![Vec::new(); k];
    for i in 0..n {
        let v = &flat[i * dim..(i + 1) * dim];
        let mut best = 0usize;
        let mut best_d = f32::INFINITY;
        for c in 0..k {
            let d = l2_sq(v, &centroids[c * dim..(c + 1) * dim]);
            if d < best_d {
                best_d = d;
                best = c;
            }
        }
        members[best].push(i);
    }

    let mut out = Vec::new();
    for (cid, idxs) in members.iter().enumerate() {
        if idxs.is_empty() {
            continue;
        }
        let centroid = &centroids[cid * dim..(cid + 1) * dim];

        // The exemplar is the most central *actual event*, not the centroid.
        // A centroid is an average; nobody can read an average.
        let mut best = idxs[0];
        let mut best_d = f32::INFINITY;
        for &i in idxs {
            let d = l2_sq(&flat[i * dim..(i + 1) * dim], centroid);
            if d < best_d {
                best_d = d;
                best = i;
            }
        }

        let evs: Vec<&Event> = idxs
            .iter()
            .map(|&i| {
                let s = &scored[i];
                events
                    .get(&(s.part, s.row))
                    .expect("every survivor event was materialized")
            })
            .collect();

        let count = evs.len();
        let avg_cost = evs.iter().map(|e| e.cost).sum::<f64>() / count as f64;
        let error_rate = evs.iter().filter(|e| e.error).count() as f64 / count as f64;
        let exemplar = {
            let s = &scored[best];
            events.get(&(s.part, s.row)).unwrap().clone()
        };
        let mut member_ids: Vec<String> = evs.iter().map(|e| e.event_id.clone()).collect();
        member_ids.sort();

        out.push(ClusterSummary {
            cluster_id: cid,
            count,
            avg_cost,
            error_rate,
            exemplar,
            member_ids,
        });
    }

    // Biggest motif first: that is the order a human wants to read it in.
    out.sort_by(|a, b| b.count.cmp(&a.count).then(a.cluster_id.cmp(&b.cluster_id)));
    Ok(out)
}

/// Recall@k of an approximate result against the exact oracle.
pub fn recall_at_k(approx: &[Hit], exact: &[Hit], k: usize) -> f32 {
    if exact.is_empty() {
        return 1.0;
    }
    let truth: BTreeSet<&str> = exact
        .iter()
        .take(k)
        .map(|h| h.event.event_id.as_str())
        .collect();
    let found = approx
        .iter()
        .take(k)
        .filter(|h| truth.contains(h.event.event_id.as_str()))
        .count();
    found as f32 / truth.len() as f32
}

/// Convert a squared-L2 distance to the cosine it corresponds to. Only used for
/// reporting; ranking always happens on the distance itself.
pub fn as_cosine(l2: f32) -> f32 {
    cosine_from_l2_sq(l2)
}
