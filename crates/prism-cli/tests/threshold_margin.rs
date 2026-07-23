//! **S12, [D-074](../../../docs/DECISIONS.md): a threshold query is bounded by the threshold, not a
//! width — and the mechanism is exercised at production-shaped margins, not just the rig's.**
//!
//! The rig's hash-embedder corpus reconstructs vectors near-exactly, so the *measured* margin ε is
//! ~1e-6 ([`pq-margin.json`](../../../testing/evidence/pq-margin.json)). That is honest for this
//! corpus and correctly receipted, but it means the relaxed-bound collection, the overfetch, the
//! within-ε observable, and the state-budget refusal would go essentially unexercised by the rig's
//! natural geometry. A real 768d embedding space re-derives a materially larger ε (issue #3). So a
//! **test-only** injection seam ([`inject_threshold_margin`](prism_engine::search::inject_threshold_margin),
//! never a production path) forces a production-plausible ε and a tiny state budget, and this test
//! gates that:
//!
//!   (a) the candidate phase **overfetches** as designed and rerank prunes back to the exact-τ answer,
//!       **byte-identical** to the un-inflated result;
//!   (b) the **within-ε counter** (`threshold_overfetch`) reports the overfetch honestly;
//!   (c) the threshold + broad-filter pathological case hits the **S9 named refusal**, on a single
//!       engine and through the cluster coordinator alike.
//!
//! And, on the natural geometry, that a threshold query recovers qualifying rows a top-`candidates`
//! width would silently drop, byte-identically at 1, 2, and 4 shards (sharding is a layout, §20).

use prism_engine::sharded::Cluster;
use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::{Query, SearchResult};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-thm-{}-{}-{}", tag, std::process::id(), n));
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

const TS: i64 = 1_760_000_000_000;
const HOT: &str = "the tool call timed out retrying";

/// `n` events whose body is exactly the query text — so each scores ~1.0 and clears any reasonable
/// threshold — spread across tenants, over a background of ordinary (low-scoring) Zipf events.
fn corpus(n_hot: usize) -> Vec<prism_types::Event> {
    let mut ev = prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 2000, 5);
    let hot = prism_engine::corpus::generate(prism_engine::corpus::Kind::Uniform, n_hot, 99)
        .into_iter()
        .enumerate()
        .map(|(i, mut e)| {
            e.event_id = format!("hot-{i:04}");
            e.body = HOT.into();
            e
        });
    ev.extend(hot);
    ev
}

/// A cross-tenant threshold query with **deliberately narrow widths**: a top-`candidates` bound of 10
/// could return at most 10 rows, so a threshold answer larger than that proves the *threshold* is the
/// operative bound, not the width.
fn threshold_query(tau: f32) -> Query {
    Query {
        text: HOT.into(),
        tenant: None,
        k: 100,
        candidates: 10,
        rerank: 10,
        nprobe: 8,
        threshold: Some(tau),
        ..Default::default()
    }
}

fn fp(r: &SearchResult) -> Vec<(String, u32)> {
    r.hits
        .iter()
        .map(|h| (h.event.event_id.clone(), h.score.to_bits()))
        .collect()
}

#[test]
fn a_threshold_query_is_bounded_by_the_threshold_and_the_mechanism_holds_at_injected_margins() {
    // Always start and end with the injection seam disarmed, so no other test in this binary — and no
    // stage of this one — inherits a forced margin.
    prism_engine::search::inject_threshold_margin(None, None);

    let n_hot = 60usize;
    let single = Engine::init(&tmp("single"), config()).unwrap();
    single.ingest(corpus(n_hot), TS).unwrap();
    let snap = single.snapshot().unwrap();

    // --- recall: the threshold bound recovers rows a width would drop -------------------------------
    // The natural geometry (measured ε ≈ 1e-6). A ranked query with the same narrow widths is bounded
    // by `candidates = 10`; the threshold query keeps every qualifying row.
    let tau = 0.5f32;
    let ranked = {
        let mut q = threshold_query(tau);
        q.threshold = None; // same widths, ranked instead of thresholded
        single.search_at(&snap, &q).unwrap()
    };
    let a = single.search_at(&snap, &threshold_query(tau)).unwrap();
    assert!(
        ranked.hits.len() <= 10,
        "a ranked query with candidates=10 must be width-bounded to 10, got {}",
        ranked.hits.len()
    );
    assert!(
        a.hits.len() > ranked.hits.len(),
        "the threshold bound must recover qualifying rows the width dropped: threshold returned {}, \
         width-bounded ranked returned {}",
        a.hits.len(),
        ranked.hits.len()
    );
    assert!(
        a.hits.len() > 10,
        "the threshold answer ({}) must exceed the candidate width (10) — proof the width is not the \
         operative bound",
        a.hits.len()
    );
    assert!(
        a.hits.iter().all(|h| h.score >= tau),
        "every returned row must clear the exact threshold"
    );
    // On the near-exact corpus the un-injected margin keeps no slop: nothing lands within ε of the bar.
    assert_eq!(
        a.counters.threshold_overfetch, 0,
        "the measured ε keeps no overfetch on this near-exact corpus"
    );

    // --- (a) + (b): inject a production-shaped ε; overfetch, then prune back byte-identically --------
    // ε = 4.0 relaxes the bound past any unit-vector distance, so the candidate phase keeps *every*
    // scanned row — including the low-scoring background that will not clear τ. Rerank prunes them.
    prism_engine::search::inject_threshold_margin(Some(4.0), None);
    let b = single.search_at(&snap, &threshold_query(tau)).unwrap();
    assert_eq!(
        fp(&a),
        fp(&b),
        "(a) the inflated-ε answer must be byte-identical to the measured-ε answer — rerank prunes \
         the overfetch back to the exact-τ set"
    );
    assert!(
        b.counters.threshold_overfetch > 0,
        "(b) the inflated ε must overfetch the low-scoring background, and the within-ε observable \
         must report it honestly — got 0"
    );
    // Honest in both directions: the overfetch is candidates the relaxed bound admitted but the exact
    // τ then rejected, so it cannot exceed what was kept beyond the answer.
    assert!(
        b.counters.threshold_overfetch
            <= b.counters
                .candidates_considered
                .saturating_sub(a.hits.len()),
        "the overfetch count ({}) must not exceed the candidates kept beyond the answer ({} − {})",
        b.counters.threshold_overfetch,
        b.counters.candidates_considered,
        a.hits.len()
    );
    prism_engine::search::inject_threshold_margin(None, None);

    // --- (c): threshold + broad filter + tiny state budget → refused by name (S9) -------------------
    // A low τ over no filter qualifies an unbounded set; a state budget of 4 is exceeded at once, and
    // the query is refused — never reranked without bound, never answered short.
    prism_engine::search::inject_threshold_margin(Some(4.0), Some(4));
    let refused = single.search_at(&snap, &threshold_query(0.0));
    let err = refused
        .expect_err("(c) a threshold query over the state budget must be refused, not answered");
    let msg = err.to_string();
    assert!(
        msg.contains("state budget") && msg.contains("D-074"),
        "(c) the refusal must be named (S9), citing the state budget and D-074: {msg}"
    );

    // The same refusal must surface through the cluster coordinator, from a shard's candidate phase.
    for n in [1usize, 4] {
        let cluster = Cluster::init(&tmp(&format!("cl-{n}")), n, config()).unwrap();
        cluster.ingest(corpus(n_hot), TS).unwrap();
        let cl_refused = cluster.search(&threshold_query(0.0));
        let cl_err = cl_refused.expect_err(&format!(
            "(c) the cluster at {n} shards must refuse, not answer"
        ));
        assert!(
            cl_err.to_string().contains("state budget"),
            "(c) the coordinator must surface the shard's named refusal at {n} shards: {}",
            cl_err
        );
    }
    prism_engine::search::inject_threshold_margin(None, None);

    // --- byte-identical at 1/2/4 under the new bounding (sharding is a layout, §20) ------------------
    let ground = fp(&a);
    for n in [1usize, 2, 4] {
        let cluster = Cluster::init(&tmp(&format!("layout-{n}")), n, config()).unwrap();
        cluster.ingest(corpus(n_hot), TS).unwrap();
        let cr = cluster.search(&threshold_query(tau)).unwrap();
        assert_eq!(
            fp(&cr),
            ground,
            "the threshold answer must be byte-identical to the single engine at {n} shards"
        );
    }
}
