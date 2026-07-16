//! **The S10 merge scheduler: bounded part count and debt under sustained ingest, and answers a
//! merge cannot change** ([merge contract §2](../../../docs/MERGE-CONTRACT.md)).
//!
//! Two properties, both prerequisites for the soak (§8):
//! - a merge is compaction, so it may not change an answer — the top-k over the same rows is
//!   byte-identical before and after any number of merge cycles (C-4/C-5);
//! - size-tiered selection keeps part count and merge debt **bounded** under sustained ingest,
//!   instead of growing without limit (small parts) or rewriting the whole store every cycle
//!   (S0 full compaction).

use prism_engine::merge::{MERGE_TIER_FANOUT, MERGE_TIER_RATIO};
use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::{Event, Query};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-sched-{}-{}-{}", tag, std::process::id(), n));
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

fn batch(tag: &str, n: usize, t: i64) -> Vec<Event> {
    (0..n)
        .map(|i| Event {
            event_id: format!("{tag}{i:06}"),
            tenant_id: format!("t{}", i % 3),
            event_time: t + i as i64,
            observed_time: t + i as i64,
            event_name: "e".into(),
            cost: 0.01,
            error: i % 7 == 0,
            body: format!("the tool call timed out retry {tag} {i}"),
            trace_id: String::new(),
            span_id: String::new(),
            attributes: Default::default(),
            idempotency_key: None,
        })
        .collect()
}

fn top_ids(engine: &Engine) -> Vec<String> {
    let q = Query {
        text: "the tool call timed out".into(),
        k: 20,
        tenant: Some("t1".into()),
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

fn parts(engine: &Engine) -> usize {
    engine.snapshot().unwrap().parts.len()
}

/// **A merge cannot change an answer.** The top-k over the same rows is identical after any number
/// of merge cycles (C-4/C-5) — merges move bytes, never answers.
#[test]
fn a_merge_never_changes_the_answer() {
    let root = tmp("invariant");
    let engine = Engine::init(&root, config()).unwrap();
    // Several batches so there is a full tier to merge.
    for i in 0..MERGE_TIER_FANOUT + 1 {
        engine
            .ingest(
                batch(&format!("b{i}"), 300, 1_760_000_000_000 + i as i64),
                i as i64,
            )
            .unwrap();
    }
    let before = top_ids(&engine);
    assert!(!before.is_empty());

    for cycle in 0..5 {
        let report = engine.merge_tiered(100 + cycle).unwrap();
        assert!(
            report.plan.is_some(),
            "the tiered merge must record its plan"
        );
        assert_eq!(
            top_ids(&engine),
            before,
            "a merge changed the answer on cycle {cycle} — a merge is compaction, not a rewrite of \
             what the store says (C-4/C-5)"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// **Sustained ingest reaches a bounded steady state.** Part count and merge debt stay bounded
/// across many ingest+merge cycles, and the plan explains every decision.
#[test]
fn sustained_ingest_reaches_a_bounded_steady_state() {
    let root = tmp("steady");
    let engine = Engine::init(&root, config()).unwrap();

    let cycles = 40usize;
    let mut max_parts_after_warmup = 0usize;
    let mut max_debt_after_warmup = 0usize;
    for c in 0..cycles {
        engine
            .ingest(
                batch(&format!("c{c}"), 200, 1_760_000_000_000 + c as i64),
                c as i64,
            )
            .unwrap();
        let report = engine.merge_tiered(1000 + c as i64).unwrap();
        let plan = report.plan.expect("every cycle records a plan");
        // Explainability: any op names its tier and fan-out reason.
        for op in &plan.ops {
            assert!(op.reason.contains("fan-out") && !op.part_ids.is_empty());
        }
        // Ignore the first few cycles while the tiers fill.
        if c >= 10 {
            max_parts_after_warmup = max_parts_after_warmup.max(parts(&engine));
            max_debt_after_warmup = max_debt_after_warmup.max(report.merge_debt);
        }
    }

    // Steady state: with 3 tenants over the default bucket count and fan-out F, part count is
    // bounded by a small multiple of (partitions × tiers × F), NOT by the number of cycles. A
    // generous ceiling that still fails an unbounded (linear-in-cycles) part count.
    let ceiling = 200usize;
    assert!(
        max_parts_after_warmup < ceiling,
        "part count did not reach a steady state: {max_parts_after_warmup} parts after warmup \
         over {cycles} cycles (ceiling {ceiling}). Ratio={MERGE_TIER_RATIO}, fan-out={MERGE_TIER_FANOUT}."
    );
    // Debt is likewise bounded (it is excess-parts-beyond-one-per-tier, which the scheduler keeps
    // shrinking), not growing with the cycle count.
    assert!(
        max_debt_after_warmup < ceiling,
        "merge debt is unbounded: {max_debt_after_warmup} after warmup"
    );
    let _ = std::fs::remove_dir_all(&root);
}
