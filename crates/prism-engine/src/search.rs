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
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

/// A row that survived the scalar mask and got an approximate distance.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    dist: f32,
    /// Carried into the heap, and it has to be. See the `Ord` impl.
    event_id: String,
    part: usize,
    row: usize,
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // A max-heap on distance: the worst candidate is on top, so a bounded heap evicts in
        // O(log n) and the scan never allocates past the budget.
        //
        // **Distance ties break on `event_id`, not on physical position.** This used to break
        // on `(part, row)`, with a comment calling that "deterministic" — and it is, in the
        // sense that it reproduces on *this* store. It is not a function of the *data*, which
        // is what D-008 established and what the query contract requires: `(score DESC,
        // event_id ASC)`, always.
        //
        // The heap is bounded, so it does not merely *order* the answer — it decides which
        // tied rows are *allowed to be* answers at all. Real telemetry repeats bodies
        // verbatim, so identical vectors, identical codes and exactly-equal distances are
        // common, and a top-k is routinely a choice among hundreds of tied rows. Breaking that
        // choice on layout means two stores holding identical rows answer the same query
        // differently, and a merge changes an unchanged answer.
        //
        // S4 proved it the expensive way: repartitioning by time window (same rows, same
        // codebook, same 3,880 rows scanned, same tied distance) changed which tied rows
        // survived the heap, and recall against the exact oracle fell from 1.00 to 0.60 —
        // while raising `nprobe` did nothing at all, because the rows were never being
        // *missed*, they were being *outvoted by their addresses*. See D-033.
        self.dist
            .total_cmp(&other.dist)
            .then(self.event_id.cmp(&other.event_id))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

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
}

impl Engine {
    /// Search the live snapshot.
    pub fn search(&self, q: &Query) -> Result<SearchResult> {
        let snap = self.snapshot()?;
        self.search_at(&snap, q)
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
                    return Err(PrismError::Invariant(format!(
                        "eligible parts span {} embedding spaces ({:?}). Scores from \
                         different embedding spaces are not comparable, so this query \
                         will not merge them. Name one with `--space <model:version>`, \
                         or finish the re-embed migration so a single space remains.",
                        spaces.len(),
                        spaces
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
            let ranked: Vec<u32> = g.coarse.rank(&qv).into_iter().map(|(id, _)| id).collect();
            c.centroids_scored += ranked.len();
            ctxs.insert(
                gid.clone(),
                SpaceContext {
                    query_vector: qv,
                    adc,
                    ranked,
                },
            );
        }

        // --- 4. scan the selected centroid ranges ---
        let mut heap: BinaryHeap<Candidate> = BinaryHeap::new();

        for (pi, r) in eligible.iter().enumerate() {
            c.parts_opened += 1;
            let ctx = ctxs.get(&r.manifest.generation_id).ok_or_else(|| {
                PrismError::Corrupt("part references an absent generation".into())
            })?;

            // The scalar columns the mask needs. Text is never touched here —
            // only survivors pay for their body.
            let scalars = r.read_scalars()?;
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

            let probes = ctx.ranked.iter().take(q.nprobe);
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
                for i in 0..range.row_count {
                    let row = range.first_row + i;

                    // Fused scalar mask. Allocation-free: it runs once per
                    // scanned row, so it must not be the expensive part of the
                    // scan it exists to make cheaper.
                    if let Some(t) = &q.tenant {
                        if !scalars.tenant_is(row, t) {
                            continue;
                        }
                    }
                    if let Some(f) = q.time_from {
                        if times[row] < f {
                            continue;
                        }
                    }
                    if let Some(t) = q.time_to {
                        if times[row] > t {
                            continue;
                        }
                    }
                    // The general predicate, fused into the same loop. Evaluated last,
                    // because tenant and time are cheap and prune whole parts.
                    if let (Some(p), Some(view)) = (&q.predicate, &rows_view) {
                        if !prism_types::predicate::eval(p, view, row)? {
                            continue;
                        }
                    }
                    c.rows_passing_filter += 1;

                    let code = &codes[i * m..(i + 1) * m];
                    let dist = ctx.adc.distance(code);

                    // Bounded candidate width: the heap never grows past it.
                    //
                    // The distance is checked *before* the event id is materialized. A
                    // candidate that loses on distance alone — the overwhelming majority —
                    // never allocates. Only one that could actually enter the heap pays for
                    // its id, so the cost of an ordering that is a function of the data is
                    // bounded by the heap, not by the scan.
                    let enters = match heap.peek() {
                        _ if heap.len() < q.candidates => true,
                        Some(worst) => match dist.total_cmp(&worst.dist) {
                            std::cmp::Ordering::Less => true,
                            std::cmp::Ordering::Greater => false,
                            std::cmp::Ordering::Equal => {
                                scalars.event_id_at(row) < worst.event_id.as_str()
                            }
                        },
                        None => true,
                    };
                    if !enters {
                        continue;
                    }

                    let cand = Candidate {
                        dist,
                        event_id: scalars.event_id_at(row).to_string(),
                        part: pi,
                        row,
                    };
                    if heap.len() >= q.candidates {
                        heap.pop();
                    }
                    heap.push(cand);
                }
            }
        }

        c.candidates_considered = heap.len();

        // --- 5. exact rerank, within the declared fetch budget ---
        let mut candidates: Vec<Candidate> = heap.into_sorted_vec(); // nearest first
        candidates.truncate(q.rerank);
        c.rerank_width = candidates.len();

        let mut by_part: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for cand in &candidates {
            by_part.entry(cand.part).or_default().push(cand.row);
        }

        let mut scored: Vec<Scored> = Vec::with_capacity(candidates.len());
        for (pi, rows) in &by_part {
            let r = eligible[*pi];
            let ctx = ctxs.get(&r.manifest.generation_id).unwrap();
            let vectors = r.read_vectors_for_rows(rows)?;
            let ids = r.read_event_ids_for_rows(rows)?;
            c.exact_vectors_fetched += vectors.len();
            c.exact_bytes_fetched += vectors.len() * dim * 4;

            for ((row, v), event_id) in rows.iter().zip(vectors).zip(ids) {
                // Exact cosine on the stored float32 vector. This is the answer;
                // the PQ distance was only ever a way to avoid computing this
                // for everything.
                let score = dot(&ctx.query_vector, &v);
                scored.push(Scored {
                    score,
                    part: *pi,
                    row: *row,
                    vector: v,
                    event_id,
                });
            }
        }

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
        let hits: Vec<Hit> = scored
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

        // --- 7. semantic grouping of the rerank survivors ---
        let clusters = match q.group_k {
            Some(gk) if gk > 0 && !scored.is_empty() => {
                Some(group(&scored, &events, gk, dim, self.store.config.seed)?)
            }
            _ => None,
        };

        // What the disk actually moved, as opposed to what the plan asked for.
        c.physical_bytes_read = eligible.iter().map(|r| r.io_bytes()).sum();

        Ok(SearchResult {
            hits,
            clusters,
            counters: c,
            generations: gen_ids.into_iter().collect(),
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
