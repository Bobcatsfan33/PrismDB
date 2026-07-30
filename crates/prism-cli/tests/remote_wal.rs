//! Cross-node durable-admission gates.
//!
//! These tests use separate hot-tier directories and one shared object-store backend. The second
//! engine therefore has none of the first node's local WAL bytes; recovering the acknowledged
//! batch proves the remote log is the durability authority for failover.

use prism_engine::storage::object::{
    CachedObjectStore, FaultConfig, FaultStore, LocalObjectStore, ObjectStore,
};
use prism_engine::storage::CACHE_QUOTA_BYTES;
use prism_engine::wal::{RemoteWal, WalRecord};
use prism_engine::{Engine, Ingestor};
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Query;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let path =
        std::env::temp_dir().join(format!("prism-remote-wal-{tag}-{}-{n}", std::process::id()));
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

fn cold(backend: Arc<dyn prism_engine::storage::object::ObjectStore>) -> Arc<CachedObjectStore> {
    Arc::new(CachedObjectStore::new(backend, CACHE_QUOTA_BYTES))
}

fn top_ids(engine: &Engine) -> Vec<String> {
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
        .map(|hit| hit.event.event_id)
        .collect()
}

#[test]
fn replacement_node_recovers_remote_ack_and_fences_old_writer() {
    let remote_root = tmp("remote");
    std::fs::create_dir_all(&remote_root).unwrap();
    let backend: Arc<dyn prism_engine::storage::object::ObjectStore> =
        Arc::new(LocalObjectStore::new(&remote_root));

    let root_a = tmp("node-a");
    let root_b = tmp("node-b");
    let engine_a = Engine::init(&root_a, config())
        .unwrap()
        .with_cold(cold(Arc::clone(&backend)));
    let engine_b = Engine::init(&root_b, config())
        .unwrap()
        .with_cold(cold(Arc::clone(&backend)));

    let writer_a = Ingestor::open_replicated(engine_a, 0).unwrap();
    let events = prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 800, 5);
    let remote_wal = RemoteWal::new(Arc::clone(&backend), 0);
    let record_id = remote_wal
        .next_record_id(writer_a.engine.ownership_epoch(), &[])
        .unwrap();
    let record = WalRecord {
        record_id,
        events: events.clone(),
        source: None,
        source_offset: None,
        created_at_ms: 1_760_000_000_000,
    };
    writer_a.wal.append_record(&record).unwrap();
    remote_wal.append(&record).unwrap();
    assert!(
        record_id >= 1u64 << 32,
        "replicated record IDs must carry their ownership epoch"
    );

    // Independent expected result: the normal deterministic engine publishing the same events.
    let baseline_root = tmp("baseline");
    let baseline = Engine::init(&baseline_root, config()).unwrap();
    baseline.ingest(events.clone(), 1_760_000_000_001).unwrap();
    let golden = top_ids(&baseline);
    assert!(!golden.is_empty());

    // A completely separate hot tier takes ownership. It has no node-A WAL, so the only route to
    // the acknowledged events is the shared object-store admission log.
    let mut writer_b = Ingestor::open_replicated(engine_b, 0).unwrap();
    let recovered = writer_b.recover(1_760_000_000_001).unwrap();
    assert_eq!(recovered.len(), 1);
    assert!(recovered[0].recovered);
    assert_eq!(recovered[0].published, events.len());
    assert_eq!(top_ids(&writer_b.engine), golden);

    // The paused old node cannot publish after the takeover.
    let mut writer_a = writer_a;
    let stale_error = writer_a
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 32, 6),
            None,
            None,
            1_760_000_000_002,
        )
        .unwrap_err()
        .to_string();
    assert!(
        stale_error.contains("write fenced"),
        "the replaced writer must fail by name: {stale_error}"
    );

    let _ = std::fs::remove_dir_all(root_a);
    let _ = std::fs::remove_dir_all(root_b);
    let _ = std::fs::remove_dir_all(baseline_root);
    let _ = std::fs::remove_dir_all(remote_root);
}

#[test]
fn remote_outage_fails_before_the_durable_ack() {
    let remote_root = tmp("fault-remote");
    std::fs::create_dir_all(&remote_root).unwrap();
    let fault = Arc::new(FaultStore::new(LocalObjectStore::new(&remote_root)));
    let backend: Arc<dyn prism_engine::storage::object::ObjectStore> = fault.clone();
    let root = tmp("fault-node");
    let engine = Engine::init(&root, config())
        .unwrap()
        .with_cold(cold(backend));
    let mut writer = Ingestor::open_replicated(engine, 7).unwrap();

    fault.set(FaultConfig {
        unavailable: true,
        ..Default::default()
    });
    let error = writer
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 32, 8),
            None,
            None,
            1_760_000_000_000,
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("remote unavailable"),
        "the outage must be explicit: {error}"
    );
    assert!(
        writer.wal.read_all().unwrap().is_empty(),
        "a request that could not reach the remote durability authority must not cross the ack point"
    );

    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(remote_root);
}

#[test]
fn immutable_record_conflicts_and_tampering_are_refused() {
    let remote_root = tmp("integrity");
    std::fs::create_dir_all(&remote_root).unwrap();
    let backend: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(&remote_root));
    let remote = RemoteWal::new(Arc::clone(&backend), 3);
    let mut record = WalRecord {
        record_id: 1u64 << 32,
        events: prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 1, 9),
        source: None,
        source_offset: None,
        created_at_ms: 1_760_000_000_000,
    };
    remote.append(&record).unwrap();

    record.created_at_ms += 1;
    let conflict = remote.append(&record).unwrap_err().to_string();
    assert!(
        conflict.contains("conflict") && conflict.contains("immutable"),
        "different bytes at one record id must fail by name: {conflict}"
    );

    record.record_id += 1;
    backend
        .put(
            "wal/shard-3/records/00000000004294967296.json",
            &serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
    let tamper = remote.read_all().unwrap_err().to_string();
    assert!(
        tamper.contains("key id") && tamper.contains("body id"),
        "a mutable or corrupt backend must be detected on recovery: {tamper}"
    );

    let _ = std::fs::remove_dir_all(remote_root);
}
