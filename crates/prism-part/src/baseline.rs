//! Drift baselines (S5) — and why they are pinned to a generation.
//!
//! A **baseline** is a description of what "normal" looked like: a set of centroids in an
//! embedding space, plus a calibrated threshold. A **novelty score** is a distance from it. So a
//! drift alarm is, in the end, one number compared against another.
//!
//! Which is exactly why this is dangerous during a migration.
//!
//! > A baseline is a statement about a distribution **in one embedding space**. When the space
//! > changes underneath it, the baseline is not *stale* — it is **meaningless**.
//!
//! Invariant 9 forbids comparing scores across embedding spaces, and a novelty score is a score.
//! An alarm that kept evaluating a new generation's events against the old generation's baseline
//! would keep producing numbers, and every number would be nonsense, and **nobody would be
//! told** — the alarm would simply stop meaning anything while continuing to look healthy. That
//! is the worst failure mode available to a monitoring system, and it is the one the generation
//! contract §7 exists to prevent.
//!
//! So: a baseline names its generation, an event is scored only against a baseline of its own
//! generation, and a migration is not complete until every baseline has been rebuilt in the new
//! space ([the generation contract](../../../docs/GENERATION-CONTRACT.md) §7).

use prism_types::error::{PrismError, Result};
use prism_types::hash::content_id;
use serde::{Deserialize, Serialize};

/// The definition of "normal", in one space, for one tenant.
///
/// Content-addressed and immutable, exactly like a generation, and for the same reason: it is
/// the yardstick every novelty score was measured against, so editing one in place would
/// silently change the meaning of every alarm ever raised from it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub baseline_id: String,
    pub tenant: String,
    /// **The space this baseline lives in.** Not decoration: it is what makes the baseline
    /// usable or not, and what makes a migration incomplete until it is rebuilt.
    pub generation_id: String,
    pub dim: usize,
    /// The shape of "normal": cluster centres of the baseline window's events.
    pub centroids: Vec<f32>,
    pub nlist: usize,
    /// The novelty above which an event is *unusual*, calibrated from the baseline window's own
    /// distribution rather than picked. A threshold picked by hand is a number somebody liked;
    /// a threshold calibrated from the data is a statement about the data.
    pub threshold: f32,
    /// What the threshold was calibrated to, in prose, so nobody has to guess.
    pub calibration: String,
    pub rows: usize,
    pub built_from_snapshot: String,
    pub built_at_ms: i64,
}

#[derive(Serialize)]
struct BaselineBody<'a> {
    tenant: &'a str,
    generation_id: &'a str,
    dim: usize,
    centroids: &'a [f32],
    nlist: usize,
    threshold: f32,
    rows: usize,
}

impl Baseline {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tenant: &str,
        generation_id: &str,
        dim: usize,
        centroids: Vec<f32>,
        nlist: usize,
        threshold: f32,
        calibration: &str,
        rows: usize,
        built_from_snapshot: &str,
        built_at_ms: i64,
    ) -> Result<Self> {
        if dim == 0 || centroids.len() != nlist * dim {
            return Err(PrismError::Invariant(format!(
                "baseline has {} centroid floats, which is not nlist ({nlist}) * dim ({dim})",
                centroids.len()
            )));
        }
        let body = BaselineBody {
            tenant,
            generation_id,
            dim,
            centroids: &centroids,
            nlist,
            threshold,
            rows,
        };
        let bytes = serde_json::to_vec(&body)?;
        Ok(Baseline {
            baseline_id: content_id(&bytes),
            tenant: tenant.to_string(),
            generation_id: generation_id.to_string(),
            dim,
            centroids,
            nlist,
            threshold,
            calibration: calibration.to_string(),
            rows,
            built_from_snapshot: built_from_snapshot.to_string(),
            built_at_ms,
        })
    }

    pub fn verify_content_address(&self) -> Result<()> {
        let body = BaselineBody {
            tenant: &self.tenant,
            generation_id: &self.generation_id,
            dim: self.dim,
            centroids: &self.centroids,
            nlist: self.nlist,
            threshold: self.threshold,
            rows: self.rows,
        };
        let id = content_id(&serde_json::to_vec(&body)?);
        if id != self.baseline_id {
            return Err(PrismError::Corrupt(format!(
                "baseline {} does not hash to its own id (got {id}); every novelty score ever \
                 measured against it is now suspect",
                self.baseline_id
            )));
        }
        Ok(())
    }

    /// How far this vector is from *normal*: one minus its similarity to the nearest centre.
    ///
    /// Vectors are normalized, so a dot product is a cosine and `1 - cos` is in `[0, 2]`.
    ///
    /// **Only ever call this with a vector from this baseline's own generation.** The engine
    /// enforces that; this type cannot, because a `&[f32]` does not know what it means. That is
    /// precisely the confusion invariant 9 exists to prevent, and the reason the check lives one
    /// layer up where the generation is known.
    pub fn novelty(&self, v: &[f32]) -> Result<f32> {
        if v.len() != self.dim {
            return Err(PrismError::Invariant(format!(
                "novelty: vector of dim {} against a baseline of dim {}",
                v.len(),
                self.dim
            )));
        }
        let mut best = f32::NEG_INFINITY;
        for c in 0..self.nlist {
            let cent = &self.centroids[c * self.dim..(c + 1) * self.dim];
            let dot: f32 = v.iter().zip(cent).map(|(a, b)| a * b).sum();
            if dot > best {
                best = dot;
            }
        }
        Ok(1.0 - best)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: Vec<f32>) -> Vec<f32> {
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.into_iter().map(|x| x / n).collect()
    }

    #[test]
    fn a_vector_on_a_centroid_has_no_novelty_and_one_far_away_has_plenty() {
        let c = unit(vec![1.0, 0.0]);
        let b = Baseline::new("t", "g", 2, c.clone(), 1, 0.5, "test", 10, "s1", 0).unwrap();
        assert!(b.novelty(&c).unwrap().abs() < 1e-6);
        let far = unit(vec![-1.0, 0.0]);
        assert!(b.novelty(&far).unwrap() > 1.9);
    }

    #[test]
    fn a_baseline_that_does_not_hash_to_its_own_id_is_refused() {
        let mut b = Baseline::new(
            "t",
            "g",
            2,
            unit(vec![1.0, 0.0]),
            1,
            0.5,
            "test",
            10,
            "s1",
            0,
        )
        .unwrap();
        b.threshold = 0.9; // the yardstick, quietly moved
        assert!(b.verify_content_address().is_err());
    }
}
