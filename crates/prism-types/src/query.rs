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
/// receipt is `testing/golden/nprobe-provenance.json`. A test asserts this
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
pub const DEFAULT_NPROBE: usize = 4;

/// Default candidate width: how many PQ-scored rows survive into the heap.
pub const DEFAULT_CANDIDATES: usize = 200;

/// Default rerank width — the declared exact-vector fetch budget.
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

    /// Which embedding space to search, as `model_id:model_version`.
    ///
    /// Only needed when a store holds parts from more than one space — mid
    /// re-embed migration, say. Scores from two embedding spaces are not
    /// comparable (invariant 9), so rather than silently merge them or silently
    /// drop one, the engine refuses and makes the caller name the space.
    pub space: Option<String>,
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
            space: None,
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
    pub snapshot_id: String,
}
