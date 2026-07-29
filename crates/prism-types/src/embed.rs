//! The embedding boundary.
//!
//! The database owns model selection, versioning and failure semantics
//! (Part III §13). In S0 the only implementation is a deterministic in-process
//! hash embedder: it makes every test, golden corpus and baseline reproducible
//! on any machine with no model weights and no network. A real GPU-served model
//! plane arrives in S13 behind this same trait.

use crate::error::{PrismError, Result};
use crate::hash::{hex, sha256};
use crate::vector::validate_and_normalize;
use serde::{Deserialize, Serialize};

/// Text beyond this is truncated *for embedding only* (on a char boundary).
/// The full body is still stored. See docs/DECISIONS.md, D-005.
pub const MAX_EMBED_INPUT_BYTES: usize = 32 * 1024;

/// Exact immutable artifacts that define a production embedding space.
///
/// Human model names and mutable tags are not enough provenance for stored
/// vectors. The three digests cover the weights, tokenizer, and complete
/// preprocessing pipeline. Their canonical digest is the only valid
/// `model_version` for a registered production model, so two artifact sets can
/// never share a score space merely because an operator reused a tag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelArtifacts {
    pub model_sha256: String,
    pub tokenizer_sha256: String,
    pub preprocessing_sha256: String,
}

impl ModelArtifacts {
    pub fn new(
        model_sha256: impl Into<String>,
        tokenizer_sha256: impl Into<String>,
        preprocessing_sha256: impl Into<String>,
    ) -> Result<Self> {
        let artifacts = Self {
            model_sha256: model_sha256.into(),
            tokenizer_sha256: tokenizer_sha256.into(),
            preprocessing_sha256: preprocessing_sha256.into(),
        };
        artifacts.validate()?;
        Ok(artifacts)
    }

    pub fn validate(&self) -> Result<()> {
        for (name, digest) in [
            ("model_sha256", &self.model_sha256),
            ("tokenizer_sha256", &self.tokenizer_sha256),
            ("preprocessing_sha256", &self.preprocessing_sha256),
        ] {
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(PrismError::Invalid(format!(
                    "{name} must be a lowercase 64-character SHA-256 digest"
                )));
            }
        }
        Ok(())
    }

    /// Full SHA-256 over an unambiguous canonical artifact tuple.
    pub fn revision(&self) -> String {
        let canonical = format!(
            "model={}\ntokenizer={}\npreprocessing={}\n",
            self.model_sha256, self.tokenizer_sha256, self.preprocessing_sha256
        );
        hex(&sha256(canonical.as_bytes()))
    }
}

/// Why text is crossing the model boundary.
///
/// Production policy authorizes a tenant for an exact model *and* purpose.
/// Query access does not imply permission to re-embed retained bodies, and an
/// ingest grant does not imply permission to run arbitrary evaluation traffic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingPurpose {
    Ingest,
    Query,
    Migration,
    Evaluation,
}

impl EmbeddingPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            EmbeddingPurpose::Ingest => "ingest",
            EmbeddingPurpose::Query => "query",
            EmbeddingPurpose::Migration => "migration",
            EmbeddingPurpose::Evaluation => "evaluation",
        }
    }
}

/// Tenant and purpose context that accompanies production inference.
#[derive(Clone, Copy, Debug)]
pub struct EmbeddingInput<'a> {
    pub tenant_id: Option<&'a str>,
    pub purpose: EmbeddingPurpose,
    pub text: &'a str,
}

/// Everything the engine is allowed to know about an embedding model.
///
/// `model_id` + `model_version` are hashed into the generation record, so a
/// change to either produces a new content address and therefore a new
/// generation — a stored byte can never silently change meaning.
pub trait Embedder: Send + Sync {
    fn model_id(&self) -> &str;
    fn model_version(&self) -> &str;
    fn dim(&self) -> usize;

    /// Exact production artifacts, when this is a registered external model.
    ///
    /// The deterministic development embedder predates the production model
    /// registry and returns `None`. A separately served model must return
    /// `Some`, and its `model_version` must equal `artifacts.revision()`.
    fn artifacts(&self) -> Option<&ModelArtifacts> {
        None
    }

    /// Returns a *normalized* vector, or an error. An error here means the
    /// event is dead-lettered: we never store an event without the semantic
    /// columns it asked for (Part III §10).
    fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Returns exactly one result per input, in input order.
    fn embed_batch(&self, texts: &[&str]) -> Vec<Result<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    /// Embed with the authorization context production policy needs.
    ///
    /// Development embedders preserve their historical behavior. A governed
    /// wrapper overrides this method and fails closed when tenant context or an
    /// exact tenant/model/purpose grant is absent.
    fn embed_scoped(&self, input: EmbeddingInput<'_>) -> Result<Vec<f32>> {
        self.embed(input.text)
    }

    /// Batch form that preserves the model plane's bounded batching behavior.
    fn embed_batch_scoped(&self, inputs: &[EmbeddingInput<'_>]) -> Vec<Result<Vec<f32>>> {
        let texts: Vec<&str> = inputs.iter().map(|input| input.text).collect();
        self.embed_batch(&texts)
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

    #[test]
    fn artifact_revision_binds_every_immutable_input() {
        let a = ModelArtifacts::new("a".repeat(64), "b".repeat(64), "c".repeat(64)).unwrap();
        let b = ModelArtifacts::new("a".repeat(64), "b".repeat(64), "d".repeat(64)).unwrap();
        assert_eq!(a.revision().len(), 64);
        assert_ne!(a.revision(), b.revision());
    }

    #[test]
    fn artifact_digests_are_strictly_canonical() {
        assert!(ModelArtifacts::new("A".repeat(64), "b".repeat(64), "c".repeat(64)).is_err());
        assert!(ModelArtifacts::new("a".repeat(63), "b".repeat(64), "c".repeat(64)).is_err());
        assert!(ModelArtifacts::new("g".repeat(64), "b".repeat(64), "c".repeat(64)).is_err());
    }
}
