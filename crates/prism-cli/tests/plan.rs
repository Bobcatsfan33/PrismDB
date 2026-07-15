//! **The S8 gate: the physical plan is invisible to the answer (docs/QUERY-CONTRACT.md §9).**
//!
//! Scalar-first, semantic-first, and interleaved are three physical strategies for **one logical
//! query** — [D-033](../../../docs/DECISIONS.md) in its plan edition, the sibling of the route's
//! selection-identity. Cost may differ; answers may not. This gate forces every strategy on the
//! golden, layout-variant, and boundary-tie corpora and asserts **byte-identical** event ids and
//! order — and, because the plan changes no score, it proves a cursor need not pin the plan by
//! paginating while the plan flips between pages.

use prism_engine::corpus::{self, Kind};
use prism_engine::plan::{self, Strategy};
use prism_engine::{oracle, tsv, Engine};
use prism_part::partition::PartitionScheme;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_sql::{compile, Session};
use prism_types::{Event, Query};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static PLAN_LOCK: Mutex<()> = Mutex::new(());
static N: AtomicU64 = AtomicU64::new(0);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-plan-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn config(window_ms: i64) -> StoreConfig {
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
            time_window_ms: window_ms,
            dedicated: Default::default(),
        },
        promote: Vec::new(),
    }
}

fn frozen_corpus() -> Vec<Event> {
    let text = std::fs::read_to_string(repo_root().join("testing/golden/v1/corpus.tsv")).unwrap();
    tsv::parse(&text).unwrap()
}

fn golden() -> oracle::Golden {
    let bytes = std::fs::read(repo_root().join("testing/golden/v1/expected.json")).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn answer(engine: &Engine, q: &Query, strategy: Strategy) -> (Vec<(String, f32)>, String) {
    plan::set_forced_plan(Some(strategy));
    let r = engine.search(q).unwrap();
    plan::set_forced_plan(None);
    (
        r.hits
            .iter()
            .map(|h| (h.event.event_id.clone(), h.score))
            .collect(),
        r.counters.plan,
    )
}

/// **The gate.** Every strategy answers every golden query identically, over two layouts.
#[test]
fn every_strategy_returns_the_same_answer() {
    let _guard = PLAN_LOCK.lock().unwrap();
    let events = frozen_corpus();
    let g = golden();

    for window in [i64::MAX / 4, 24 * 60 * 60 * 1000] {
        let root = tmp("gate");
        let engine = Engine::init(&root, config(window)).unwrap();
        for (i, chunk) in events.chunks(200).enumerate() {
            engine
                .ingest(chunk.to_vec(), 1_760_000_000_000 + i as i64)
                .unwrap();
        }

        for exp in &g.expectations {
            let q = exp.query.to_query();
            let (reference, ref_plan) = answer(&engine, &q, Strategy::Interleaved);
            assert_eq!(ref_plan, "interleaved");
            for &s in &[Strategy::ScalarFirst, Strategy::SemanticFirst] {
                let (got, used) = answer(&engine, &q, s);
                assert_eq!(used, s.name(), "the forced plan did not take");
                assert_eq!(
                    reference, got,
                    "strategy {} answered differently than interleaved on query `{}`. The plan may \
                     cost differently; it may not answer differently (query contract §9).",
                    s.name(),
                    exp.query.text
                );
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// **Plan-invariance holds under a real predicate, where the strategies genuinely diverge in
/// work** — scalar-first distances only the survivors, semantic-first predicates only the
/// admittable rows. The answer is identical; the counters are not, and that is the whole point.
#[test]
fn strategies_diverge_in_work_but_not_in_answer() {
    let _guard = PLAN_LOCK.lock().unwrap();
    let mut evs = corpus::generate(Kind::Zipf, 3_000, 7);
    for (i, e) in evs.iter_mut().enumerate() {
        e.tenant_id = "alpha".into();
        e.event_id = format!("e{i:06}");
        e.cost = if i % 10 == 0 { 0.5 } else { 0.001 }; // a selective predicate: cost > 0.1
    }
    let root = tmp("predicate");
    let engine = Engine::init(&root, config(24 * 60 * 60 * 1000)).unwrap();
    for chunk in evs.chunks(250) {
        engine.ingest(chunk.to_vec(), 1_760_000_000_000).unwrap();
    }

    let q = Query {
        text: "the tool call timed out".into(),
        k: 10,
        tenant: Some("alpha".into()),
        predicate: Some(prism_types::predicate::Predicate::Cmp(
            Box::new(prism_types::predicate::Predicate::Column("cost".into())),
            prism_types::predicate::CmpOp::Gt,
            Box::new(prism_types::predicate::Predicate::Literal(
                prism_types::predicate::Literal::Float(0.1),
            )),
        )),
        ..Default::default()
    };

    plan::set_forced_plan(Some(Strategy::Interleaved));
    let inter = engine.search(&q).unwrap();
    plan::set_forced_plan(Some(Strategy::ScalarFirst));
    let scalar = engine.search(&q).unwrap();
    plan::set_forced_plan(Some(Strategy::SemanticFirst));
    let semantic = engine.search(&q).unwrap();
    plan::set_forced_plan(None);

    let ids = |r: &prism_types::SearchResult| -> Vec<String> {
        r.hits.iter().map(|h| h.event.event_id.clone()).collect()
    };
    assert_eq!(ids(&inter), ids(&scalar), "scalar-first changed the answer");
    assert_eq!(
        ids(&inter),
        ids(&semantic),
        "semantic-first changed the answer"
    );

    // The work genuinely diverges: scalar-first computes far fewer distances (only survivors of a
    // 1-in-10 predicate), semantic-first far fewer predicate evals. If they did NOT diverge, the
    // strategies would be a distinction without a difference.
    assert!(
        scalar.counters.distances_computed < inter.counters.distances_computed,
        "scalar-first ({}) did not compute fewer distances than interleaved ({})",
        scalar.counters.distances_computed,
        inter.counters.distances_computed
    );
    assert!(
        semantic.counters.predicate_evals < inter.counters.predicate_evals,
        "semantic-first ({}) did not evaluate the predicate fewer times than interleaved ({})",
        semantic.counters.predicate_evals,
        inter.counters.predicate_evals
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// **A cursor survives a plan flip between pages — so it need not pin the plan.**
///
/// Unlike the route (whose scores differ, so its cursor pins it — D-052), the plan changes no
/// score, so a page-2 keyset boundary is identical whichever strategy computed it. Paginate while
/// flipping the plan on every page and assert the pages tile the single-plan answer exactly.
#[test]
fn a_cursor_survives_a_plan_flip_between_pages() {
    let _guard = PLAN_LOCK.lock().unwrap();
    let events = frozen_corpus();
    let root = tmp("planflip");
    let engine = Engine::init(&root, config(24 * 60 * 60 * 1000)).unwrap();
    for (i, chunk) in events.chunks(200).enumerate() {
        engine
            .ingest(chunk.to_vec(), 1_760_000_000_000 + i as i64)
            .unwrap();
    }

    let sess = Session {
        tenant: "t0".into(),
    };
    let plan_sql =
        "SELECT event_id FROM events WHERE embedding ≈≈ 'the tool call timed out' LIMIT 5";
    let compiled = compile(plan_sql, &sess).unwrap();

    let page_ids = |res: &prism_engine::sql::SqlResult| -> Vec<String> {
        res.rows
            .iter()
            .map(|r| r[0].1.as_str().unwrap().to_string())
            .collect()
    };

    // Reference: one plan, all pages.
    plan::set_forced_plan(Some(Strategy::Interleaved));
    let mut whole = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let res = engine.run_sql(&compiled, cursor.as_deref()).unwrap();
        whole.extend(page_ids(&res));
        match res.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    plan::set_forced_plan(None);

    // Flip the plan on every page. It must tile identically -- the cursor does NOT pin the plan.
    let strategies = [
        Strategy::ScalarFirst,
        Strategy::SemanticFirst,
        Strategy::Interleaved,
    ];
    let mut tiled = Vec::new();
    let mut cursor: Option<String> = None;
    let mut page = 0usize;
    loop {
        plan::set_forced_plan(Some(strategies[page % 3]));
        let res = engine.run_sql(&compiled, cursor.as_deref()).unwrap();
        plan::set_forced_plan(None);
        tiled.extend(page_ids(&res));
        page += 1;
        match res.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }

    assert!(page >= 2, "the result did not span multiple pages");
    let unique: std::collections::BTreeSet<&String> = tiled.iter().collect();
    assert_eq!(
        unique.len(),
        tiled.len(),
        "a plan flip produced a duplicate row across pages"
    );
    assert_eq!(
        whole, tiled,
        "paginating while flipping the plan between pages did not reproduce the single-plan \
         answer. Because the plan changes no score, the cursor need not pin it -- but only if \
         plan-invariance holds, which this proves."
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// **Worst-cell regret, not average (directive 3, docs/QUERY-CONTRACT.md §9).**
///
/// Across a selectivity matrix, the plan the optimizer chooses must be within the declared regret
/// bound of the *best* fixed plan in **every** cell — not on average. An optimizer that wins on
/// average by losing badly in one cell is worse than a fixed heuristic for the customer in that
/// cell. Cost is a deterministic proxy from the actual counters, so this gate has no wall-clock
/// noise: it measures whether the optimizer's crude selectivity estimate is *good enough to pick a
/// plan within the bound*, which is exactly what matters.
#[test]
fn the_optimizer_is_within_the_regret_bound_in_every_cell() {
    let _guard = PLAN_LOCK.lock().unwrap();

    let bound = plan::PLAN_REGRET_BOUND_PCT as f64 / 100.0;
    // A matrix of predicate selectivities: the fraction of rows with cost above the threshold.
    let selectivities = [0.01f64, 0.05, 0.1, 0.25, 0.5, 0.9, 1.0];

    let mut worst_regret = 0.0f64;
    let mut worst_cell = String::new();

    for &target_sel in &selectivities {
        let mut evs = corpus::generate(Kind::Zipf, 4_000, 11);
        let cutoff = (target_sel * 1000.0) as u64;
        for (i, e) in evs.iter_mut().enumerate() {
            e.tenant_id = "alpha".into();
            e.event_id = format!("e{i:06}");
            // Spread the "hot" (cost > 0.1) rows UNIFORMLY across partitions via a hash, so
            // selectivity is representative rather than clustered in one part. Adversarial
            // per-partition skew is a real estimation hazard, but chasing it is cardinality-
            // estimation research (directive 7); S16's benchmarks will find where it hurts.
            let h = (i as u64).wrapping_mul(2_654_435_761) % 1000;
            e.cost = if h < cutoff { 0.5 } else { 0.001 };
        }
        let root = tmp("regret");
        let engine = Engine::init(&root, config(24 * 60 * 60 * 1000)).unwrap();
        for chunk in evs.chunks(400) {
            engine.ingest(chunk.to_vec(), 1_760_000_000_000).unwrap();
        }

        let q = Query {
            text: "the tool call timed out".into(),
            k: 10,
            tenant: Some("alpha".into()),
            predicate: Some(prism_types::predicate::Predicate::Cmp(
                Box::new(prism_types::predicate::Predicate::Column("cost".into())),
                prism_types::predicate::CmpOp::Gt,
                Box::new(prism_types::predicate::Predicate::Literal(
                    prism_types::predicate::Literal::Float(0.1),
                )),
            )),
            ..Default::default()
        };

        let cost_of = |r: &prism_types::SearchResult| -> u64 {
            plan::actual_cost(r.counters.distances_computed, r.counters.predicate_evals)
        };

        // The optimizer's choice (unforced), and its actual cost.
        plan::set_forced_plan(None);
        let chosen = engine.search(&q).unwrap();
        let chosen_cost = cost_of(&chosen);

        // The best fixed plan's actual cost.
        let mut best = u64::MAX;
        for &s in &Strategy::ALL {
            plan::set_forced_plan(Some(s));
            best = best.min(cost_of(&engine.search(&q).unwrap()));
        }
        plan::set_forced_plan(None);

        let regret = (chosen_cost as f64 - best as f64) / best.max(1) as f64;
        if regret > worst_regret {
            worst_regret = regret;
            worst_cell = format!(
                "sel~{target_sel}: chose `{}` cost {chosen_cost}, best {best}",
                chosen.counters.plan
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    assert!(
        worst_regret <= bound,
        "worst-cell regret {:.1}% exceeds the {:.0}% bound, in cell [{worst_cell}]. The optimizer \
         is not choosing among the three strategies well enough (directive 3).",
        worst_regret * 100.0,
        bound * 100.0
    );
}

/// **The calibration harness (directive 5, §14): estimate-vs-actual error is a visible number.**
///
/// The optimizer's decisions are only as good as its selectivity estimate. This tracks the gap
/// between the estimated and actual selectivity across the matrix, so cost-model drift is a
/// number CI watches, not a slow surprise. The bound is generous — the estimator is crude by
/// design (directive 7) — but it must not be *unboundedly* wrong, or the regret bound is luck.
#[test]
fn the_selectivity_estimate_is_calibrated_across_the_matrix() {
    let _guard = PLAN_LOCK.lock().unwrap();
    let selectivities = [0.05f64, 0.1, 0.25, 0.5, 0.9];
    let mut worst_abs_err = 0.0f64;

    for &target in &selectivities {
        let mut evs = corpus::generate(Kind::Zipf, 4_000, 13);
        let cutoff = (target * 1000.0) as u64;
        for (i, e) in evs.iter_mut().enumerate() {
            e.tenant_id = "alpha".into();
            e.event_id = format!("e{i:06}");
            let h = (i as u64).wrapping_mul(2_654_435_761) % 1000;
            e.cost = if h < cutoff { 0.5 } else { 0.001 };
        }
        let root = tmp("calib");
        let engine = Engine::init(&root, config(24 * 60 * 60 * 1000)).unwrap();
        for chunk in evs.chunks(400) {
            engine.ingest(chunk.to_vec(), 1_760_000_000_000).unwrap();
        }
        let q = Query {
            text: "the tool call timed out".into(),
            k: 10,
            tenant: Some("alpha".into()),
            explain: true,
            predicate: Some(prism_types::predicate::Predicate::Cmp(
                Box::new(prism_types::predicate::Predicate::Column("cost".into())),
                prism_types::predicate::CmpOp::Gt,
                Box::new(prism_types::predicate::Predicate::Literal(
                    prism_types::predicate::Literal::Float(0.1),
                )),
            )),
            ..Default::default()
        };
        // The optimizer's ESTIMATE (unforced run).
        plan::set_forced_plan(None);
        let est = engine
            .search(&q)
            .unwrap()
            .explain
            .expect("EXPLAIN requested")
            .estimated_selectivity;
        // The TRUE selectivity: force interleaved, which evaluates the predicate over EVERY scanned
        // row, so its actual_selectivity is the real pass rate (not a semantic-first near-subset).
        plan::set_forced_plan(Some(Strategy::Interleaved));
        let actual = engine
            .search(&q)
            .unwrap()
            .explain
            .expect("EXPLAIN requested")
            .actual_selectivity;
        plan::set_forced_plan(None);
        let err = (est - actual).abs();
        worst_abs_err = worst_abs_err.max(err);
        let _ = std::fs::remove_dir_all(&root);
    }

    // A crude sampled estimator should land within a wide but finite band; if it drifts past this,
    // the cost model is steering on a number that no longer tracks reality (§14).
    assert!(
        worst_abs_err <= 0.20,
        "the selectivity estimate drifted {:.2} from actual at worst -- the cost model is steering \
         on a stale number (docs/QUERY-CONTRACT.md §14)",
        worst_abs_err
    );
}
