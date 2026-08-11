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

/// `inject_threshold_margin` is a **process-global** seam, and cargo runs the tests in this binary
/// in parallel — so a test that forces a margin corrupts any concurrent test's measurement. Every
/// test here takes this lock for its whole body. Found the honest way: the aggregation gate below
/// passed alone and failed in-file, which is the signature of exactly this hazard.
static SEAM: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

/// A thing that answers a query: a single engine, or a cluster at some shard count. Named because
/// the boxed closure is otherwise a complex type, and the prune gate below runs the identical
/// assertions against all four.
type Answerer = (usize, Box<dyn Fn(&Query) -> SearchResult>);

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
    let _seam = SEAM.lock().unwrap_or_else(|e| e.into_inner());
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
    // The re-derived real ε (0.30, S13 dir 2 — up from the hash corpus's degenerate 1e-6) is large
    // enough that the margin now **bites on natural geometry**: the un-injected path already
    // overfetches and rerank prunes it back to the exact-τ set. This is exactly what the hash corpus's
    // 1e-6 ε could never exercise; the margin-injection below stays as the extreme-ε stress.
    assert!(
        a.counters.threshold_overfetch > 0,
        "the real-derived ε must exercise the overfetch on natural geometry, not only under injection"
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

/// **The exact-τ prune in `finalize`, in the two regimes where it is observable** (§22, [D-074](../../../../docs/DECISIONS.md)).
///
/// A mutation sweep found that deleting `scored.retain(|s| s.score >= tau)` was caught by **nothing**
/// across 158 tests — including the test above, whose own assertion reads *"every returned row must
/// clear the exact threshold"*. The reason is not that the prune does nothing: a probe showed it
/// removing **339 of 767** scored rows. It is that `hits` is `scored.iter().take(q.k)` over a vector
/// already sorted by score descending, so **while more than `k` rows clear τ, every sub-τ row sorts
/// below the top-`k` and is invisible** — 428 rows cleared τ against `k = 100`. The prune only shows
/// through the answer in two regimes, and nothing exercised either:
///
/// - **(i) fewer than `k` rows clear τ** — the "fewer than `k` clearing it is the honest count" case
///   the code comment names. Here the take cannot mask anything, because there is nothing past the
///   qualifying set to take.
/// - **(ii) a grouped threshold query** — `take` becomes `scored.len()` and, more to the point,
///   `group` runs over `scored` *after* the prune, so the clustering sees exactly the qualifying set.
///
/// Both are asserted at 1, 2 and 4 shards, because the prune lives in the shared `finalize` that the
/// coordinator also calls — a cluster must not resolve the bar differently from a single engine.
///
/// The armed traps matter more than usual here: this test is worthless unless it is genuinely *in*
/// the under-`k` regime, so it asserts that it is, rather than assuming a fixture keeps it there.
#[test]
fn the_exact_threshold_prune_is_observable_in_both_regimes_at_every_shard_count() {
    let _seam = SEAM.lock().unwrap_or_else(|e| e.into_inner());
    prism_engine::search::inject_threshold_margin(None, None);

    let n_hot = 60usize;
    let tau = 0.5f32;
    // `k` deliberately far above the qualifying count, so the top-`k` truncation CANNOT hide a
    // sub-τ row. This is regime (i) by construction, and the armed trap below proves it held.
    let wide_k = 5_000usize;
    let base = || {
        let mut q = threshold_query(tau);
        q.k = wide_k;
        q.candidates = 400;
        q.rerank = 400;
        q
    };

    let single = Engine::init(&tmp("prune-single"), config()).unwrap();
    single.ingest(corpus(n_hot), TS).unwrap();
    // The prune-has-work precondition, measured where the counter is actually populated.
    let overfetch_seen = single.search(&base()).unwrap().counters.threshold_overfetch;

    let engines: Vec<Answerer> = {
        let mut v: Vec<Answerer> = Vec::new();
        let s = Engine::init(&tmp("prune-e0"), config()).unwrap();
        s.ingest(corpus(n_hot), TS).unwrap();
        v.push((0, Box::new(move |q: &Query| s.search(q).unwrap())));
        for n in [1usize, 2, 4] {
            let c = Cluster::init(&tmp(&format!("prune-c{n}")), n, config()).unwrap();
            c.ingest(corpus(n_hot), TS).unwrap();
            v.push((n, Box::new(move |q: &Query| c.search(q).unwrap())));
        }
        v
    };

    let mut failures: Vec<String> = Vec::new();
    for (n, run) in &engines {
        let where_ = if *n == 0 {
            "single engine".to_string()
        } else {
            format!("{n} shards")
        };

        // --- regime (i): fewer than k rows clear τ -------------------------------------------------
        let r = run(&base());
        assert!(!r.hits.is_empty(), "[{where_}] regime (i) returned nothing");
        // ARMED TRAP: we are genuinely under `k`. If this ever fails the regime stopped being
        // exercised and every assertion below would be masked by the top-k take, exactly as before.
        assert!(
            r.hits.len() < wide_k,
            "[{where_}] regime (i) is NOT armed: {} hits at k={wide_k} means the take could still \
             be masking the prune",
            r.hits.len()
        );
        // ARMED TRAP: there was something to prune. Established ONCE, on the single engine, over the
        // identical corpus — because `threshold_overfetch` is set in the per-shard candidate phase
        // (`search.rs`) and the coordinator builds its counters with `..Default::default()`, so a
        // CLUSTER query always reports 0 for it. That is a real observability gap in its own right
        // (§22 calls the overfetch "a monitored number") and is reported rather than papered over
        // here; the arming below does not depend on it, and the catches are asserted on every engine.
        assert!(
            overfetch_seen > 0,
            "the fixture is NOT armed: the single engine overfetched nothing, so the prune had no \
             work to do on this corpus and a green result would prove nothing"
        );
        // THE CATCH: every returned row clears the exact bar.
        let below: Vec<(String, f32)> = r
            .hits
            .iter()
            .filter(|h| h.score < tau)
            .map(|h| (h.event.event_id.clone(), h.score))
            .collect();
        if !below.is_empty() {
            failures.push(format!(
                "[{where_}] regime (i): {} returned row(s) do not clear τ={tau} — the \
                 exact-threshold prune did not run: {:?}",
                below.len(),
                &below[..below.len().min(5)]
            ));
        }

        // --- regime (ii): a grouped threshold query ------------------------------------------------
        let mut gq = base();
        gq.group_k = Some(4);
        let g = run(&gq);
        let clusters = g
            .clusters
            .as_ref()
            .unwrap_or_else(|| panic!("[{where_}] regime (ii) produced no clusters"));
        // ARMED TRAP: the grouped path really did cluster something.
        let clustered: usize = clusters.iter().map(|c| c.count).sum();
        assert!(
            clustered > 0,
            "[{where_}] regime (ii) is NOT armed: nothing was clustered"
        );
        // THE CATCH: the grouped answer obeys the same bar...
        let g_below = g.hits.iter().filter(|h| h.score < tau).count();
        if g_below != 0 {
            failures.push(format!(
                "[{where_}] regime (ii): {g_below} grouped row(s) do not clear τ={tau} — the prune \
                 did not run before grouping"
            ));
        }
        // ...and the clustering saw exactly the qualifying set, not the overfetched one. `group` runs
        // over `scored` AFTER the prune, so this count is the prune's footprint on the grouped path.
        if clustered != r.hits.len() {
            failures.push(format!(
                "[{where_}] regime (ii): grouping saw {clustered} rows but only {} clear τ — the \
                 clustering ran over the overfetched set",
                r.hits.len()
            ));
        }
    }

    // Collected, not aborted: every engine's verdict is visible in one run, so the prune's
    // sensitivity at 1, 2 and 4 shards can never again be "unknown because the first one failed".
    assert!(
        failures.is_empty(),
        "the exact-threshold prune is not holding — {} failure(s):\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
}

/// **The overfetch counter is true in a cluster, not just on a single engine** ([query §22](../../../../docs/QUERY-CONTRACT.md)).
///
/// §22 calls `threshold_overfetch` a **monitored number** — the observable that makes ε's adequacy
/// checkable rather than hoped for. It was true only single-node: the collector produces it inside
/// each shard, and the coordinator built its counters with `..Default::default()`, so **every cluster
/// query reported 0**. A number that silently depends on which path produced it is worse than no
/// number, because a dashboard cannot tell the difference — the overfetch would have read as
/// "margin is never exercised" for exactly the deployment shape the margin exists for.
///
/// The fold is a sum: each shard bounds its own candidates by `2(1−τ) + ε` (D-074), so the query's
/// overfetch is the total the exact τ prunes back. Sharding is a layout, so the total must equal the
/// single engine's over the identical corpus — at every shard count.
#[test]
fn the_threshold_overfetch_counter_is_aggregated_across_shards() {
    let _seam = SEAM.lock().unwrap_or_else(|e| e.into_inner());
    prism_engine::search::inject_threshold_margin(None, None);
    let n_hot = 60usize;
    let tau = 0.5f32;

    let single = Engine::init(&tmp("agg-single"), config()).unwrap();
    single.ingest(corpus(n_hot), TS).unwrap();
    let expected = single
        .search(&threshold_query(tau))
        .unwrap()
        .counters
        .threshold_overfetch;

    // ARMED: the counter must be nonzero, or "equal" would be satisfied by two zeros and this test
    // would certify the very bug it exists to catch.
    assert!(
        expected > 0,
        "not armed: the single engine overfetched nothing on this corpus, so 0 == 0 would pass \
         while the coordinator still reported nothing"
    );

    for n in [1usize, 2, 4] {
        let cluster = Cluster::init(&tmp(&format!("agg-{n}")), n, config()).unwrap();
        cluster.ingest(corpus(n_hot), TS).unwrap();
        let got = cluster
            .search(&threshold_query(tau))
            .unwrap()
            .counters
            .threshold_overfetch;
        assert_eq!(
            got, expected,
            "{n}-shard: threshold_overfetch is {got}, single engine reports {expected} — the \
             coordinator is not folding in each shard's contribution"
        );
    }
}
