//! **The S10 ENOSPC gate: the disk filling is a named condition, never a corruption**
//! ([merge contract §3](../../../docs/MERGE-CONTRACT.md)).
//!
//! This clause exists because the storage engine's own build host ran out of disk during the
//! project. Out-of-space is a *returned error*, not a crash, so unlike the abort-based kill points
//! it is exercised in-process: fill the disk at each write boundary and assert the operation fails
//! with `OutOfSpace`, the store still opens and `verify()`s, it is old-or-new-never-hybrid, and it
//! succeeds unchanged once space returns. Merge admission is proven too: a nearly-full disk defers
//! the merge with a named reason and leaves the store untouched.
//!
//! Injection is process-global, so every test here holds `ENOSPC_LOCK`.

use prism_engine::Engine;
use prism_part::partition::PartitionScheme;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::error::PrismError;
use prism_types::Event;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static ENOSPC_LOCK: Mutex<()> = Mutex::new(());
static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-enospc-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn config() -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 64,
        nlist: 32,
        pq_m: 8,
        seed: 1234,
        kmeans_restarts: 1,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        partitions: PartitionScheme {
            buckets: 16,
            time_window_ms: 24 * 60 * 60 * 1000,
            dedicated: Default::default(),
        },
        promote: Vec::new(),
    }
}

fn batch(tag: &str, n: usize, t: i64) -> Vec<Event> {
    (0..n)
        .map(|i| Event {
            event_id: format!("{tag}{i:05}"),
            tenant_id: "alpha".into(),
            event_time: t + i as i64,
            observed_time: t + i as i64,
            event_name: "e".into(),
            cost: 0.01,
            error: false,
            body: format!("the tool call timed out {tag} {i}"),
            trace_id: String::new(),
            span_id: String::new(),
            attributes: Default::default(),
            idempotency_key: None,
        })
        .collect()
}

/// True row count from the live snapshot's parts (not a search, which returns only the top-k
/// rerank survivors).
fn count(engine: &Engine) -> usize {
    let snap = engine.snapshot().unwrap();
    engine
        .open_parts(&snap)
        .unwrap()
        .iter()
        .map(|r| r.manifest.row_count)
        .sum()
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    // Poison-tolerant: one test's assertion failure must not cascade into the others.
    ENOSPC_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// After an injected fault, the store must open, verify, and hold exactly the pre-fault rows.
fn assert_healthy(root: &PathBuf, expect_rows: usize) {
    let engine = Engine::open(root).unwrap();
    engine
        .catalog()
        .verify()
        .expect("store must verify after an out-of-space fault");
    assert_eq!(
        count(&engine),
        expect_rows,
        "the store did not land on the pre-fault state"
    );
}

/// **A full disk while writing a part fails with a named error and changes nothing; the next
/// write, once space returns, succeeds** (merge contract §3).
#[test]
fn out_of_space_writing_a_part_is_named_and_recovers() {
    let _g = lock();
    let root = tmp("part");
    let engine = Engine::init(&root, config()).unwrap();
    engine
        .ingest(batch("a", 100, 1_760_000_000_000), 1)
        .unwrap();
    assert_eq!(count(&engine), 100);

    prism_part::faults::inject_out_of_space(Some("part.columns"));
    let err = engine
        .ingest(batch("b", 100, 1_760_000_100_000), 2)
        .unwrap_err();
    prism_part::faults::inject_out_of_space(None);
    assert!(
        matches!(err, PrismError::OutOfSpace(_)),
        "a full disk must surface as OutOfSpace, got: {err}"
    );

    // Nothing changed, and the store is healthy.
    assert_healthy(&root, 100);

    // Space returned: the same write now succeeds.
    let engine = Engine::open(&root).unwrap();
    engine
        .ingest(batch("b", 100, 1_760_000_100_000), 3)
        .unwrap();
    assert_eq!(count(&engine), 200);
    let _ = std::fs::remove_dir_all(&root);
}

/// **A full disk during the catalog commit — at the snapshot write and at the CURRENT swap — is
/// old-or-new, never hybrid.**
#[test]
fn out_of_space_committing_a_snapshot_is_old_or_new() {
    for point in ["catalog.snapshot", "catalog.current"] {
        let _g = lock();
        let root = tmp("commit");
        let engine = Engine::init(&root, config()).unwrap();
        engine.ingest(batch("a", 80, 1_760_000_000_000), 1).unwrap();

        prism_part::faults::inject_out_of_space(Some(point));
        let err = engine
            .ingest(batch("b", 80, 1_760_000_100_000), 2)
            .unwrap_err();
        prism_part::faults::inject_out_of_space(None);
        assert!(
            matches!(err, PrismError::OutOfSpace(_)),
            "commit at {point} must surface OutOfSpace, got: {err}"
        );

        // CURRENT still names the pre-fault snapshot: old, not hybrid.
        assert_healthy(&root, 80);

        let engine = Engine::open(&root).unwrap();
        engine.ingest(batch("b", 80, 1_760_000_100_000), 3).unwrap();
        assert_eq!(count(&engine), 160);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// **A merge on a nearly-full disk is deferred with a named reason and leaves the store
/// untouched; once space returns it runs** (merge contract §3, admission).
#[test]
fn a_merge_is_deferred_when_the_disk_is_nearly_full() {
    let _g = lock();
    let root = tmp("admit");
    let engine = Engine::init(&root, config()).unwrap();
    // Two batches → two parts, so there is something to merge.
    engine
        .ingest(batch("a", 100, 1_760_000_000_000), 1)
        .unwrap();
    engine
        .ingest(batch("b", 100, 1_760_000_100_000), 2)
        .unwrap();
    let before = engine.snapshot().unwrap().snapshot_id;

    // Pretend the device has almost nothing free: the merge must refuse before writing.
    prism_part::disk::set_available_override(Some(1024));
    let report = engine.merge(3).unwrap();
    prism_part::disk::set_available_override(None);
    assert!(
        report.deferred.is_some(),
        "the merge was not deferred on a nearly-full disk"
    );
    let reason = report.deferred.unwrap();
    assert!(
        reason.contains("insufficient free space") && reason.contains("margin"),
        "the deferral did not name the condition: {reason}"
    );
    // The store is exactly as it was — not started-and-stranded.
    assert_eq!(engine.snapshot().unwrap().snapshot_id, before);
    assert_eq!(report.parts_out, report.parts_in);

    // Space returned: the merge now runs and compacts the two parts.
    let engine = Engine::open(&root).unwrap();
    let report = engine.merge(4).unwrap();
    assert!(report.deferred.is_none());
    assert!(
        report.parts_out < report.parts_in,
        "the merge did not compact"
    );
    assert_eq!(count(&engine), 200);
    let _ = std::fs::remove_dir_all(&root);
}
