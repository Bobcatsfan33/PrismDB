//! The D-094 disaster drill, **with encryption enabled** (S14, [D-095](../../../../docs/DECISIONS.md),
//! [encryption contract §11](../../../../docs/ENCRYPTION-CONTRACT.md)).
//!
//! The capstone. Every other S14 gate proves one mechanism in isolation; this one proves they
//! compose under the operation they exist for — losing a node and rebuilding it from the object
//! store alone, through the real `prism` binary, with every durable byte sealed.
//!
//! It is the plaintext drill's twin on purpose. Same shape, same assertions, same process
//! isolation, plus the three things encryption adds:
//!
//! - the replacement node restores through the **CLI's** key service, not a test harness one;
//! - the acked-but-unpublished tail is a **sealed** remote admission record, and is still recovered
//!   in full, because the ack is the promise and encryption does not get to weaken it;
//! - **no event body appears anywhere in the bucket** — the property all of it is for.
//!
//! **Scope label, inherited verbatim and not quietly upgraded.** The replacement node is
//! *process-isolated, not host-isolated*: a separate process with its own data root over a durable
//! object store on the same machine. And the key custody is the **software keystore**, so this
//! proves the *code path*, not the custody ([§11](../../../../docs/ENCRYPTION-CONTRACT.md)). A live
//! KMS run stays an external gate, and the receipt below names the backend so no reader can mistake
//! one for the other.

use prism_engine::keys::{KeyProvider, SoftwareKeystore};
use prism_engine::storage::object::{CachedObjectStore, LocalObjectStore, ObjectStore};
use prism_engine::storage::CACHE_QUOTA_BYTES;
use prism_engine::wal::{RemoteWal, WalCrypto, WalRecord};
use prism_engine::{Engine, Ingestor};
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Query;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static N: AtomicU64 = AtomicU64::new(0);

fn prism() -> &'static str {
    env!("CARGO_BIN_EXE_prism")
}

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let path =
        std::env::temp_dir().join(format!("prism-encdrill-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn config() -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 64,
        nlist: 16,
        pq_m: 8,
        seed: 9,
        kmeans_restarts: 1,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        partitions: Default::default(),
        promote: Vec::new(),
    }
}

const ACTIVE_KEY: &str = "drill-key-v1";
const ACTIVE_KEY_HEX: &str = "1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f";
const ACTIVE_KEY_BYTES: [u8; 32] = [0x1f; 32];

/// The keystore file the replacement node's `prism` processes are given. Mode 600, because the CLI
/// refuses to read one that other accounts on the host can.
fn keystore_file(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("keys.json");
    std::fs::write(
        &path,
        format!(r#"{{"active":"{ACTIVE_KEY}","keys":{{"{ACTIVE_KEY}":"{ACTIVE_KEY_HEX}"}}}}"#),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

fn keys() -> Arc<dyn KeyProvider> {
    Arc::new(SoftwareKeystore::new(ACTIVE_KEY, ACTIVE_KEY_BYTES))
}

fn cold(backend: &Arc<dyn ObjectStore>) -> Arc<CachedObjectStore> {
    Arc::new(CachedObjectStore::new(
        Arc::clone(backend),
        CACHE_QUOTA_BYTES,
    ))
}

fn engine_on(root: &Path, backend: &Arc<dyn ObjectStore>) -> Engine {
    Engine::init(root, config())
        .unwrap()
        .with_cold(cold(backend))
        .with_keys(keys())
}

fn open_on(root: &Path, backend: &Arc<dyn ObjectStore>) -> Engine {
    Engine::open(root)
        .unwrap()
        .with_cold(cold(backend))
        .with_keys(keys())
}

fn answer(engine: &Engine) -> Vec<String> {
    engine
        .search(&Query {
            text: "the tool call timed out retrying".into(),
            k: 25,
            tenant: Some("tenant-northwind-t1-inc".into()),
            rerank: 50,
            ..Default::default()
        })
        .unwrap()
        .hits
        .into_iter()
        .map(|hit| hit.event.event_id)
        .collect()
}

/// Run a `prism` subcommand in its own process, with the staging keystore in its environment.
fn run(store_dir: &Path, keystore: &Path, args: &[&str]) -> std::process::Output {
    Command::new(prism())
        .args(args)
        .env("PRISM_OBJECT_STORE_DIR", store_dir)
        .env("PRISM_STAGING_KEYSTORE_FILE", keystore)
        .env_remove("PRISM_S3_ENDPOINT")
        .output()
        .expect("run prism")
}

fn ok(out: &std::process::Output, what: &str) -> String {
    assert!(
        out.status.success(),
        "{what} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The longest event bodies in a corpus — the most distinctive strings to hunt for in the bucket.
fn distinctive_bodies(events: &[prism_types::Event], n: usize) -> Vec<String> {
    let mut bodies: Vec<String> = events
        .iter()
        .map(|e| e.body.clone())
        .filter(|b| b.len() >= 24)
        .collect();
    bodies.sort_by_key(|b| std::cmp::Reverse(b.len()));
    bodies.dedup();
    bodies.truncate(n);
    assert!(
        !bodies.is_empty(),
        "the corpus must produce bodies long enough to search for"
    );
    bodies
}

/// Every distinct tenant name in a corpus — the DATA-01 needle.
fn tenant_names(events: &[prism_types::Event]) -> Vec<String> {
    let mut v: Vec<String> = events
        .iter()
        .map(|e| e.tenant_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    v.sort();
    assert!(v.len() >= 2, "the drill must span several distinct tenants");
    v
}

/// Assert no event body is legible in any object the bucket holds.
fn assert_no_plaintext_bodies(backend: &Arc<dyn ObjectStore>, bodies: &[String], when: &str) {
    let mut scanned = 0usize;
    for prefix in ["parts/", "wal/", "catalog/", "generations/"] {
        for key in backend.list(prefix).unwrap() {
            let bytes = backend.get(&key).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            for body in bodies {
                assert!(
                    !text.contains(body.as_str()),
                    "{when}: object `{key}` holds an event body in the clear"
                );
            }
            scanned += 1;
        }
    }
    assert!(
        scanned > 3,
        "{when}: only {scanned} object(s) scanned -- this check is worthless if the bucket is empty"
    );
}

#[test]
fn the_disaster_drill_restores_an_encrypted_replacement_node_from_backup_alone() {
    let store_dir = tmp("objstore");
    std::fs::create_dir_all(&store_dir).unwrap();
    let backend: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(&store_dir));
    let keystore = keystore_file(&tmp("keystore"));

    let root_a = tmp("node-a");
    let root_b = tmp("node-b");

    // 1. Publish a customer-shaped dataset on node A, encrypted.
    // Long, distinctive tenant names: a two-character needle turns up inside ciphertext by chance,
    // so scanning for `t0` would prove nothing (the sealing gates learned this the hard way).
    let named = |mut e: prism_types::Event| {
        e.tenant_id = format!("tenant-northwind-{}-inc", e.tenant_id);
        e
    };
    let published: Vec<prism_types::Event> =
        prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 900, 5)
            .into_iter()
            .map(named)
            .collect();
    let engine_a = engine_on(&root_a, &backend);
    engine_a.acquire_ownership().unwrap();
    engine_a
        .ingest(published.clone(), 1_760_000_000_000)
        .unwrap();
    let expected_answer = answer(&engine_a);
    let expected_snapshot = engine_a.snapshot().unwrap();
    assert!(!expected_answer.is_empty(), "the drill needs a real answer");
    for id in expected_snapshot.part_ids() {
        assert!(
            engine_a.open_part(&id).unwrap().is_encrypted(),
            "the drill is only meaningful if node A's parts are actually sealed: {id}"
        );
    }

    let backup = engine_a.backup_published().unwrap();
    assert_eq!(backup.snapshot_id, expected_snapshot.snapshot_id);
    assert_eq!(backup.parts.len(), expected_snapshot.part_ids().len());

    // 2. Acknowledge further events that are NOT yet published, into a SEALED remote admission log.
    let acked_only: Vec<prism_types::Event> =
        prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 120, 11)
            .into_iter()
            .map(named)
            .collect();
    let writer_a = Ingestor::open_replicated(open_on(&root_a, &backend), 0).unwrap();
    let remote_wal = RemoteWal::new(Arc::clone(&backend), 0)
        .with_crypto(Arc::new(WalCrypto::new(keys()).unwrap()));
    let record_id = remote_wal
        .next_record_id(writer_a.engine.ownership_epoch(), &[])
        .unwrap();
    let record = WalRecord {
        record_id,
        events: acked_only.clone(),
        source: None,
        source_offset: None,
        created_at_ms: 1_760_000_000_001,
    };
    writer_a.wal.append_record(&record).unwrap();
    remote_wal.append(&record).unwrap();
    let recovery_point_events = acked_only.len();

    // Everything durable is now in the bucket. Before touching recovery: is any of it legible?
    let bodies = distinctive_bodies(&published, 5);
    let acked_bodies = distinctive_bodies(&acked_only, 5);
    assert_no_plaintext_bodies(&backend, &bodies, "after backup");
    // **DATA-01: no tenant NAME either.** Same mechanism that proved no event body appears, applied
    // to the identity half. Armed by `tenant_names`, which requires several distinct tenants.
    let names = tenant_names(&published);
    assert_no_plaintext_bodies(&backend, &names, "after backup (tenant names)");
    assert_no_plaintext_bodies(
        &backend,
        &acked_bodies,
        "after the acked tail was made durable",
    );

    // The complete expected answer AFTER the acked tail is published, derived independently by a
    // clean *plaintext* engine — so the comparison also re-proves answer-invariance end to end.
    let baseline_root = tmp("baseline");
    let baseline = Engine::init(&baseline_root, config()).unwrap();
    baseline
        .ingest(published.clone(), 1_760_000_000_000)
        .unwrap();
    baseline
        .ingest(acked_only.clone(), 1_760_000_000_002)
        .unwrap();
    let expected_after_replay = answer(&baseline);

    // 3. Destroy the node-local disk.
    drop(writer_a);
    std::fs::remove_dir_all(&root_a).unwrap();
    assert!(!root_a.exists());

    // 4-5. A replacement node: separate process, own data root, key service from the environment.
    let recovery_started = prism_engine::engine::now_ms();
    ok(
        &run(
            &store_dir,
            &keystore,
            &["init", "--path", root_b.to_str().unwrap()],
        ),
        "init the replacement node",
    );
    let hydrate_out = ok(
        &run(
            &store_dir,
            &keystore,
            &["hydrate", "--path", root_b.to_str().unwrap()],
        ),
        "hydrate the encrypted replacement node",
    );
    let hydrated: serde_json::Value = serde_json::from_str(hydrate_out.trim()).unwrap();
    assert_eq!(hydrated["status"], "hydrated");
    assert_eq!(hydrated["snapshot_id"], expected_snapshot.snapshot_id);
    assert_eq!(
        hydrated["parts"].as_u64().unwrap() as usize,
        expected_snapshot.part_ids().len()
    );

    let restored = open_on(&root_b, &backend);
    assert_eq!(
        answer(&restored),
        expected_answer,
        "an encrypted node restored from backup must answer byte-for-byte identically"
    );
    assert_eq!(
        restored.snapshot().unwrap().snapshot_id,
        expected_snapshot.snapshot_id
    );

    // Hydration installed CIPHERTEXT and decrypts on read; it never staged plaintext (§5).
    for id in restored.snapshot().unwrap().part_ids() {
        let reader = restored.open_part(&id).unwrap();
        assert!(
            reader.is_encrypted(),
            "restored part {id} must still be sealed -- hydration must not stage plaintext"
        );
        assert_eq!(
            reader.encryption_envelope().unwrap().wrapping_key_id,
            ACTIVE_KEY
        );
    }
    drop(restored);

    // 6. Replay the sealed remote admission log — the only route to the acked tail.
    let recover_out = ok(
        &run(
            &store_dir,
            &keystore,
            &[
                "recover",
                "--path",
                root_b.to_str().unwrap(),
                "--replicated",
                "true",
                "--shard-id",
                "0",
            ],
        ),
        "replay the sealed remote admission log",
    );
    let recovered: serde_json::Value = serde_json::from_str(recover_out.trim()).unwrap();
    assert_eq!(
        recovered["recovered_events"].as_u64().unwrap() as usize,
        recovery_point_events,
        "every acknowledged event must be recovered -- the ack is the promise, and sealing it \
         does not weaken it"
    );
    let recovery_time_ms = prism_engine::engine::now_ms().saturating_sub(recovery_started);

    // 7. The complete answer after replay equals the independently derived plaintext expectation.
    let replayed = open_on(&root_b, &backend);
    assert_eq!(
        answer(&replayed),
        expected_after_replay,
        "after replay the restored encrypted node must equal a clean plaintext engine fed the \
         same events"
    );

    // 8. And the bucket is still opaque, now including everything recovery published.
    assert_no_plaintext_bodies(&backend, &bodies, "after recovery");
    assert_no_plaintext_bodies(&backend, &names, "after recovery (tenant names)");
    assert_no_plaintext_bodies(&backend, &acked_bodies, "after recovery");

    // 9. The receipt. It names the backend, because a gate passed against a software keystore has
    //    proven the code and not the custody (§11).
    let receipt = serde_json::json!({
        "drill": "published-part backup and hydration (D-094), encryption enabled (D-095)",
        "isolation": "process-isolated, not host-isolated",
        "key_backend": prism_engine::keys::BACKEND_SOFTWARE_KEYSTORE,
        "scope_label": "staging-shaped measurements; proves the CODE PATH, not key custody. \
                        NOT customer-scale RPO/RTO evidence (EXT-DR), NOT independent-host \
                        evidence (P14), NOT a live-KMS run (external gate)",
        "recovery_point": {
            "acknowledged_events_recovered": recovery_point_events,
            "acknowledged_events_lost": 0,
        },
        "recovery_time_ms_staging": recovery_time_ms,
        "restored_parts": expected_snapshot.part_ids().len(),
        "restored_bytes": backup.bytes,
        "plaintext_bodies_found_in_bucket": 0,
    });
    assert_eq!(receipt["recovery_point"]["acknowledged_events_lost"], 0);
    assert_eq!(receipt["key_backend"], "software-keystore");
    eprintln!("{receipt}");

    drop(replayed);
    for r in [root_b, baseline_root, store_dir] {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn a_replacement_node_without_the_keystore_refuses_to_hydrate_an_encrypted_backup() {
    // The other half of the capstone: the bucket alone is not enough. A replacement node that has
    // every byte and none of the keys must refuse by name and install nothing -- which is what
    // makes the ciphertext in the bucket worth anything.
    let store_dir = tmp("nokeys-objstore");
    std::fs::create_dir_all(&store_dir).unwrap();
    let backend: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(&store_dir));
    let keystore = keystore_file(&tmp("nokeys-keystore"));

    let root_a = tmp("nokeys-node-a");
    let engine_a = engine_on(&root_a, &backend);
    engine_a
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 300, 5),
            1_760_000_000_000,
        )
        .unwrap();
    engine_a.backup_published().unwrap();
    drop(engine_a);

    let root_b = tmp("nokeys-node-b");
    // Note the absence: no PRISM_STAGING_KEYSTORE_FILE in this process's environment.
    let init = Command::new(prism())
        .args(["init", "--path", root_b.to_str().unwrap()])
        .env("PRISM_OBJECT_STORE_DIR", &store_dir)
        .env_remove("PRISM_STAGING_KEYSTORE_FILE")
        .env_remove("PRISM_S3_ENDPOINT")
        .output()
        .unwrap();
    assert!(init.status.success());

    let out = Command::new(prism())
        .args(["hydrate", "--path", root_b.to_str().unwrap()])
        .env("PRISM_OBJECT_STORE_DIR", &store_dir)
        .env_remove("PRISM_STAGING_KEYSTORE_FILE")
        .env_remove("PRISM_S3_ENDPOINT")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a keyless node must not report a successful restore of an encrypted backup"
    );
    let err = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        err.contains("no key service configured"),
        "the refusal must name the missing key service: {err}"
    );

    let parts = root_b.join("parts");
    if parts.exists() {
        for entry in std::fs::read_dir(&parts).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                name.starts_with(".hydrate-"),
                "a refused keyless restore installed part `{name}`"
            );
        }
    }

    // The keystore exists; it was simply never given to that process. Hand it over and the same
    // node restores -- so the refusal was about custody, not about a broken backup.
    ok(
        &run(
            &store_dir,
            &keystore,
            &["hydrate", "--path", root_b.to_str().unwrap()],
        ),
        "hydrate once the keystore is supplied",
    );

    for r in [root_a, root_b, store_dir] {
        let _ = std::fs::remove_dir_all(r);
    }
}
