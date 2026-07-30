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
use prism_types::vector::validate_and_normalize;
use prism_types::{Embedder, HashEmbedder, ModelArtifacts};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const MAX_REGISTRY_BYTES: u64 = 1024 * 1024;
const MAX_MODELS: usize = 128;
const MAX_BATCH_ITEMS: usize = 256;
const MAX_BATCH_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

pub trait ModelPlane: Send + Sync {
    /// Authorize and reserve usage before an ingest crosses its durable ACK
    /// point. The default development plane has no external policy.
    fn preflight(
        &self,
        _model_id: &str,
        _model_version: &str,
        inputs: &[prism_types::EmbeddingInput<'_>],
    ) -> Vec<Result<()>> {
        inputs.iter().map(|_| Ok(())).collect()
    }

    /// Resolve the exact model a generation was written under.
    fn embedder(
        &self,
        model_id: &str,
        model_version: &str,
        dim: usize,
        expected_artifacts: Option<&ModelArtifacts>,
    ) -> Result<Arc<dyn Embedder>>;

    /// Resolve a registry-pinned model before a generation exists to carry its
    /// expected artifact tuple.
    fn candidate_embedder(
        &self,
        model_id: &str,
        model_version: &str,
        dim: usize,
    ) -> Result<Arc<dyn Embedder>> {
        self.embedder(model_id, model_version, dim, None)
    }

    /// The model new writes use.
    fn default_embedder(&self, dim: usize) -> Result<Arc<dyn Embedder>>;
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
        expected_artifacts: Option<&ModelArtifacts>,
    ) -> Result<Arc<dyn Embedder>> {
        if expected_artifacts.is_some() {
            return Err(PrismError::Invariant(
                "a registered production generation cannot be served by the development hash \
                 model plane"
                    .into(),
            ));
        }
        if model_id != "hash-embedder" {
            return Err(PrismError::NotFound(format!(
                "this build has no model `{model_id}`; the parts written under it \
                 cannot be queried without it, and guessing would put the query \
                 in the wrong embedding space"
            )));
        }
        Ok(Arc::new(HashEmbedder::with_version(dim, model_version)))
    }

    fn default_embedder(&self, dim: usize) -> Result<Arc<dyn Embedder>> {
        Ok(Arc::new(HashEmbedder::with_version(dim, &self.version)))
    }
}

/// One immutable model entry approved for production use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredModel {
    pub model_id: String,
    /// Must equal the canonical revision of `artifacts`.
    pub model_version: String,
    pub dim: usize,
    pub artifacts: ModelArtifacts,
}

impl RegisteredModel {
    pub fn validate(&self) -> Result<()> {
        if self.model_id.trim().is_empty() || self.model_id.len() > 256 {
            return Err(PrismError::Invalid(
                "registered model_id must contain 1..=256 characters".into(),
            ));
        }
        if self.dim == 0 || self.dim > 65_536 {
            return Err(PrismError::Invalid(format!(
                "registered model `{}` has invalid dimension {}",
                self.model_id, self.dim
            )));
        }
        self.artifacts.validate()?;
        let revision = self.artifacts.revision();
        if self.model_version != revision {
            return Err(PrismError::Invalid(format!(
                "registered model `{}` uses version `{}` but its immutable artifacts revise to \
                 `{revision}`; mutable aliases are forbidden",
                self.model_id, self.model_version
            )));
        }
        Ok(())
    }
}

/// Operator-controlled allowlist. Nothing returned by the inference service
/// becomes trusted merely because the service claims to serve it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRegistry {
    pub default_model_id: String,
    pub default_model_version: String,
    pub models: Vec<RegisteredModel>,
}

impl ModelRegistry {
    pub fn load(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(MAX_REGISTRY_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_REGISTRY_BYTES {
            return Err(PrismError::Invalid(format!(
                "model registry exceeds {MAX_REGISTRY_BYTES} bytes"
            )));
        }
        let registry: Self = serde_json::from_slice(&bytes)?;
        registry.validate()?;
        Ok(registry)
    }

    pub fn validate(&self) -> Result<()> {
        if self.models.is_empty() || self.models.len() > MAX_MODELS {
            return Err(PrismError::Invalid(format!(
                "model registry must contain 1..={MAX_MODELS} models"
            )));
        }
        let mut keys = BTreeSet::new();
        for model in &self.models {
            model.validate()?;
            let key = (model.model_id.as_str(), model.model_version.as_str());
            if !keys.insert(key) {
                return Err(PrismError::Invalid(format!(
                    "duplicate registered model {}:{}",
                    model.model_id, model.model_version
                )));
            }
        }
        self.resolve(&self.default_model_id, &self.default_model_version)?;
        Ok(())
    }

    fn resolve(&self, model_id: &str, model_version: &str) -> Result<&RegisteredModel> {
        self.models
            .iter()
            .find(|model| model.model_id == model_id && model.model_version == model_version)
            .ok_or_else(|| {
                PrismError::NotFound(format!(
                    "model `{model_id}:{model_version}` is not in the production registry"
                ))
            })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceRequest {
    pub protocol_version: u32,
    pub model_id: String,
    pub model_version: String,
    pub artifacts: ModelArtifacts,
    pub texts: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InferenceItem {
    Ok { vector: Vec<f32> },
    Error { error: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceResponse {
    pub protocol_version: u32,
    pub model_id: String,
    pub model_version: String,
    pub artifacts: ModelArtifacts,
    pub outputs: Vec<InferenceItem>,
}

/// Boundary to a separately supervised inference process.
pub trait InferenceTransport: Send + Sync {
    fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse>;
}

/// Length-bounded JSON-lines protocol over a local Unix socket. The model
/// service can own CUDA, weights, batching queues, and its own crash lifecycle
/// without sharing an address space with the storage engine.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct UnixInferenceTransport {
    socket_path: PathBuf,
    timeout: Duration,
}

#[cfg(unix)]
impl UnixInferenceTransport {
    pub fn new(socket_path: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout,
        }
    }
}

#[cfg(unix)]
impl InferenceTransport for UnixInferenceTransport {
    fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        use std::net::Shutdown;
        use std::os::unix::net::UnixStream;

        let mut stream = UnixStream::connect(&self.socket_path).map_err(|error| {
            PrismError::Io(format!(
                "model service socket {} unavailable: {error}",
                self.socket_path.display()
            ))
        })?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        let request_bytes = serde_json::to_vec(request)?;
        if request_bytes.len() > MAX_REQUEST_BYTES {
            return Err(PrismError::Invalid(format!(
                "encoded model request is {} bytes; limit is {MAX_REQUEST_BYTES}",
                request_bytes.len()
            )));
        }
        stream.write_all(&request_bytes)?;
        stream.write_all(b"\n")?;
        stream.shutdown(Shutdown::Write)?;

        let mut response = Vec::new();
        BufReader::new(stream)
            .take(MAX_RESPONSE_BYTES + 1)
            .read_until(b'\n', &mut response)?;
        if response.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(PrismError::Invalid(format!(
                "model service response exceeds {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        if response.is_empty() {
            return Err(PrismError::Io(
                "model service closed without a response".into(),
            ));
        }
        Ok(serde_json::from_slice(&response)?)
    }
}

pub struct ProductionModelPlane {
    registry: ModelRegistry,
    models: BTreeMap<(String, String), RegisteredModel>,
    transport: Arc<dyn InferenceTransport>,
}

impl ProductionModelPlane {
    pub fn new(registry: ModelRegistry, transport: Arc<dyn InferenceTransport>) -> Result<Self> {
        registry.validate()?;
        let models = registry
            .models
            .iter()
            .cloned()
            .map(|model| ((model.model_id.clone(), model.model_version.clone()), model))
            .collect();
        Ok(Self {
            registry,
            models,
            transport,
        })
    }

    /// Fail startup unless the default registered model answers one validated
    /// request under the exact artifact identity the engine will persist.
    pub fn warmup(&self) -> Result<()> {
        self.default_embedder(
            self.registry
                .resolve(
                    &self.registry.default_model_id,
                    &self.registry.default_model_version,
                )?
                .dim,
        )?
        .embed("PrismDB model-plane warmup")
        .map(|_| ())
    }

    fn registered(&self, model_id: &str, model_version: &str) -> Result<&RegisteredModel> {
        self.models
            .get(&(model_id.to_string(), model_version.to_string()))
            .ok_or_else(|| {
                PrismError::NotFound(format!(
                    "model `{model_id}:{model_version}` is not in the production registry"
                ))
            })
    }
}

impl ModelPlane for ProductionModelPlane {
    fn embedder(
        &self,
        model_id: &str,
        model_version: &str,
        dim: usize,
        expected_artifacts: Option<&ModelArtifacts>,
    ) -> Result<Arc<dyn Embedder>> {
        let model = self.registered(model_id, model_version)?;
        if model.dim != dim {
            return Err(PrismError::Invariant(format!(
                "registered model `{model_id}:{model_version}` is {}d, store is {dim}d",
                model.dim
            )));
        }
        let expected = expected_artifacts.ok_or_else(|| {
            PrismError::Invariant(format!(
                "generation `{model_id}:{model_version}` has no exact artifact provenance; \
                 production inference refuses to guess"
            ))
        })?;
        if expected != &model.artifacts {
            return Err(PrismError::Invariant(format!(
                "registry artifacts for `{model_id}:{model_version}` do not match the generation; \
                 a model reload may not silently change an embedding space"
            )));
        }
        Ok(Arc::new(RemoteEmbedder {
            model: model.clone(),
            transport: self.transport.clone(),
        }))
    }

    fn default_embedder(&self, dim: usize) -> Result<Arc<dyn Embedder>> {
        let model = self
            .registered(
                &self.registry.default_model_id,
                &self.registry.default_model_version,
            )
            .expect("validated registry must contain its default");
        if model.dim != dim {
            return Err(PrismError::Invariant(format!(
                "default registered model is {}d, store is {dim}d",
                model.dim
            )));
        }
        Ok(Arc::new(RemoteEmbedder {
            model: model.clone(),
            transport: self.transport.clone(),
        }))
    }

    fn candidate_embedder(
        &self,
        model_id: &str,
        model_version: &str,
        dim: usize,
    ) -> Result<Arc<dyn Embedder>> {
        let model = self.registered(model_id, model_version)?;
        if model.dim != dim {
            return Err(PrismError::Invariant(format!(
                "registered model `{model_id}:{model_version}` is {}d, store is {dim}d",
                model.dim
            )));
        }
        Ok(Arc::new(RemoteEmbedder {
            model: model.clone(),
            transport: self.transport.clone(),
        }))
    }
}

struct RemoteEmbedder {
    model: RegisteredModel,
    transport: Arc<dyn InferenceTransport>,
}

impl RemoteEmbedder {
    fn infer_batch(&self, texts: &[&str]) -> Result<Vec<InferenceItem>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if texts.len() > MAX_BATCH_ITEMS {
            return Err(PrismError::Invalid(format!(
                "embedding batch has {} items; limit is {MAX_BATCH_ITEMS}",
                texts.len()
            )));
        }
        let bytes: usize = texts.iter().map(|text| text.len()).sum();
        if bytes > MAX_BATCH_INPUT_BYTES {
            return Err(PrismError::Invalid(format!(
                "embedding batch has {bytes} input bytes; limit is {MAX_BATCH_INPUT_BYTES}"
            )));
        }
        let response = self.transport.infer(&InferenceRequest {
            protocol_version: 1,
            model_id: self.model.model_id.clone(),
            model_version: self.model.model_version.clone(),
            artifacts: self.model.artifacts.clone(),
            texts: texts.iter().map(|text| (*text).to_string()).collect(),
        })?;
        if response.protocol_version != 1
            || response.model_id != self.model.model_id
            || response.model_version != self.model.model_version
            || response.artifacts != self.model.artifacts
        {
            return Err(PrismError::Invariant(
                "model service response identity does not match the registered request; \
                 refusing vectors from a crashed/reloaded/wrong model"
                    .into(),
            ));
        }
        if response.outputs.len() != texts.len() {
            return Err(PrismError::Invariant(format!(
                "model service returned {} outputs for {} inputs",
                response.outputs.len(),
                texts.len()
            )));
        }
        Ok(response.outputs)
    }

    fn validate_vector(&self, mut vector: Vec<f32>) -> Result<Vec<f32>> {
        if vector.len() != self.model.dim {
            return Err(PrismError::Invariant(format!(
                "model service returned {} dimensions for registered {}d model",
                vector.len(),
                self.model.dim
            )));
        }
        if vector.iter().any(|value| !value.is_finite()) {
            return Err(PrismError::Invalid(
                "model service returned a non-finite embedding component".into(),
            ));
        }
        let norm_sq: f64 = vector
            .iter()
            .map(|value| f64::from(*value) * f64::from(*value))
            .sum();
        let norm = norm_sq.sqrt();
        if !(0.999..=1.001).contains(&norm) {
            return Err(PrismError::Invalid(format!(
                "model service returned embedding norm {norm:.6}; registered output contract is \
                 [0.999, 1.001]"
            )));
        }
        // Remove only floating-point accumulation drift after the strict norm
        // gate; this never rescales a materially wrong service response.
        validate_and_normalize(&mut vector)?;
        Ok(vector)
    }
}

impl Embedder for RemoteEmbedder {
    fn model_id(&self) -> &str {
        &self.model.model_id
    }

    fn model_version(&self) -> &str {
        &self.model.model_version
    }

    fn dim(&self) -> usize {
        self.model.dim
    }

    fn artifacts(&self) -> Option<&ModelArtifacts> {
        Some(&self.model.artifacts)
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_batch(&[text])
            .pop()
            .expect("one input always produces one result")
    }

    fn embed_batch(&self, texts: &[&str]) -> Vec<Result<Vec<f32>>> {
        let mut results = Vec::with_capacity(texts.len());
        let mut start = 0;
        while start < texts.len() {
            let mut end = start;
            let mut bytes = 0;
            while end < texts.len() && end - start < MAX_BATCH_ITEMS {
                let next = texts[end].len();
                if end > start && bytes + next > MAX_BATCH_INPUT_BYTES {
                    break;
                }
                bytes += next;
                end += 1;
            }
            let chunk = &texts[start..end];
            match self.infer_batch(chunk) {
                Ok(outputs) => results.extend(outputs.into_iter().map(|output| match output {
                    InferenceItem::Ok { vector } => self.validate_vector(vector),
                    InferenceItem::Error { error } => Err(PrismError::Invalid(format!(
                        "model service rejected input: {error}"
                    ))),
                })),
                Err(error) => {
                    let message = error.to_string();
                    results.extend(chunk.iter().map(|_| Err(PrismError::Io(message.clone()))));
                }
            }
            start = end;
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Copy)]
    enum FakeMode {
        Honest,
        WrongIdentity,
        BadDimension,
        BadNorm,
        Crash,
    }

    struct FakeTransport {
        mode: FakeMode,
        calls: AtomicUsize,
    }

    impl FakeTransport {
        fn new(mode: FakeMode) -> Self {
            Self {
                mode,
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl InferenceTransport for FakeTransport {
        fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if matches!(self.mode, FakeMode::Crash) {
                return Err(PrismError::Io("model process crashed".into()));
            }
            let dim = if matches!(self.mode, FakeMode::BadDimension) {
                7
            } else {
                8
            };
            let value = if matches!(self.mode, FakeMode::BadNorm) {
                2.0
            } else {
                1.0
            };
            let outputs = request
                .texts
                .iter()
                .map(|_| {
                    let mut vector = vec![0.0; dim];
                    vector[0] = value;
                    InferenceItem::Ok { vector }
                })
                .collect();
            Ok(InferenceResponse {
                protocol_version: 1,
                model_id: if matches!(self.mode, FakeMode::WrongIdentity) {
                    "reloaded-wrong-model".into()
                } else {
                    request.model_id.clone()
                },
                model_version: request.model_version.clone(),
                artifacts: request.artifacts.clone(),
                outputs,
            })
        }
    }

    fn artifacts(byte: char) -> ModelArtifacts {
        ModelArtifacts::new(byte.to_string().repeat(64), "b".repeat(64), "c".repeat(64)).unwrap()
    }

    fn registered_model() -> RegisteredModel {
        let artifacts = artifacts('a');
        RegisteredModel {
            model_id: "sentence-transformer".into(),
            model_version: artifacts.revision(),
            dim: 8,
            artifacts,
        }
    }

    fn registry() -> ModelRegistry {
        let model = registered_model();
        ModelRegistry {
            default_model_id: model.model_id.clone(),
            default_model_version: model.model_version.clone(),
            models: vec![model],
        }
    }

    #[test]
    fn resolves_an_older_model_version() {
        let p = HashModelPlane::at_version("2");
        let old = p.embedder("hash-embedder", "1", 32, None).unwrap();
        assert_eq!(old.model_version(), "1");
        assert_eq!(p.default_embedder(32).unwrap().model_version(), "2");
    }

    #[test]
    fn an_unknown_model_is_an_error_not_a_substitution() {
        let p = HashModelPlane::new();
        assert!(p.embedder("some-transformer", "1", 32, None).is_err());
    }

    #[test]
    fn registry_requires_content_derived_model_versions() {
        let mut registry = registry();
        registry.models[0].model_version = "latest".into();
        registry.default_model_version = "latest".into();
        assert!(registry
            .validate()
            .unwrap_err()
            .to_string()
            .contains("mutable aliases"));
    }

    #[test]
    fn committed_registry_example_is_valid() {
        let registry: ModelRegistry =
            serde_json::from_str(include_str!("../../../testing/model-registry.example.json"))
                .unwrap();
        registry.validate().unwrap();
    }

    #[test]
    fn remote_embedder_batches_and_validates_outputs() {
        let transport = Arc::new(FakeTransport::new(FakeMode::Honest));
        let plane = ProductionModelPlane::new(registry(), transport.clone()).unwrap();
        let model = registered_model();
        let embedder = plane
            .embedder(
                &model.model_id,
                &model.model_version,
                model.dim,
                Some(&model.artifacts),
            )
            .unwrap();

        let results = embedder.embed_batch(&["one", "two", "three"]);
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        assert_eq!(results.len(), 3);
        assert!(results.into_iter().all(|result| result.is_ok()));
        assert_eq!(embedder.artifacts(), Some(&model.artifacts));
    }

    #[test]
    fn oversized_work_is_split_into_bounded_batches() {
        let transport = Arc::new(FakeTransport::new(FakeMode::Honest));
        let plane = ProductionModelPlane::new(registry(), transport.clone()).unwrap();
        let model = registered_model();
        let embedder = plane
            .embedder(
                &model.model_id,
                &model.model_version,
                model.dim,
                Some(&model.artifacts),
            )
            .unwrap();
        let owned: Vec<String> = (0..(MAX_BATCH_ITEMS + 1))
            .map(|index| format!("item-{index}"))
            .collect();
        let texts: Vec<&str> = owned.iter().map(String::as_str).collect();

        assert!(embedder
            .embed_batch(&texts)
            .into_iter()
            .all(|result| result.is_ok()));
        assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn wrong_identity_after_reload_never_returns_a_vector() {
        let plane = ProductionModelPlane::new(
            registry(),
            Arc::new(FakeTransport::new(FakeMode::WrongIdentity)),
        )
        .unwrap();
        let model = registered_model();
        let embedder = plane
            .embedder(
                &model.model_id,
                &model.model_version,
                model.dim,
                Some(&model.artifacts),
            )
            .unwrap();
        let error = embedder.embed("query").unwrap_err().to_string();
        assert!(error.contains("response identity"));
    }

    #[test]
    fn crash_and_malformed_outputs_fail_closed() {
        for (mode, expected) in [
            (FakeMode::Crash, "crashed"),
            (FakeMode::BadDimension, "dimensions"),
            (FakeMode::BadNorm, "norm"),
        ] {
            let plane =
                ProductionModelPlane::new(registry(), Arc::new(FakeTransport::new(mode))).unwrap();
            let model = registered_model();
            let embedder = plane
                .embedder(
                    &model.model_id,
                    &model.model_version,
                    model.dim,
                    Some(&model.artifacts),
                )
                .unwrap();
            let error = embedder.embed("query").unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn generation_artifacts_must_match_registry_exactly() {
        let plane =
            ProductionModelPlane::new(registry(), Arc::new(FakeTransport::new(FakeMode::Honest)))
                .unwrap();
        let model = registered_model();
        let wrong = artifacts('d');
        let mismatched = match plane.embedder(
            &model.model_id,
            &model.model_version,
            model.dim,
            Some(&wrong),
        ) {
            Ok(_) => panic!("mismatched artifacts were accepted"),
            Err(error) => error,
        };
        assert!(mismatched.to_string().contains("do not match"));
        let absent = match plane.embedder(&model.model_id, &model.model_version, model.dim, None) {
            Ok(_) => panic!("missing artifact provenance was accepted"),
            Err(error) => error,
        };
        assert!(absent.to_string().contains("no exact artifact provenance"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_transport_round_trips_the_bounded_protocol() {
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

        static SOCKET_COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "prism-model-{}-{}.sock",
            std::process::id(),
            SOCKET_COUNTER.fetch_add(1, AtomicOrdering::SeqCst)
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: InferenceRequest = serde_json::from_str(&line).unwrap();
            let response = InferenceResponse {
                protocol_version: 1,
                model_id: request.model_id,
                model_version: request.model_version,
                artifacts: request.artifacts,
                outputs: request
                    .texts
                    .iter()
                    .map(|_| InferenceItem::Ok {
                        vector: vec![1.0, 0.0],
                    })
                    .collect(),
            };
            serde_json::to_writer(&stream, &response).unwrap();
            (&stream).write_all(b"\n").unwrap();
        });

        let model_artifacts = artifacts('a');
        let transport = UnixInferenceTransport::new(&path, Duration::from_secs(2));
        let response = transport
            .infer(&InferenceRequest {
                protocol_version: 1,
                model_id: "m".into(),
                model_version: model_artifacts.revision(),
                artifacts: model_artifacts,
                texts: vec!["one".into(), "two".into()],
            })
            .unwrap();
        assert_eq!(response.outputs.len(), 2);
        server.join().unwrap();
        std::fs::remove_file(path).ok();
    }
}
