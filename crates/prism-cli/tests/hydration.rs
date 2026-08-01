//! The published-part backup and hydration gates (S14, [D-094](../../../../docs/DECISIONS.md)).
//!
//! The headline test is **the disaster drill itself, automated**: publish a customer-shaped
//! dataset, acknowledge further pre-publication events, destroy the node-local disk, bring up a
//! replacement node **as a separate `prism` process with a separate data root**, restore the
//! catalog, generations and published parts, replay the remote admission log, and prove the
//! restored node answers byte-for-byte identically and that the superseded epoch cannot publish.
//!
//! **Scope label, stated so the receipt cannot be over-read.** The replacement node here is
//! *process-isolated, not host-isolated*: it is a separate process with its own data root over a
//! shared durable object store on the same machine. That proves the **mechanism** — that nothing of
//! the lost node's local disk is required to restore it. It is **not** customer-scale RPO/RTO
//! evidence and not independent-host evidence; those stay with EXT-DR and the P14 load increment.
//! The recovery-point and recovery-time numbers below are recorded as **staging-shaped**.

use prism_engine::storage::object::{CachedObjectStore, LocalObjectStore, ObjectStore};
use prism_engine::storage::{ShardPlacement, CACHE_QUOTA_BYTES};
use prism_engine::wal::{RemoteWal, WalRecord};
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
    let path = std::env::temp_dir().join(format!("prism-hydrate-{tag}-{}-{n}", std::process::id()));
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

fn cold(backend: Arc<dyn ObjectStore>) -> Arc<CachedObjectStore> {
    Arc::new(CachedObjectStore::new(backend, CACHE_QUOTA_BYTES))
}

fn engine_on(root: &Path, backend: &Arc<dyn ObjectStore>) -> Engine {
    Engine::init(root, config())
        .unwrap()
        .with_cold(cold(Arc::clone(backend)))
}

fn open_on(root: &Path, backend: &Arc<dyn ObjectStore>) -> Engine {
    Engine::open(root)
        .unwrap()
        .with_cold(cold(Arc::clone(backend)))
}

/// The complete answer a drill compares byte-for-byte: every hit id, in order.
fn answer(engine: &Engine) -> Vec<String> {
    engine
        .search(&Query {
            text: "the tool call timed out retrying".into(),
            k: 25,
            tenant: Some("t1".into()),
            rerank: 50,
            ..Default::default()
        })
        .unwrap()
        .hits
        .into_iter()
        .map(|hit| hit.event.event_id)
        .collect()
}

/// Run a `prism` subcommand in its own process against a directory-backed object store.
fn run(store_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(prism())
        .args(args)
        .env("PRISM_OBJECT_STORE_DIR", store_dir)
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

// ---------------------------------------------------------------------------------------------
// The drill
// ---------------------------------------------------------------------------------------------

#[test]
fn the_disaster_drill_restores_a_replacement_node_from_backup_alone() {
    let store_dir = tmp("objstore");
    std::fs::create_dir_all(&store_dir).unwrap();
    let backend: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(&store_dir));

    let root_a = tmp("node-a");
    let root_b = tmp("node-b");

    // 1. Publish a customer-shaped dataset on node A.
    let published = prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 900, 5);
    let engine_a = engine_on(&root_a, &backend);
    engine_a.acquire_ownership().unwrap();
    engine_a
        .ingest(published.clone(), 1_760_000_000_000)
        .unwrap();
    let expected_answer = answer(&engine_a);
    let expected_snapshot = engine_a.snapshot().unwrap();
    assert!(
        !expected_answer.is_empty(),
        "the drill needs a real answer to compare"
    );

    // Back it up: every published part, the generations they need, and the catalog mirror.
    let backup = engine_a.backup_published().unwrap();
    assert_eq!(backup.snapshot_id, expected_snapshot.snapshot_id);
    assert_eq!(backup.parts.len(), expected_snapshot.part_ids().len());
    assert!(backup.bytes > 0);

    // 2. Acknowledge further events that are NOT yet published. The remote admission record is the
    //    promise a producer has already been told is safe.
    let acked_only = prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 120, 11);
    let writer_a = Ingestor::open_replicated(open_on(&root_a, &backend), 0).unwrap();
    let remote_wal = RemoteWal::new(Arc::clone(&backend), 0);
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

    // The complete expected answer AFTER the acked tail is published, derived independently by a
    // clean engine ingesting both batches.
    let baseline_root = tmp("baseline");
    let baseline = Engine::init(&baseline_root, config()).unwrap();
    baseline
        .ingest(published.clone(), 1_760_000_000_000)
        .unwrap();
    baseline
        .ingest(acked_only.clone(), 1_760_000_000_002)
        .unwrap();
    let expected_after_replay = answer(&baseline);

    // 3. Destroy the node-local disk. Everything node A had that was not on the object store is
    //    gone: the hot tier, the local WAL, the catalog, the generations.
    drop(writer_a);
    std::fs::remove_dir_all(&root_a).unwrap();
    assert!(!root_a.exists());

    // 4. A replacement node — a separate process, its own data root, nothing of node A's disk.
    let recovery_started = prism_engine::engine::now_ms();
    ok(
        &run(&store_dir, &["init", "--path", root_b.to_str().unwrap()]),
        "init the replacement node",
    );

    // 5. Restore catalog, generations, and published parts.
    let hydrate_out = ok(
        &run(&store_dir, &["hydrate", "--path", root_b.to_str().unwrap()]),
        "hydrate the replacement node",
    );
    let hydrated: serde_json::Value = serde_json::from_str(hydrate_out.trim()).unwrap();
    assert_eq!(hydrated["status"], "hydrated");
    assert_eq!(hydrated["snapshot_id"], expected_snapshot.snapshot_id);
    assert_eq!(
        hydrated["parts"].as_u64().unwrap() as usize,
        expected_snapshot.part_ids().len()
    );

    // The restored node answers identically to the pre-disaster node — from backup alone, before
    // any WAL replay.
    let restored = open_on(&root_b, &backend);
    assert_eq!(
        answer(&restored),
        expected_answer,
        "a node restored from backup must answer byte-for-byte identically"
    );
    // 7a. Snapshots compare byte-for-byte.
    assert_eq!(
        restored.snapshot().unwrap().snapshot_id,
        expected_snapshot.snapshot_id
    );
    drop(restored);

    // 6. Replay the remote admission log — the only route to the acked-but-unpublished tail, since
    //    node A's local WAL died with its disk. A separate process again.
    let recover_out = ok(
        &run(
            &store_dir,
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
        "replay the remote admission log",
    );
    let recovered: serde_json::Value = serde_json::from_str(recover_out.trim()).unwrap();
    assert_eq!(
        recovered["recovered_events"].as_u64().unwrap() as usize,
        recovery_point_events,
        "every acknowledged event must be recovered — the ack is the promise"
    );
    let recovery_time_ms = prism_engine::engine::now_ms().saturating_sub(recovery_started);

    // 7b. The complete answer after replay equals the independently derived expectation.
    let replayed = open_on(&root_b, &backend);
    assert_eq!(
        answer(&replayed),
        expected_after_replay,
        "after replay the restored node must equal a clean engine fed the same events"
    );

    // 8. The superseded epoch cannot publish. The replacement acquired a higher epoch during
    //    recovery, so a writer still holding the old one is fenced by name.
    let stale_root = tmp("stale-writer");
    let stale_engine = engine_on(&stale_root, &backend);
    stale_engine.acquire_ownership().unwrap();
    let overtaking = open_on(&root_b, &backend);
    overtaking.acquire_ownership().unwrap();
    let stale_error = stale_engine
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 16, 3),
            1_760_000_000_003,
        )
        .unwrap_err()
        .to_string();
    assert!(
        stale_error.contains("write fenced"),
        "the superseded epoch must be refused by name: {stale_error}"
    );

    // 9. Record the drill's measurements — labelled for what they are.
    let receipt = serde_json::json!({
        "drill": "published-part backup and hydration (D-094)",
        "isolation": "process-isolated, not host-isolated",
        "scope_label": "staging-shaped measurements; NOT customer-scale RPO/RTO evidence (EXT-DR) \
                        and NOT independent-host evidence (P14)",
        "recovery_point": {
            "acknowledged_events_recovered": recovery_point_events,
            "acknowledged_events_lost": 0,
        },
        "recovery_time_ms_staging": recovery_time_ms,
        "restored_parts": expected_snapshot.part_ids().len(),
        "restored_bytes": backup.bytes,
    });
    assert_eq!(receipt["recovery_point"]["acknowledged_events_lost"], 0);
    eprintln!("{receipt}");

    drop(replayed);
    drop(overtaking);
    for r in [root_b, baseline_root, stale_root, store_dir] {
        let _ = std::fs::remove_dir_all(r);
    }
}

// ---------------------------------------------------------------------------------------------
// Every failure mode fails by name
// ---------------------------------------------------------------------------------------------

/// Build a backed-up store and return `(store_dir, backend, source_root)`.
fn backed_up(tag: &str) -> (PathBuf, Arc<dyn ObjectStore>, PathBuf) {
    let store_dir = tmp(&format!("{tag}-objstore"));
    std::fs::create_dir_all(&store_dir).unwrap();
    let backend: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(&store_dir));
    let root = tmp(&format!("{tag}-source"));
    let engine = engine_on(&root, &backend);
    engine
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 300, 4),
            1_760_000_000_000,
        )
        .unwrap();
    engine.backup_published().unwrap();
    (store_dir, backend, root)
}

/// The first backed-up part id, and the key of one of its files.
fn a_backed_up_file(backend: &Arc<dyn ObjectStore>, file_suffix: &str) -> String {
    backend
        .list("parts/")
        .unwrap()
        .into_iter()
        .find(|k| k.ends_with(file_suffix))
        .unwrap_or_else(|| panic!("no backed-up file ending in {file_suffix}"))
}

#[test]
fn a_corrupt_backup_file_is_refused_by_name() {
    let (store_dir, backend, root) = backed_up("corrupt");
    let key = a_backed_up_file(&backend, "manifest.bin");
    let mut bytes = backend.get(&key).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff; // same length, different bytes
    backend.put(&key, &bytes).unwrap();

    let dest = tmp("corrupt-dest");
    let engine = engine_on(&dest, &backend);
    let err = engine.hydrate_from_backup(None).unwrap_err().to_string();
    assert!(
        err.contains("corrupt") && err.contains("SHA-256"),
        "a corrupt file must name itself: {err}"
    );
    for r in [store_dir, root, dest] {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn a_truncated_backup_file_is_refused_by_name() {
    let (store_dir, backend, root) = backed_up("truncated");
    let key = a_backed_up_file(&backend, "manifest.bin");
    let bytes = backend.get(&key).unwrap();
    backend.put(&key, &bytes[..bytes.len() / 2]).unwrap();

    let dest = tmp("truncated-dest");
    let engine = engine_on(&dest, &backend);
    let err = engine.hydrate_from_backup(None).unwrap_err().to_string();
    assert!(
        err.contains("truncated"),
        "a short file must be named a truncation, not a corruption: {err}"
    );
    for r in [store_dir, root, dest] {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn a_missing_backup_file_is_refused_by_name() {
    let (store_dir, backend, root) = backed_up("missing");
    let key = a_backed_up_file(&backend, "manifest.bin");
    backend.delete(&key).unwrap();

    let dest = tmp("missing-dest");
    let engine = engine_on(&dest, &backend);
    let err = engine.hydrate_from_backup(None).unwrap_err().to_string();
    assert!(
        err.contains("does not hold") || err.contains("no backup receipt"),
        "a missing object must be named: {err}"
    );
    for r in [store_dir, root, dest] {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn a_missing_generation_is_refused_rather_than_mixing_generations() {
    let (store_dir, backend, root) = backed_up("gen-missing");
    for key in backend.list("generations/").unwrap() {
        backend.delete(&key).unwrap();
    }

    let dest = tmp("gen-missing-dest");
    let engine = engine_on(&dest, &backend);
    let err = engine.hydrate_from_backup(None).unwrap_err().to_string();
    assert!(
        err.contains("generation") && err.contains("mix generations"),
        "a restore without its codebook must refuse rather than mix generations: {err}"
    );
    for r in [store_dir, root, dest] {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn a_generation_that_is_not_what_it_claims_is_refused_by_content_address() {
    let (store_dir, backend, root) = backed_up("gen-wrong");
    let key = backend
        .list("generations/")
        .unwrap()
        .into_iter()
        .next()
        .expect("a backed-up generation");
    let mut g: serde_json::Value = serde_json::from_slice(&backend.get(&key).unwrap()).unwrap();
    // Keep the declared id, change a field the content address covers. `model_version` is part of
    // the addressed body, so the recomputed id no longer matches the one the file claims.
    g["model_version"] = serde_json::json!("tampered-in-place");
    backend.put(&key, &serde_json::to_vec(&g).unwrap()).unwrap();

    let dest = tmp("gen-wrong-dest");
    let engine = engine_on(&dest, &backend);
    let err = engine.hydrate_from_backup(None).unwrap_err().to_string();
    assert!(
        err.contains("content address") || err.contains("generation"),
        "a codebook that is not what it claims must be refused: {err}"
    );
    for r in [store_dir, root, dest] {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn a_part_belonging_to_another_shard_is_refused_by_name() {
    let (store_dir, backend, root) = backed_up("wrong-tenant");
    let dest = tmp("wrong-tenant-dest");
    let engine = engine_on(&dest, &backend);

    // A placement that owns nothing this backup holds: shard 1 of a 1-shard cluster can never be
    // the owner (every bucket routes to shard 0), so every part is foreign.
    let placement = ShardPlacement {
        scheme: config().partitions,
        shard_id: 1,
        shard_count: 1,
    };
    let err = engine
        .hydrate_from_backup(Some(&placement))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("does not route to shard") && err.contains("routing fault"),
        "a part that arrived at the wrong shard must be named a routing fault: {err}"
    );
    for r in [store_dir, root, dest] {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn hydration_refuses_to_overwrite_a_live_database() {
    let (store_dir, backend, root) = backed_up("live");

    // A destination that is already a serving node with its own published data. It publishes to its
    // own object store — two independent live stores sharing one mirror key space would be the
    // split-brain D-069 exists to detect, which is a different test.
    let dest = tmp("live-dest");
    let live = Engine::init(&dest, config()).unwrap();
    live.ingest(
        prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 120, 7),
        1_760_000_000_005,
    )
    .unwrap();
    let before = live.snapshot().unwrap().snapshot_id;
    drop(live);

    // Now point that same live store at the backup and try to restore over it.
    let live = open_on(&dest, &backend);
    let err = live.hydrate_from_backup(None).unwrap_err().to_string();
    assert!(
        err.contains("refusing to hydrate onto a live store"),
        "restoring over a serving node must be refused by name: {err}"
    );
    assert_eq!(
        live.snapshot().unwrap().snapshot_id,
        before,
        "a refused hydration must not have touched the live catalog"
    );
    for r in [store_dir, root, dest] {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn a_part_never_completely_backed_up_is_refused_rather_than_half_restored() {
    let (store_dir, backend, root) = backed_up("no-receipt");
    // Delete the receipt but leave the files: the exact state a crash mid-backup leaves.
    let receipt = backend
        .list("parts/")
        .unwrap()
        .into_iter()
        .find(|k| k.ends_with("BACKUP.json"))
        .expect("a receipt");
    backend.delete(&receipt).unwrap();

    let dest = tmp("no-receipt-dest");
    let engine = engine_on(&dest, &backend);
    let err = engine.hydrate_from_backup(None).unwrap_err().to_string();
    assert!(
        err.contains("never completely backed up"),
        "a part with files but no receipt is not backed up, and must say so: {err}"
    );
    assert!(
        !engine.store.current_path().exists()
            || std::fs::read_to_string(engine.store.current_path())
                .unwrap()
                .trim()
                .is_empty(),
        "a refused hydration must never leave CURRENT naming a partially restored snapshot"
    );
    for r in [store_dir, root, dest] {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn reconciliation_reclaims_a_dead_parts_whole_backup_set_and_never_a_live_one() {
    let (store_dir, backend, root) = backed_up("reconcile");
    let engine = open_on(&root, &backend);

    let live_keys: Vec<String> = backend
        .list("parts/")
        .unwrap()
        .into_iter()
        .filter(|k| !k.ends_with("BACKUP.json"))
        .collect();
    assert!(
        live_keys.len() > 1,
        "a part's backup set is more than its cold tier: {live_keys:?}"
    );

    // A part no snapshot names, aged past the horizon, loses its whole set — not just rerank.vec.
    let dead = "part-that-no-snapshot-names";
    for file in ["manifest.bin", "rerank.vec", "BACKUP.json"] {
        backend
            .put(&format!("parts/{dead}/{file}"), b"orphan")
            .unwrap();
    }
    // Ages are measured against the *filesystem's* mtime, so the horizon must be pushed past the
    // real clock — a hardcoded epoch in the past clamps every age to zero and graces everything.
    let far_future = prism_engine::engine::now_ms() + 30 * 24 * 60 * 60 * 1000;
    let report = engine
        .reconcile_remote_orphans(3, far_future, true)
        .unwrap();

    // The live part keeps every one of its objects, receipt included — at the same horizon that
    // reclaims the dead one, so this is protection on the merits and not youth.
    for key in &live_keys {
        assert!(
            !report.removed.contains(key),
            "reconciliation must never sweep a live part's backup: {key}"
        );
    }
    for file in ["manifest.bin", "rerank.vec", "BACKUP.json"] {
        let key = format!("parts/{dead}/{file}");
        assert!(
            report.removed.contains(&key),
            "a dead part's whole backup set must be reclaimed; {key} was not in {:?}",
            report.removed
        );
    }
    for r in [store_dir, root] {
        let _ = std::fs::remove_dir_all(r);
    }
}
