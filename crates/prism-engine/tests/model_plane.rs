//! S13 model-plane gate: a crash or wrong-model reload cannot publish or
//! mislabel semantic bytes, and every successful generation records exact
//! artifact hashes.

use prism_engine::corpus::{self, Kind};
use prism_engine::{
    Engine, InferenceItem, InferenceRequest, InferenceResponse, InferenceTransport, ModelRegistry,
    ProductionModelPlane, RegisteredModel,
};
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::error::{PrismError, Result};
use prism_types::hash::sha256;
use prism_types::{validate_and_normalize, ModelArtifacts, Query};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "prism-model-plane-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn config() -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 8,
        nlist: 4,
        pq_m: 2,
        seed: 42,
        kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        partitions: Default::default(),
        promote: Vec::new(),
    }
}

fn artifacts() -> ModelArtifacts {
    ModelArtifacts::new("a".repeat(64), "b".repeat(64), "c".repeat(64)).unwrap()
}

fn registry() -> ModelRegistry {
    let artifacts = artifacts();
    let model = RegisteredModel {
        model_id: "registered-test-model".into(),
        model_version: artifacts.revision(),
        dim: 8,
        artifacts,
    };
    ModelRegistry {
        default_model_id: model.model_id.clone(),
        default_model_version: model.model_version.clone(),
        models: vec![model],
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Honest,
    Crash,
    WrongReload,
}

struct Transport {
    mode: Mode,
}

impl InferenceTransport for Transport {
    fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        if matches!(self.mode, Mode::Crash) {
            return Err(PrismError::Io("inference process exited".into()));
        }
        let outputs = request
            .texts
            .iter()
            .map(|text| {
                let digest = sha256(text.as_bytes());
                let mut vector: Vec<f32> = digest[..8]
                    .iter()
                    .map(|byte| (f32::from(*byte) - 127.5) / 127.5)
                    .collect();
                validate_and_normalize(&mut vector).unwrap();
                InferenceItem::Ok { vector }
            })
            .collect();
        Ok(InferenceResponse {
            protocol_version: 1,
            model_id: if matches!(self.mode, Mode::WrongReload) {
                "different-model-after-reload".into()
            } else {
                request.model_id.clone()
            },
            model_version: request.model_version.clone(),
            artifacts: request.artifacts.clone(),
            outputs,
        })
    }
}

fn plane(mode: Mode) -> Arc<ProductionModelPlane> {
    Arc::new(
        ProductionModelPlane::new(registry(), Arc::new(Transport { mode }))
            .expect("valid test registry"),
    )
}

#[test]
fn model_crash_and_wrong_reload_leave_the_catalog_unchanged() {
    let root = tmp();
    let engine = Engine::init(&root, config())
        .unwrap()
        .with_plane(plane(Mode::Honest));
    let first = engine
        .ingest(corpus::generate(Kind::Uniform, 80, 11), 1_760_000_000_000)
        .unwrap();
    assert_eq!(first.admitted, 80);

    let generation = engine
        .catalog()
        .get_generation(&first.generation_id)
        .unwrap();
    assert_eq!(generation.model_artifacts, Some(artifacts()));
    assert_eq!(generation.model_version, artifacts().revision());
    generation.verify_content_address().unwrap();
    let stable_snapshot = engine.snapshot().unwrap();
    let stable_parts = stable_snapshot.part_ids();
    drop(engine);

    // A serving process that reloaded different bytes/identity cannot answer a
    // query in the persisted space. The catalog and parts remain untouched.
    let wrong = Engine::open(&root)
        .unwrap()
        .with_plane(plane(Mode::WrongReload));
    let error = wrong
        .search(&Query {
            text: "tool call timed out".into(),
            ..Default::default()
        })
        .unwrap_err();
    assert!(error.to_string().contains("response identity"));
    assert_eq!(
        wrong.snapshot().unwrap().snapshot_id,
        stable_snapshot.snapshot_id
    );
    assert_eq!(wrong.snapshot().unwrap().part_ids(), stable_parts);
    drop(wrong);

    // A process crash during ingest dead-letters the whole batch. No partial
    // part and no new snapshot become visible under the old generation label.
    let crashed = Engine::open(&root).unwrap().with_plane(plane(Mode::Crash));
    let report = crashed
        .ingest(corpus::generate(Kind::Uniform, 20, 19), 1_760_000_000_001)
        .unwrap();
    assert_eq!(report.admitted, 0);
    assert_eq!(report.dead_lettered, 20);
    assert!(report.part_id.is_none());
    assert_eq!(
        crashed.snapshot().unwrap().snapshot_id,
        stable_snapshot.snapshot_id
    );
    assert_eq!(crashed.snapshot().unwrap().part_ids(), stable_parts);

    std::fs::remove_dir_all(root).ok();
}
