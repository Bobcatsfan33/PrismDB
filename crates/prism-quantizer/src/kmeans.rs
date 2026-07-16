use prism_types::error::{PrismError, Result};
use prism_types::rng::Rng;
use prism_types::vector::l2_sq;
use serde::{Deserialize, Serialize};

/// Lloyd's algorithm with k-means++ seeding. Deterministic given `seed`.
///
/// Returns `k * dim` centroids. Empty clusters are re-seeded from the point
/// currently furthest from its own centroid — deterministically, so two runs
/// with the same seed produce byte-identical codebooks. A codebook that is not
/// reproducible cannot be content-addressed.
/// How many independent k-means++ restarts to run, keeping the best by inertia.
///
/// **Policy** (C-1). k-means++ seeds itself from a *random draw*, and a single draw is a
/// lottery: one unlucky init produces a codebook whose centroids describe the data badly, and
/// since a codebook is the meaning of every byte encoded under it, "unlucky" is not a word that
/// belongs anywhere near it.
///
/// S5 found this the hard way. The training sample became order-independent (charter C-4, keyed
/// on `event_id` rather than on position) — which is *correct* — and recall promptly fell below
/// its floor, because the old order happened to hand k-means++ a lucky first point. A pipeline
/// whose quality depends on a lucky draw is not a pipeline, it is a coincidence. Restarts make
/// the outcome depend on the data instead.
///
/// Five is a deliberate compromise: training is offline and its cost is linear in this number,
/// while the variance it removes is most of the variance there is.
pub const KMEANS_RESTARTS: usize = 5;

/// Train, restarting `KMEANS_RESTARTS` times and keeping the codebook that fits best.
///
/// "Best" is **inertia**: the sum of squared distances from every training point to its nearest
/// centroid. Lower is a tighter description of the data. Ties break on the restart index, which
/// is deterministic, so the same inputs always give the same codebook.
pub fn kmeans(
    vectors: &[f32],
    n: usize,
    dim: usize,
    k: usize,
    iters: usize,
    seed: u64,
) -> Result<Vec<f32>> {
    kmeans_restarts(vectors, n, dim, k, iters, seed, KMEANS_RESTARTS)
}

/// Train with an explicit restart count. Exists so the C-1 sweep can *measure* the constant
/// rather than assert it, and so an operator with an unusual corpus can pay for a better fit.
pub fn kmeans_restarts(
    vectors: &[f32],
    n: usize,
    dim: usize,
    k: usize,
    iters: usize,
    seed: u64,
    restarts: usize,
) -> Result<Vec<f32>> {
    let mut best: Option<(f64, Vec<f32>)> = None;
    for r in 0..restarts.max(1) {
        // Each restart is its own deterministic draw. Derived from the store seed, so the whole
        // thing stays reproducible.
        let c = kmeans_once(
            vectors,
            n,
            dim,
            k,
            iters,
            seed.wrapping_add(r as u64 * 0x9E37_79B9),
        )?;
        let inertia = inertia(vectors, n, dim, &c, k);
        if best.as_ref().map_or(true, |(b, _)| inertia < *b) {
            best = Some((inertia, c));
        }
    }
    Ok(best.expect("restarts is at least 1").1)
}

/// Sum of squared distances from each point to its nearest centroid. The thing restarts minimize.
fn inertia(vectors: &[f32], n: usize, dim: usize, centroids: &[f32], k: usize) -> f64 {
    let point = |i: usize| &vectors[i * dim..(i + 1) * dim];
    let mut total = 0.0f64;
    for i in 0..n {
        let mut best = f32::INFINITY;
        for c in 0..k {
            let d = l2_sq(point(i), &centroids[c * dim..(c + 1) * dim]);
            if d < best {
                best = d;
            }
        }
        total += best as f64;
    }
    total
}

fn kmeans_once(
    vectors: &[f32],
    n: usize,
    dim: usize,
    k: usize,
    iters: usize,
    seed: u64,
) -> Result<Vec<f32>> {
    if n == 0 {
        return Err(PrismError::Invalid("cannot train on zero vectors".into()));
    }
    if k == 0 {
        return Err(PrismError::Invalid("k must be positive".into()));
    }
    if vectors.len() != n * dim {
        return Err(PrismError::Invalid(format!(
            "vector buffer is {} floats, expected {}",
            vectors.len(),
            n * dim
        )));
    }

    let mut rng = Rng::new(seed);
    let point = |i: usize| &vectors[i * dim..(i + 1) * dim];

    // --- k-means++ seeding ---
    let mut centroids = vec![0.0f32; k * dim];
    let first = rng.below(n);
    centroids[..dim].copy_from_slice(point(first));

    // d2[i] = squared distance from point i to its nearest chosen centroid.
    let mut d2: Vec<f32> = (0..n).map(|i| l2_sq(point(i), point(first))).collect();

    for c in 1..k {
        let total: f64 = d2.iter().map(|&x| x as f64).sum();
        let chosen = if total <= 0.0 {
            // Fewer distinct points than clusters: fall back to a round-robin
            // pick so training still terminates with a valid codebook.
            c % n
        } else {
            let target = rng.next_f32() as f64 * total;
            let mut acc = 0.0f64;
            let mut pick = n - 1;
            for (i, &w) in d2.iter().enumerate() {
                acc += w as f64;
                if acc >= target {
                    pick = i;
                    break;
                }
            }
            pick
        };
        centroids[c * dim..(c + 1) * dim].copy_from_slice(point(chosen));
        for (i, nearest) in d2.iter_mut().enumerate() {
            let d = l2_sq(point(i), &centroids[c * dim..(c + 1) * dim]);
            if d < *nearest {
                *nearest = d;
            }
        }
    }

    // --- Lloyd iterations ---
    let mut assign = vec![0usize; n];
    for _ in 0..iters {
        let mut moved = false;
        for i in 0..n {
            let mut best = 0usize;
            let mut best_d = f32::INFINITY;
            for c in 0..k {
                let d = l2_sq(point(i), &centroids[c * dim..(c + 1) * dim]);
                if d < best_d {
                    best_d = d;
                    best = c;
                }
            }
            if assign[i] != best {
                moved = true;
            }
            assign[i] = best;
            d2[i] = best_d;
        }

        let mut sums = vec![0.0f64; k * dim];
        let mut counts = vec![0usize; k];
        for (i, &c) in assign.iter().enumerate() {
            counts[c] += 1;
            let p = point(i);
            for j in 0..dim {
                sums[c * dim + j] += p[j] as f64;
            }
        }

        for c in 0..k {
            if counts[c] > 0 {
                for j in 0..dim {
                    centroids[c * dim + j] = (sums[c * dim + j] / counts[c] as f64) as f32;
                }
            } else {
                // Re-seed the empty cluster from the worst-served point.
                let worst = d2
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                centroids[c * dim..(c + 1) * dim].copy_from_slice(point(worst));
                d2[worst] = 0.0;
                moved = true;
            }
        }

        if !moved {
            break;
        }
    }

    Ok(centroids)
}

/// k-means++ seeding only — the `k` initial centroids, no Lloyd refinement.
///
/// Exposed so the mini-batch fit (S9's semantic aggregate) can seed the same way the codebook
/// trainer does, then refine by streaming mini-batches instead of by full passes. Deterministic
/// given `seed`: the D²-weighted draws come from a `seed`-initialized [`Rng`], and points are
/// considered in the order the caller supplies them — which for S9 is **logical** (`event_id`)
/// order, so the initialization is a function of the data, not of the layout ([C-7](../../../docs/DECISIONS.md)).
pub fn kmeans_plusplus_init(
    vectors: &[f32],
    n: usize,
    dim: usize,
    k: usize,
    seed: u64,
) -> Result<Vec<f32>> {
    if n == 0 {
        return Err(PrismError::Invalid("cannot seed on zero vectors".into()));
    }
    if k == 0 {
        return Err(PrismError::Invalid("k must be positive".into()));
    }
    if vectors.len() != n * dim {
        return Err(PrismError::Invalid(format!(
            "vector buffer is {} floats, expected {}",
            vectors.len(),
            n * dim
        )));
    }
    let mut rng = Rng::new(seed);
    let point = |i: usize| &vectors[i * dim..(i + 1) * dim];
    let mut centroids = vec![0.0f32; k * dim];
    let first = rng.below(n);
    centroids[..dim].copy_from_slice(point(first));
    let mut d2: Vec<f32> = (0..n).map(|i| l2_sq(point(i), point(first))).collect();
    for c in 1..k {
        let total: f64 = d2.iter().map(|&x| x as f64).sum();
        let chosen = if total <= 0.0 {
            c % n
        } else {
            let target = rng.next_f32() as f64 * total;
            let mut acc = 0.0f64;
            let mut pick = n - 1;
            for (i, &w) in d2.iter().enumerate() {
                acc += w as f64;
                if acc >= target {
                    pick = i;
                    break;
                }
            }
            pick
        };
        centroids[c * dim..(c + 1) * dim].copy_from_slice(point(chosen));
        for (i, nearest) in d2.iter_mut().enumerate() {
            let d = l2_sq(point(i), &centroids[c * dim..(c + 1) * dim]);
            if d < *nearest {
                *nearest = d;
            }
        }
    }
    Ok(centroids)
}

/// The nearest centroid to `v` among `k` centroids of width `dim`, ties broken on centroid index.
/// The shared assignment rule for the mini-batch fit and its final labeling pass.
pub fn nearest_centroid(v: &[f32], centroids: &[f32], k: usize, dim: usize) -> (usize, f32) {
    let mut best = 0usize;
    let mut best_d = f32::INFINITY;
    for c in 0..k {
        let d = l2_sq(v, &centroids[c * dim..(c + 1) * dim]);
        if d < best_d {
            best_d = d;
            best = c;
        }
    }
    (best, best_d)
}

/// Streaming (mini-batch) k-means (S9). Seeds with k-means++ ([`kmeans_plusplus_init`]) and then
/// refines the centroids by streaming `vectors` in the **order given** — for S9 that is `event_id`
/// order — in chunks of `batch_size`, for `epochs` passes. Each pass reassigns every point to its
/// nearest centroid and recomputes each centroid as the **logical-order mean** of its assigned
/// points; the per-center accumulators are the only state that persists across a pass, so the fit
/// never holds more than a chunk of points beyond `k*dim` — it clusters a filtered set far larger
/// than memory would hold as vectors, which full-batch [`kmeans`] cannot.
///
/// Deterministic given `(vectors order, seed, epochs)`: the sums accumulate in the supplied
/// (logical) index order regardless of `batch_size`, so the float reductions are reproducible
/// ([determinism contract §13](../../../docs/DETERMINISM-CONTRACT.md)) and `batch_size` changes
/// memory, never the answer. Empty clusters re-seed from the worst-served point, deterministically.
///
/// Returns the centroids and the final **inertia** (sum of squared distances to the nearest
/// centroid), which the caller turns into a cluster-quality signal — low inertia relative to the
/// data's spread is real structure; near the spread is noise (the no-structure corpus).
pub fn kmeans_minibatch(
    vectors: &[f32],
    n: usize,
    dim: usize,
    k: usize,
    seed: u64,
    batch_size: usize,
    epochs: usize,
) -> Result<(Vec<f32>, f64)> {
    if k == 0 {
        return Err(PrismError::Invalid("k must be positive".into()));
    }
    if n == 0 {
        return Err(PrismError::Invalid("cannot cluster zero vectors".into()));
    }
    if vectors.len() != n * dim {
        return Err(PrismError::Invalid(format!(
            "vector buffer is {} floats, expected {}",
            vectors.len(),
            n * dim
        )));
    }
    let k = k.min(n);
    let point = |i: usize| &vectors[i * dim..(i + 1) * dim];
    let batch = batch_size.max(1);

    let mut centroids = kmeans_plusplus_init(vectors, n, dim, k, seed)?;

    for _ in 0..epochs.max(1) {
        // Per-center accumulators for this pass, reset each epoch. The running distance to the
        // nearest centroid feeds the empty-cluster re-seed.
        let mut sums = vec![0.0f64; k * dim];
        let mut counts = vec![0usize; k];
        let mut worst_d = 0.0f32;
        let mut worst_i = 0usize;
        let mut changed = false;
        let mut start = 0usize;
        while start < n {
            let end = (start + batch).min(n);
            for i in start..end {
                let (c, d) = nearest_centroid(point(i), &centroids, k, dim);
                counts[c] += 1;
                let p = point(i);
                for j in 0..dim {
                    sums[c * dim + j] += p[j] as f64;
                }
                if d > worst_d {
                    worst_d = d;
                    worst_i = i;
                }
            }
            start = end;
        }
        for c in 0..k {
            if counts[c] > 0 {
                for j in 0..dim {
                    let m = (sums[c * dim + j] / counts[c] as f64) as f32;
                    if m != centroids[c * dim + j] {
                        changed = true;
                    }
                    centroids[c * dim + j] = m;
                }
            } else {
                // Re-seed the empty cluster from the point currently worst-served.
                centroids[c * dim..(c + 1) * dim].copy_from_slice(point(worst_i));
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let inertia = inertia(vectors, n, dim, &centroids, k);
    Ok((centroids, inertia))
}

/// The coarse (IVF) centroids: the resident index that prunes the scan.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CoarseCodebook {
    pub dim: usize,
    pub nlist: usize,
    /// `nlist * dim` floats.
    pub centroids: Vec<f32>,
}

impl CoarseCodebook {
    pub fn train(vectors: &[f32], n: usize, dim: usize, nlist: usize, seed: u64) -> Result<Self> {
        Self::train_restarts(vectors, n, dim, nlist, seed, KMEANS_RESTARTS)
    }

    pub fn train_restarts(
        vectors: &[f32],
        n: usize,
        dim: usize,
        nlist: usize,
        seed: u64,
        restarts: usize,
    ) -> Result<Self> {
        // With fewer training points than clusters, asking for `nlist` clusters
        // is meaningless; shrink and record the truth rather than fabricate
        // empty partitions.
        let nlist = nlist.min(n).max(1);
        let centroids = kmeans_restarts(vectors, n, dim, nlist, 25, seed, restarts)?;
        Ok(CoarseCodebook {
            dim,
            nlist,
            centroids,
        })
    }

    pub fn centroid(&self, c: usize) -> &[f32] {
        &self.centroids[c * self.dim..(c + 1) * self.dim]
    }

    /// The nearest centroid to `v`, and its squared distance.
    pub fn assign(&self, v: &[f32]) -> (u32, f32) {
        let mut best = 0u32;
        let mut best_d = f32::INFINITY;
        for c in 0..self.nlist {
            let d = l2_sq(v, self.centroid(c));
            if d < best_d {
                best_d = d;
                best = c as u32;
            }
        }
        (best, best_d)
    }

    /// All centroids ranked nearest-first. The query scores this tiny set, then
    /// takes the first `nprobe`.
    pub fn rank(&self, q: &[f32]) -> Vec<(u32, f32)> {
        let mut scored: Vec<(u32, f32)> = (0..self.nlist)
            .map(|c| (c as u32, l2_sq(q, self.centroid(c))))
            .collect();
        // Tie-break on centroid id so `nprobe` selection is deterministic.
        scored.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_types::rng::Rng;

    /// Three well-separated blobs in 8-d.
    fn blobs(n_per: usize, dim: usize, seed: u64) -> (Vec<f32>, Vec<usize>) {
        let mut rng = Rng::new(seed);
        let mut v = Vec::new();
        let mut labels = Vec::new();
        for b in 0..3usize {
            for _ in 0..n_per {
                for j in 0..dim {
                    let center = if j % 3 == b { 5.0 } else { 0.0 };
                    v.push(center + rng.normal() * 0.2);
                }
                labels.push(b);
            }
        }
        (v, labels)
    }

    #[test]
    fn recovers_well_separated_blobs() {
        let (v, labels) = blobs(40, 9, 1);
        let n = labels.len();
        let cb = CoarseCodebook::train(&v, n, 9, 3, 42).unwrap();
        // Points with the same true label must land in the same cluster.
        let assigned: Vec<u32> = (0..n)
            .map(|i| cb.assign(&v[i * 9..(i + 1) * 9]).0)
            .collect();
        for b in 0..3 {
            let idx: Vec<usize> = (0..n).filter(|&i| labels[i] == b).collect();
            let first = assigned[idx[0]];
            assert!(
                idx.iter().all(|&i| assigned[i] == first),
                "blob {b} was split across clusters"
            );
        }
    }

    #[test]
    fn training_is_deterministic_and_byte_identical() {
        let (v, labels) = blobs(20, 6, 3);
        let a = CoarseCodebook::train(&v, labels.len(), 6, 4, 7).unwrap();
        let b = CoarseCodebook::train(&v, labels.len(), 6, 4, 7).unwrap();
        assert_eq!(a, b, "same seed must produce an identical codebook");
    }

    #[test]
    fn ranking_is_sorted_and_total() {
        let (v, labels) = blobs(10, 6, 5);
        let cb = CoarseCodebook::train(&v, labels.len(), 6, 5, 11).unwrap();
        let r = cb.rank(&v[..6]);
        assert_eq!(r.len(), cb.nlist);
        for w in r.windows(2) {
            assert!(w[0].1 <= w[1].1);
        }
        // The nearest ranked centroid is the assigned one.
        assert_eq!(r[0].0, cb.assign(&v[..6]).0);
    }

    #[test]
    fn nlist_shrinks_rather_than_fabricating_empty_partitions() {
        let v = vec![1.0, 0.0, 0.0, 1.0]; // two points, 2-d
        let cb = CoarseCodebook::train(&v, 2, 2, 16, 1).unwrap();
        assert_eq!(cb.nlist, 2);
    }

    #[test]
    fn rejects_malformed_training_input() {
        assert!(kmeans(&[], 0, 4, 2, 5, 1).is_err());
        assert!(kmeans(&[1.0, 2.0], 5, 4, 2, 5, 1).is_err());
    }

    #[test]
    fn minibatch_is_deterministic_and_order_faithful() {
        let (v, labels) = blobs(30, 6, 9);
        let n = labels.len();
        let a = kmeans_minibatch(&v, n, 6, 3, 42, 16, 8).unwrap();
        let b = kmeans_minibatch(&v, n, 6, 3, 42, 16, 8).unwrap();
        assert_eq!(a.0, b.0, "same seed + same order must be byte-identical");
        assert_eq!(a.1, b.1);
    }

    #[test]
    fn minibatch_recovers_well_separated_blobs() {
        let (v, labels) = blobs(40, 9, 2);
        let n = labels.len();
        let (centroids, _) = kmeans_minibatch(&v, n, 9, 3, 7, 32, 12).unwrap();
        let assigned: Vec<usize> = (0..n)
            .map(|i| nearest_centroid(&v[i * 9..(i + 1) * 9], &centroids, 3, 9).0)
            .collect();
        for b in 0..3 {
            let idx: Vec<usize> = (0..n).filter(|&i| labels[i] == b).collect();
            let first = assigned[idx[0]];
            assert!(
                idx.iter().all(|&i| assigned[i] == first),
                "blob {b} was split across clusters"
            );
        }
    }
}
