//! An encrypted store answers identically, and a tenant's key cannot open another tenant's data
//! (S14, [D-095](../../../../docs/DECISIONS.md)).
//!
//! These run through the **engine**, not the part writer, so they exercise the path a query
//! actually takes: ingest mints a DEK per part and wraps it, the manifest carries the envelope, and
//! opening a part resolves the cipher back through the key service.

use prism_engine::keys::{KeyFault, KeyProvider, SoftwareKeystore};
use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Query;
use std::sync::Arc;

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("prism-encstore-{tag}-{}", std::process::id()));
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

fn events() -> Vec<prism_types::Event> {
    prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 600, 5)
}

#[test]
fn an_encrypted_store_answers_exactly_as_a_plaintext_one_does() {
    let plain_root = tmp("plain");
    let enc_root = tmp("enc");

    let plain = Engine::init(&plain_root, config()).unwrap();
    plain.ingest(events(), 1_760_000_000_000).unwrap();
    let expected = answer(&plain);
    assert!(!expected.is_empty(), "the control must return hits");

    let keystore: Arc<dyn KeyProvider> = Arc::new(SoftwareKeystore::new("key-v1", [11u8; 32]));
    let enc = Engine::init(&enc_root, config())
        .unwrap()
        .with_keys(Arc::clone(&keystore));
    enc.ingest(events(), 1_760_000_000_000).unwrap();

    // Answer-invariance: encryption is a property of stored bytes, never of an answer.
    assert_eq!(
        answer(&enc),
        expected,
        "an encrypted store must answer identically"
    );

    // Every published part really is sealed, and its envelope names the key explicitly.
    let snap = enc.snapshot().unwrap();
    assert!(!snap.part_ids().is_empty());
    for id in snap.part_ids() {
        let reader = enc.open_part(&id).unwrap();
        assert!(reader.is_encrypted(), "part {id} should be sealed");
        let envelope = reader.encryption_envelope().unwrap();
        assert_eq!(envelope.wrapping_key_id, "key-v1");
        assert!(!envelope.wrapped_dek.is_empty());
    }

    for r in [plain_root, enc_root] {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn a_query_spanning_several_encrypted_parts_opens_each_under_its_own_dek() {
    // Every part is sealed under its **own** freshly minted DEK, so many parts share one wrapping
    // key id and one epoch while holding entirely different keys. A resident-key cache keyed on the
    // wrapping key therefore hands the first part's DEK to every part after it. That fails closed
    // on the AEAD tag rather than returning wrong rows -- but it makes any read touching more than
    // one encrypted part fail outright, and a fixture with a single part never sees it.
    let plain_root = tmp("multi-plain");
    let enc_root = tmp("multi-enc");

    let plain = Engine::init(&plain_root, config()).unwrap();
    plain.ingest(events(), 1_760_000_000_000).unwrap();

    let keystore: Arc<dyn KeyProvider> = Arc::new(SoftwareKeystore::new("key-v1", [11u8; 32]));
    let enc = Engine::init(&enc_root, config())
        .unwrap()
        .with_keys(Arc::clone(&keystore));
    enc.ingest(events(), 1_760_000_000_000).unwrap();

    let ids = enc.snapshot().unwrap().part_ids();
    assert!(
        ids.len() > 1,
        "this gate is only meaningful across several parts, and the fixture has {}",
        ids.len()
    );

    // Distinct DEKs under one wrapping key id: precisely the shape the cache key must survive.
    let deks: std::collections::BTreeSet<Vec<u8>> = ids
        .iter()
        .map(|id| {
            let e = enc.open_part(id).unwrap().encryption_envelope().unwrap();
            assert_eq!(
                e.wrapping_key_id, "key-v1",
                "one wrapping key for all parts"
            );
            e.wrapped_dek
        })
        .collect();
    assert_eq!(deks.len(), ids.len(), "each part must carry its own DEK");

    // One engine, one resident-key cache, every tenant read in turn.
    for tenant in ["t1", "t2", "t3", "t4", "t5"] {
        let q = |e: &Engine| {
            e.search(&Query {
                text: "the tool call timed out retrying".into(),
                k: 15,
                tenant: Some(tenant.into()),
                rerank: 40,
                ..Default::default()
            })
            .map(|r| {
                r.hits
                    .into_iter()
                    .map(|h| h.event.event_id)
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            q(&enc).unwrap_or_else(|e| panic!("tenant {tenant} failed to read: {e}")),
            q(&plain).unwrap(),
            "tenant {tenant} must answer identically on the encrypted store"
        );
    }

    for r in [plain_root, enc_root] {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn a_store_without_the_key_service_cannot_read_its_own_encrypted_parts() {
    let root = tmp("nokeys");
    let keystore: Arc<dyn KeyProvider> = Arc::new(SoftwareKeystore::new("key-v1", [11u8; 32]));
    let engine = Engine::init(&root, config())
        .unwrap()
        .with_keys(Arc::clone(&keystore));
    engine.ingest(events(), 1_760_000_000_000).unwrap();
    drop(engine);

    // Re-open with no key service at all: the data is on disk, and it stays shut.
    let blind = Engine::open(&root).unwrap();
    let err = match blind.open_part(&blind.snapshot().unwrap().part_ids()[0]) {
        Ok(_) => panic!("a store with no key service must not open its own encrypted part"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("no key service configured"),
        "a keyless open must refuse by name: {err}"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_key_service_outage_fails_closed_and_recovers_cleanly_on_retry() {
    let root = tmp("outage");
    let keystore = Arc::new(SoftwareKeystore::new("key-v1", [11u8; 32]));
    let engine = Engine::init(&root, config())
        .unwrap()
        .with_keys(Arc::clone(&keystore) as Arc<dyn KeyProvider>);
    engine.ingest(events(), 1_760_000_000_000).unwrap();
    let expected = answer(&engine);

    // The cache would otherwise mask the outage: revocation and outages must bite a RUNNING process.
    engine.clear_key_cache().unwrap();
    keystore.set_fault(KeyFault::Unreachable).unwrap();
    let err = engine
        .search(&Query {
            text: "the tool call timed out retrying".into(),
            k: 5,
            tenant: Some("t1".into()),
            ..Default::default()
        })
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("key service unreachable"),
        "an outage must be named, not mistaken for damage: {err}"
    );

    // The catalog is untouched by the refusal, and access returning is enough.
    keystore.set_fault(KeyFault::None).unwrap();
    assert_eq!(
        answer(&engine),
        expected,
        "once the key service returns, the same store must answer exactly as before"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_revoked_key_shuts_the_data_and_restoring_access_reopens_it() {
    let root = tmp("revoked");
    let keystore = Arc::new(SoftwareKeystore::new("key-v1", [11u8; 32]));
    let engine = Engine::init(&root, config())
        .unwrap()
        .with_keys(Arc::clone(&keystore) as Arc<dyn KeyProvider>);
    engine.ingest(events(), 1_760_000_000_000).unwrap();
    let expected = answer(&engine);
    let snapshot_before = engine.snapshot().unwrap().snapshot_id;

    engine.clear_key_cache().unwrap();
    keystore.revoke("key-v1").unwrap();
    let err = answer_err(&engine);
    assert!(err.contains("revoked"), "revocation must be named: {err}");
    assert_eq!(
        engine.snapshot().unwrap().snapshot_id,
        snapshot_before,
        "a refused read must not have disturbed catalog state"
    );

    keystore.restore_revoked("key-v1").unwrap();
    assert_eq!(answer(&engine), expected, "recovery must be clean");
    let _ = std::fs::remove_dir_all(root);
}

fn answer_err(engine: &Engine) -> String {
    engine
        .search(&Query {
            text: "the tool call timed out retrying".into(),
            k: 5,
            tenant: Some("t1".into()),
            ..Default::default()
        })
        .unwrap_err()
        .to_string()
}

#[test]
fn one_tenants_key_cannot_open_another_tenants_parts() {
    // Two stores, two DEKs, two independent key services — the shape a dedicated-bucket tenant has.
    let a_root = tmp("tenant-a");
    let b_root = tmp("tenant-b");
    let a_keys: Arc<dyn KeyProvider> = Arc::new(SoftwareKeystore::new("tenant-a-key", [1u8; 32]));
    let b_keys: Arc<dyn KeyProvider> = Arc::new(SoftwareKeystore::new("tenant-b-key", [2u8; 32]));

    let a = Engine::init(&a_root, config())
        .unwrap()
        .with_keys(Arc::clone(&a_keys));
    a.ingest(events(), 1_760_000_000_000).unwrap();
    let a_part = a.snapshot().unwrap().part_ids()[0].clone();
    drop(a);

    // Tenant B's key service does not hold tenant A's key, and must not be asked to guess.
    let b_view = Engine::open(&a_root)
        .unwrap()
        .with_keys(Arc::clone(&b_keys));
    let err = match b_view.open_part(&a_part) {
        Ok(_) => panic!("tenant B must not be able to open tenant A's part"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("does not hold wrapping key"),
        "tenant B must be refused by name, never fall back to a key it does hold: {err}"
    );

    let _ = std::fs::remove_dir_all(a_root);
    let _ = std::fs::remove_dir_all(b_root);
}
