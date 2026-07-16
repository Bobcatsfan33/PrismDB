//! **The S10 tombstone gate: a delete is logical at once, physical at merge, and idempotent**
//! ([merge contract §6](../../../docs/MERGE-CONTRACT.md)).
//!
//! A delete writes a tombstone (one atomic catalog commit), so the row vanishes from queries
//! immediately while it is still physically present; a merge reconciles it away and clears the
//! tombstone. Re-deleting an already-deleted id is a no-op. Deletes operate on a live store and
//! never touch a frozen receipt corpus.

use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::{Event, Query};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-tomb-{}-{}-{}", tag, std::process::id(), n));
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

fn batch(n: usize) -> Vec<Event> {
    (0..n)
        .map(|i| Event {
            event_id: format!("e{i:05}"),
            tenant_id: "alpha".into(),
            event_time: 1_760_000_000_000 + i as i64,
            observed_time: 1_760_000_000_000 + i as i64,
            event_name: "e".into(),
            cost: 0.01,
            error: false,
            body: format!("the tool call timed out retry {i}"),
            trace_id: String::new(),
            span_id: String::new(),
            attributes: Default::default(),
            idempotency_key: None,
        })
        .collect()
}

fn hit_ids(engine: &Engine) -> Vec<String> {
    let q = Query {
        text: "the tool call timed out".into(),
        k: 100,
        tenant: Some("alpha".into()),
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

fn rows_on_disk(engine: &Engine) -> usize {
    let snap = engine.snapshot().unwrap();
    engine
        .open_parts(&snap)
        .unwrap()
        .iter()
        .map(|r| r.manifest.row_count)
        .sum()
}

/// **A delete is logical immediately and physical at merge.**
#[test]
fn a_delete_is_logical_at_once_and_physical_at_merge() {
    let root = tmp("del");
    let engine = Engine::init(&root, config()).unwrap();
    engine.ingest(batch(300), 1).unwrap();
    // Two batches so a query returns several matches and a merge has something to do.
    engine.ingest(batch(300), 2).unwrap(); // same ids: duplicates, reconciled at merge

    let victims: Vec<String> = (0..50).map(|i| format!("e{i:05}")).collect();
    assert!(hit_ids(&engine).iter().any(|id| victims.contains(id)));

    // Delete: the rows vanish from queries at once, though they are still on disk.
    let added = engine.delete(&victims, 3).unwrap();
    assert_eq!(added, 50);
    let after = hit_ids(&engine);
    assert!(
        after.iter().all(|id| !victims.contains(id)),
        "a deleted row still appeared in a query"
    );
    let rows_before_merge = rows_on_disk(&engine);
    assert!(
        rows_before_merge >= 300,
        "the rows should still be physically present before a merge"
    );

    // Merge reconciles them away and clears the tombstone.
    let report = engine.merge(4).unwrap();
    assert!(
        report.rows_out <= 250,
        "the merge did not physically drop the deleted rows"
    );
    assert!(
        engine.snapshot().unwrap().tombstones.is_empty(),
        "a full merge must clear reconciled tombstones"
    );
    // Still gone from queries after the merge.
    let after_merge = hit_ids(&engine);
    assert!(after_merge.iter().all(|id| !victims.contains(id)));
    assert!(!after_merge.is_empty(), "the non-deleted rows must survive");

    let _ = std::fs::remove_dir_all(&root);
}

/// **Re-deleting an already-deleted id is a no-op (idempotent).**
#[test]
fn delete_is_idempotent() {
    let root = tmp("idem");
    let engine = Engine::init(&root, config()).unwrap();
    engine.ingest(batch(100), 1).unwrap();
    let ids = vec!["e00001".to_string(), "e00002".to_string()];
    assert_eq!(engine.delete(&ids, 2).unwrap(), 2);
    // The same delete again adds nothing and does not churn the catalog.
    let before = engine.snapshot().unwrap().snapshot_id;
    assert_eq!(engine.delete(&ids, 3).unwrap(), 0);
    assert_eq!(
        engine.snapshot().unwrap().snapshot_id,
        before,
        "a no-op delete must not create a new snapshot"
    );
    let _ = std::fs::remove_dir_all(&root);
}
