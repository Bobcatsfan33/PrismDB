//! **The S11 storage gates.** This file grows across the sprint; it starts with the one gate that
//! is localized and load-bearing: the rerank **fetch budget** is enforceable reality
//! ([storage contract §6](../../../docs/STORAGE-CONTRACT.md)) — a plan declares a byte budget for
//! the cold tier, execution is bounded by it, and exhaustion is a **named** degradation carried in
//! EXPLAIN, never a silent over-fetch.

use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Query;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

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
