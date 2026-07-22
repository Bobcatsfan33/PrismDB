//! **The phased == monolith oracle (S12 inc2).** The phased search path (candidate phase + rerank
//! phase) must be identical to the frozen monolithic implementation it replaced — not only on the
//! rows it returns, but on **every outcome class**: the same error, the same named refusal, the same
//! degraded behavior, the same counters, for the same inputs. An early return (empty, bridge,
//! space-error), a budget exhaustion, or a named refusal is exactly where a phased path forks
//! silently — most gates assert on result rows, and an early return produces none — so this oracle
//! asserts *outcome identity*, over the golden corpus and the unhappy paths alike.
//!
//! Deleted with `search_at_monolith` at the S12 inc2 exit criterion, once this gate has run green.

use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::error::Result;
use prism_types::{Query, SearchResult};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-oracle-{}-{}-{}", tag, std::process::id(), n));
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

/// The outcome class of a search, comparable to the bit: the whole serialized result on success
/// (rows, counters, explain, degraded flags, snapshot), or the exact error string on failure.
fn outcome(r: Result<SearchResult>) -> std::result::Result<String, String> {
    match r {
        Ok(sr) => Ok(serde_json::to_string(&sr).unwrap()),
        Err(e) => Err(e.to_string()),
    }
}

fn base(tenant: Option<&str>) -> Query {
    Query {
        text: "the tool call timed out retrying".into(),
        k: 15,
        tenant: tenant.map(str::to_string),
        rerank: 40,
        explain: true,
        ..Default::default()
    }
}

/// Every query shape that forks the search path — happy answers, empty sets, budget exhaustion,
/// thresholds (few or none clearing the bar), grouping, forced plans, and invalid inputs — answered
/// by the phased path and the monolith, must produce the identical outcome class.
#[test]
fn the_phased_path_is_identical_to_the_monolith_over_happy_and_unhappy_paths() {
    let root = tmp("oracle");
    let engine = Engine::init(&root, config()).unwrap();
    engine
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 3000, 5),
            1_760_000_000_000,
        )
        .unwrap();
    let snap = engine.snapshot().unwrap();

    let mut queries: Vec<(&str, Query)> = Vec::new();

    // Happy: per-tenant golden searches, varying k / rerank / nprobe / adaptive.
    for t in ["t0", "t1", "t2", "t3", "t4"] {
        queries.push(("golden", base(Some(t))));
        let mut q = base(Some(t));
        q.k = 3;
        q.rerank = 8;
        q.nprobe = 4;
        queries.push(("small-k", q));
        let mut q = base(Some(t));
        q.adaptive = true;
        queries.push(("adaptive", q));
    }

    // Cross-tenant (no tenant filter): touches every part.
    queries.push(("cross-tenant", base(None)));

    // Empty eligible: a tenant with no data.
    queries.push(("empty-tenant", base(Some("nobody-here"))));

    // Budget-starved: a tiny fetch budget forces the named degradation (fetch_budget_exhausted).
    let mut q = base(Some("t1"));
    q.fetch_budget_bytes = Some(5 * 64 * 4); // room for ~5 exact vectors of many
    queries.push(("budget-starved", q));
    let mut q = base(None);
    q.fetch_budget_bytes = Some(64 * 4); // room for one vector
    queries.push(("budget-starved-x", q));

    // Threshold: some clear the bar, some (a high bar) clear none — fewer than k, the honest count.
    let mut q = base(Some("t2"));
    q.threshold = Some(0.5);
    queries.push(("threshold-mid", q));
    let mut q = base(Some("t2"));
    q.threshold = Some(0.999);
    queries.push(("threshold-high", q));

    // Grouping (semantic GROUP BY over the rerank survivors).
    let mut q = base(Some("t3"));
    q.group_k = Some(4);
    queries.push(("group-k", q));

    // Forced plans (plan-invariance): each strategy, same answer.
    for plan in ["interleaved", "scalar-first", "semantic-first"] {
        let mut q = base(Some("t0"));
        q.plan = Some(plan.to_string());
        queries.push(("forced-plan", q));
    }

    // Invalid inputs: named refusals (k=0, nprobe=0, empty text).
    let mut q = base(Some("t1"));
    q.k = 0;
    queries.push(("invalid-k", q));
    let mut q = base(Some("t1"));
    q.nprobe = 0;
    queries.push(("invalid-nprobe", q));
    let mut q = base(Some("t1"));
    q.text = "   ".into();
    queries.push(("invalid-text", q));

    let mut compared = 0;
    for (tag, q) in &queries {
        let phased = outcome(engine.search_at(&snap, q));
        let mono = outcome(engine.search_at_monolith(&snap, q));
        assert_eq!(
            phased, mono,
            "phased path diverged from the monolith on `{tag}` query: {q:?}"
        );
        compared += 1;
    }
    assert!(compared >= 20, "the oracle must cover a real battery");

    let _ = std::fs::remove_dir_all(&root);
}
