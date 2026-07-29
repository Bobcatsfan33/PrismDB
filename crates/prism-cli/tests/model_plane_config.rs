//! CLI-level S13 proof: model-plane configuration is fail-closed before store
//! creation, and the configured process performs a real startup warmup.

#![cfg(unix)]

use prism_engine::{
    InferenceItem, InferenceRequest, InferenceResponse, ModelRegistry, RegisteredModel,
};
use prism_types::ModelArtifacts;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn prism() -> &'static str {
    env!("CARGO_BIN_EXE_prism")
}

fn work_dir(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "prism-model-config-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn registry() -> ModelRegistry {
    let artifacts = ModelArtifacts::new("a".repeat(64), "b".repeat(64), "c".repeat(64)).unwrap();
    let model = RegisteredModel {
        model_id: "registered-cli-test".into(),
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

fn write_registry(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("registry.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&registry()).unwrap()).unwrap();
    path
}

fn write_policy(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let model = &registry().models[0];
    let path = dir.join("model-policy.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "default_action": "deny",
            "tenants": [{
                "tenant_id": "acme",
                "grants": [{
                    "model_id": model.model_id,
                    "model_version": model.model_version,
                    "purposes": ["ingest", "query", "migration", "evaluation"]
                }],
                "max_inputs_per_minute": 1000,
                "max_input_bytes_per_minute": 1048576
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn serve_one_warmup(listener: UnixListener) {
    let (mut stream, _) = listener.accept().unwrap();
    let mut request_line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut request_line)
        .unwrap();
    let request: InferenceRequest = serde_json::from_str(&request_line).unwrap();
    assert_eq!(request.protocol_version, 1);
    assert_eq!(request.texts, ["PrismDB model-plane warmup"]);
    let response = InferenceResponse {
        protocol_version: 1,
        model_id: request.model_id,
        model_version: request.model_version,
        artifacts: request.artifacts,
        outputs: vec![InferenceItem::Ok {
            vector: vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        }],
    };
    serde_json::to_writer(&mut stream, &response).unwrap();
    stream.write_all(b"\n").unwrap();
}

#[test]
fn partial_model_configuration_fails_before_creating_a_store() {
    let dir = work_dir("partial");
    let store = dir.join("store");
    let registry = write_registry(&dir);

    let output = Command::new(prism())
        .args([
            "init",
            "--path",
            store.to_str().unwrap(),
            "--dim",
            "8",
            "--nlist",
            "4",
            "--pq-m",
            "2",
        ])
        .env("PRISM_MODEL_REGISTRY", &registry)
        .env_remove("PRISM_MODEL_SOCKET")
        .env_remove("PRISM_S3_ENDPOINT")
        .output()
        .expect("run prism");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("must be configured together"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!store.exists(), "failed startup left a partial store");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn configured_init_performs_a_real_identity_checked_warmup() {
    let dir = work_dir("warmup");
    let store = dir.join("store");
    let registry_path = write_registry(&dir);
    let policy_path = write_policy(&dir);
    let audit_path = dir.join("model-usage.jsonl");
    let socket = dir.join("model.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || serve_one_warmup(listener));

    let output = Command::new(prism())
        .args([
            "init",
            "--path",
            store.to_str().unwrap(),
            "--dim",
            "8",
            "--nlist",
            "4",
            "--pq-m",
            "2",
        ])
        .env("PRISM_MODEL_REGISTRY", &registry_path)
        .env("PRISM_MODEL_SOCKET", &socket)
        .env("PRISM_MODEL_TIMEOUT_MS", "2000")
        .env("PRISM_MODEL_POLICY", &policy_path)
        .env("PRISM_MODEL_AUDIT_LOG", &audit_path)
        .env_remove("PRISM_ALLOW_UNGOVERNED_MODEL")
        .env_remove("PRISM_S3_ENDPOINT")
        .output()
        .expect("run prism");

    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(store.exists());
    assert!(audit_path.exists());
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn production_model_without_tenant_governance_fails_before_store_creation() {
    let dir = work_dir("governance");
    let store = dir.join("store");
    let registry_path = write_registry(&dir);
    let socket = dir.join("model.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || serve_one_warmup(listener));

    let output = Command::new(prism())
        .args([
            "init",
            "--path",
            store.to_str().unwrap(),
            "--dim",
            "8",
            "--nlist",
            "4",
            "--pq-m",
            "2",
        ])
        .env("PRISM_MODEL_REGISTRY", &registry_path)
        .env("PRISM_MODEL_SOCKET", &socket)
        .env("PRISM_MODEL_TIMEOUT_MS", "2000")
        .env_remove("PRISM_MODEL_POLICY")
        .env_remove("PRISM_MODEL_AUDIT_LOG")
        .env_remove("PRISM_ALLOW_UNGOVERNED_MODEL")
        .env_remove("PRISM_S3_ENDPOINT")
        .output()
        .expect("run prism");

    server.join().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("production model inference requires PRISM_MODEL_POLICY"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!store.exists(), "failed governance left a partial store");
    std::fs::remove_dir_all(dir).ok();
}
