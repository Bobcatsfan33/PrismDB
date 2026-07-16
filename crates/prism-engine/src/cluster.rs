//! `GROUP BY semantic_cluster(embedding, k)` (S9) — grouping an arbitrarily large *filtered set*
//! by meaning, deterministically.
//!
//! This is the flagship aggregate. Unlike `search.rs::group`, which clusters the survivors of a
//! top-k, this clusters *every row a predicate admits* — which is why it must be bounded before it
//! runs ([query contract §17](../../../docs/QUERY-CONTRACT.md)) and why its determinism is a
//! charter amendment of its own ([C-7](../../../docs/DECISIONS.md), [determinism contract
//! §13–§15](../../../docs/DETERMINISM-CONTRACT.md)):
//!
//! - the PRNG is seeded from **content** — `SHA-256(sorted event_ids ‖ k ‖ generation)` — never a
//!   clock, so the clusters are a function of the data and not of *when* the query ran;
//! - rows are consumed in **logical (`event_id`) order**, never scan order, so the clusters are a
//!   function of the data and not of *which part or route* fetched a row;
//! - the per-cluster aggregates are the **merge of partial states in canonical (shard-id) order**,
//!   so a distributed answer will equal this single-node one exactly (§14);
//! - exemplars are a **C-4 bounded selection on the exact score** (§15): most-central real event,
//!   ties on `event_id`.
//!
//! The clustering itself reuses [`prism_quantizer::kmeans_minibatch`]; everything here is the
//! determinism, the bounding, and the mergeable state that wrap it.

use crate::engine::Engine;
use crate::rowsource::EventRow;
use prism_part::part::PartReader;
use prism_types::error::{PrismError, Result};
use prism_types::hash::content_id;
use prism_types::predicate::{self, Predicate};
use prism_types::vector::l2_sq;
use prism_types::{ClusterSummary, Event};

// --- policy bounds (C-3), all in the C-1 registry --------------------------------------------

/// The largest `k` a `semantic_cluster` may ask for.
///
/// **Policy** ([C-3](../../../docs/DECISIONS.md)): a cluster result is something a human *reads* —
/// the exemplars are the product. Past a couple hundred groups it stops being an explanation and
/// becomes a second dataset, and the legibility that justifies the whole feature is gone. 256 is
/// the declared ceiling: a query asking for more is refused with a named limit, never silently
/// clamped, because clamping answers a different question than the one asked. Measurement cannot
/// pick this — it is a statement about what a person can read.
pub const MAX_SEMANTIC_K: usize = 256;

/// The clustering working-set budget, in bytes.
///
/// **Policy** ([C-3](../../../docs/DECISIONS.md)): a `semantic_cluster` over a billion-row filtered
/// set must **decline**, not OOM the node — the S2 lesson (bound the aggregate before it exists),
/// applied to clustering state. The working set is the filtered rows' vectors plus the `k`
/// centroids; a query whose `(rows, dim, k)` would exceed this is refused with a named limit
/// *before* the first vector is gathered. 512 MiB admits every corpus S9 clusters and refuses the
/// sets that would need the streaming PQ-code fit filed for the distributed sprint.
pub const SEMANTIC_STATE_BUDGET_BYTES: usize = 512 * 1024 * 1024;

/// Rows per mini-batch in the k-means fit.
///
/// **Policy** ([C-3](../../../docs/DECISIONS.md)): a throughput/quality knob, not an optimum. Large
/// enough that per-batch overhead is amortized, small enough that the running mean adapts within an
/// epoch. When it exceeds the row count every epoch is a full deterministic pass.
pub const SEMANTIC_MINIBATCH_SIZE: usize = 4096;

/// Mini-batch epochs (passes over the filtered set) in the fit.
///
/// **Policy** ([C-3](../../../docs/DECISIONS.md)), bound by measurement: the epoch count is chosen
/// to clear the `ARI ≥ 0.8` floor on the frozen labeled corpus with margin, and the gate test
/// `clustering_recovers_the_true_labels` **binds it** — drop it too low and the ARI floor fails, so
/// the committed test is the receipt. More epochs buy quality with linear cost; this is where the
/// curve has flattened on the corpus. Corpus-conditional.
pub const SEMANTIC_MINIBATCH_EPOCHS: usize = 15;

/// Independent seeded restarts of the fit, keeping the lowest-inertia result.
///
/// **Policy** ([C-3](../../../docs/DECISIONS.md)), bound by measurement: k-means++ seeds from a
/// single random draw, and a single draw is a lottery — the exact defect
/// [D-036](../../../docs/DECISIONS.md) killed once for the codebook. Restarts make the outcome a
/// function of the data, not of a lucky init, and the ARI gate binds the count (a single draw
/// scored 0.76 on the corpus; five clears the floor). Each restart's seed is `content_seed + r·φ`,
/// so the choice stays a deterministic function of the data (C-7). Corpus-conditional.
pub const SEMANTIC_MINIBATCH_RESTARTS: usize = 5;

/// The confidence floor below which a `semantic_cluster` result is reported **low-confidence**.
///
/// **Policy** ([C-3](../../../docs/DECISIONS.md)): a cluster quality of `1 − inertia_k / inertia_1`
/// near zero means the `k` clusters describe the data barely better than one blob does — i.e. there
/// is no structure, and the honest answer on uniform noise is "low confidence", asserted rather
/// than dressed up as `k` confident groups. 0.25 is the declared line between "real structure" and
/// "noise dressed as clusters": on the frozen corpus the no-structure shape scores ~0.13 and every
/// labeled shape ~0.38–0.44, so the line sits with margin on both sides. Corpus-conditional.
pub const CLUSTER_CONFIDENCE_MIN: f64 = 0.25;

/// How the filtered set is cut into logical shards for the mergeable partial states.
///
/// **Policy**: a shard is a contiguous run of `event_id`-sorted rows, so shard order *is* logical
/// order and the merge equals the single-pass fold exactly (§14). This is the cut size; it changes
/// how many partials there are, never the answer.
const SHARD_SIZE: usize = 8192;

// --- request / result -------------------------------------------------------------------------

/// A `semantic_cluster` over a filtered set. There is no query vector — the aggregate clusters the
/// rows a predicate admits, it does not search for the nearest to a point.
#[derive(Clone, Debug)]
pub struct ClusterRequest {
    pub tenant: String,
    pub predicate: Option<Predicate>,
    pub time_from: Option<i64>,
    pub time_to: Option<i64>,
    pub k: usize,
    /// The embedding space to cluster in (`model_id:model_version`); `None` = the active space.
    pub space: Option<String>,
    /// Emit a per-row `(event_id, cluster_id)` assignment in the result. Off by default — the
    /// filtered set can be huge and the aggregate's output is the exemplars and stats (§15), not a
    /// row-by-row labeling. The ARI gate turns it on to score against ground truth.
    pub with_assignments: bool,
}

impl ClusterRequest {
    /// A request for one tenant with no predicate and no time bound, in the active space.
    pub fn new(tenant: impl Into<String>, k: usize) -> Self {
        ClusterRequest {
            tenant: tenant.into(),
            predicate: None,
            time_from: None,
            time_to: None,
            k,
            space: None,
            with_assignments: false,
        }
    }
}

/// Whether the clustering found real structure. On uniform noise the honest answer is
/// `LowConfidence`, not `k` confident groups (query contract §17, no-structure corpus).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    High,
    Low,
}

#[derive(Clone, Debug)]
pub struct SemanticClusterResult {
    /// Groups in the contract order: `count DESC, exemplar.event_id ASC` (§16). `cluster_id` is the
    /// index into this order — query-scoped and ephemeral (§15).
    pub clusters: Vec<ClusterSummary>,
    pub confidence: Confidence,
    /// `1 − inertia_k / inertia_1` ∈ [0, 1]: how much better `k` clusters describe the data than
    /// one does. The number behind `confidence`.
    pub quality: f64,
    pub k_effective: usize,
    pub rows: usize,
    /// The space these clusters live in — a cluster is only meaningful within one geometry.
    pub space: String,
    /// Per-row `(event_id, cluster_id)`, in `event_id` order — populated only when the request
    /// asked for it. `cluster_id` is the ephemeral ordered id (§15/§16).
    pub assignments: Vec<(String, usize)>,
    /// The fitted centroids, `k_effective * dim` floats, indexed by the **ephemeral ordered**
    /// cluster id (§16) — so `centroids[cluster_id]` is the center of the group at that position.
    /// `SEMANTIC_DIFF` assigns another window's rows to these to find which clusters are novel.
    pub centroids: Vec<f32>,
    pub dim: usize,
}

// --- a gathered row: the exact vector, plus the scalars the aggregates need ------------------

pub(crate) struct Row {
    pub(crate) event: Event,
    pub(crate) vector: Vec<f32>,
}

// --- mergeable partial state (directive 1, determinism contract §14) --------------------------

/// The most-central candidate for a cluster's exemplar: min exact distance to the centroid, ties
/// on `event_id` (C-4). Mergeable: combining two candidates keeps the better one by the same rule,
/// so the winner is independent of merge order.
#[derive(Clone)]
struct ExemplarCandidate {
    dist: f32,
    event: Event,
}

impl ExemplarCandidate {
    /// True if `self` is a *worse* exemplar than `other` — larger distance, or equal distance and
    /// a larger `event_id`. The C-4 rule, so the merge is order-invariant.
    fn worse_than(&self, other: &ExemplarCandidate) -> bool {
        match self.dist.total_cmp(&other.dist) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => self.event.event_id > other.event.event_id,
        }
    }
}

#[derive(Clone, Default)]
struct PartialCluster {
    count: usize,
    cost_sum: f64,
    error_count: usize,
    exemplar: Option<ExemplarCandidate>,
}

/// A partial cluster-aggregate state for one logical shard. In S9 all shards are local; the type
/// exists so the distributed sprint (S12) merges exactly these, in exactly this canonical order.
struct Partial {
    /// Ascending shard id = ascending `event_id` range = canonical merge order.
    shard_id: usize,
    clusters: Vec<PartialCluster>,
}

impl Engine {
    /// Cluster a filtered set by meaning. The deterministic aggregate — see the module docs and
    /// [C-7](../../../docs/DECISIONS.md).
    pub fn semantic_cluster(&self, req: &ClusterRequest) -> Result<SemanticClusterResult> {
        let dim = self.store.config.dim;

        // --- 1. bound it before it exists (query contract §17) -----------------------------
        if req.k == 0 {
            return Err(PrismError::Invalid(
                "semantic_cluster needs k >= 1".to_string(),
            ));
        }
        if req.k > MAX_SEMANTIC_K {
            return Err(PrismError::Invalid(format!(
                "semantic_cluster asked for k = {} clusters, over the MAX_SEMANTIC_K limit of {} \
                 (a cluster result is read by a human; past this it stops being an explanation). \
                 Ask for fewer clusters.",
                req.k, MAX_SEMANTIC_K
            )));
        }

        // --- 2. gather the filtered set, in one space (invariant 9) -------------------------
        let (rows, space) = self.gather_filtered(req, dim)?;
        let n = rows.len();
        if n == 0 {
            return Ok(SemanticClusterResult {
                clusters: Vec::new(),
                confidence: Confidence::Low,
                quality: 0.0,
                k_effective: 0,
                rows: 0,
                space,
                assignments: Vec::new(),
                centroids: Vec::new(),
                dim,
            });
        }

        // The working set is admitted against the state budget *after* we know the row count but
        // the vectors are already gathered, so the budget check also caps what a single node holds.
        let working_bytes = n
            .saturating_mul(dim)
            .saturating_mul(4)
            .saturating_add(req.k.saturating_mul(dim).saturating_mul(4));
        if working_bytes > SEMANTIC_STATE_BUDGET_BYTES {
            return Err(PrismError::Invalid(format!(
                "semantic_cluster over {n} rows at dim {dim} would hold {working_bytes} bytes of \
                 clustering state, over the SEMANTIC_STATE_BUDGET_BYTES limit of {SEMANTIC_STATE_BUDGET_BYTES}. \
                 Narrow the filter, or wait for the streaming PQ-code fit (filed for the distributed \
                 sprint). PrismDB declines rather than OOM the node."
            )));
        }

        let k = req.k.min(n);

        // --- 3. logical order: sort by event_id (determinism contract §13) ------------------
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| rows[a].event.event_id.cmp(&rows[b].event.event_id));

        // Flat vectors in logical order — the buffer the fit and the aggregates both stream.
        let mut flat: Vec<f32> = Vec::with_capacity(n * dim);
        for &i in &order {
            flat.extend_from_slice(&rows[i].vector);
        }

        // --- 4. content seed: a function of the data, never a clock (C-7) --------------------
        let seed = self.content_seed(&order, &rows, k, &space);

        // --- 5. mini-batch k-means fit, in logical order, best of N seeded restarts ----------
        // A single k-means++ draw is a lottery (D-036); restarts make the fit a function of the
        // data. Each restart's seed is derived from the content seed, so the choice stays
        // deterministic, and ties on inertia break on the (deterministic) restart index.
        let mut centroids = Vec::new();
        let mut inertia_k = f64::INFINITY;
        for r in 0..SEMANTIC_MINIBATCH_RESTARTS.max(1) {
            let (c, inertia) = prism_quantizer::kmeans_minibatch(
                &flat,
                n,
                dim,
                k,
                seed.wrapping_add(r as u64 * 0x9E37_79B9),
                SEMANTIC_MINIBATCH_SIZE,
                SEMANTIC_MINIBATCH_EPOCHS,
            )?;
            if inertia < inertia_k {
                inertia_k = inertia;
                centroids = c;
            }
        }

        // Cluster quality vs a single blob: how much structure the k clusters actually captured.
        let inertia_1 = inertia_around_mean(&flat, n, dim);
        let quality = if inertia_1 > 0.0 {
            (1.0 - inertia_k / inertia_1).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let confidence = if quality >= CLUSTER_CONFIDENCE_MIN {
            Confidence::High
        } else {
            Confidence::Low
        };

        // --- 6. shard the logical stream, one partial per shard, merge in canonical order ----
        let partials = self.shard_and_reduce(&order, &rows, &flat, &centroids, k, dim);
        let merged = merge_partials(partials, k);

        // --- 7. shape the groups: order by size DESC, exemplar.event_id ASC (§16) ------------
        // Carry each summary's raw cluster index so `cluster_id`s (and any assignments) can be
        // remapped to the contract order.
        let mut with_raw: Vec<(usize, ClusterSummary)> = Vec::new();
        for (raw, pc) in merged.iter().enumerate() {
            if pc.count == 0 {
                continue;
            }
            let exemplar = pc
                .exemplar
                .as_ref()
                .expect("a non-empty cluster has an exemplar")
                .event
                .clone();
            with_raw.push((
                raw,
                ClusterSummary {
                    cluster_id: 0, // assigned below, from the order
                    count: pc.count,
                    avg_cost: pc.cost_sum / pc.count as f64,
                    error_rate: pc.error_count as f64 / pc.count as f64,
                    exemplar,
                    member_ids: Vec::new(), // filtered set can be huge; member ids are not returned
                },
            ));
        }
        with_raw.sort_by(|a, b| {
            b.1.count
                .cmp(&a.1.count)
                .then(a.1.exemplar.event_id.cmp(&b.1.exemplar.event_id))
        });
        // raw cluster index -> ephemeral ordered id (position in the contract order, §15).
        let mut raw_to_ordered = vec![usize::MAX; k];
        let mut clusters: Vec<ClusterSummary> = Vec::with_capacity(with_raw.len());
        for (idx, (raw, mut c)) in with_raw.into_iter().enumerate() {
            c.cluster_id = idx;
            raw_to_ordered[raw] = idx;
            clusters.push(c);
        }

        let assignments = if req.with_assignments {
            order
                .iter()
                .enumerate()
                .map(|(pos, &ri)| {
                    let v = &flat[pos * dim..(pos + 1) * dim];
                    let (raw, _) = prism_quantizer::nearest_centroid(v, &centroids, k, dim);
                    (rows[ri].event.event_id.clone(), raw_to_ordered[raw])
                })
                .collect()
        } else {
            Vec::new()
        };

        // Centroids reindexed by ephemeral ordered id, so `centroids[cluster_id]` is that group's
        // center (empty raw clusters carry no ordered id and are dropped).
        let mut ordered_centroids = vec![0.0f32; clusters.len() * dim];
        for (raw, &ord) in raw_to_ordered.iter().enumerate() {
            if ord != usize::MAX {
                ordered_centroids[ord * dim..(ord + 1) * dim]
                    .copy_from_slice(&centroids[raw * dim..(raw + 1) * dim]);
            }
        }

        Ok(SemanticClusterResult {
            clusters,
            confidence,
            quality,
            k_effective: k,
            rows: n,
            space,
            assignments,
            centroids: ordered_centroids,
            dim,
        })
    }

    /// Gather the rows a predicate admits, for one tenant, in one embedding space. Refuses a
    /// filtered set that spans two spaces with the invariant-9 teaching error (query contract §18).
    pub(crate) fn gather_filtered(
        &self,
        req: &ClusterRequest,
        dim: usize,
    ) -> Result<(Vec<Row>, String)> {
        let snap = self.snapshot()?;
        let mut rows: Vec<Row> = Vec::new();
        let mut space: Option<String> = None;

        for e in &snap.parts {
            let Some(r) = e.located() else { continue };
            if !r.tenants.iter().any(|t| t == &req.tenant) {
                continue;
            }
            let part_space = self
                .catalog()
                .get_generation(&r.partition.generation)?
                .space();
            if let Some(want) = &req.space {
                if &part_space != want {
                    continue;
                }
            }
            match &space {
                None => space = Some(part_space.clone()),
                Some(s) if s != &part_space => {
                    // Two spaces in the filtered set is exactly the cross-space distance the
                    // contract refuses to compute (query contract §18, invariant 9).
                    let (a, b) = if s < &part_space {
                        (s.clone(), part_space.clone())
                    } else {
                        (part_space.clone(), s.clone())
                    };
                    return Err(PrismError::Invariant(format!(
                        "this semantic_cluster spans two embedding spaces — {a} and {b} — whose \
                         vectors are not comparable (a distance in one is not a distance in the \
                         other). PrismDB will not cluster across them. Name one space with `USING \
                         SPACE`, or finish the re-embed migration so a single space remains."
                    )));
                }
                _ => {}
            }

            let reader = PartReader::open(&self.store.part_dir(&r.part_id))?;
            let all = reader.read_all()?;
            for (i, ev) in all.events.iter().enumerate() {
                if ev.tenant_id != req.tenant {
                    continue;
                }
                // Logically-deleted rows are not clustered (merge contract §6).
                if snap.is_tombstoned(&ev.event_id) {
                    continue;
                }
                if let Some(from) = req.time_from {
                    if ev.event_time < from {
                        continue;
                    }
                }
                if let Some(to) = req.time_to {
                    if ev.event_time >= to {
                        continue;
                    }
                }
                if let Some(pred) = &req.predicate {
                    let src = EventRow {
                        event: ev,
                        score: 0.0,
                    };
                    if !predicate::eval(pred, &src, 0)? {
                        continue;
                    }
                }
                rows.push(Row {
                    event: ev.clone(),
                    vector: all.vectors[i * dim..(i + 1) * dim].to_vec(),
                });
            }
        }

        Ok((rows, space.unwrap_or_default()))
    }

    /// The content seed: `SHA-256(sorted event_ids ‖ k ‖ space)`, folded to a u64. A function of
    /// the data — identical inputs seed identically, forever, everywhere (C-7).
    fn content_seed(&self, order: &[usize], rows: &[Row], k: usize, space: &str) -> u64 {
        let mut material: Vec<u8> = Vec::new();
        for &i in order {
            material.extend_from_slice(rows[i].event.event_id.as_bytes());
            material.push(0); // unambiguous separator
        }
        material.extend_from_slice(&(k as u64).to_le_bytes());
        material.extend_from_slice(space.as_bytes());
        let digest = content_id(&material); // hex SHA-256
        let bytes = digest.as_bytes();
        let mut seed = [0u8; 8];
        for (j, s) in seed.iter_mut().enumerate() {
            *s = bytes.get(j).copied().unwrap_or(0);
        }
        u64::from_le_bytes(seed)
    }

    /// Cut the logical stream into contiguous shards and reduce each to a partial state. The rows
    /// are already in `event_id` order, so a shard is an `event_id` range and shard order is
    /// canonical order (§14).
    fn shard_and_reduce(
        &self,
        order: &[usize],
        rows: &[Row],
        flat: &[f32],
        centroids: &[f32],
        k: usize,
        dim: usize,
    ) -> Vec<Partial> {
        let n = order.len();
        let mut partials: Vec<Partial> = Vec::new();
        let mut shard_id = 0usize;
        let mut start = 0usize;
        while start < n {
            let end = (start + SHARD_SIZE).min(n);
            let mut clusters = vec![PartialCluster::default(); k];
            for pos in start..end {
                let ri = order[pos];
                let v = &flat[pos * dim..(pos + 1) * dim];
                let (c, _) = prism_quantizer::nearest_centroid(v, centroids, k, dim);
                let pc = &mut clusters[c];
                pc.count += 1;
                pc.cost_sum += rows[ri].event.cost;
                if rows[ri].event.error {
                    pc.error_count += 1;
                }
                // Exemplar: exact distance to the centroid, ties on event_id (§15).
                let dist = l2_sq(v, &centroids[c * dim..(c + 1) * dim]);
                let cand = ExemplarCandidate {
                    dist,
                    event: rows[ri].event.clone(),
                };
                let replace = match &pc.exemplar {
                    Some(cur) => !cand.worse_than(cur),
                    None => true,
                };
                if replace {
                    pc.exemplar = Some(cand);
                }
            }
            partials.push(Partial { shard_id, clusters });
            shard_id += 1;
            start = end;
        }
        partials
    }
}

/// Merge partial states in **canonical (shard-id) order** into one per-cluster aggregate. Sorts by
/// shard id first, so the *physical* order partials arrive in cannot change the result (§14) — the
/// property the merge-invariance test asserts directly.
fn merge_partials(mut partials: Vec<Partial>, k: usize) -> Vec<PartialCluster> {
    partials.sort_by_key(|p| p.shard_id);
    let mut merged = vec![PartialCluster::default(); k];
    for p in &partials {
        for (c, pc) in p.clusters.iter().enumerate() {
            merged[c].count += pc.count;
            merged[c].cost_sum += pc.cost_sum;
            merged[c].error_count += pc.error_count;
            if let Some(cand) = &pc.exemplar {
                let replace = match &merged[c].exemplar {
                    Some(cur) => !cand.worse_than(cur),
                    None => true,
                };
                if replace {
                    merged[c].exemplar = Some(cand.clone());
                }
            }
        }
    }
    merged
}

/// Inertia around the single global mean (the `k = 1` inertia): the data's total spread. The
/// denominator of the cluster-quality signal. Summed in the logical order `flat` is already in.
fn inertia_around_mean(flat: &[f32], n: usize, dim: usize) -> f64 {
    let mut mean = vec![0.0f64; dim];
    for i in 0..n {
        let v = &flat[i * dim..(i + 1) * dim];
        for j in 0..dim {
            mean[j] += v[j] as f64;
        }
    }
    for m in mean.iter_mut() {
        *m /= n as f64;
    }
    let meanf: Vec<f32> = mean.iter().map(|&m| m as f32).collect();
    let mut total = 0.0f64;
    for i in 0..n {
        total += l2_sq(&flat[i * dim..(i + 1) * dim], &meanf) as f64;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, cost: f64, error: bool) -> Event {
        Event {
            event_id: id.to_string(),
            tenant_id: "alpha".into(),
            event_time: 0,
            observed_time: 0,
            event_name: String::new(),
            cost,
            error,
            body: String::new(),
            trace_id: String::new(),
            span_id: String::new(),
            attributes: prism_types::attributes::Attributes::new(),
            idempotency_key: None,
        }
    }

    fn cand(dist: f32, id: &str) -> ExemplarCandidate {
        ExemplarCandidate {
            dist,
            event: ev(id, 0.0, false),
        }
    }

    /// **Exemplar ties break on `event_id`, never on arrival** (C-4, query contract §15): equal
    /// distance keeps the smaller id, whichever was seen first.
    #[test]
    fn exemplar_ties_break_on_identity() {
        let a = cand(1.0, "e5");
        let b = cand(1.0, "e2");
        assert!(
            a.worse_than(&b),
            "equal distance: larger id is the worse exemplar"
        );
        assert!(!b.worse_than(&a));
        // A strictly nearer candidate wins regardless of id.
        let near = cand(0.5, "e9");
        assert!(!near.worse_than(&a));
    }

    // (cluster, count, cost_sum, error_count, exemplar(dist,id)) — a compact test fixture.
    type Cell<'a> = (usize, usize, f64, usize, Option<(f32, &'a str)>);
    fn part(shard_id: usize, cells: &[Cell]) -> Partial {
        let k = cells.iter().map(|c| c.0).max().map(|m| m + 1).unwrap_or(0);
        let mut clusters = vec![PartialCluster::default(); k];
        for &(c, count, cost_sum, error_count, ex) in cells {
            clusters[c] = PartialCluster {
                count,
                cost_sum,
                error_count,
                exemplar: ex.map(|(d, id)| cand(d, id)),
            };
        }
        Partial { shard_id, clusters }
    }

    /// **Partials merge in canonical (shard-id) order — the physical arrival order cannot change
    /// the result** (determinism contract §14, directive 1). The property, not merely correctness:
    /// the same partials in a scrambled order produce byte-identical aggregates.
    #[test]
    fn partial_merge_is_order_invariant() {
        let k = 2;
        let p0 = part(
            0,
            &[
                (0, 3, 3.0, 1, Some((0.4, "e10"))),
                (1, 1, 0.5, 0, Some((0.9, "e20"))),
            ],
        );
        let p1 = part(
            1,
            &[
                (0, 2, 1.0, 0, Some((0.2, "e05"))),
                (1, 4, 2.0, 2, Some((0.7, "e21"))),
            ],
        );
        let p2 = part(
            2,
            &[
                (0, 1, 0.5, 0, Some((0.3, "e07"))),
                (1, 2, 1.0, 1, Some((0.6, "e22"))),
            ],
        );

        let canonical = merge_partials(
            vec![
                part(
                    0,
                    &[
                        (0, 3, 3.0, 1, Some((0.4, "e10"))),
                        (1, 1, 0.5, 0, Some((0.9, "e20"))),
                    ],
                ),
                part(
                    1,
                    &[
                        (0, 2, 1.0, 0, Some((0.2, "e05"))),
                        (1, 4, 2.0, 2, Some((0.7, "e21"))),
                    ],
                ),
                part(
                    2,
                    &[
                        (0, 1, 0.5, 0, Some((0.3, "e07"))),
                        (1, 2, 1.0, 1, Some((0.6, "e22"))),
                    ],
                ),
            ],
            k,
        );
        // A different physical order of the same partials.
        let scrambled = merge_partials(vec![p2, p0, p1], k);

        for c in 0..k {
            assert_eq!(canonical[c].count, scrambled[c].count);
            assert_eq!(
                canonical[c].cost_sum.to_bits(),
                scrambled[c].cost_sum.to_bits()
            );
            assert_eq!(canonical[c].error_count, scrambled[c].error_count);
            assert_eq!(
                canonical[c].exemplar.as_ref().map(|e| &e.event.event_id),
                scrambled[c].exemplar.as_ref().map(|e| &e.event.event_id),
                "the merged exemplar depended on arrival order"
            );
        }
        // The exemplar of cluster 0 is the globally nearest: e05 at 0.2.
        assert_eq!(
            canonical[0].exemplar.as_ref().unwrap().event.event_id,
            "e05"
        );
    }
}

#[cfg(test)]
mod bench {
    use prism_types::rng::Rng;

    /// A rough end-to-end throughput probe for the fit (run with `--ignored --nocapture`). Prints
    /// rows/sec for `kmeans_minibatch` at the shipped params, from which the 100M projection follows.
    #[test]
    #[ignore]
    fn bench_fit_throughput() {
        let dim = 64usize;
        let k = 8usize;
        let n = 100_000usize;
        let mut rng = Rng::new(1);
        // Eight gaussian-ish blobs so the fit does real work.
        let mut v = vec![0.0f32; n * dim];
        for i in 0..n {
            let b = i % k;
            for j in 0..dim {
                let c = if j % k == b { 3.0 } else { 0.0 };
                v[i * dim + j] = c + rng.normal() * 0.3;
            }
        }
        let restarts = super::SEMANTIC_MINIBATCH_RESTARTS;
        let epochs = super::SEMANTIC_MINIBATCH_EPOCHS;
        let start = std::time::Instant::now();
        for r in 0..restarts {
            let _ = prism_quantizer::kmeans_minibatch(
                &v,
                n,
                dim,
                k,
                7 + r as u64,
                super::SEMANTIC_MINIBATCH_SIZE,
                epochs,
            )
            .unwrap();
        }
        let secs = start.elapsed().as_secs_f64();
        let rows_per_sec = n as f64 / secs;
        eprintln!("fit {n} rows k={k} dim={dim} restarts={restarts} epochs={epochs}: {secs:.3}s => {rows_per_sec:.0} rows/s");
        eprintln!("projected 100M rows: {:.1}s", 100_000_000.0 / rows_per_sec);
    }
}
