//! **The S11 MinIO integration gate** — the hand-rolled S3 client against a **real S3 server**
//! ([storage contract §1](../../../docs/STORAGE-CONTRACT.md)), no mock anywhere in the path.
//!
//! Runs only when `PRISM_S3_ENDPOINT` is set (MinIO in CI); skips locally where there is no server.
//! This is the gate that flips S11 from 🟡 to ✅ once it is green against MinIO in CI: put/get/
//! ranged-get/head/delete/list round-trip, the CAS create race (D-066/D-067), and the capability
//! check — all against the real wire, verifying the SigV4 signing and the S3 semantics end-to-end.

use prism_engine::storage::object::{
    assert_cas_capability, cas_publish, CasOutcome, FaultConfig, FaultStore, ObjectStore,
};
use prism_engine::storage::s3::{S3Config, S3ObjectStore};
use prism_engine::storage::sigv4::Credentials;

fn minio() -> Option<S3Config> {
    let endpoint = std::env::var("PRISM_S3_ENDPOINT").ok()?;
    Some(S3Config {
        endpoint,
        region: std::env::var("PRISM_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
        bucket: std::env::var("PRISM_S3_BUCKET").unwrap_or_else(|_| "prism".into()),
        credentials: Credentials {
            access_key: std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| "minioadmin".into()),
            secret_key: std::env::var("AWS_SECRET_ACCESS_KEY")
                .unwrap_or_else(|_| "minioadmin".into()),
        },
        fixed_amz_date: None,
    })
}

#[test]
fn the_hand_rolled_s3_client_round_trips_against_minio() {
    let Some(cfg) = minio() else {
        eprintln!("skipping MinIO integration: PRISM_S3_ENDPOINT is not set");
        return;
    };
    let store = S3ObjectStore::new(cfg);

    // The backend must provide conditional-create, or we refuse it.
    assert_cas_capability(&store).expect("MinIO must provide If-None-Match conditional create");

    // Put / get / ranged get / head.
    store.delete("it/k").ok();
    store.put("it/k", b"hello world").unwrap();
    assert_eq!(store.get("it/k").unwrap(), b"hello world");
    assert_eq!(store.get_range("it/k", 6, 5).unwrap(), b"world");
    assert_eq!(store.head("it/k").unwrap(), Some(11));

    // CAS publication: create wins, our-own succeeds, a different write conflicts (D-067).
    store.delete("it/CURRENT").ok();
    assert_eq!(
        cas_publish(&store, "it/CURRENT", b"snap-a").unwrap(),
        CasOutcome::Created
    );
    assert_eq!(
        cas_publish(&store, "it/CURRENT", b"snap-a").unwrap(),
        CasOutcome::AlreadyOurs
    );
    assert_eq!(
        cas_publish(&store, "it/CURRENT", b"snap-b").unwrap(),
        CasOutcome::Conflict
    );

    // List sees the keys we wrote.
    let keys = store.list("it/").unwrap();
    assert!(
        keys.iter().any(|k| k == "it/k"),
        "list did not return it/k: {keys:?}"
    );

    // Delete removes it; head then reports absence.
    store.delete("it/k").unwrap();
    assert!(store.head("it/k").unwrap().is_none());
    store.delete("it/CURRENT").ok();
}

use prism_engine::storage::object::CachedObjectStore;
use prism_engine::storage::CACHE_QUOTA_BYTES;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Query;
use std::sync::Arc;

fn s3(cfg: &S3Config) -> S3ObjectStore {
    S3ObjectStore::new(cfg.clone())
}

fn top_ids(engine: &prism_engine::Engine) -> Vec<String> {
    let q = Query {
        text: "the tool call timed out retrying".into(),
        k: 15,
        tenant: Some("t1".into()),
        rerank: 40,
        ..Default::default()
    };
    engine
        .search(&q)
        .unwrap()
        .hits
        .iter()
        .map(|h| h.event.event_id.clone())
        .collect()
}

/// **The S11 sprint-closer: answer-invariance forced hot / cold / mixed THROUGH MinIO** — the cold
/// tier lives on the real remote, and a cache state never changes the answer (storage §3), while a
/// transient remote fault retries to the correct answer and a dead remote names itself (storage §4).
#[test]
fn answer_invariance_through_minio_under_fault() {
    let Some(cfg) = minio() else {
        eprintln!("skipping MinIO answer-invariance: PRISM_S3_ENDPOINT is not set");
        return;
    };
    let root = std::env::temp_dir().join(format!("prism-s3ai-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let engine = prism_engine::Engine::init(
        &root,
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
        },
    )
    .unwrap();
    engine
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 2000, 5),
            1_760_000_000_000,
        )
        .unwrap();
    let baseline = top_ids(&engine); // local cold tier

    // Push the cold tier onto MinIO, then answer the same query with the cold tier on the remote.
    let uploaded = engine.upload_cold_tier(&s3(&cfg)).unwrap();
    assert!(uploaded > 0, "no cold-tier objects uploaded");

    // Forced cold: a fresh cache over the S3 backend — every block a miss, fetched from MinIO.
    let remote = prism_engine::Engine::open(&root)
        .unwrap()
        .with_cold(Arc::new(CachedObjectStore::new(
            Arc::new(s3(&cfg)),
            CACHE_QUOTA_BYTES,
        )));
    let cold = top_ids(&remote);
    assert_eq!(
        cold, baseline,
        "the cold tier on MinIO answered differently — storage §3 violated"
    );
    assert!(
        remote.cold.cache().stats().misses > 0,
        "the cold run must miss the cache"
    );

    // Forced hot: the same engine again — blocks now cached, identical answer.
    let hot = top_ids(&remote);
    assert!(
        remote.cold.cache().stats().hits > 0,
        "the hot run must hit the cache"
    );
    assert_eq!(
        hot, baseline,
        "a warm cache changed the answer through MinIO"
    );

    // Under a transient fault: a fresh cache over a FaultStore-wrapped MinIO; one injected 5xx is
    // ridden out by the bounded retry, and the answer is correct — not short.
    let fault = Arc::new(FaultStore::new(s3(&cfg)));
    let faulted = prism_engine::Engine::open(&root)
        .unwrap()
        .with_cold(Arc::new(CachedObjectStore::new(
            fault.clone(),
            CACHE_QUOTA_BYTES,
        )));
    fault.set(FaultConfig {
        fail_next: true,
        ..Default::default()
    });
    assert_eq!(
        top_ids(&faulted),
        baseline,
        "a transient MinIO fault must retry to the correct answer"
    );

    // A dead remote is named, never a silently short result set.
    let fault2 = Arc::new(FaultStore::new(s3(&cfg)));
    let dead = prism_engine::Engine::open(&root)
        .unwrap()
        .with_cold(Arc::new(CachedObjectStore::new(
            fault2.clone(),
            CACHE_QUOTA_BYTES,
        )));
    fault2.set(FaultConfig {
        unavailable: true,
        ..Default::default()
    });
    let q = Query {
        text: "the tool call timed out retrying".into(),
        k: 15,
        tenant: Some("t1".into()),
        rerank: 40,
        ..Default::default()
    };
    let err = dead.search(&q).unwrap_err().to_string();
    assert!(
        err.contains("remote unavailable"),
        "a dead MinIO must be named, not a short answer: {err}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
