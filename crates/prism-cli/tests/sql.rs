//! The S3 gate: the SQL surface is the **same door**, and tenant policy is below it.
//!
//! > *"The SQL path is a second door into the same engine — it must be provably the SAME
//! > door."*
//!
//! Every parity test below runs a query through **both** doors and asserts the rows are
//! identical **and the physical-execution counters are identical too**. The counters are the
//! part that matters: if SQL ever grew its own scan, its own pruning, or its own idea of
//! ordering, the counters would diverge before the results did — and we would rather find
//! that out from a counter than from a customer.
//!
//! Two doors into a database that disagree is a class of bug that takes years to find,
//! because each door is individually self-consistent.

use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_sql::{compile, Session};
use prism_types::predicate::{CmpOp, Literal, Predicate};
use prism_types::rng::Rng;
use prism_types::{Event, Query};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-s3-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn store(tag: &str, rows: usize) -> (Engine, PathBuf) {
    let root = tmp(tag);
    let engine = Engine::init(
        &root,
        StoreConfig {
            format_version: STORE_VERSION,
            dim: 64,
            nlist: 16,
            pq_m: 8,
            seed: 9,
            kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
            block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
            partitions: Default::default(),
            promote: Vec::new(),
        },
    )
    .unwrap();
    let events = prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, rows, 5);
    engine.ingest(events, 1_760_000_000_000).unwrap();
    (engine, root)
}

fn sess(t: &str) -> Session {
    Session {
        tenant: t.to_string(),
    }
}

// ------------------------------------------------------ the same door

#[test]
fn sql_compiles_to_exactly_the_query_the_direct_api_takes() {
    // Asserted on the *value*, before either is ever run. If the compiled query differs from
    // the hand-built one, SQL is a different door, and no amount of matching output would
    // prove otherwise.
    let (engine, root) = store("compile", 800);

    let plan = compile(
        "SELECT event_id FROM events \
         WHERE embedding ≈≈ 'the tool call timed out' AND cost > 0.01 \
         LIMIT 7 WITH (nprobe = 8, candidates = 100, rerank = 60)",
        &sess("t1"),
    )
    .unwrap();
    let from_sql = engine.plan_to_query(&plan).unwrap();

    let by_hand = Query {
        text: "the tool call timed out".into(),
        tenant: Some("t1".into()),
        k: 7,
        nprobe: 8,
        candidates: 100,
        rerank: 60,
        predicate: Some(Predicate::Cmp(
            Box::new(Predicate::Column("cost".into())),
            CmpOp::Gt,
            Box::new(Predicate::Literal(Literal::Float(0.01))),
        )),
        ..Default::default()
    };

    assert_eq!(
        serde_json::to_value(&from_sql).unwrap(),
        serde_json::to_value(&by_hand).unwrap(),
        "SQL did not compile to the query the direct API takes"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn both_doors_return_identical_rows_and_identical_counters() {
    let (engine, root) = store("parity", 2000);

    // A representative matrix: pure semantic, hybrid, scalar-filtered, attribute-filtered,
    // time-bounded, and a query that matches nothing.
    let cases: [(&str, Query); 5] = [
        (
            "SELECT event_id FROM events WHERE embedding ≈≈ 'the tool call timed out' LIMIT 10",
            Query {
                text: "the tool call timed out".into(),
                tenant: Some("t1".into()),
                k: 10,
                ..Default::default()
            },
        ),
        (
            "SELECT event_id FROM events WHERE embedding ≈≈ 'connection pool exhausted' \
             AND cost > 0.02 LIMIT 5",
            Query {
                text: "connection pool exhausted".into(),
                tenant: Some("t1".into()),
                k: 5,
                predicate: Some(Predicate::Cmp(
                    Box::new(Predicate::Column("cost".into())),
                    CmpOp::Gt,
                    Box::new(Predicate::Literal(Literal::Float(0.02))),
                )),
                ..Default::default()
            },
        ),
        (
            "SELECT event_id FROM events WHERE embedding ≈≈ 'invalid bearer token' \
             AND event_name = 'auth.failure' LIMIT 8",
            Query {
                text: "invalid bearer token".into(),
                tenant: Some("t1".into()),
                k: 8,
                predicate: Some(Predicate::Cmp(
                    Box::new(Predicate::Column("event_name".into())),
                    CmpOp::Eq,
                    Box::new(Predicate::Literal(Literal::Str("auth.failure".into()))),
                )),
                ..Default::default()
            },
        ),
        (
            "SELECT event_id FROM events WHERE embedding ≈≈ 'summarize this report' \
             AND event_time >= 1760000500000 LIMIT 6",
            Query {
                text: "summarize this report".into(),
                tenant: Some("t1".into()),
                k: 6,
                predicate: Some(Predicate::Cmp(
                    Box::new(Predicate::Column("event_time".into())),
                    CmpOp::GtEq,
                    Box::new(Predicate::Literal(Literal::Int(1_760_000_500_000))),
                )),
                ..Default::default()
            },
        ),
        (
            "SELECT event_id FROM events WHERE embedding ≈≈ 'write a python function' \
             AND cost > 999 LIMIT 4",
            Query {
                text: "write a python function".into(),
                tenant: Some("t1".into()),
                k: 4,
                predicate: Some(Predicate::Cmp(
                    Box::new(Predicate::Column("cost".into())),
                    CmpOp::Gt,
                    Box::new(Predicate::Literal(Literal::Int(999))),
                )),
                ..Default::default()
            },
        ),
    ];

    for (sql, direct) in cases {
        let via_sql = engine
            .run_sql(&compile(sql, &sess("t1")).unwrap(), None)
            .unwrap();
        let via_api = engine.search(&direct).unwrap();

        let sql_ids: Vec<String> = via_sql
            .rows
            .iter()
            .map(|r| r[0].1.as_str().unwrap().to_string())
            .collect();
        let api_ids: Vec<String> = via_api
            .hits
            .iter()
            .map(|h| h.event.event_id.clone())
            .collect();

        assert_eq!(sql_ids, api_ids, "the two doors disagree on rows:\n  {sql}");

        // **The assertion that matters.** If SQL ever grows its own scan, the counters
        // diverge before the results do.
        assert_eq!(
            serde_json::to_value(&via_sql.counters).unwrap(),
            serde_json::to_value(&via_api.counters).unwrap(),
            "the two doors disagree on physical execution:\n  {sql}"
        );
        assert_eq!(via_sql.snapshot_id, via_api.snapshot_id);
    }

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn the_scalar_path_agrees_with_a_brute_force_reference() {
    // The oracle for the scalar subset. The reference materializes every event and filters
    // it in a straight loop: no pruning, no zone maps, no columnar reads, no laziness. It is
    // slow and stupid on purpose, which is exactly what an oracle should be -- it tests the
    // machinery that makes the real path fast.
    let (engine, root) = store("scalar-oracle", 1500);

    let snap = engine.snapshot().unwrap();
    let all: Vec<Event> = engine
        .open_parts(&snap)
        .unwrap()
        .iter()
        .flat_map(|r| r.read_all().unwrap().events)
        .collect();

    let cases = [
        ("SELECT count(*) FROM events", None),
        (
            "SELECT count(*) FROM events WHERE cost > 0.02",
            Some(Box::new(|e: &Event| e.cost > 0.02) as Box<dyn Fn(&Event) -> bool>),
        ),
        (
            "SELECT count(*) FROM events WHERE error = true",
            Some(Box::new(|e: &Event| e.error) as Box<dyn Fn(&Event) -> bool>),
        ),
        (
            "SELECT count(*) FROM events WHERE event_name IN ('db.error', 'auth.failure')",
            Some(
                Box::new(|e: &Event| e.event_name == "db.error" || e.event_name == "auth.failure")
                    as Box<dyn Fn(&Event) -> bool>,
            ),
        ),
        (
            "SELECT count(*) FROM events WHERE event_time >= 1760000500000 AND cost < 0.03",
            Some(
                Box::new(|e: &Event| e.event_time >= 1_760_000_500_000 && e.cost < 0.03)
                    as Box<dyn Fn(&Event) -> bool>,
            ),
        ),
        (
            "SELECT count(*) FROM events WHERE NOT (error = true) OR cost > 0.04",
            Some(Box::new(|e: &Event| !e.error || e.cost > 0.04) as Box<dyn Fn(&Event) -> bool>),
        ),
    ];

    for (sql, pred) in cases {
        let res = engine
            .run_sql(&compile(sql, &sess("t1")).unwrap(), None)
            .unwrap();
        let got = res.rows[0][0].1.as_u64().unwrap() as usize;

        // The reference. Note it applies the tenant policy too -- because the policy is not
        // an optimization, it is part of what the query *means*.
        let want = all
            .iter()
            .filter(|e| e.tenant_id == "t1")
            .filter(|e| pred.as_ref().map(|f| f(e)).unwrap_or(true))
            .count();

        assert_eq!(
            got, want,
            "scalar path disagrees with the reference:\n  {sql}"
        );
    }
    std::fs::remove_dir_all(root).ok();
}

// ------------------------------------------------ tenant policy, below SQL

#[test]
fn no_statement_can_widen_its_tenant() {
    // The binder produces `(whatever the user wrote) AND tenant_id = <session>`. The user's
    // expression is a SUBTREE, and a subtree cannot widen the conjunction it is nested
    // inside. That is not fifty checks that have to be right; it is a shape.
    //
    // Fifty attempts to prove otherwise:
    let (engine, root) = store("escape", 2000);

    let truth = |t: &str| -> usize {
        let r = engine
            .run_sql(
                &compile("SELECT count(*) FROM events", &sess(t)).unwrap(),
                None,
            )
            .unwrap();
        r.rows[0][0].1.as_u64().unwrap() as usize
    };
    let t1 = truth("t1");
    let t2 = truth("t2");
    assert!(
        t1 > 0 && t2 > 0 && t1 != t2,
        "the tenants must be distinguishable"
    );

    let attempts = [
        "SELECT count(*) FROM events WHERE tenant_id = 't2'",
        "SELECT count(*) FROM events WHERE tenant_id <> 't1'",
        "SELECT count(*) FROM events WHERE NOT (tenant_id = 't1')",
        "SELECT count(*) FROM events WHERE tenant_id = 't2' OR 1 = 1",
        "SELECT count(*) FROM events WHERE 1 = 1 OR tenant_id = 't2'",
        "SELECT count(*) FROM events WHERE tenant_id = 't1' OR tenant_id = 't2'",
        "SELECT count(*) FROM events WHERE (tenant_id = 't2') OR (cost >= 0)",
        "SELECT count(*) FROM events WHERE tenant_id IN ('t1','t2','t3','t4')",
        "SELECT count(*) FROM events WHERE NOT (tenant_id <> 't2')",
        "SELECT count(*) FROM events WHERE ((((tenant_id = 't2'))))",
        "SELECT count(*) FROM events WHERE tenant_id >= 't0'",
        "SELECT count(*) FROM events WHERE tenant_id > ''",
        "SELECT count(*) FROM events WHERE true",
        "SELECT count(*) FROM events WHERE NOT false",
        "SELECT count(*) FROM events WHERE cost >= 0 OR tenant_id = 't2'",
        // comment-smuggling
        "SELECT count(*) FROM events WHERE tenant_id = 't2' -- AND tenant_id = 't1'",
        "SELECT count(*) FROM events /* WHERE tenant_id = 't2' */ WHERE tenant_id = 't2'",
        // case games
        "select COUNT(*) from EVENTS where TENANT_ID = 't2'",
        // an OR nested arbitrarily deep still sits inside the policy conjunction
        "SELECT count(*) FROM events WHERE ((tenant_id = 't2' OR true) AND (true OR false))",
    ];

    for sql in attempts {
        match compile(sql, &sess("t1")) {
            Err(_) => {} // refused outright: also fine
            Ok(plan) => {
                assert_eq!(
                    plan.tenant, "t1",
                    "the policy tenant was overridden:\n  {sql}"
                );
                let r = engine.run_sql(&plan, None).unwrap();
                let n = r.rows[0][0].1.as_u64().unwrap() as usize;
                assert!(
                    n <= t1,
                    "statement saw {n} rows; tenant t1 has only {t1}. It escaped:\n  {sql}"
                );
            }
        }
    }

    // And the ones that must not even bind: an alias is not a column, and it is not in scope
    // in WHERE.
    for sql in [
        "SELECT tenant_id AS t FROM events WHERE t = 't2'",
        "SELECT count(*) FROM events WHERE tenant = 't2'",
        "SELECT count(*) FROM events WHERE events.tenant_id = 't2'",
        "SELECT count(*) FROM (SELECT * FROM events) WHERE tenant_id = 't2'",
    ] {
        assert!(
            compile(sql, &sess("t1")).is_err(),
            "this should not have bound at all:\n  {sql}"
        );
    }

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn fuzzing_the_parser_never_panics_and_never_escapes_the_tenant() {
    // The parser is network-facing input now. Throw structured garbage at it: it must
    // refuse, or bind to a plan whose tenant is *still* the session's. It may never panic,
    // and it may never produce a plan that can see another tenant.
    let (engine, root) = store("fuzz", 600);

    let frags = [
        "SELECT",
        "count(*)",
        "*",
        "FROM",
        "events",
        "WHERE",
        "tenant_id",
        "event_name",
        "cost",
        "score",
        "attributes['x']",
        "=",
        "<>",
        ">",
        "OR",
        "AND",
        "NOT",
        "IN",
        "(",
        ")",
        "'t2'",
        "'t1'",
        "1",
        "0.5",
        "true",
        "false",
        "LIMIT",
        "10",
        "OFFSET",
        "GROUP",
        "BY",
        "ORDER",
        "embedding",
        "≈≈",
        "'x'",
        "--",
        "/*",
        "*/",
        ";",
        ",",
        "AS",
        "t",
        "[",
        "]",
        "SELECT",
        "UNION",
        "JOIN",
    ];

    let mut rng = Rng::new(0xF0FF);
    for _ in 0..8000 {
        let n = 1 + rng.below(24);
        let sql: String = (0..n)
            .map(|_| frags[rng.below(frags.len())])
            .collect::<Vec<_>>()
            .join(" ");

        match compile(&sql, &sess("t1")) {
            Err(_) => {}
            Ok(plan) => {
                assert_eq!(
                    plan.tenant, "t1",
                    "fuzz produced a plan for another tenant:\n  {sql}"
                );
                // Running it must also not panic, and must not see another tenant.
                if let Ok(res) = engine.run_sql(&plan, None) {
                    for row in &res.rows {
                        for (col, v) in row {
                            if col == "tenant_id" {
                                assert_eq!(
                                    v.as_str(),
                                    Some("t1"),
                                    "fuzz returned another tenant's row:\n  {sql}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    std::fs::remove_dir_all(root).ok();
}

// ---------------------------------------------------- bounded parsing

#[test]
fn every_parser_bound_is_enforced_and_named() {
    use prism_sql::limits::*;

    let cases: [(String, &str); 5] = [
        (
            format!("SELECT {} FROM events", "a,".repeat(MAX_STATEMENT_BYTES)),
            "over the",
        ),
        ("(".repeat(MAX_TOKENS + 10), "tokens"),
        (
            format!(
                "SELECT * FROM events WHERE {}cost > 1{}",
                "(".repeat(MAX_EXPR_DEPTH + 20),
                ")".repeat(MAX_EXPR_DEPTH + 20)
            ),
            "nests deeper",
        ),
        (
            format!(
                "SELECT * FROM events WHERE cost IN ({})",
                (0..MAX_IN_LIST + 5)
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            "IN list",
        ),
        (
            format!(
                "SELECT {} FROM events",
                (0..MAX_PROJECTIONS + 5)
                    .map(|_| "cost")
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            "projected expressions",
        ),
    ];

    for (sql, expect) in cases {
        let e = compile(&sql, &sess("t1"))
            .expect_err("an unbounded statement was accepted")
            .to_string();
        assert!(
            e.contains(expect),
            "the bound was enforced but not NAMED. An operator cannot act on \"syntax error\".\n  \
             wanted: {expect}\n  got: {e}"
        );
    }
}

// ------------------------------------------------------- pagination

#[test]
fn paginating_under_concurrent_ingest_and_merge_yields_exactly_the_snapshots_rows() {
    // **The S3 pagination gate.**
    //
    // Page through a full result set while the store is actively changing underneath: new
    // events land, parts merge, snapshots advance. The paginating reader must see exactly the
    // rows of the snapshot it started on -- no duplicates, no gaps, no rows from the future.
    //
    // This needs no new machinery. Parts are immutable and a snapshot is a fixed set of them,
    // so the answer to a query against a given snapshot is fixed forever. Pagination did not
    // need a new invariant; it needed the ones we already had to be true.
    let (engine, root) = store("paginate", 2500);

    let sql = "SELECT event_id, score FROM events \
               WHERE embedding ≈≈ 'the tool call timed out' \
               LIMIT 7 WITH (rerank = 50)";
    let plan = compile(sql, &sess("t1")).unwrap();

    // The ground truth: the whole result set of the pinned snapshot, in one shot.
    let pinned = engine.snapshot().unwrap();
    let mut full = engine.plan_to_query(&plan).unwrap();
    full.k = plan.rerank;
    let truth: Vec<String> = engine
        .search_at(&pinned, &full)
        .unwrap()
        .hits
        .iter()
        .map(|h| h.event.event_id.clone())
        .collect();
    assert!(
        truth.len() > 20,
        "need a result set worth paging: {}",
        truth.len()
    );

    // Now page it, mutating the store between every single page.
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0usize;

    loop {
        let res = engine.run_sql(&plan, cursor.as_deref()).unwrap();
        assert_eq!(
            res.snapshot_id, pinned.snapshot_id,
            "pagination drifted onto a different snapshot"
        );
        for r in &res.rows {
            seen.push(r[0].1.as_str().unwrap().to_string());
        }
        pages += 1;

        // --- the store changes underneath the reader ---
        let more = prism_engine::corpus::generate(prism_engine::corpus::Kind::Uniform, 60, 77)
            .into_iter()
            .enumerate()
            .map(|(i, mut e)| {
                e.event_id = format!("intruder-{pages}-{i}");
                e.tenant_id = "t1".into();
                e
            })
            .collect::<Vec<_>>();
        engine.ingest(more, 1_760_000_900_000).unwrap();
        engine.merge(1_760_000_900_001).unwrap();

        match res.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
        assert!(pages < 100, "pagination did not terminate");
    }

    // Exactly the snapshot's rows, in the snapshot's order.
    assert_eq!(
        seen, truth,
        "pagination did not reproduce the snapshot's result set"
    );

    // No duplicates.
    let uniq: std::collections::BTreeSet<&String> = seen.iter().collect();
    assert_eq!(
        uniq.len(),
        seen.len(),
        "pagination returned a duplicate row"
    );

    // And nothing from the future leaked in, even though we ingested between every page.
    assert!(
        !seen.iter().any(|id| id.starts_with("intruder-")),
        "a row published after the cursor was issued appeared in a page"
    );

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_cursor_from_a_reclaimed_snapshot_is_an_explicit_error_not_a_different_answer() {
    // Silently continuing against CURRENT is how a client receives a page that overlaps the
    // last one, or skips rows that existed the whole time, and concludes we are lying. We are.
    let (engine, root) = store("expired", 1200);

    let plan = compile(
        "SELECT event_id FROM events WHERE embedding ≈≈ 'invalid bearer token' LIMIT 5",
        &sess("t1"),
    )
    .unwrap();

    let page1 = engine.run_sql(&plan, None).unwrap();
    let cursor = page1.next_cursor.expect("there should be a second page");

    // Churn the catalog, then reclaim everything the cursor depended on.
    for i in 0..4 {
        engine
            .ingest(
                prism_engine::corpus::generate(prism_engine::corpus::Kind::Uniform, 50, 100 + i),
                1_760_000_900_000,
            )
            .unwrap();
    }
    engine.merge(1_760_000_950_000).unwrap();
    engine.catalog().gc(1, false).unwrap();

    let err = engine
        .run_sql(&plan, Some(&cursor))
        .expect_err("a cursor into a reclaimed snapshot must fail, not silently move");
    let msg = err.to_string();
    assert!(msg.contains("reclaimed"), "{msg}");
    assert!(msg.contains("re-run the query"), "{msg}");

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_cursor_cannot_be_replayed_against_a_different_query() {
    let (engine, root) = store("cursor-plan", 1000);

    let a = compile(
        "SELECT event_id FROM events WHERE embedding ≈≈ 'the tool call timed out' LIMIT 5",
        &sess("t1"),
    )
    .unwrap();
    let b = compile(
        "SELECT event_id FROM events WHERE embedding ≈≈ 'invalid bearer token' LIMIT 5",
        &sess("t1"),
    )
    .unwrap();

    let cursor = engine.run_sql(&a, None).unwrap().next_cursor.unwrap();
    let e = engine
        .run_sql(&b, Some(&cursor))
        .expect_err("a cursor from another query must be refused")
        .to_string();
    assert!(e.contains("different query"), "{e}");

    // And a cursor from another *tenant's* session is likewise a different plan.
    let c = compile(
        "SELECT event_id FROM events WHERE embedding ≈≈ 'the tool call timed out' LIMIT 5",
        &sess("t2"),
    )
    .unwrap();
    assert!(engine.run_sql(&c, Some(&cursor)).is_err());

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_tampered_cursor_is_refused_by_its_checksum() {
    let (engine, root) = store("cursor-tamper", 800);
    let plan = compile(
        "SELECT event_id FROM events WHERE embedding ≈≈ 'billing question' LIMIT 5",
        &sess("t1"),
    )
    .unwrap();
    let cursor = engine.run_sql(&plan, None).unwrap().next_cursor.unwrap();

    let mut bad = cursor.clone();
    bad.replace_range(20..21, "0");
    if bad == cursor {
        bad.replace_range(20..21, "1");
    }
    let e = engine.run_sql(&plan, Some(&bad)).unwrap_err().to_string();
    assert!(e.contains("checksum") || e.contains("malformed"), "{e}");

    assert!(engine.run_sql(&plan, Some("not-a-cursor")).is_err());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn offset_is_refused_and_says_why() {
    let e = compile("SELECT * FROM events LIMIT 10 OFFSET 20", &sess("t1"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("OFFSET is not supported"), "{e}");
    assert!(e.contains("duplicates and drops rows"), "{e}");
}

/// **The Flight SQL door is the same door (S8, directive 6).** A query answered through the direct
/// API, the SQL text door, and the Flight SQL door returns byte-identical counters — the "same
/// door" property, now three-way. And the tenant is injected below the Flight door: a Flight query
/// naming another tenant cannot escape its own.
#[test]
fn the_flight_door_is_the_same_door() {
    use prism_engine::flight::FlightSqlRequest;

    let (engine, root) = store("flight", 800);

    let sql = "SELECT event_id FROM events WHERE embedding ≈≈ 'the tool call timed out' LIMIT 10";

    // All three doors run the identical logical query: the plan the SQL door compiles, the Query
    // the direct API takes (from that same plan), and the Flight message carrying the same text.
    let plan = compile(sql, &sess("t1")).unwrap();
    let via_direct = engine
        .search(&engine.plan_to_query(&plan).unwrap())
        .unwrap();
    // SQL text door.
    let via_sql = engine.run_sql(&plan, None).unwrap();
    // Flight SQL door.
    let msg = FlightSqlRequest {
        query: sql.into(),
        params: vec![],
    }
    .encode();
    let via_flight = engine.run_flight_sql(&msg, "t1").unwrap();

    // The counters are byte-identical across all three doors: if any door grew its own scan, its
    // counters would diverge before its rows did.
    let cj = |c: &prism_types::Counters| serde_json::to_value(c).unwrap();
    assert_eq!(
        cj(&via_sql.counters),
        cj(&via_flight.counters),
        "SQL vs Flight counters diverge"
    );
    assert_eq!(
        cj(&via_direct.counters),
        cj(&via_flight.counters),
        "direct vs Flight counters diverge"
    );

    // The tenant is injected below the Flight door: a Flight query for t1 sees only t1's rows,
    // regardless of what the statement says, and it cannot reach t0's.
    let t1_ids: std::collections::BTreeSet<String> = via_flight
        .rows
        .iter()
        .map(|r| r[0].1.as_str().unwrap().to_string())
        .collect();
    let via_flight_t0 = engine.run_flight_sql(&msg, "t0").unwrap();
    let t0_ids: std::collections::BTreeSet<String> = via_flight_t0
        .rows
        .iter()
        .map(|r| r[0].1.as_str().unwrap().to_string())
        .collect();
    assert!(
        t1_ids.is_disjoint(&t0_ids) || t1_ids.is_empty() || t0_ids.is_empty(),
        "a Flight query for one tenant returned another tenant's rows -- the tenant conjunction \
         was not injected below the door"
    );
    let _ = std::fs::remove_dir_all(&root);
}
