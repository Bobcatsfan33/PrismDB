//! Queries, results, and the physical-execution counters.
//!
//! Part III §11 requires four *separate* controls — `k`, `nprobe`, candidate
//! width, rerank width — and requires that the physical consequences of a plan
//! be reportable rather than assumed. The counters below are the S0 ancestor of
//! `EXPLAIN`: pruning is a number you can assert on in a test, not a claim.

use crate::event::Event;
use serde::{Deserialize, Serialize};

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
            nprobe: 4,
            candidates: 200,
            rerank: 50,
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
