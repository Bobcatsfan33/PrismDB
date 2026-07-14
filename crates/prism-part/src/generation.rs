//! Immutable, content-addressed semantic generations — the master invariant.
//!
//! A generation is the tuple (embedding model, coarse codebook, PQ codebook).
//! Together they define what every stored byte *means*. A PQ code of `0x1f` in
//! sub-quantizer 3 is not a number; it is "the 31st codeword of the 3rd
//! sub-quantizer of generation g" — and if the codebook is edited, every code
//! byte in every part silently changes meaning without a single byte of data
//! changing.
//!
//! So codebooks are never edited. A generation's id *is* the hash of its
//! contents, parts pin the generation they were written under, and a query
//! spanning two generations builds one ADC table per generation and merges only
//! at exact-score time, in a space both agree on.

use prism_quantizer::{CoarseCodebook, PqCodebook};
use prism_types::error::{PrismError, Result};
use prism_types::hash::content_id;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Generation {
    /// SHA-256 (truncated) of every field below. Derived, never assigned.
    pub generation_id: String,
    pub model_id: String,
    pub model_version: String,
    pub dim: usize,
    pub coarse: CoarseCodebook,
    pub pq: PqCodebook,
    /// Free-text note: how this generation was trained. Provenance, not policy.
    pub trained_from: String,
}

/// The content-addressed payload: everything except the id itself.
#[derive(Serialize)]
struct GenerationBody<'a> {
    model_id: &'a str,
    model_version: &'a str,
    dim: usize,
    coarse: &'a CoarseCodebook,
    pq: &'a PqCodebook,
}

impl Generation {
    pub fn new(
        model_id: &str,
        model_version: &str,
        dim: usize,
        coarse: CoarseCodebook,
        pq: PqCodebook,
        trained_from: &str,
    ) -> Result<Self> {
        if coarse.dim != dim || pq.dim != dim {
            return Err(PrismError::Invariant(format!(
                "generation dim {dim} disagrees with coarse dim {} / pq dim {}",
                coarse.dim, pq.dim
            )));
        }
        let body = GenerationBody {
            model_id,
            model_version,
            dim,
            coarse: &coarse,
            pq: &pq,
        };
        // serde_json's struct serialization preserves declaration order, so the
        // same inputs always hash to the same bytes.
        let bytes = serde_json::to_vec(&body)?;
        Ok(Generation {
            generation_id: content_id(&bytes),
            model_id: model_id.to_string(),
            model_version: model_version.to_string(),
            dim,
            coarse,
            pq,
            trained_from: trained_from.to_string(),
        })
    }

    /// Recompute the content address and compare. A generation that does not
    /// hash to its own id has been tampered with or corrupted, and every byte
    /// that depends on it is now suspect.
    pub fn verify_content_address(&self) -> Result<()> {
        let body = GenerationBody {
            model_id: &self.model_id,
            model_version: &self.model_version,
            dim: self.dim,
            coarse: &self.coarse,
            pq: &self.pq,
        };
        let bytes = serde_json::to_vec(&body)?;
        let actual = content_id(&bytes);
        if actual != self.generation_id {
            return Err(PrismError::Corrupt(format!(
                "generation {} does not hash to its own id (computed {actual}); \
                 a codebook was mutated in place",
                self.generation_id
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_types::rng::Rng;
    use prism_types::vector::validate_and_normalize;

    fn corpus(n: usize, dim: usize, seed: u64) -> Vec<f32> {
        let mut rng = Rng::new(seed);
        let mut v = Vec::new();
        for i in 0..n {
            let cluster = i % 4;
            let mut row: Vec<f32> = (0..dim)
                .map(|j| (if j % 4 == cluster { 3.0 } else { 0.0 }) + rng.normal() * 0.4)
                .collect();
            validate_and_normalize(&mut row).unwrap();
            v.extend_from_slice(&row);
        }
        v
    }

    fn gen(version: &str, seed: u64) -> Generation {
        let dim = 8;
        let v = corpus(100, dim, seed);
        let coarse = CoarseCodebook::train(&v, 100, dim, 4, 1).unwrap();
        let pq = PqCodebook::train(&v, 100, dim, 2, 1).unwrap();
        Generation::new("hash-embedder", version, dim, coarse, pq, "test").unwrap()
    }

    #[test]
    fn same_contents_same_id() {
        assert_eq!(gen("1", 7).generation_id, gen("1", 7).generation_id);
    }

    #[test]
    fn a_new_model_version_is_a_new_generation() {
        assert_ne!(gen("1", 7).generation_id, gen("2", 7).generation_id);
    }

    #[test]
    fn different_codebooks_are_different_generations() {
        assert_ne!(gen("1", 7).generation_id, gen("1", 8).generation_id);
    }

    #[test]
    fn mutating_a_codebook_in_place_is_detected() {
        let mut g = gen("1", 7);
        g.coarse.centroids[0] += 0.001;
        let err = g.verify_content_address().unwrap_err();
        assert!(matches!(err, PrismError::Corrupt(_)));
    }

    #[test]
    fn dim_disagreement_is_refused_at_construction() {
        let dim = 8;
        let v = corpus(50, dim, 3);
        let coarse = CoarseCodebook::train(&v, 50, dim, 3, 1).unwrap();
        let pq = PqCodebook::train(&v, 50, dim, 2, 1).unwrap();
        let err = Generation::new("m", "1", 16, coarse, pq, "test").unwrap_err();
        assert!(matches!(err, PrismError::Invariant(_)));
    }
}
