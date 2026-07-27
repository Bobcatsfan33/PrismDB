//! Queries, results, and the physical-execution counters.
//!
//! Part III §11 requires four *separate* controls — `k`, `nprobe`, candidate
//! width, rerank width — and requires that the physical consequences of a plan
//! be reportable rather than assumed. The counters below are the S0 ancestor of
//! `EXPLAIN`: pruning is a number you can assert on in a test, not a claim.

use crate::event::Event;
use serde::{Deserialize, Serialize};

/// The default probe count.
///
/// **Derived, not chosen** (charter C-1), and **geometry-SENSITIVE** — re-derived per real-embedding
/// corpus and generation, forever. It is the smallest `nprobe` whose *p1* recall@10 clears 0.8 on the
/// golden corpus at the reference configuration; the receipt is `testing/evidence/nprobe.json`. A test
/// asserts this constant still equals the shipped `chosen_nprobe`, so the default cannot drift from its
/// evidence.
///
/// It is picked on the **tail**, not the mean, because cluster-boundary queries have their true
/// neighbours split across two centroids and a single probe reaches only one — a mean cannot see the
/// query class that fails completely. A default tuned on a mean is one that works until it matters.
///
/// **S13 dir 2 — re-derived on real-v1 (all-mpnet 768d, nlist=64): the number is 11, up from the hash
/// corpus's 6.** This is the uncomfortable direction, and it is the honest one. The hash embedder's
/// handful of near-identical motifs could not produce *real* boundaries, so `nprobe=6` was measured
/// against geometry that flattered it. The first probe count whose p1 recall@10 clears 0.8 on real
/// geometry is **11**, and holding that tail costs a **~19% scan fraction — ~1.75× the 10.9% nprobe=6
/// bought** on the same corpus. That scan cost is the latency the recall contract is actually paid for;
/// it is the number S16's benchmarks stand on.
///
/// **Downstream of [`KMEANS_RESTARTS`](prism_quantizer::kmeans::KMEANS_RESTARTS)** (charter C-8): this
/// value was first derived at restarts=5 as 14, but the restarts re-derivation ([D-083](../../../docs/DECISIONS.md))
/// found restarts=5 to be a jagged sub-optimum and pinned the plateau at restarts=8 — whose better
/// codebook holds the tail at **11**, not 14. The 14 is retained as `config_superseded` in the receipt.
///
/// This number is not universal: a different `nlist`, embedder, corpus, or codebook re-derives it
/// (`tests/rederive_nprobe.rs`). The hash-corpus sweep is retained as the paired series.
pub const DEFAULT_NPROBE: usize = 11;

/// Adaptive-probing margin (S6, [issue #1](https://github.com/Bobcatsfan33/PrismDB/issues/1)).
///
/// **Tuned** (charter C-1), **geometry-SENSITIVE**, receipt `testing/evidence/adaptive.json`. When a
/// query sits near a cluster boundary — nearly equidistant to several centroids — its true neighbours
/// are split, and probing only the base `nprobe` misses half ([`DEFAULT_NPROBE`]'s whole reason for
/// existing). This margin says *how nearly equal* counts as "on the boundary": a centroid beyond the
/// base is also probed when its distance is within `(1 + ADAPTIVE_MARGIN)` of the base's last probed
/// centroid.
///
/// **v1 is MONOTONE ONLY.** Adaptive probing may add probes above the base; it may never subtract, so
/// recall can only improve and every `nprobe`/width receipt stays valid as a *floor*.
///
/// **S13 dir 2 — re-derived on real-v1, and the number fell from 0.05 to 0.02.** The hash corpus could
/// never recover its starved tail to the floor within the cost budget, so the margin was picked by a
/// cost ceiling (largest that helped at all); the real benefit-driven derivation was deferred to a
/// real-embedding corpus. real-v1 delivers it: a base *starved* one probe below the shipping default
/// (13) recovers from p1 recall 0.700 to 0.900 at margin **0.02**, so the smallest margin that recovers
/// the tail to the 0.8 floor is 0.02 — the benefit-first choice (`evidence::select_adaptive_margin`),
/// with the cost-ceiling rule retained as the fallback that keeps the hash series at 0.05. Note the
/// tightening interaction: at the shipping base (14) there are only 2 probes to [`ADAPTIVE_MAX_NPROBE`],
/// so the benefit signal comes entirely from the starved base.
pub const ADAPTIVE_MARGIN: f32 = 0.02;

/// The hard ceiling on adaptive probing. **Policy** (C-1): a query may never probe more than this
/// many centroids however tight its margins, so a pathological query cannot turn an approximate
/// scan into a full one. It bounds worst-case query cost, which measurement of *average* queries
/// cannot see.
pub const ADAPTIVE_MAX_NPROBE: usize = 16;

/// The effective probe count for a query whose ranked centroid distances (ascending) are
/// `dists`, given a base `nprobe`.
///
/// **Monotone: the result is always `>= base`.** It extends the base to include centroids nearly
/// as close as the base's boundary — the signature of a query sitting between clusters — and caps
/// at `max`. On a query deep inside one cluster the next centroids are much farther, the margin is
/// not met, and the result stays exactly at `base`. So easy queries pay nothing and only boundary
/// queries probe wider.
pub fn adaptive_nprobe(dists: &[f32], base: usize, margin: f32, max: usize) -> usize {
    let base = base.min(dists.len());
    if base == 0 {
        return 0;
    }
    let boundary = dists[base - 1];
    let cap = max.max(base).min(dists.len());
    let mut k = base;
    // ADC distances are squared L2, so non-negative; `boundary * (1+margin)` is well-defined.
    while k < cap && dists[k] <= boundary * (1.0 + margin) {
        k += 1;
    }
    k
}

/// Default candidate width: how many PQ-scored rows survive into the heap.
///
/// **Derived, not chosen** (charter C-1), and derived *jointly* with [`DEFAULT_RERANK`] —
/// the two interact, so an independent single-axis sweep of either measures a cross-section
/// of a surface and reports it as the surface. The candidate width decides *who is allowed
/// to be reranked*; the rerank width decides *how many of them actually are*. A rerank
/// budget of 200 buys nothing if only 50 candidates ever entered the heap.
///
/// **S13 dir 2 — re-derived on real-v1: recall is *flat* in candidates.** At a fixed rerank width,
/// candidate widths from 25 to 800 give identical p1 recall — because [`DEFAULT_NPROBE`] (14) already
/// delivers the true neighbours into even a 50-wide heap, so a wider heap buys nothing but memory and
/// I/O. So the candidate width is pinned to the rerank width (`= 50`, the minimum feasible since
/// `rerank <= candidates`). The receipt is `testing/evidence/widths.json` (geometry-sensitive).
pub const DEFAULT_CANDIDATES: usize = 50;

/// Default rerank width — the declared exact-vector fetch budget.
///
/// Derived jointly with [`DEFAULT_CANDIDATES`]. On the *hash* corpus the binding constraint was **not
/// recall**: every grid point cleared the tail floors (PQ's top-10 already held the true top-10), so
/// the sweep would have chosen `rerank = 10` — overfitting a synthetic corpus — were it not for a
/// *policy* bound. **The paginated result set *is* the rerank survivor set** (`docs/QUERY-CONTRACT.md`
/// §4), so a rerank of 10 at a page size of 10 makes the first page the whole result and the cursor
/// decorative; the derivation carries `MIN_PAGEABLE_ROWS = 50`.
///
/// **S13 dir 2 — on real-v1 the recall floor now BINDS, at exactly that policy floor.** rerank=25 gives
/// p1 recall 0.600 (fails the 0.8 floor), rerank=50 gives 0.900 (clears), and recall saturates there —
/// so the recall-only minimum rerank rose from 10 (hash) to **50**, coinciding with `MIN_PAGEABLE_ROWS`.
/// The value is unchanged but now **doubly justified**: recall *and* pagination both land on 50, where
/// the hash corpus had only policy holding it up. Rerank stays the expensive control (an exact vector
/// is ~32x a coded row); the candidate heap costs memory, not I/O.
pub const DEFAULT_RERANK: usize = 50;

/// The **overfetch margin** for threshold-query candidate bounding ([D-074](../../../docs/DECISIONS.md)).
///
/// A `similarity > τ` query bounds the candidate phase *by the threshold*, not a width: on unit
/// vectors `cos ≥ τ` ⇔ `l2² ≤ 2(1−τ)`, and the candidate phase, holding only the PQ approximation of
/// `l2²`, keeps rows with `PQ_dist ≤ 2(1−τ) + ε`. Rerank then applies the exact `τ`, so a false
/// positive costs an overfetch and a false negative is bounded by `ε`. `ε` is a **measured** p999 of
/// the PQ quantization error (`testing/evidence/pq-margin.json`) — the quantile IS the recall
/// contract. It is **geometry- and generation-sensitive** (a property of the vectors and the
/// codebook): **re-derived on the real-embedding corpus** (S13 dir 2, `real-v1`, all-mpnet 768d) to
/// `0.30`, up from `1e-6` on the hash golden corpus — **~5.5 orders of magnitude**. The hash corpus's
/// handful of motifs reconstructed near-exactly and hid the PQ error entirely; continuous 768d vectors
/// have a real error distribution (p999 ≈ 0.28), so the margin the recall contract actually needs is
/// this large. The most geometry-sensitive constant in the ledger.
pub const THRESHOLD_OVERFETCH_MARGIN_EPSILON: f32 = 0.30;

/// The **state budget** for a threshold query: the most candidates its relaxed bound may keep before
/// the query is **refused by name** rather than reranking without bound ([D-074](../../../docs/DECISIONS.md),
/// the [S9](../../../docs/DECISIONS.md) named-limit pattern). A low threshold over a broad filter can
/// make the qualifying set unbounded; that is a query to refuse, not to answer slowly or short.
pub const THRESHOLD_STATE_BUDGET: usize = 100_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Query {
    pub text: String,
    /// Scalar predicates. Tenant is separate from a general filter because in
    /// the real system it is injected below SQL by the authorization layer and
    /// is not removable by the caller (Part III §11).
    pub tenant: Option<String>,
    pub time_from: Option<i64>,
    pub time_to: Option<i64>,

    /// How many hits to return.
    pub k: usize,
    /// How many coarse centroids to probe.
    pub nprobe: usize,
    /// Candidate width: how many PQ-scored rows survive into the heap.
    pub candidates: usize,
    /// Rerank width: how many candidates get their exact vector fetched.
    /// This is the *declared fetch budget* — exact bytes never exceed it.
    pub rerank: usize,
    /// If set, cluster the rerank survivors into this many semantic groups.
    pub group_k: Option<usize>,

    /// A row predicate, evaluated in the fused scan mask.
    ///
    /// Lives in `prism-types`, not in the SQL crate, so the **direct API can build exactly
    /// what SQL compiles to**. Two filter languages that are supposed to agree is precisely
    /// the bug the "same door" rule exists to prevent.
    #[serde(default)]
    pub predicate: Option<crate::predicate::Predicate>,

    /// Which embedding space to search, as `model_id:model_version`.
    ///
    /// Only needed when a store holds parts from more than one space — mid
    /// re-embed migration, say. Scores from two embedding spaces are not
    /// comparable (invariant 9), so rather than silently merge them or silently
    /// drop one, the engine refuses and makes the caller name the space.
    pub space: Option<String>,

    /// Adaptive probing (S6). When `true` (the default), a boundary query may probe *above*
    /// `nprobe` up to [`ADAPTIVE_MAX_NPROBE`]; it never probes below `nprobe`, so recall can only
    /// improve. Set `false` to pin the flat behaviour — the receipts do this to measure the floor
    /// that adaptive probing sits on top of.
    #[serde(default = "default_true")]
    pub adaptive: bool,

    /// Override the adaptive margin. `None` uses [`ADAPTIVE_MARGIN`]; the sweep that derives that
    /// constant sets it, so the receipt measures the real mechanism rather than a copy of it.
    #[serde(default)]
    pub adaptive_margin: Option<f32>,

    /// Force the physical execution strategy (S8). `None` lets the optimizer choose on cost. The
    /// plan-invariance gate forces each strategy to prove they answer identically. Stringly-typed
    /// so `prism-types` need not depend on the engine's `Strategy` enum.
    #[serde(default)]
    pub plan: Option<String>,

    /// A similarity threshold: return only rows whose exact rerank score is at or above this
    /// (docs/QUERY-CONTRACT.md §12). Applied to the exact score, after rerank, before `k`. `None`
    /// is the S0 behaviour (top-k, no threshold).
    #[serde(default)]
    pub threshold: Option<f32>,

    /// Ask the engine to attach an [`Explain`] (S8, §14): the optimizer's estimates alongside the
    /// actuals. Off by default -- EXPLAIN is a diagnostic, not free.
    #[serde(default)]
    pub explain: bool,

    /// Force the rerank route (S7). `None` lets the cost model decide — which, with the GPU off,
    /// is always CPU. The selection-identity and route-flip gates set this to prove the route is
    /// invisible to the answer. A stringly-typed pass-through so `prism-types` need not depend on
    /// the engine's `Route` enum; the engine parses it.
    #[serde(default)]
    pub force_route: Option<String>,

    /// The cold-tier **fetch budget**, in bytes (S11, [storage contract §6](../../../docs/STORAGE-CONTRACT.md)).
    /// `None` = unbounded (fetch every rerank survivor's exact vector, the pre-S11 behaviour). When
    /// set, execution fetches at most this many bytes of exact vectors — reranking the most-promising
    /// candidates that fit and flagging the result `fetch_budget_exhausted` — rather than fetching
    /// unbounded. A plan declares it; execution is bounded by it.
    #[serde(default)]
    pub fetch_budget_bytes: Option<usize>,

    /// **Opt in to a best-effort partial answer** when a shard is unreachable ([query §21](../../../docs/QUERY-CONTRACT.md)).
    /// `false` (the default, and the only safe default) means a distributed query that cannot reach a
    /// shard it needs **fails, with the shard named** — never a silently short result. `true` asks the
    /// coordinator to answer from the shards it *can* reach and label what it dropped in
    /// [`SearchResult::missing_shards`]. This is per-query and never a global or config default: a
    /// silently partial answer to a security or novelty question is the worst failure this product can
    /// produce, so receiving a partial answer is impossible without having asked for one right here.
    #[serde(default)]
    pub best_effort: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Query {
    fn default() -> Self {
        Query {
            text: String::new(),
            tenant: None,
            time_from: None,
            time_to: None,
            k: 10,
            nprobe: DEFAULT_NPROBE,
            candidates: DEFAULT_CANDIDATES,
            rerank: DEFAULT_RERANK,
            group_k: None,
            predicate: None,
            space: None,
            adaptive: true,
            adaptive_margin: None,
            force_route: None,
            plan: None,
            threshold: None,
            explain: false,
            fetch_budget_bytes: None,
            best_effort: false,
        }
    }
}

/// What the query physically did. Every field is measured, none is estimated.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Counters {
    pub parts_total: usize,
    /// Parts eliminated by tenant / time / zone-map metadata alone.
    pub parts_pruned: usize,
    /// Parts whose column files were actually opened.
    pub parts_opened: usize,
    pub centroids_scored: usize,
    /// Contiguous centroid ranges read (the unit of coalesced I/O).
    pub ranges_scanned: usize,
    pub rows_scanned_pq: usize,
    pub pq_bytes_scanned: usize,
    /// Rows that survived the scalar mask fused into the scan.
    pub rows_passing_filter: usize,
    pub candidates_considered: usize,
    pub rerank_width: usize,
    pub exact_bytes_fetched: usize,
    pub exact_vectors_fetched: usize,
    /// Rows in eligible parts. `rows_scanned_pq / rows_eligible` is the
    /// fraction of the pruned set the centroid index made us touch.
    pub rows_eligible: usize,

    /// **Bytes actually pulled off the disk**, as opposed to the logical bytes the
    /// plan asked for.
    ///
    /// The gap between this and `pq_bytes_scanned + exact_bytes_fetched` is the
    /// block layer's over-read: a 300-byte centroid range that lives inside a 64 KiB
    /// block costs 64 KiB, and no logical counter can see that. It is what the disk
    /// charges, and it is the number that decides the block size
    /// (`testing/evidence/block-size.json`).
    #[serde(default)]
    pub physical_bytes_read: usize,
    /// Which SIMD kernel scanned this query (S6). Observable so the determinism gate can name the
    /// path it exercised and the per-ISA baseline can attribute its numbers. The *answer* does not
    /// depend on this — that is the whole point of the determinism contract — but knowing which
    /// kernel produced it is how we prove that.
    #[serde(default)]
    pub scan_isa: String,
    /// Coarse centroids actually probed, summed across generations — the *effective* nprobe after
    /// adaptive widening (S6). Equal to the base `nprobe` when no query hit a boundary; larger
    /// when adaptive probing added centroids. `probes_widened` counts how many of those were the
    /// heuristic's doing.
    #[serde(default)]
    pub probes_taken: usize,
    #[serde(default)]
    pub probes_widened: usize,
    /// Which route reranked this query (S7): `cpu`, `gpu-reference`, or `cuda`. The route is
    /// invisible to the *answer* (selection-identity), but observable so a degradation is not
    /// silent.
    #[serde(default)]
    pub rerank_route: String,
    /// True if a device route degraded to CPU mid-query after a fault. A GPU that quietly stopped
    /// being used is a GPU you are paying for and not getting.
    #[serde(default)]
    pub route_degraded: bool,
    /// Which physical strategy scanned this query (S8): `interleaved`, `scalar-first`, or
    /// `semantic-first`. Observable so the plan is inspectable, but the *answer* does not depend
    /// on it -- that is plan-invariance (docs/QUERY-CONTRACT.md §9).
    #[serde(default)]
    pub plan: String,
    /// Distances actually computed. The strategy-varying cost: scalar-first computes a distance
    /// only for predicate survivors, so this is far below `rows_scanned_pq` when the predicate is
    /// selective; semantic-first and interleaved compute one per probed row.
    #[serde(default)]
    pub distances_computed: usize,
    /// Predicate evaluations actually run. Semantic-first evaluates the predicate only for rows
    /// near enough to enter the selection, so this is far below `rows_scanned_pq` when the
    /// distance already narrows hard.
    #[serde(default)]
    pub predicate_evals: usize,
    /// Cold-tier object requests this query issued (S11): one per part whose exact vectors were
    /// fetched, coalesced ranged reads within a part counting as one logical request. The two-tier
    /// bill, on every query (storage contract §6).
    #[serde(default)]
    pub object_requests: usize,
    /// True if the declared `fetch_budget_bytes` was exhausted mid-rerank, so only the
    /// most-promising candidates that fit the budget were reranked — a **named** degradation, not a
    /// silent over-fetch or under-answer (storage contract §6).
    #[serde(default)]
    pub fetch_budget_exhausted: bool,
    /// How many shards a **best-effort** distributed query dropped as unreachable ([query §21](../../../docs/QUERY-CONTRACT.md)).
    /// The count mirrors [`SearchResult::missing_shards`] into the observable counters, so a degraded
    /// answer is a monitored number, not just a field a caller might skip reading. `0` on a complete
    /// answer (and a fail-named query never returns — it errors).
    #[serde(default)]
    pub shards_missing: usize,
    /// How many fragments a distributed query **hedged** — re-issued to cut tail latency ([query §21](../../../docs/QUERY-CONTRACT.md),
    /// [D-079](../../../docs/DECISIONS.md)). A hedge changes latency, never the answer (the pinned
    /// snapshot makes a re-issued fragment byte-identical), so this is an observable, not a correctness
    /// flag. `0` on a query that hedged nothing.
    #[serde(default)]
    pub hedges_issued: usize,
    /// For a threshold query only: how many candidates the relaxed bound kept that landed **within ε
    /// of the exact bar** — the overfetch the margin bought ([D-074](../../../docs/DECISIONS.md)).
    /// Rerank prunes them against the exact `τ`, so they never reach the answer; the count is the
    /// **observable** that keeps margin adequacy a monitored number rather than a hope.
    #[serde(default)]
    pub threshold_overfetch: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hit {
    pub event: Event,
    /// Cosine similarity from the exact vector. Approximate PQ distances never
    /// reach the surface — they only decide who gets reranked.
    pub score: f32,
    pub centroid: u32,
}

/// One semantic group: the shape of the flagship aggregate, at S0 scale.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterSummary {
    pub cluster_id: usize,
    pub count: usize,
    pub avg_cost: f64,
    pub error_rate: f64,
    /// The most central *actual event* in the group. Legibility is the product.
    pub exemplar: Event,
    pub member_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchResult {
    pub hits: Vec<Hit>,
    pub clusters: Option<Vec<ClusterSummary>>,
    pub counters: Counters,
    /// Which embedding space these scores live in. Scores from different
    /// generations are never compared without a bridge (invariant 9), so the
    /// generation is part of the result, not metadata about it.
    pub generations: Vec<String>,
    /// Set when this answer crossed an embedding-space boundary through a **declared bridge**
    /// (generation contract §6). Names the policy that produced it.
    ///
    /// A bridged answer must never be mistakable for a native one. The two are not the same kind
    /// of thing: a native score is a cosine in one geometry, and a bridged result is a *fusion of
    /// ranks* from two geometries that were never comparable. Silence about that would make the
    /// output a lie by omission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<String>,
    /// The optimizer's estimates alongside the query's actuals (S8, docs/QUERY-CONTRACT.md §14).
    /// Present when the caller asked to EXPLAIN. An optimizer that cannot say *why* it chose a plan
    /// is one nobody can debug, so the reason is carried, not just the choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explain: Option<Explain>,
    /// The shards a **best-effort** distributed query could not reach, each with the reason
    /// ([query §21](../../../docs/QUERY-CONTRACT.md)). Empty on a complete answer. A non-empty report
    /// is only ever produced for a query that set [`Query::best_effort`] — a fail-named query errors
    /// instead — so a partial answer is always *labelled* a partial answer, impossible to mistake for
    /// a complete one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_shards: Vec<MissingShard>,
    pub snapshot_id: String,
}

/// A shard a best-effort query dropped, and why ([query §21](../../../docs/QUERY-CONTRACT.md)).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MissingShard {
    pub shard: usize,
    pub reason: String,
}

/// What the optimizer estimated, and what actually happened (S8). Every control carries both, so
/// cost-model drift is a visible number (the calibration harness), not a slow surprise.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Explain {
    pub chosen_plan: String,
    pub plan_reason: String,
    pub chosen_route: String,
    /// The optimizer's selectivity estimate for the predicate, and the actual fraction of scanned
    /// rows that passed. Their gap is what the calibration harness tracks.
    pub estimated_selectivity: f64,
    pub actual_selectivity: f64,
    /// Estimate and actual for the four controls, and for the physical work.
    pub estimated_nprobe: usize,
    pub actual_nprobe: usize,
    pub actual_candidates: usize,
    pub actual_rerank: usize,
    pub actual_k: usize,
    pub actual_parts_opened: usize,
    pub actual_ranges_scanned: usize,
    pub actual_bytes_read: usize,
    /// The cold-tier economics (S11, storage contract §6): object requests issued, exact-vector
    /// bytes retrieved, and an estimated per-query cost in micro-units (`object_requests ×
    /// request-cost + retrieved_bytes × byte-cost`). Plus the declared fetch budget and whether it
    /// was exhausted, so the two-tier bill and its bound are both on the query.
    #[serde(default)]
    pub object_requests: usize,
    #[serde(default)]
    pub retrieved_bytes: usize,
    #[serde(default)]
    pub estimated_cost_micros: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_fetch_budget_bytes: Option<usize>,
    #[serde(default)]
    pub fetch_budget_exhausted: bool,
}

#[cfg(test)]
mod adaptive_tests {
    use super::*;

    #[test]
    fn adaptive_is_monotone_never_below_the_base() {
        // Whatever the distances and margin, the result is >= base. This is the v1 guarantee that
        // keeps every existing receipt valid as a floor (issue #1).
        let dists = [0.1f32, 0.11, 0.5, 0.9, 1.3, 2.0, 2.1, 2.2];
        for base in 1..=6 {
            for margin in [0.0f32, 0.05, 0.15, 0.5, 5.0] {
                let k = adaptive_nprobe(&dists, base, margin, ADAPTIVE_MAX_NPROBE);
                assert!(
                    k >= base.min(dists.len()),
                    "adaptive dropped below the base"
                );
                assert!(k <= dists.len());
            }
        }
    }

    #[test]
    fn a_boundary_query_probes_wider_and_an_easy_one_does_not() {
        let base = 2;
        // Boundary: the 3rd and 4th centroids are nearly as close as the 2nd -- neighbours are
        // split across them, so we must reach them.
        let boundary = [1.00f32, 1.02, 1.05, 1.08, 9.0, 9.1];
        assert!(
            adaptive_nprobe(&boundary, base, 0.15, 8) > base,
            "a boundary query did not widen its probe count"
        );
        // Easy: a sharp cliff after the base -- the next centroid is 9x farther, nothing to gain.
        let easy = [1.00f32, 1.02, 9.0, 9.1, 9.2, 9.3];
        assert_eq!(
            adaptive_nprobe(&easy, base, 0.15, 8),
            base,
            "an easy query wasted probes it did not need"
        );
    }

    #[test]
    fn the_cap_is_never_exceeded() {
        // Everything nearly tied: without a cap this would probe all 20. The cap holds.
        let dists: Vec<f32> = (0..20).map(|i| 1.0 + i as f32 * 0.001).collect();
        assert_eq!(adaptive_nprobe(&dists, 2, 1.0, 5), 5);
    }
}
