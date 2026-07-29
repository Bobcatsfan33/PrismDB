//! The **frozen real-embedding corpus** (S13 directive 1) — a fixture, not shipped engine behaviour.
//!
//! A small open 768d model (`all-mpnet-base-v2`) embedded each distinct body **once**, offline
//! (`scripts/gen-real-corpus.py`); the vectors are committed as data under `testing/corpus/real-v1/`.
//! Nothing here runs a model or the network: [`ReplayModelPlane`] simply looks a body's committed
//! embedding up. This is what lets the C-3/C-6 re-derivation and the measured 768d worksheet run
//! against a **realistic continuous geometry** instead of the hash embedder's degenerate motifs — and
//! it stays deterministic at CI, because the embeddings are frozen data, not a live inference.

use crate::model::ModelPlane;
use prism_types::error::{PrismError, Result};
use prism_types::{Embedder, Event};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub const MODEL_ID: &str = "all-mpnet-base-v2";
pub const MODEL_VERSION: &str = "1";
pub const DIM: usize = 768;

/// A query drawn from the frozen corpus: its topic (the ground-truth cluster it belongs to) and text.
#[derive(Clone, Debug)]
pub struct RealQuery {
    pub topic: String,
    pub text: String,
}

/// A **cluster-boundary** query (S13 dir 2): its text lands between topics `a` and `b`, so its true
/// neighbours are split across two centroids. This is the query class a mean recall hides and a small
/// `nprobe` fails completely on — the tail the nprobe default is derived to hold.
#[derive(Clone, Debug)]
pub struct BoundaryQuery {
    pub topic_a: String,
    pub topic_b: String,
    pub text: String,
}

/// The loaded frozen corpus: the committed body→embedding map, the events, and the queries.
pub struct RealCorpus {
    embeddings: Arc<HashMap<String, Vec<f32>>>,
    pub events: Vec<Event>,
    pub queries: Vec<RealQuery>,
    /// Cluster-boundary queries (`boundary_queries.jsonl`); empty if the corpus predates them.
    pub boundary_queries: Vec<BoundaryQuery>,
}

impl RealCorpus {
    /// Load `testing/corpus/real-v1/` (or another version directory): the `bodies.txt` /
    /// `embeddings.f32` pair (row `i` of the raw little-endian f32 file is the embedding of line `i`),
    /// the `events.jsonl`, and the `queries.jsonl`.
    pub fn load(dir: &Path) -> Result<RealCorpus> {
        let bodies = std::fs::read_to_string(dir.join("bodies.txt"))?;
        let bodies: Vec<&str> = bodies.lines().collect();
        let raw = std::fs::read(dir.join("embeddings.f32"))?;
        let want = bodies.len() * DIM * 4;
        if raw.len() != want {
            return Err(PrismError::Corrupt(format!(
                "embeddings.f32 is {} bytes, expected {want} for {} bodies × {DIM} f32",
                raw.len(),
                bodies.len()
            )));
        }
        let mut embeddings = HashMap::with_capacity(bodies.len());
        for (i, body) in bodies.iter().enumerate() {
            let mut v = Vec::with_capacity(DIM);
            for j in 0..DIM {
                let off = (i * DIM + j) * 4;
                v.push(f32::from_le_bytes([
                    raw[off],
                    raw[off + 1],
                    raw[off + 2],
                    raw[off + 3],
                ]));
            }
            embeddings.insert(body.to_string(), v);
        }

        let events: Vec<Event> = std::fs::read_to_string(dir.join("events.jsonl"))?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Event>(l).map_err(PrismError::from))
            .collect::<Result<_>>()?;

        let queries: Vec<RealQuery> = std::fs::read_to_string(dir.join("queries.jsonl"))?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l)?;
                Ok(RealQuery {
                    topic: v["topic"].as_str().unwrap_or_default().to_string(),
                    text: v["text"].as_str().unwrap_or_default().to_string(),
                })
            })
            .collect::<Result<_>>()?;

        // Boundary queries are optional (a corpus may predate S13 dir 2). Absent file → empty set.
        let boundary_queries: Vec<BoundaryQuery> =
            match std::fs::read_to_string(dir.join("boundary_queries.jsonl")) {
                Ok(s) => s
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| {
                        let v: serde_json::Value = serde_json::from_str(l)?;
                        Ok(BoundaryQuery {
                            topic_a: v["topic_a"].as_str().unwrap_or_default().to_string(),
                            topic_b: v["topic_b"].as_str().unwrap_or_default().to_string(),
                            text: v["text"].as_str().unwrap_or_default().to_string(),
                        })
                    })
                    .collect::<Result<_>>()?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(PrismError::from(e)),
            };

        Ok(RealCorpus {
            embeddings: Arc::new(embeddings),
            events,
            queries,
            boundary_queries,
        })
    }

    /// The default location, resolved from the crate manifest so tests need no argument.
    pub fn default_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testing/corpus/real-v1")
    }

    pub fn load_default() -> Result<RealCorpus> {
        Self::load(&Self::default_dir())
    }

    /// The model plane that serves this corpus's committed embeddings.
    pub fn plane(&self) -> Arc<ReplayModelPlane> {
        Arc::new(ReplayModelPlane {
            embeddings: self.embeddings.clone(),
        })
    }
}

/// A [`ModelPlane`] that replays committed embeddings — the S13-fixture stand-in for the real model
/// plane, so a generation trained on the frozen corpus resolves the same vectors at query time.
pub struct ReplayModelPlane {
    embeddings: Arc<HashMap<String, Vec<f32>>>,
}

impl ModelPlane for ReplayModelPlane {
    fn embedder(
        &self,
        model_id: &str,
        model_version: &str,
        dim: usize,
        expected_artifacts: Option<&prism_types::ModelArtifacts>,
    ) -> Result<Arc<dyn Embedder>> {
        if expected_artifacts.is_some() {
            return Err(PrismError::Invariant(
                "the frozen replay fixture cannot serve a registered production generation".into(),
            ));
        }
        if model_id != MODEL_ID {
            return Err(PrismError::Invalid(format!(
                "this fixture plane serves `{MODEL_ID}`, not `{model_id}` (invariant 9: a part's \
                 model must be resolvable to query it)"
            )));
        }
        if dim != DIM {
            return Err(PrismError::Invalid(format!(
                "the real corpus is {DIM}-dimensional, not {dim}"
            )));
        }
        Ok(Arc::new(ReplayEmbedder {
            embeddings: self.embeddings.clone(),
            version: model_version.to_string(),
        }))
    }

    fn default_embedder(&self, _dim: usize) -> Result<Arc<dyn Embedder>> {
        Ok(Arc::new(ReplayEmbedder {
            embeddings: self.embeddings.clone(),
            version: MODEL_VERSION.to_string(),
        }))
    }
}

/// Looks a body's committed embedding up. The corpus is closed: an unknown text is a named error, not
/// a fabricated vector — a query outside the frozen set has no ground truth to be measured against.
pub struct ReplayEmbedder {
    embeddings: Arc<HashMap<String, Vec<f32>>>,
    version: String,
}

impl Embedder for ReplayEmbedder {
    fn model_id(&self) -> &str {
        MODEL_ID
    }
    fn model_version(&self) -> &str {
        &self.version
    }
    fn dim(&self) -> usize {
        DIM
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.embeddings.get(text).cloned().ok_or_else(|| {
            PrismError::Invalid(format!(
                "text is not in the frozen real corpus (its embedding was never committed): {:.60}",
                text
            ))
        })
    }
}
