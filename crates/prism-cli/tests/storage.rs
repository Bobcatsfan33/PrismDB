//! **The S11 storage gates.** This file grows across the sprint; it starts with the one gate that
//! is localized and load-bearing: the rerank **fetch budget** is enforceable reality
//! ([storage contract §6](../../../docs/STORAGE-CONTRACT.md)) — a plan declares a byte budget for
//! the cold tier, execution is bounded by it, and exhaustion is a **named** degradation carried in
//! EXPLAIN, never a silent over-fetch.

use prism_engine::storage::object::{
    CachedObjectStore, FaultConfig, FaultStore, LocalObjectStore, ObjectStore,
};
use prism_engine::storage::CACHE_QUOTA_BYTES;
use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Query;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!(
        "prism-storage-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
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

fn store() -> (Engine, PathBuf) {
    let root = tmp("s");
    let engine = Engine::init(&root, config()).unwrap();
    let events = prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 2000, 5);
    engine.ingest(events, 1_760_000_000_000).unwrap();
    (engine, root)
}

/// **The declared byte budget bounds the cold-tier fetch, and exhaustion is named.**
#[test]
fn the_fetch_budget_bounds_the_cold_tier_and_names_exhaustion() {
    let (engine, root) = store();
    let bytes_per_vector = 64 * 4; // dim * f32

    // Unbounded: fetches every rerank survivor's exact vector, not flagged.
    let q = Query {
        text: "the tool call timed out retrying".into(),
        k: 10,
        tenant: Some("t1".into()),
        rerank: 50,
        explain: true,
        ..Default::default()
    };
    let full = engine.search(&q).unwrap();
    assert!(!full.counters.fetch_budget_exhausted);
    let full_bytes = full.counters.exact_bytes_fetched;
    assert!(full_bytes > 0);

    // Budgeted to 10 vectors' worth: the fetch must not exceed it, and it must say it was capped.
    let budget = 10 * bytes_per_vector;
    let mut qb = q.clone();
    qb.fetch_budget_bytes = Some(budget);
    let limited = engine.search(&qb).unwrap();
    assert!(
        limited.counters.exact_bytes_fetched <= budget,
        "the fetch exceeded the declared budget: {} > {budget}",
        limited.counters.exact_bytes_fetched
    );
    assert!(
        limited.counters.fetch_budget_exhausted,
        "the budget was exhausted but not flagged — a silent over-/under-fetch"
    );
    assert!(limited.counters.exact_bytes_fetched < full_bytes);

    // EXPLAIN carries the two-tier bill and the bound.
    let ex = limited.explain.expect("explain requested");
    assert_eq!(ex.declared_fetch_budget_bytes, Some(budget));
    assert!(ex.fetch_budget_exhausted);
    assert!(
        ex.object_requests >= 1,
        "a cold fetch is at least one object request"
    );
    assert_eq!(ex.retrieved_bytes, limited.counters.exact_bytes_fetched);
    assert!(
        ex.estimated_cost_micros > 0,
        "the two-tier cost estimate must be nonzero"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// --- the object store, its faults, and the cache (storage contract §1, §2, §4) ---

/// **CAS create: two writers race, exactly one wins** (storage contract §2, D-066).
#[test]
fn put_if_absent_is_a_compare_and_swap() {
    let root = tmp("cas");
    std::fs::create_dir_all(&root).unwrap();
    let store = LocalObjectStore::new(&root);
    assert!(
        store.put_if_absent("catalog/CURRENT", b"snap-a").unwrap(),
        "first create must win"
    );
    assert!(
        !store.put_if_absent("catalog/CURRENT", b"snap-b").unwrap(),
        "second create must lose"
    );
    assert_eq!(
        store.get("catalog/CURRENT").unwrap(),
        b"snap-a",
        "the loser must not overwrite"
    );
    assert_eq!(store.head("catalog/CURRENT").unwrap(), Some(6));
    assert_eq!(store.head("catalog/missing").unwrap(), None);
    let _ = std::fs::remove_dir_all(&root);
}

/// **A truncated/out-of-range read is a named-byte error, never a silent short read** (storage §1).
#[test]
fn a_truncated_read_is_named() {
    let root = tmp("trunc");
    std::fs::create_dir_all(&root).unwrap();
    let store = LocalObjectStore::new(&root);
    store.put("parts/p/rerank.vec", &[7u8; 100]).unwrap();
    // Reading past the end names the shortfall.
    let err = store.get_range("parts/p/rerank.vec", 80, 40).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("truncated") && msg.contains("only 100 bytes"),
        "{msg}"
    );
    // A valid range succeeds and returns exactly the bytes asked for.
    assert_eq!(
        store.get_range("parts/p/rerank.vec", 10, 20).unwrap().len(),
        20
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// **The fault wrapper injects remote-style failures, each named** (storage §1).
#[test]
fn injected_faults_are_named() {
    let root = tmp("fault");
    std::fs::create_dir_all(&root).unwrap();
    let local = LocalObjectStore::new(&root);
    local.put("k", b"hello world").unwrap();
    let store = FaultStore::new(local);

    store.set(FaultConfig {
        unavailable: true,
        ..Default::default()
    });
    let err = store.get_range("k", 0, 5).unwrap_err().to_string();
    assert!(err.contains("remote unavailable"), "{err}");

    store.set(FaultConfig {
        truncate_reads: true,
        ..Default::default()
    });
    let err = store.get_range("k", 0, 5).unwrap_err().to_string();
    assert!(err.contains("truncated"), "{err}");

    store.set(FaultConfig {
        fail_next: true,
        ..Default::default()
    });
    assert!(
        store.get_range("k", 0, 5).is_err(),
        "the injected 5xx must fail the call"
    );
    assert_eq!(
        store.get_range("k", 0, 5).unwrap(),
        b"hello",
        "fail_next clears after one call"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// **A corrupt cache block is detected by content hash, evicted, and repaired from the remote**
/// (storage contract §4).
#[test]
fn a_corrupt_cache_block_is_repaired_from_the_remote() {
    let root = tmp("cache");
    std::fs::create_dir_all(&root).unwrap();
    let local = LocalObjectStore::new(&root);
    local
        .put("parts/p/rerank.vec", &(0u8..255).collect::<Vec<u8>>())
        .unwrap();
    let cached = CachedObjectStore::new(Arc::new(local), CACHE_QUOTA_BYTES);

    // Warm the cache (a miss populates it), then a hit serves from cache.
    let want = cached
        .get_range_cached("parts/p/rerank.vec", 0, 64)
        .unwrap();
    assert_eq!(cached.cache().stats().misses, 1);
    assert_eq!(
        cached
            .get_range_cached("parts/p/rerank.vec", 0, 64)
            .unwrap(),
        want
    );
    assert_eq!(cached.cache().stats().hits, 1);

    // Corrupt the cached block: the next read must detect it, refetch, and still be correct.
    assert!(cached.cache().corrupt_entry("parts/p/rerank.vec", 0, 64));
    let after = cached
        .get_range_cached("parts/p/rerank.vec", 0, 64)
        .unwrap();
    assert_eq!(
        after, want,
        "a corrupt cache block must be repaired to the true bytes"
    );
    assert_eq!(
        cached.cache().stats().corrupt_repaired,
        1,
        "the repair must be counted"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// **Remote unavailable is a named degradation: cached data serves, uncached fails named — never a
/// silent partial answer** (storage contract §4, the S12 slow-shard rule early).
#[test]
fn remote_unavailable_serves_cache_and_names_the_miss() {
    let root = tmp("degrade");
    std::fs::create_dir_all(&root).unwrap();
    let local = LocalObjectStore::new(&root);
    local
        .put(
            "parts/p/rerank.vec",
            &(0u8..=255).cycle().take(4096).collect::<Vec<u8>>(),
        )
        .unwrap();
    // Share the fault handle so the outage can be toggled after the cache is warm.
    let fault = Arc::new(FaultStore::new(local));
    let cached = CachedObjectStore::new(fault.clone(), CACHE_QUOTA_BYTES);

    // Warm one block while the remote is up.
    let warm = cached
        .get_range_cached("parts/p/rerank.vec", 0, 64)
        .unwrap();

    // Remote goes down.
    fault.set(FaultConfig {
        unavailable: true,
        ..Default::default()
    });

    // The cached block still serves — a query answerable from cache succeeds.
    assert_eq!(
        cached
            .get_range_cached("parts/p/rerank.vec", 0, 64)
            .unwrap(),
        warm,
        "a cached block must still serve when the remote is down"
    );
    // An uncached block fails with the remote condition NAMED — never a silent partial.
    let err = cached
        .get_range_cached("parts/p/rerank.vec", 2048, 64)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("remote unavailable"),
        "an uncached read against a dead remote must name the condition, not return a partial: {err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
