//! CLI-level proof that a remote configuration cannot silently select plaintext.

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn prism() -> &'static str {
    env!("CARGO_BIN_EXE_prism")
}

fn store_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "prism-transport-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ))
}

#[test]
fn remote_init_selects_production_tls_without_an_override() {
    let store = store_path("tls");

    let output = Command::new(prism())
        .args(["init", "--path", store.to_str().expect("UTF-8 path")])
        .env("PRISM_S3_ENDPOINT", "s3.us-east-1.amazonaws.com:443")
        .env("AWS_ACCESS_KEY_ID", "test-access-key")
        .env("AWS_SECRET_ACCESS_KEY", "test-secret-key")
        .env("AWS_SESSION_TOKEN", "test-session-token")
        .env_remove("PRISM_ALLOW_INSECURE_S3")
        .output()
        .expect("run prism");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(store.exists());
}

#[test]
fn local_minio_requires_a_conspicuous_plaintext_override() {
    let store = store_path("local");

    let output = Command::new(prism())
        .args(["init", "--path", store.to_str().expect("UTF-8 path")])
        .env("PRISM_S3_ENDPOINT", "127.0.0.1:9000")
        .env("PRISM_ALLOW_INSECURE_S3", "true")
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .output()
        .expect("run prism");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(store.exists());
}

#[test]
fn plaintext_override_cannot_target_a_remote_hostname() {
    let store = store_path("remote");

    let output = Command::new(prism())
        .args(["init", "--path", store.to_str().expect("UTF-8 path")])
        .env("PRISM_S3_ENDPOINT", "minio.corp.example:9000")
        .env("PRISM_ALLOW_INSECURE_S3", "true")
        .output()
        .expect("run prism");

    assert!(!output.status.success(), "remote plaintext endpoint booted");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("loopback-only"), "{stderr}");
    assert!(!store.exists());
}

#[test]
fn production_transport_rejects_implicit_development_credentials() {
    let store = store_path("missing-credentials");

    let output = Command::new(prism())
        .args(["init", "--path", store.to_str().expect("UTF-8 path")])
        .env("PRISM_S3_ENDPOINT", "s3.us-east-1.amazonaws.com:443")
        .env_remove("PRISM_ALLOW_INSECURE_S3")
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .output()
        .expect("run prism");

    assert!(
        !output.status.success(),
        "production accepted implicit credentials"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("production S3 requires a non-empty AWS_ACCESS_KEY_ID"),
        "{stderr}"
    );
    assert!(!store.exists());
}

#[test]
fn shard_server_has_no_plaintext_or_optional_client_auth_mode() {
    let output = Command::new(prism())
        .args([
            "shard-serve",
            "--path",
            "/does/not/matter",
            "--listen",
            "127.0.0.1:7443",
            "--shard-id",
            "0",
        ])
        .output()
        .expect("run prism");

    assert!(!output.status.success(), "shard server booted without mTLS");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing required flag --cert"), "{stderr}");
}

#[test]
fn shard_server_refuses_an_ephemeral_production_port() {
    let output = Command::new(prism())
        .args([
            "shard-serve",
            "--path",
            "/does/not/matter",
            "--listen",
            "127.0.0.1:0",
            "--shard-id",
            "0",
            "--cert",
            "/does/not/matter",
            "--key",
            "/does/not/matter",
            "--client-ca",
            "/does/not/matter",
        ])
        .output()
        .expect("run prism");

    assert!(!output.status.success(), "shard server accepted port zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("port 0 is test-only"), "{stderr}");
}

#[test]
fn remote_coordinator_has_no_plaintext_or_anonymous_mode() {
    let topology = store_path("coordinator-topology");
    std::fs::write(
        &topology,
        r#"{
          "version": 1,
          "shards": [{
            "shard_id": 0,
            "address": "127.0.0.1:7443",
            "server_name": "shard.test"
          }]
        }"#,
    )
    .unwrap();
    let output = Command::new(prism())
        .args([
            "coordinator-search",
            "--topology",
            topology.to_str().expect("UTF-8 path"),
            "--query",
            "payment service",
        ])
        .output()
        .expect("run prism");

    assert!(
        !output.status.success(),
        "remote coordinator booted without a client identity"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing required flag --cert"), "{stderr}");
    std::fs::remove_file(topology).unwrap();
}
