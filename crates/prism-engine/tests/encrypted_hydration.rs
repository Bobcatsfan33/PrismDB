//! Restoring an encrypted store fails closed **without corruption** (S14, [D-094](../../../../docs/DECISIONS.md),
//! [encryption contract §8](../../../../docs/ENCRYPTION-CONTRACT.md)).
//!
//! Fail-closed on its own is cheap — anything that refuses is fail-closed. The property the contract
//! actually claims is *fail-closed without corruption*: a key service that is unreachable, denied,
//! revoked, or simply missing this store's key must leave the node exactly as it found it, and a
//! retry once access returns must succeed cleanly rather than needing an operator to clean up
//! first.
//!
//! That is why decryption sits **inside** the D-094 staging boundary: decrypt, verify, then rename.
//! A restore that installed parts first and discovered it could not open them afterwards would
//! leave a half-restored node behind every KMS blip.

use prism_engine::keys::{KeyFault, KeyProvider, SoftwareKeystore};
use prism_engine::storage::object::{CachedObjectStore, LocalObjectStore, ObjectStore};
use prism_engine::storage::CACHE_QUOTA_BYTES;
use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Query;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-enchyd-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
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

fn events() -> Vec<prism_types::Event> {
    prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 400, 4)
}

fn answer(engine: &Engine) -> Vec<String> {
    engine
        .search(&Query {
            text: "the tool call timed out retrying".into(),
            k: 15,
            tenant: Some("t1".into()),
            rerank: 40,
            ..Default::default()
        })
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.event.event_id)
        .collect()
}

fn cold(backend: &Arc<dyn ObjectStore>) -> Arc<CachedObjectStore> {
    Arc::new(CachedObjectStore::new(
        Arc::clone(backend),
        CACHE_QUOTA_BYTES,
    ))
}

/// An encrypted source store, backed up, plus its backend, keystore and expected answer.
struct Source {
    backend: Arc<dyn ObjectStore>,
    keystore: Arc<SoftwareKeystore>,
    expected: Vec<String>,
    roots: Vec<PathBuf>,
}

fn backed_up_source(tag: &str) -> Source {
    let store_dir = tmp(&format!("{tag}-objects"));
    let root = tmp(&format!("{tag}-src"));
    let backend: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(&store_dir));
    let keystore = Arc::new(SoftwareKeystore::new("key-v1", [11u8; 32]));

    let engine = Engine::init(&root, config())
        .unwrap()
        .with_cold(cold(&backend))
        .with_keys(Arc::clone(&keystore) as Arc<dyn KeyProvider>);
    engine.ingest(events(), 1_760_000_000_000).unwrap();
    let expected = answer(&engine);
    assert!(!expected.is_empty(), "the source must return hits");
    engine.backup_published().unwrap();

    Source {
        backend,
        keystore,
        expected,
        roots: vec![store_dir, root],
    }
}

/// A replacement node: empty data root, same bucket, whichever key service is passed.
fn replacement(
    root: &Path,
    backend: &Arc<dyn ObjectStore>,
    keys: Option<Arc<dyn KeyProvider>>,
) -> Engine {
    let engine = Engine::init(root, config())
        .unwrap()
        .with_cold(cold(backend));
    match keys {
        Some(k) => engine.with_keys(k),
        None => engine,
    }
}

/// Nothing has been installed: no part directory, and `CURRENT` names nothing.
fn assert_untouched(engine: &Engine, why: &str) {
    let parts = engine.store.parts_dir();
    if parts.exists() {
        for entry in std::fs::read_dir(&parts).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                name.starts_with(".hydrate-"),
                "{why}: part `{name}` was installed before the refusal -- a failed restore must \
                 leave staging directories only"
            );
        }
    }
    let current = engine.store.current_path();
    assert!(
        !current.exists() || std::fs::read_to_string(&current).unwrap().trim().is_empty(),
        "{why}: CURRENT names a snapshot after a refused restore"
    );
}

#[test]
fn a_key_service_outage_mid_restore_leaves_no_partial_state_and_retries_cleanly() {
    let src = backed_up_source("outage");
    let dest = tmp("outage-dest");
    let dest_keys = Arc::new(SoftwareKeystore::new("key-v1", [11u8; 32]));
    let node = replacement(
        &dest,
        &src.backend,
        Some(Arc::clone(&dest_keys) as Arc<dyn KeyProvider>),
    );

    dest_keys.set_fault(KeyFault::Unreachable).unwrap();
    let err = node.hydrate_from_backup(None).unwrap_err().to_string();
    assert!(
        err.contains("key service unreachable"),
        "an outage must be named, not mistaken for a corrupt backup: {err}"
    );
    assert_untouched(&node, "after a key-service outage");

    // The other half of the property, and the one that costs an operator a night if it is missing:
    // access returning is *enough*. No cleanup, no --force, no second data root.
    dest_keys.set_fault(KeyFault::None).unwrap();
    node.hydrate_from_backup(None)
        .expect("a retry after access returns must succeed cleanly");
    assert_eq!(
        answer(&node),
        src.expected,
        "the restored node must answer exactly as the source did"
    );

    for r in src.roots.iter().chain(std::iter::once(&dest)) {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn a_restore_onto_a_node_holding_the_wrong_key_installs_nothing() {
    // The wrong deployment's key service. It must be refused by name and never probed against the
    // keys it does hold -- and, crucially, refused before anything lands on this node's disk.
    let src = backed_up_source("wrong-key");
    let dest = tmp("wrong-key-dest");
    let other: Arc<dyn KeyProvider> =
        Arc::new(SoftwareKeystore::new("someone-elses-key", [9u8; 32]));
    let node = replacement(&dest, &src.backend, Some(other));

    let err = node.hydrate_from_backup(None).unwrap_err().to_string();
    assert!(
        err.contains("does not hold wrapping key"),
        "a foreign key service must be refused by name: {err}"
    );
    assert_untouched(&node, "after a wrong-key refusal");

    for r in src.roots.iter().chain(std::iter::once(&dest)) {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn a_restore_onto_a_node_with_no_key_service_installs_nothing() {
    // Restoring ciphertext onto a node that cannot open it would point CURRENT at a snapshot
    // nothing can serve -- the same failure the generation ordering exists to prevent.
    let src = backed_up_source("no-keys");
    let dest = tmp("no-keys-dest");
    let node = replacement(&dest, &src.backend, None);

    let err = node.hydrate_from_backup(None).unwrap_err().to_string();
    assert!(
        err.contains("no key service configured"),
        "a keyless restore of an encrypted backup must refuse by name: {err}"
    );
    assert_untouched(&node, "after a keyless refusal");

    for r in src.roots.iter().chain(std::iter::once(&dest)) {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn a_revoked_key_refuses_the_restore_and_leaves_the_node_untouched() {
    let src = backed_up_source("revoked");
    let dest = tmp("revoked-dest");
    let dest_keys = Arc::new(SoftwareKeystore::new("key-v1", [11u8; 32]));
    let node = replacement(
        &dest,
        &src.backend,
        Some(Arc::clone(&dest_keys) as Arc<dyn KeyProvider>),
    );

    dest_keys.revoke("key-v1").unwrap();
    let err = node.hydrate_from_backup(None).unwrap_err().to_string();
    assert!(err.contains("revoked"), "revocation must be named: {err}");
    assert_untouched(&node, "after a revoked-key refusal");

    dest_keys.restore_revoked("key-v1").unwrap();
    node.hydrate_from_backup(None)
        .expect("restoring access must be enough");
    assert_eq!(answer(&node), src.expected);

    for r in src.roots.iter().chain(std::iter::once(&dest)) {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn a_backup_taken_before_a_rotation_still_restores_through_the_retired_key() {
    // A rotation must not make yesterday's backup unreadable. The key that sealed this data is no
    // longer the active one, but it is still authorized to unwrap -- and restore must use the id
    // the part names rather than the id that happens to be current.
    let src = backed_up_source("retired");
    src.keystore.expand("key-v2", [22u8; 32]).unwrap();
    src.keystore.activate("key-v2").unwrap();

    let dest = tmp("retired-dest");
    let node = replacement(
        &dest,
        &src.backend,
        Some(Arc::clone(&src.keystore) as Arc<dyn KeyProvider>),
    );
    node.hydrate_from_backup(None)
        .expect("a backup taken before the rotation must still restore");
    assert_eq!(
        answer(&node),
        src.expected,
        "and it must answer exactly as it did before the rotation"
    );

    for r in src.roots.iter().chain(std::iter::once(&dest)) {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn a_restore_that_will_not_decrypt_is_named_as_such_and_installs_nothing() {
    // Stored-byte integrity and logical identity are different checks (contract section 4a). A part
    // whose ciphertext passes its receipt's SHA-256 but fails the AEAD tag must be reported as what
    // it is -- and must still install nothing.
    let src = backed_up_source("bad-tag");
    let dest = tmp("bad-tag-dest");

    // Corrupt a column INSIDE the backup and re-stamp the receipt, so byte-integrity passes and
    // only the tag can catch it.
    let key = src
        .backend
        .list("parts/")
        .unwrap()
        .into_iter()
        .find(|k| k.ends_with("rerank.vec"))
        .expect("a column object");
    let mut bytes = src.backend.get(&key).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    src.backend.put(&key, &bytes).unwrap();

    let part_id = key
        .strip_prefix("parts/")
        .and_then(|r| r.split_once('/'))
        .map(|(id, _)| id.to_string())
        .unwrap();
    let receipt_key = format!("parts/{part_id}/BACKUP.json");
    let mut receipt: serde_json::Value =
        serde_json::from_slice(&src.backend.get(&receipt_key).unwrap()).unwrap();
    for file in receipt["files"].as_array_mut().unwrap() {
        if file["name"] == "rerank.vec" {
            file["sha256"] =
                serde_json::json!(prism_types::hash::hex(&prism_types::hash::sha256(&bytes)));
        }
    }
    src.backend
        .put(&receipt_key, &serde_json::to_vec(&receipt).unwrap())
        .unwrap();

    let node = replacement(
        &dest,
        &src.backend,
        Some(Arc::clone(&src.keystore) as Arc<dyn KeyProvider>),
    );
    let err = node.hydrate_from_backup(None).unwrap_err().to_string();
    assert!(
        err.contains("will not decrypt"),
        "a part that survives its receipt but fails the tag must say so: {err}"
    );
    assert_untouched(&node, "after a decryption failure");

    for r in src.roots.iter().chain(std::iter::once(&dest)) {
        let _ = std::fs::remove_dir_all(r);
    }
}
