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
/// **Derived, not chosen.** It is the smallest `nprobe` whose *p1* recall@10
/// clears 0.8 on the golden corpus at the reference configuration, and the
/// receipt is `testing/evidence/nprobe.json`. A test asserts this
/// constant still equals the `chosen_nprobe` in that file, so the default cannot
/// drift away from the evidence that produced it.
///
/// It is picked on the **tail**, not the mean. S0 defaulted to a round number and
/// reported a mean recall of ~0.90 at `nprobe=1` — while the *minimum* was 0.000,
/// because cluster-boundary queries have their true neighbours split across two
/// centroids and a single probe reaches only one of them. A default tuned on a
/// mean is a default that works until it matters.
///
/// The sweep that produced it is unambiguous. At `nprobe=1`, *topic* queries —
/// aimed at the middle of a cluster — score a flawless mean recall of 1.000,
/// while *cluster-boundary* queries fail outright on 5 of 56. The mean across
/// everything is still 0.904, which is how a system with total failures in it
/// gets described as "90% accurate". `nprobe=4` is the first probe count at
/// which no query returns nothing, and it costs a 14.9% scan fraction.
///
/// This number is not universal. A different `nlist`, a different embedding
/// model, or a different corpus will have a different answer — re-derive it with
/// `prism golden sweep`. Adaptive per-query probing is issue #1, targeted at S6.
pub const DEFAULT_NPROBE: usize = 6;

/// Adaptive-probing margin (S6, [issue #1](https://github.com/Bobcatsfan33/PrismDB/issues/1)).
///
/// **Tuned** (charter C-1), receipt `testing/evidence/adaptive.json`. When a query sits near a
/// cluster boundary — nearly equidistant to several centroids — its true neighbours are split
/// across those centroids, and probing only the base `nprobe` reaches some of them and misses the
/// rest ([`DEFAULT_NPROBE`]'s whole reason for existing). This margin says *how nearly equal*
/// counts as "on the boundary": a centroid beyond the base is also probed when its distance is
/// within `(1 + ADAPTIVE_MARGIN)` of the base's last probed centroid.
///
/// **v1 is MONOTONE ONLY.** Adaptive probing may add probes above the base; it may never subtract.
/// So recall can only improve, and every existing `nprobe`/width receipt remains valid as a
/// *floor*. The cost-reduction direction — *fewer* probes on easy queries — is deferred until the
/// real-embedding corpus exists ([issue #3](https://github.com/Bobcatsfan33/PrismDB/issues/3)),
/// because it tunes against cluster geometry the hash-embedder corpus cannot represent.
pub const ADAPTIVE_MARGIN: f32 = 0.05;

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
/// The receipt is `testing/evidence/widths.json`.
pub const DEFAULT_CANDIDATES: usize = 50;

/// Default rerank width — the declared exact-vector fetch budget.
///
/// Derived jointly with [`DEFAULT_CANDIDATES`], and **the binding constraint is not
/// recall**: on the golden corpus every point in the grid clears the tail floors, because
/// PQ's top-10 already contains the true top-10. Left there, the sweep would have chosen
/// `rerank = 10` — the hard floor, since you cannot return ten hits from fewer than ten
/// reranked rows — and that would be overfitting to a synthetic corpus with unusually
/// well-separated motifs.
///
/// It would also quietly break pagination. **The paginated result set *is* the rerank
/// survivor set** (`docs/QUERY-CONTRACT.md` §4), so a rerank width of 10 with a page size
/// of 10 makes the first page the entire result and the cursor decorative. So the
/// derivation carries a *policy* bound — `MIN_PAGEABLE_ROWS = 50`, five pages at the
/// default page size — and that bound is what actually selects the value.
///
/// Rerank is the expensive control: an exact vector is ~32x a coded row, so one rerank
/// fetch costs 32 rows of scanning. The candidate heap costs memory, not I/O.
pub const DEFAULT_RERANK: usize = 50;

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

    /// Force the rerank route (S7). `None` lets the cost model decide — which, with the GPU off,
    /// is always CPU. The selection-identity and route-flip gates set this to prove the route is
    /// invisible to the answer. A stringly-typed pass-through so `prism-types` need not depend on
    /// the engine's `Route` enum; the engine parses it.
    #[serde(default)]
    pub force_route: Option<String>,
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
    pub snapshot_id: String,
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
