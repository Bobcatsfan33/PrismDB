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
pub fn kmeans(
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
        // With fewer training points than clusters, asking for `nlist` clusters
        // is meaningless; shrink and record the truth rather than fabricate
        // empty partitions.
        let nlist = nlist.min(n).max(1);
        let centroids = kmeans(vectors, n, dim, nlist, 25, seed)?;
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
}
