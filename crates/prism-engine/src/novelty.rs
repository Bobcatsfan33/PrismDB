//! `NOVELTY ... AGAINST` and `SEMANTIC_DIFF` (S9 primitives) — distance from known structure.
//!
//! Both are cheap because they reuse structures that already exist: novelty is the S5 drift
//! [`Baseline`](prism_part::baseline::Baseline) (a cluster summary of "normal"), and semantic-diff
//! is the S9 [`semantic_cluster`](crate::cluster) aggregate asked a comparative question. No exotic
//! ML — "how far from what we have seen before" is a distance, and it is explainable, which is the
//! product.
//!
//! The one rule both obey absolutely is **invariant 9**: a distance is only a distance *within one
//! embedding space*. A `NOVELTY` that scores rows in one space against a baseline built in another
//! is not a smaller number or a bigger number — it is a meaningless number, and it is refused with
//! the same teaching error a cross-space ranking gets ([query contract §18](../../../docs/QUERY-CONTRACT.md)).

use crate::cluster::ClusterRequest;
use crate::drift::{quantile_fraction, DRIFT_FIRE_MULTIPLE};
use crate::engine::Engine;
use prism_types::error::{PrismError, Result};
use prism_types::{ClusterSummary, Event};

/// One row's novelty against a baseline: how far it is from the nearest "normal" centroid, and
/// whether that clears the baseline's calibrated threshold.
#[derive(Clone, Debug)]
pub struct NoveltyRow {
    pub event_id: String,
    pub novelty: f32,
    pub is_novel: bool,
}

/// The result of `NOVELTY ... AGAINST`: per-row scores plus the fired/threshold summary.
#[derive(Clone, Debug)]
pub struct NoveltyScan {
    pub rows: Vec<NoveltyRow>,
    pub baseline_id: String,
    pub generation_id: String,
    pub threshold: f32,
    pub novel_fraction: f64,
    /// True when the novel fraction is several times the ~1% the baseline calibrated as normal —
    /// not noise, a different distribution ([`DRIFT_FIRE_MULTIPLE`]).
    pub fired: bool,
}

impl Engine {
    /// Score a filtered set against a **named** baseline. The baseline's generation must be the
    /// same embedding space as the rows, or the call is the invariant-9 error (query contract §18).
    pub fn novelty_against(&self, req: &ClusterRequest, baseline_id: &str) -> Result<NoveltyScan> {
        let dim = self.store.config.dim;
        let (rows, space) = self.gather_filtered(req, dim)?;
        let b = self.catalog().get_baseline(baseline_id)?;
        let baseline_space = self.catalog().get_generation(&b.generation_id)?.space();
        if baseline_space != space {
            return Err(PrismError::Invariant(format!(
                "this NOVELTY compares rows in {space} against a baseline built in \
                 {baseline_space} — a distance between two embedding spaces is not a distance (a \
                 cosine of 0.8 in one is not a cosine of 0.8 in the other). PrismDB will not \
                 compute it. Either name a baseline in {space}, or rebuild the baseline in this \
                 generation (`prism baseline build`)."
            )));
        }

        let mut out = Vec::with_capacity(rows.len());
        let mut novel = 0usize;
        for r in &rows {
            let nov = b.novelty(&r.vector)?;
            let is_novel = nov > b.threshold;
            if is_novel {
                novel += 1;
            }
            out.push(NoveltyRow {
                event_id: r.event.event_id.clone(),
                novelty: nov,
                is_novel,
            });
        }
        let novel_fraction = if rows.is_empty() {
            0.0
        } else {
            novel as f64 / rows.len() as f64
        };
        Ok(NoveltyScan {
            rows: out,
            baseline_id: b.baseline_id.clone(),
            generation_id: b.generation_id.clone(),
            threshold: b.threshold,
            novel_fraction,
            fired: novel_fraction > DRIFT_FIRE_MULTIPLE as f64 * quantile_fraction(),
        })
    }

    /// `SEMANTIC_DIFF(a, b, k)` — the clusters with mass in *b* and **none** in *a*: behaviour that
    /// exists in the second window and existed nowhere in the first. Built on `semantic_cluster`:
    /// cluster *b* into `k` groups, then assign every row of *a* to the nearest *b*-centroid; a
    /// cluster no *a*-row lands in is new. Deterministic, because the clustering is (C-7), and
    /// same-space, because a cross-space assignment is the invariant-9 error.
    pub fn semantic_diff(
        &self,
        a: &ClusterRequest,
        b: &ClusterRequest,
        k: usize,
    ) -> Result<Vec<ClusterSummary>> {
        let dim = self.store.config.dim;

        let mut breq = b.clone();
        breq.k = k;
        breq.with_assignments = false;
        let rb = self.semantic_cluster(&breq)?;
        if rb.clusters.is_empty() {
            return Ok(Vec::new());
        }

        let (arows, aspace) = self.gather_filtered(a, dim)?;
        if aspace != rb.space && !arows.is_empty() {
            return Err(PrismError::Invariant(format!(
                "SEMANTIC_DIFF compares two windows across embedding spaces — {aspace} and {} — \
                 whose distances are not comparable (invariant 9). PrismDB will not diff them. \
                 Name one space, or finish the re-embed migration.",
                rb.space
            )));
        }

        let n_clusters = rb.clusters.len();
        let mut a_count = vec![0usize; n_clusters];
        for r in &arows {
            let (c, _) =
                prism_quantizer::nearest_centroid(&r.vector, &rb.centroids, n_clusters, dim);
            a_count[c] += 1;
        }

        // A cluster is novel-in-b iff no row of a lands in it. Order is already the contract order
        // (§16); it is preserved by the filter.
        Ok(rb
            .clusters
            .into_iter()
            .filter(|c| a_count[c.cluster_id] == 0)
            .collect())
    }
}

/// The exemplar `event_id`s of the novel clusters — the durable, comparable output (§15).
pub fn novel_exemplars(diff: &[ClusterSummary]) -> Vec<&Event> {
    diff.iter().map(|c| &c.exemplar).collect()
}
