//! **S12 §21 — hedges are free, because of the pinned vector** ([query §21](../../../docs/QUERY-CONTRACT.md),
//! [D-079](../../../docs/DECISIONS.md)).
//!
//! A slow shard's fragment can be re-issued (hedged) to cut tail latency. This is free of correctness
//! risk for one reason: a fragment runs against the **pinned snapshot vector**, so a re-issue is
//! **byte-identical** to the original — the coordinator asserts it bit-for-bit (a divergence is a named
//! invariant violation), so deduping the duplicate is trivial and the answer never changes. And the
//! **blast radius is bounded**: past an in-flight cap, hedging stops, so a slow cluster cannot hedge
//! itself into collapse.
//!
//! Its own binary: the hedge/cap seams are process-global.

use prism_engine::sharded::{inject_hedge_shards, inject_max_inflight, Cluster};
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::{Query, SearchResult};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);
const TS: i64 = 1_760_000_000_000;

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-hedge-{}-{}-{}", tag, std::process::id(), n));
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

fn cross_tenant_query() -> Query {
    Query {
        text: "the tool call timed out retrying".into(),
        tenant: None,
        k: 20,
        rerank: 60,
        nprobe: 8,
        candidates: 200,
        ..Default::default()
    }
}

fn matching_corpus(n: usize) -> Vec<prism_types::Event> {
    prism_engine::corpus::generate(prism_engine::corpus::Kind::Uniform, n, 99)
        .into_iter()
        .enumerate()
        .map(|(i, mut e)| {
            e.event_id = format!("m-{i:04}");
            e.body = "the tool call timed out retrying".into();
            e
        })
        .collect()
}

fn fp(r: &SearchResult) -> Vec<(String, u32)> {
    r.hits
        .iter()
        .map(|h| (h.event.event_id.clone(), h.score.to_bits()))
        .collect()
}

#[test]
fn a_hedged_fragment_is_byte_identical_deduped_and_blast_radius_bounded() {
    let cluster = Cluster::init(&tmp("hedge"), 4, config()).unwrap();
    cluster.ingest(matching_corpus(400), TS).unwrap();

    // Baseline: no hedging.
    let base = cluster.search(&cross_tenant_query()).unwrap();
    assert_eq!(
        base.counters.hedges_issued, 0,
        "the baseline must not hedge"
    );

    // Hedge every shard, both rounds. The coordinator re-issues each fragment against the pinned
    // snapshot and compares it **bit-for-bit** to the original; a divergence would error. So a query
    // that succeeds with hedges_issued > 0 *is* the byte-identical proof, and dedup keeping the
    // original must leave the answer unchanged.
    inject_hedge_shards(&[0, 1, 2, 3]);
    let hedged = cluster.search(&cross_tenant_query()).unwrap();
    assert!(
        hedged.counters.hedges_issued > 0,
        "hedging did not fire — the fragment re-issue path was never exercised"
    );
    assert_eq!(
        fp(&hedged),
        fp(&base),
        "hedging changed the answer — a hedge must change latency, never the result"
    );

    // Blast radius: with a low in-flight cap, hedging stops before it can amplify load — a slow
    // cluster must not hedge itself into collapse.
    inject_max_inflight(Some(5));
    let capped = cluster.search(&cross_tenant_query()).unwrap();
    assert_eq!(fp(&capped), fp(&base), "capped hedging changed the answer");
    assert!(
        capped.counters.hedges_issued < hedged.counters.hedges_issued,
        "the in-flight cap did not bound hedging (capped {} vs uncapped {})",
        capped.counters.hedges_issued,
        hedged.counters.hedges_issued
    );

    inject_hedge_shards(&[]);
    inject_max_inflight(None);
}
