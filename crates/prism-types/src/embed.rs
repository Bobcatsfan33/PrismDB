//! The embedding boundary.
//!
//! The database owns model selection, versioning and failure semantics
//! (Part III §13). In S0 the only implementation is a deterministic in-process
//! hash embedder: it makes every test, golden corpus and baseline reproducible
//! on any machine with no model weights and no network. A real GPU-served model
//! plane arrives in S13 behind this same trait.

use crate::error::{PrismError, Result};
use crate::vector::validate_and_normalize;

/// Text beyond this is truncated *for embedding only* (on a char boundary).
/// The full body is still stored. See docs/DECISIONS.md, D-005.
pub const MAX_EMBED_INPUT_BYTES: usize = 32 * 1024;

/// Everything the engine is allowed to know about an embedding model.
///
/// `model_id` + `model_version` are hashed into the generation record, so a
/// change to either produces a new content address and therefore a new
/// generation — a stored byte can never silently change meaning.
pub trait Embedder: Send + Sync {
    fn model_id(&self) -> &str;
    fn model_version(&self) -> &str;
    fn dim(&self) -> usize;

    /// Returns a *normalized* vector, or an error. An error here means the
    /// event is dead-lettered: we never store an event without the semantic
    /// columns it asked for (Part III §10).
    fn embed(&self, text: &str) -> Result<Vec<f32>>;

    fn embed_batch(&self, texts: &[&str]) -> Vec<Result<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

/// A deterministic bag-of-features hash embedder (the "hashing trick").
///
/// Unigrams and bigrams are hashed into `dim` buckets with a hashed sign, then
/// the vector is L2-normalized. Texts that share vocabulary land near each
/// other, which is all the geometry the S0 tests need: it produces genuine
/// cluster structure for k-means, genuine ADC error for the recall contract to
/// measure, and identical bytes on every machine.
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dim: usize,
    version: String,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        assert!(dim > 0, "dim must be positive");
        HashEmbedder {
            dim,
            version: "1".to_string(),
        }
    }

    /// A distinct version produces a distinct generation. Used by the re-embed
    /// migration tests to prove two generations can coexist.
    pub fn with_version(dim: usize, version: &str) -> Self {
        HashEmbedder {
            dim,
            version: version.to_string(),
        }
    }

    fn fnv1a64(bytes: &[u8], seed: u64) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        h
    }

    fn tokenize(text: &str) -> Vec<String> {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_lowercase())
            .collect()
    }

    fn add_feature(&self, v: &mut [f32], feature: &str, weight: f32) {
        let h = Self::fnv1a64(feature.as_bytes(), 0);
        let bucket = (h % self.dim as u64) as usize;
        // A second, independent hash decides the sign, so unrelated features
        // colliding in a bucket tend to cancel rather than accumulate.
        let sign = if Self::fnv1a64(feature.as_bytes(), 0x9E37_79B9) & 1 == 0 {
            1.0
        } else {
            -1.0
        };
        v[bucket] += sign * weight;
    }
}

impl Embedder for HashEmbedder {
    fn model_id(&self) -> &str {
        "hash-embedder"
    }

    fn model_version(&self) -> &str {
        &self.version
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let truncated = if text.len() > MAX_EMBED_INPUT_BYTES {
            let mut end = MAX_EMBED_INPUT_BYTES;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            &text[..end]
        } else {
            text
        };

        let tokens = Self::tokenize(truncated);
        if tokens.is_empty() {
            return Err(PrismError::Invalid(
                "text has no tokens; it would produce a zero-norm embedding".into(),
            ));
        }

        let mut v = vec![0.0f32; self.dim];
        for t in &tokens {
            self.add_feature(&mut v, t, 1.0);
        }
        // Bigrams carry a little word order, which gives near-duplicate texts a
        // visibly tighter cosine than bag-of-words alone.
        for w in tokens.windows(2) {
            let bigram = format!("{}_{}", w[0], w[1]);
            self.add_feature(&mut v, &bigram, 0.5);
        }

        validate_and_normalize(&mut v)?;
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::dot;

    #[test]
    fn is_deterministic() {
        let e = HashEmbedder::new(64);
        assert_eq!(
            e.embed("the agent called a tool").unwrap(),
            e.embed("the agent called a tool").unwrap()
        );
    }

    #[test]
    fn similar_text_is_nearer_than_unrelated_text() {
        let e = HashEmbedder::new(128);
        let q = e
            .embed("the payment api returned a rate limit error")
            .unwrap();
        let near = e
            .embed("the payment api returned a rate limit failure")
            .unwrap();
        let far = e
            .embed("summarize this poem about the sea in three lines")
            .unwrap();
        assert!(
            dot(&q, &near) > dot(&q, &far),
            "near={} far={}",
            dot(&q, &near),
            dot(&q, &far)
        );
    }

    #[test]
    fn output_is_unit_norm() {
        let e = HashEmbedder::new(96);
        let v = e.embed("hello world").unwrap();
        assert!((dot(&v, &v) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn tokenless_text_is_an_error_not_a_zero_vector() {
        let e = HashEmbedder::new(64);
        assert!(e.embed("").is_err());
        assert!(e.embed("   \n\t ").is_err());
        assert!(e.embed("!!! ??? ...").is_err());
    }

    #[test]
    fn oversized_text_is_truncated_for_embedding_not_rejected() {
        let e = HashEmbedder::new(64);
        let huge = "lorem ipsum ".repeat(10_000);
        assert!(huge.len() > MAX_EMBED_INPUT_BYTES);
        let v = e.embed(&huge).unwrap();
        assert!((dot(&v, &v) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn version_change_is_visible_to_the_caller() {
        let a = HashEmbedder::new(64);
        let b = HashEmbedder::with_version(64, "2");
        assert_ne!(a.model_version(), b.model_version());
    }
}
