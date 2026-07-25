//! **S12 §21 — partial failure is named, never silent** ([query §21](../../../docs/QUERY-CONTRACT.md)).
//!
//! A distributed query that cannot reach a shard it needs **fails by default, with the shard named** —
//! never a silently short result. A caller may opt in **per query** to a best-effort partial answer,
//! which carries a structured `missing_shards` report (shard ids + reason) and mirrors the count into
//! the counters. A partial answer is therefore impossible to receive without having asked for one.
//!
//! And partial results do not mix with semantic aggregates: a best-effort `GROUP BY` over an
//! incomplete shard set is **refused by name**, because a cluster distribution over missing data is
//! not comparable to a complete one.
//!
//! This exercises the coordinator's partial-failure **semantics** through a test-only unreachable
//! seam at the coordinator boundary — not transport-level partition behaviour, which stays a named
//! wall. Its own binary: the injection is a process-global, so it must not run beside other cluster
//! tests.

use prism_engine::sharded::{inject_unreachable_shards, Cluster};
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::{Query, SearchResult};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);
const TS: i64 = 1_760_000_000_000;

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-pf-{}-{}-{}", tag, std::process::id(), n));
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

fn cross_tenant_query(group_k: Option<usize>) -> Query {
    Query {
        text: "the tool call timed out retrying".into(),
        tenant: None,
        k: 20,
        rerank: 60,
        nprobe: 8,
        candidates: 200,
        group_k,
        ..Default::default()
    }
}

fn ids(r: &SearchResult) -> Vec<String> {
    r.hits.iter().map(|h| h.event.event_id.clone()).collect()
}

/// Events whose body is exactly the query text — so they dominate the top-k — spread across tenants,
/// so every shard holds some and a dropped shard leaves a meaningful (non-empty) partial answer.
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

#[test]
fn a_partial_answer_is_fail_named_by_default_and_labelled_only_on_opt_in() {
    let cluster = Cluster::init(&tmp("partial"), 4, config()).unwrap();
    cluster.ingest(matching_corpus(400), TS).unwrap();

    // The complete answer, all shards reachable.
    let complete = ids(&cluster.search(&cross_tenant_query(None)).unwrap());
    assert!(!complete.is_empty(), "the corpus must answer the query");

    // Shard 1 goes unreachable at the coordinator boundary.
    inject_unreachable_shards(&[1]);

    // DEFAULT: fail, with the shard named — never a silently short result.
    let err = cluster
        .search(&cross_tenant_query(None))
        .expect_err("a query touching an unreachable shard must fail by default, not return short");
    let msg = err.to_string();
    assert!(
        msg.contains("shard 1 unreachable") && msg.contains("best_effort"),
        "the failure must name the shard and the opt-in: {msg}"
    );

    // OPT-IN: a labelled partial answer.
    let mut q = cross_tenant_query(None);
    q.best_effort = true;
    let partial = cluster.search(&q).unwrap();
    assert_eq!(
        partial.missing_shards.len(),
        1,
        "the dropped shard must be reported"
    );
    assert_eq!(partial.missing_shards[0].shard, 1);
    assert!(
        !partial.missing_shards[0].reason.is_empty(),
        "the report must carry a reason"
    );
    assert_eq!(
        partial.counters.shards_missing, 1,
        "the count must mirror into the counters"
    );
    assert!(
        !partial.hits.is_empty(),
        "the reachable shards still answer"
    );

    // Every hit in the partial answer comes from a **reachable** shard — dropping shard 1 never
    // fabricates a hit and never returns one of shard 1's rows. (A subset-of-complete check would be
    // wrong here: with tied scores, dropping a shard legitimately promotes other shards' rows into
    // the top-k, so the partial answer can hold rows the complete top-k did not.)
    for h in &partial.hits {
        assert_ne!(
            cluster.shard_index(&h.event.tenant_id),
            1,
            "the partial answer returned a row from the dropped shard 1: {}",
            h.event.event_id
        );
    }

    // AGGREGATE REFUSAL: a best-effort semantic GROUP BY over an incomplete shard set is refused.
    let mut gq = cross_tenant_query(Some(4));
    gq.best_effort = true;
    let gerr = cluster
        .search(&gq)
        .expect_err("a best-effort GROUP BY over an incomplete shard set must be refused");
    let gmsg = gerr.to_string();
    assert!(
        gmsg.contains("not comparable") && gmsg.contains("GROUP BY"),
        "the aggregate refusal must be named: {gmsg}"
    );

    // Reset, and prove the label is impossible to receive without a real miss: best-effort with every
    // shard reachable reports NOTHING missing.
    inject_unreachable_shards(&[]);
    let mut q2 = cross_tenant_query(None);
    q2.best_effort = true;
    let whole = cluster.search(&q2).unwrap();
    assert!(
        whole.missing_shards.is_empty() && whole.counters.shards_missing == 0,
        "best-effort with all shards reachable must report no missing shards"
    );
    assert_eq!(
        ids(&whole),
        complete,
        "a complete best-effort answer equals the ground truth"
    );
}
