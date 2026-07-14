//! The model plane seam.
//!
//! The database owns model selection, versioning and failure semantics
//! (Part III §13), but it must be able to reach *any* generation's model, not
//! just the active one: a part written under model version 1 can only be
//! queried by embedding the query with model version 1. Asking version 2 for
//! that vector would produce a number in the wrong space — the exact failure
//! invariant 9 exists to prevent.
//!
//! In S0 the plane is in-process and deterministic. In S13 it becomes a
//! separately supervised GPU service behind this same trait, because a CUDA
//! fault must not be able to touch the storage engine.

use prism_types::error::{PrismError, Result};
use prism_types::{Embedder, HashEmbedder};
use std::sync::Arc;

pub trait ModelPlane: Send + Sync {
    /// Resolve the exact model a generation was written under.
    fn embedder(
        &self,
        model_id: &str,
        model_version: &str,
        dim: usize,
    ) -> Result<Arc<dyn Embedder>>;

    /// The model new writes use.
    fn default_embedder(&self, dim: usize) -> Arc<dyn Embedder>;
}

/// The S0 plane: the deterministic hash embedder, at any version.
#[derive(Debug, Clone, Default)]
pub struct HashModelPlane {
    pub version: String,
}

impl HashModelPlane {
    pub fn new() -> Self {
        HashModelPlane {
            version: "1".to_string(),
        }
    }

    pub fn at_version(version: &str) -> Self {
        HashModelPlane {
            version: version.to_string(),
        }
    }
}

impl ModelPlane for HashModelPlane {
    fn embedder(
        &self,
        model_id: &str,
        model_version: &str,
        dim: usize,
    ) -> Result<Arc<dyn Embedder>> {
        if model_id != "hash-embedder" {
            return Err(PrismError::NotFound(format!(
                "this build has no model `{model_id}`; the parts written under it \
                 cannot be queried without it, and guessing would put the query \
                 in the wrong embedding space"
            )));
        }
        Ok(Arc::new(HashEmbedder::with_version(dim, model_version)))
    }

    fn default_embedder(&self, dim: usize) -> Arc<dyn Embedder> {
        Arc::new(HashEmbedder::with_version(dim, &self.version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_an_older_model_version() {
        let p = HashModelPlane::at_version("2");
        let old = p.embedder("hash-embedder", "1", 32).unwrap();
        assert_eq!(old.model_version(), "1");
        assert_eq!(p.default_embedder(32).model_version(), "2");
    }

    #[test]
    fn an_unknown_model_is_an_error_not_a_substitution() {
        let p = HashModelPlane::new();
        assert!(p.embedder("some-transformer", "1", 32).is_err());
    }
}
