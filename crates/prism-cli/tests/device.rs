//! **The S7 gate (device edition): the route is invisible to the answer.**
//!
//! S7 ships "GPU-ready, GPU-off" — no CUDA hardware, no GPU CI runner, so the GPU gate is *not*
//! claimed. What is claimed and tested here is the device-agnostic contract every route must obey
//! ([docs/DETERMINISM-CONTRACT.md](../../../docs/DETERMINISM-CONTRACT.md) §8–§11), proved against
//! the **CPU reference of the GPU route** — the definition the real CUDA kernel will one day have
//! to match:
//!
//! - **Run-to-run determinism** — same query, same answer, every time (unit-tested in `gpu::rerank`).
//! - **Selection-identity** — the CPU and GPU-reference routes return byte-identical event ids in
//!   byte-identical order; scores agree within the documented tolerance.
//! - **Route-flip pagination** — a cursor survives the route changing between pages.
//! - **Fault containment** — a device fault at any phase degrades to CPU, never a failed query.

use prism_engine::corpus::{self, Kind};
use prism_engine::gpu::{self, Phase, Route, RERANK_ROUTE_TOLERANCE};
use prism_engine::{tsv, Engine};
use prism_part::partition::PartitionScheme;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_sql::{compile, Session};
use prism_types::{Event, Query};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

// The forced-route and fault globals are process-wide, like S6's ISA ceiling, so tests that set
// them must not run concurrently. One lock serializes them.
static ROUTE_LOCK: Mutex<()> = Mutex::new(());
static N: AtomicU64 = AtomicU64::new(0);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-dev-{}-{}-{}", tag, std::process::id(), n));
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

fn golden() -> prism_engine::oracle::Golden {
    let bytes = std::fs::read(repo_root().join("testing/golden/v1/expected.json")).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Answer a query on a forced route.
fn answer(engine: &Engine, q: &Query, route: Route) -> (Vec<(String, f32)>, String, bool) {
    gpu::set_forced_route(Some(route));
    let r = engine.search(q).unwrap();
    gpu::set_forced_route(None);
    (
        r.hits
            .iter()
            .map(|h| (h.event.event_id.clone(), h.score))
            .collect(),
        r.counters.rerank_route,
        r.counters.route_degraded,
    )
}

/// **Selection-identity.** Over the golden queries and a layout of the frozen corpus, the CPU and
/// GPU-reference routes return the same event ids in the same order; scores agree within the
/// documented tolerance and are *not* required to be identical (a GPU sums differently).
#[test]
fn the_cpu_and_gpu_routes_select_identically() {
    let _guard = ROUTE_LOCK.lock().unwrap();
    let events = frozen_corpus();
    let g = golden();

    let root = tmp("select");
    let engine = Engine::init(&root, config(24 * 60 * 60 * 1000)).unwrap();
    for (i, chunk) in events.chunks(200).enumerate() {
        engine
            .ingest(chunk.to_vec(), 1_760_000_000_000 + i as i64)
            .unwrap();
    }

    let mut score_diffs_seen = false;
    for exp in &g.expectations {
        let q = exp.query.to_query();
        let (cpu, cpu_route, _) = answer(&engine, &q, Route::Cpu);
        let (gpu_ans, gpu_route, degraded) = answer(&engine, &q, Route::GpuReference);

        assert_eq!(cpu_route, "cpu");
        assert_eq!(gpu_route, "gpu-reference", "the forced route did not take");
        assert!(
            !degraded,
            "no fault was injected, so nothing should degrade"
        );

        // Event ids and order: IDENTICAL. This is selection-identity.
        let cpu_ids: Vec<&String> = cpu.iter().map(|(id, _)| id).collect();
        let gpu_ids: Vec<&String> = gpu_ans.iter().map(|(id, _)| id).collect();
        assert_eq!(
            cpu_ids, gpu_ids,
            "the CPU and GPU-reference routes returned different event ids or order for query \
             `{}`. The route must be invisible to the answer (determinism contract §9) -- a \
             different selection on a different device is two databases wearing one API.",
            exp.query.text
        );

        // Scores: within tolerance, and genuinely allowed to differ.
        for ((id, cs), (_, gs)) in cpu.iter().zip(&gpu_ans) {
            assert!(
                (cs - gs).abs() <= RERANK_ROUTE_TOLERANCE,
                "route scores for `{id}` differ by {} > tolerance {RERANK_ROUTE_TOLERANCE}",
                (cs - gs).abs()
            );
            if cs.to_bits() != gs.to_bits() {
                score_diffs_seen = true;
            }
        }
    }
    // If the routes never produced a different score bit, the tolerance path is untested and this
    // gate proves nothing about it. The GPU reference reduces in tree order precisely so it does.
    assert!(
        score_diffs_seen,
        "the CPU and GPU routes produced bit-identical scores on every query, so the tolerance was \
         never exercised. The GPU reference is supposed to sum in a different order and round \
         differently -- if it does not, selection-identity is being tested against a no-op."
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// **Selection-identity holds even when the scan is all ties.** The boundary-tie corpus, where the
/// candidate heap must choose among rows at identical distances — exactly where a score difference
/// between routes would flip the answer if C-4's id tie-break did not hold.
#[test]
fn the_routes_agree_even_when_the_scan_is_all_ties() {
    let _guard = ROUTE_LOCK.lock().unwrap();
    let bodies = [
        "the tool call timed out",
        "connection reset by peer",
        "rate limit exceeded",
        "model returned empty",
        "invalid tool arguments",
        "context length exceeded",
    ];
    let events: Vec<Event> = (0..3_000)
        .map(|i| {
            let mut e = corpus::generate(Kind::Zipf, 1, i as u64)[0].clone();
            e.tenant_id = "alpha".into();
            e.event_id = format!("e{i:06}");
            e.body = bodies[i % bodies.len()].to_string();
            e
        })
        .collect();

    let root = tmp("ties");
    let engine = Engine::init(&root, config(24 * 60 * 60 * 1000)).unwrap();
    for chunk in events.chunks(250) {
        engine.ingest(chunk.to_vec(), 1_760_000_000_000).unwrap();
    }

    let q = Query {
        text: "the tool call timed out".into(),
        k: 25,
        candidates: 40,
        rerank: 40,
        nprobe: 8,
        tenant: Some("alpha".into()),
        ..Default::default()
    };
    let (cpu, _, _) = answer(&engine, &q, Route::Cpu);
    let (gpu_ans, _, _) = answer(&engine, &q, Route::GpuReference);
    let cpu_ids: Vec<&String> = cpu.iter().map(|(id, _)| id).collect();
    let gpu_ids: Vec<&String> = gpu_ans.iter().map(|(id, _)| id).collect();
    assert_eq!(
        cpu_ids, gpu_ids,
        "the routes selected different tied rows. When every candidate is at the same distance, \
         selection is decided entirely by the event-id tie-break -- and it must be identical on \
         every route, or a score difference has leaked into the answer."
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// **A cursor survives a route flip between pages (determinism contract §9).**
///
/// This is *why* selection-identity matters: a cursor pins a position in the total order, and if
/// routing page 2 to the GPU changed that order, the cursor from page 1 (computed on the CPU)
/// would point into a different sequence — duplicates or gaps. So paginate a result set while
/// flipping the route on every page, and assert the pages tile the single-route answer **exactly**:
/// same rows, same order, no duplicate, no gap.
#[test]
fn a_cursor_survives_a_route_flip_between_pages() {
    let _guard = ROUTE_LOCK.lock().unwrap();
    let events = frozen_corpus();
    let root = tmp("routeflip");
    let engine = Engine::init(&root, config(24 * 60 * 60 * 1000)).unwrap();
    for (i, chunk) in events.chunks(200).enumerate() {
        engine
            .ingest(chunk.to_vec(), 1_760_000_000_000 + i as i64)
            .unwrap();
    }

    let sess = Session {
        tenant: "t0".into(),
    };
    // A LIMIT small enough that the rerank survivor set spans several pages.
    let sql = "SELECT event_id FROM events WHERE embedding ≈≈ 'the tool call timed out' LIMIT 5";
    let plan = compile(sql, &sess).unwrap();

    let page_ids = |res: &prism_engine::sql::SqlResult| -> Vec<String> {
        res.rows
            .iter()
            .map(|r| r[0].1.as_str().unwrap().to_string())
            .collect()
    };

    // The reference: the whole result on one route, unpaginated enough to compare against.
    gpu::set_forced_route(Some(Route::Cpu));
    let mut whole = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let res = engine.run_sql(&plan, cursor.as_deref()).unwrap();
        whole.extend(page_ids(&res));
        match res.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    gpu::set_forced_route(None);

    // Now paginate again, FLIPPING the route on every page. The pages must tile `whole` exactly.
    let mut tiled = Vec::new();
    let mut cursor: Option<String> = None;
    let mut page = 0;
    loop {
        // gpu-reference on even pages, cpu on odd -- the route genuinely changes mid-pagination.
        let route = if page % 2 == 0 {
            Route::GpuReference
        } else {
            Route::Cpu
        };
        gpu::set_forced_route(Some(route));
        let res = engine.run_sql(&plan, cursor.as_deref()).unwrap();
        gpu::set_forced_route(None);
        tiled.extend(page_ids(&res));
        page += 1;
        match res.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }

    assert!(
        page >= 2,
        "the result did not span multiple pages; the test proves nothing"
    );
    // No duplicate.
    let unique: std::collections::BTreeSet<&String> = tiled.iter().collect();
    assert_eq!(
        unique.len(),
        tiled.len(),
        "a route flip produced a DUPLICATE row across pages -- the cursor did not survive it"
    );
    // Exact tiling: same rows, same order, no gap.
    assert_eq!(
        whole, tiled,
        "paginating with the route flipping between pages did not reproduce the single-route \
         answer. A cursor must survive a route change, or routing is not allowed to exist \
         (determinism contract §9)."
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// **A device fault at every phase degrades to CPU — never a failed query, never a wrong answer.**
#[test]
fn a_device_fault_at_every_phase_degrades_to_cpu() {
    let _guard = ROUTE_LOCK.lock().unwrap();
    let events = frozen_corpus();
    let root = tmp("fault");
    let engine = Engine::init(&root, config(24 * 60 * 60 * 1000)).unwrap();
    for (i, chunk) in events.chunks(200).enumerate() {
        engine
            .ingest(chunk.to_vec(), 1_760_000_000_000 + i as i64)
            .unwrap();
    }

    let q = Query {
        text: "the tool call timed out".into(),
        k: 10,
        tenant: Some("t0".into()),
        ..Default::default()
    };

    // The correct answer, on the CPU, with nothing injected.
    let (cpu_answer, _, _) = answer(&engine, &q, Route::Cpu);

    for phase in Phase::ALL {
        gpu::set_forced_route(Some(Route::GpuReference));
        gpu::set_fault(Some(phase));
        let res = engine.search(&q).unwrap(); // MUST NOT be an Err
        gpu::set_fault(None);
        gpu::set_forced_route(None);

        assert!(
            res.counters.route_degraded,
            "a device fault at {} did not record a degradation",
            phase.name()
        );
        assert_eq!(
            res.counters.rerank_route,
            "cpu",
            "after a fault at {}, the query should have finished on the CPU",
            phase.name()
        );
        let got: Vec<(String, f32)> = res
            .hits
            .iter()
            .map(|h| (h.event.event_id.clone(), h.score))
            .collect();
        assert_eq!(
            got,
            cpu_answer,
            "a device fault at {} produced a DIFFERENT answer than the CPU. A fault must degrade \
             to the CPU answer, never a wrong one (determinism contract §11).",
            phase.name()
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
