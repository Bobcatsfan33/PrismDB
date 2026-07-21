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
struct Scored {
    score: f32,
    part: usize,
    row: usize,
    vector: Vec<f32>,
    event_id: String,
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

    /// Search a **specific** snapshot.
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
        //
        // Different codebook generations within the same embedding space are
        // fine: each gets its own table, and they merge at exact-score time,
        // where both agree on what a vector means.
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
        self.rerank_phase(
            snap,
            q,
            &eligible,
            &ctxs,
            &gen_ids,
            &plan_choice,
            candidates,
            c,
        )
    }

    /// The **rerank phase**: exact-score a bounded PQ candidate set, apply the fetch budget and the
    /// similarity threshold, take the top-`k` with the C-4 `event_id` tie-break, materialize, and
    /// group. Factored out of [`search_at`](Self::search_at) so a distributed query reranks a
    /// *global* candidate set with the **same code** a single node runs on a local one — one
    /// implementation, no divergence ([query §20](../../../docs/QUERY-CONTRACT.md)). Single-store
    /// search is the degenerate one-shard case: `search_at` = candidate phase then this.
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

        // --- 5. exact rerank, within the declared fetch budget ---
        let mut candidates = candidates;
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
            generations: gen_ids.iter().cloned().collect(),
            bridge: None,
            explain,
            snapshot_id: snap.snapshot_id.clone(),
        })
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
