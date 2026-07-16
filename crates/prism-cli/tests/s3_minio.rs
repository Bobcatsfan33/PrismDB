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
use prism_engine::storage::{CACHE_QUOTA_BYTES, MULTIPART_THRESHOLD_BYTES};
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

/// **Boundary (c) against MinIO — remote-orphan reconciliation.** Reconciliation reclaims a remote
/// object only when it is absent from the live set AND older than the reader-lease horizon. A
/// crashed publication's cold tier and a catalog mirror snapshot past the recovery depth are
/// reclaimed; a referenced part's cold tier, a live mirror snapshot, and any just-uploaded object
/// (an in-flight publication) are protected (storage §2, invariant-6 shape).
#[test]
fn reconcile_reclaims_remote_orphans_but_never_the_live_set() {
    let Some(cfg) = minio() else {
        eprintln!("skipping MinIO reconciliation: PRISM_S3_ENDPOINT is not set");
        return;
    };
    let root = std::env::temp_dir().join(format!("prism-s3rec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    let engine = prism_engine::Engine::init(&root, store_cfg())
        .unwrap()
        .with_cold(Arc::new(CachedObjectStore::new(
            Arc::new(s3(&cfg)),
            CACHE_QUOTA_BYTES,
        )));
    engine
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 2000, 5),
            1_760_000_000_000,
        )
        .unwrap();
    let live_snapshot = engine.snapshot().unwrap().snapshot_id;
    let live_parts = engine.snapshot().unwrap().part_ids();
    assert!(!live_parts.is_empty());

    // Plant orphans on the remote: a cold tier no snapshot names, and a mirror snapshot past the
    // recovery depth. Plus a live mirror snapshot, which must be protected.
    let probe = s3(&cfg);
    probe.put("parts/p99999999-orphanaaaa/rerank.vec", b"orphaned bytes").unwrap();
    probe.put("catalog/SNAPSHOT-s00000099", b"stale mirror").unwrap();
    probe
        .put(&format!("catalog/SNAPSHOT-{live_snapshot}"), b"live mirror")
        .unwrap();

    // Leak an incomplete multipart upload — a large-object publication that crashed mid-upload
    // leaves server-side parts that only a multipart enumeration shows. Keyed under `parts/`, so
    // reconciliation's `parts/` sweep is what must abort it.
    let mp_key = format!("parts/p88888888-mpleak-{}/rerank.vec", std::process::id());
    let upload_id = probe.initiate_multipart(&mp_key).unwrap();
    probe
        .upload_part(&mp_key, &upload_id, 1, &vec![7u8; 5 * 1024 * 1024])
        .unwrap();
    assert!(
        probe
            .list_multipart("parts/")
            .unwrap()
            .iter()
            .any(|u| u.key == mp_key && u.upload_id == upload_id),
        "the leaked multipart upload is not listed as in-progress"
    );

    // A fresh orphan, uploaded just now: reconciliation must grace it (its commit could be moments
    // away), so a pass at wall-clock-now reclaims nothing and aborts no upload.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let graced = engine.reconcile_remote_orphans(1, now_ms, false).unwrap();
    assert!(
        graced.removed.is_empty(),
        "a fresh orphan was swept inside its grace horizon: {:?}",
        graced.removed
    );
    assert!(
        graced.aborted_uploads.is_empty(),
        "a fresh in-flight multipart upload was aborted: {:?}",
        graced.aborted_uploads
    );
    assert!(graced.too_young >= 1, "the fresh orphans must count as too_young");

    // Now well past the horizon: the two orphans are reclaimed, the live set is not.
    let horizon = prism_part::catalog::GC_HORIZON_MS;
    let aged = engine
        .reconcile_remote_orphans(1, now_ms + horizon + 60_000, false)
        .unwrap();
    assert!(
        aged.removed.contains(&"parts/p99999999-orphanaaaa/rerank.vec".to_string()),
        "the orphan cold tier was not reclaimed: {:?}",
        aged.removed
    );
    assert!(
        aged.removed.contains(&"catalog/SNAPSHOT-s00000099".to_string()),
        "the stale mirror snapshot was not reclaimed: {:?}",
        aged.removed
    );
    // The live mirror snapshot and every referenced cold tier survived.
    assert!(
        !aged.removed.iter().any(|k| k == &format!("catalog/SNAPSHOT-{live_snapshot}")),
        "reconciliation swept the LIVE mirror snapshot — recovery target destroyed"
    );
    for id in &live_parts {
        let key = format!("parts/{id}/rerank.vec");
        assert!(!aged.removed.contains(&key), "a live cold tier was reclaimed: {key}");
        assert!(
            probe.head(&key).unwrap().is_some(),
            "a referenced part's cold tier is gone from MinIO after reconciliation: {key}"
        );
    }
    assert_eq!(aged.protected_parts, live_parts.len());
    assert!(aged.protected_mirrors >= 1);

    // The stale multipart upload was aborted and no longer enumerates.
    assert!(
        aged.aborted_uploads.contains(&mp_key),
        "the stale multipart upload was not aborted: {:?}",
        aged.aborted_uploads
    );
    assert!(
        !probe
            .list_multipart("parts/")
            .unwrap()
            .iter()
            .any(|u| u.upload_id == upload_id),
        "the aborted multipart upload still enumerates"
    );

    // The store still answers — reconciliation touched no live byte.
    assert!(!top_ids(&engine).is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

/// **Boundary (d) against MinIO — multipart upload.** An object at/above the threshold goes up as a
/// real multipart upload (initiate → parts → complete, all plain SigV4) and comes back byte-
/// identical, with ranged reads working on the assembled object. Keyed under `it/` so the reconcile
/// test's `parts/` multipart sweep never races this in-flight upload.
#[test]
fn a_large_object_round_trips_through_multipart() {
    let Some(cfg) = minio() else {
        eprintln!("skipping MinIO multipart: PRISM_S3_ENDPOINT is not set");
        return;
    };
    let store = S3ObjectStore::new(cfg);
    let key = format!("it/big-{}", std::process::id());
    store.delete(&key).ok();

    // 19 MiB → two parts (16 MiB + 3 MiB); a deterministic fill so the byte-compare is meaningful.
    let n = MULTIPART_THRESHOLD_BYTES + 3 * 1024 * 1024;
    let data: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();

    store.put(&key, &data).unwrap(); // >= threshold → multipart path
    assert_eq!(store.head(&key).unwrap(), Some(n as u64));
    let got = store.get(&key).unwrap();
    assert_eq!(got.len(), n, "multipart object came back a different length");
    assert!(got == data, "multipart object came back changed");
    assert_eq!(
        store.get_range(&key, 100, 10).unwrap(),
        &data[100..110],
        "a ranged read of the assembled multipart object is wrong"
    );

    store.delete(&key).ok();
}

fn store_cfg() -> StoreConfig {
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

/// **Boundary (a) against MinIO — remote-durable cold-tier publication.** An engine whose cold
/// backend *is* MinIO uploads and verifies every new part's cold tier to the remote *during*
/// ingest, before the catalog references the part (storage §2). Afterward the objects are on MinIO
/// (HEAD confirms), a second publish is an idempotent no-op, and the bytes are byte-identical to the
/// local cold tier — a query answered from MinIO matches a query answered from the local disk.
#[test]
fn cold_tier_is_published_and_verified_to_minio_before_reference() {
    let Some(cfg) = minio() else {
        eprintln!("skipping MinIO publication: PRISM_S3_ENDPOINT is not set");
        return;
    };
    let root = std::env::temp_dir().join(format!("prism-s3pub-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    // An engine whose cold tier lives on MinIO from the start.
    let engine = prism_engine::Engine::init(&root, store_cfg())
        .unwrap()
        .with_cold(Arc::new(CachedObjectStore::new(
            Arc::new(s3(&cfg)),
            CACHE_QUOTA_BYTES,
        )));
    engine
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 2000, 5),
            1_760_000_000_000,
        )
        .unwrap();

    // Publication happened during the commit: every part's cold tier is durable on MinIO.
    let probe = s3(&cfg);
    let ids = engine.snapshot().unwrap().part_ids();
    assert!(!ids.is_empty());
    for id in &ids {
        let key = format!("parts/{id}/rerank.vec");
        let head = probe.head(&key).unwrap();
        assert!(
            head.is_some(),
            "part {id} was referenced but its cold tier is not on MinIO at {key}"
        );
        // Idempotent: re-publishing uploads nothing new and still verifies.
        engine.publish_part_cold(id).unwrap();
        assert_eq!(probe.head(&key).unwrap(), head, "re-publish changed the object");
    }

    // The published bytes are the truth: a query answered from MinIO equals one answered from the
    // local cold tier (a fresh default-local reopen of the same store).
    let from_minio = top_ids(&engine);
    let local = prism_engine::Engine::open(&root).unwrap();
    let from_local = top_ids(&local);
    assert_eq!(
        from_minio, from_local,
        "the cold tier published to MinIO answered differently from the local one"
    );

    let _ = std::fs::remove_dir_all(&root);
}
