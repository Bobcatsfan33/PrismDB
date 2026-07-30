//! **S13 dir 2 — the single re-baseline pass** on the final self-consistent constants (D-083).
//!
//! A MEASUREMENT exercise, not a tuning one: same engine, honest constants (restarts=8, nprobe=11,
//! widths 50/50, block 16 KiB), measured on the real 768d corpus. It refreshes the headline baselines
//! that predated the re-derivation — per-ISA scan, query p50/p95/p99, and the composed broad-τ cost —
//! and decomposes each delta so a reader can tell "the engine got slower" (false) from "the engine is
//! now measured at correct settings on realistic data" (true). The comparison that matters is NOT
//! hash-v1 vs real-v1 (different worlds) but real-v1-at-honest-constants vs the naive config a reader
//! would have picked on the SAME corpus: nprobe=6 is fast and WRONG here (p1 recall below the floor).
//!
//! `#[ignore]`, release. Writes `testing/evidence/rebaseline-real-v1.json`.

use prism_engine::realcorpus::RealCorpus;
use prism_engine::{oracle, Engine};
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_quantizer::kernel;
use prism_types::query::{DEFAULT_CANDIDATES, DEFAULT_NPROBE, DEFAULT_RERANK};
use prism_types::Query;
use std::path::PathBuf;
use std::time::Instant;

static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp() -> PathBuf {
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-rb-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn config(block_size: u32) -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 768,
        nlist: 64,
        pq_m: 96,
        seed: 42,
        kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
        block_size,
        partitions: Default::default(),
        promote: Vec::new(),
    }
}

const K: usize = 10;

fn real_golden(engine: &Engine, corpus: &RealCorpus) -> oracle::Golden {
    let mut expectations = Vec::new();
    let mut mk = |text: &str, kind: &str| {
        let gq = oracle::GoldenQuery {
            text: text.to_string(),
            kind: kind.to_string(),
            tenant: None,
            time_from: None,
            time_to: None,
            k: K,
        };
        let hits = engine.exact_search(&gq.to_query()).unwrap();
        expectations.push(oracle::GoldenExpectation {
            expected_ids: hits.iter().map(|h| h.event.event_id.clone()).collect(),
            expected_scores: hits.iter().map(|h| h.score).collect(),
            query: gq,
        });
    };
    for q in &corpus.queries {
        mk(&q.text, oracle::KIND_TOPIC);
    }
    for q in &corpus.boundary_queries {
        mk(&q.text, oracle::KIND_BOUNDARY);
    }
    oracle::Golden {
        corpus_kind: "real-v1".into(),
        corpus_rows: corpus.events.len(),
        corpus_seed: 42,
        dim: 768,
        nlist: 64,
        pq_m: 96,
        expectations,
    }
}

fn pct(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = (((sorted.len() - 1) as f64) * q).round() as usize;
    sorted[i]
}

/// Warm passes then measured passes over the topic queries at a given (nprobe, block). Returns
/// (sorted latencies ms, scan_rows_per_sec) at the current ISA ceiling.
fn measure(engine: &Engine, golden: &oracle::Golden, nprobe: usize) -> (Vec<f64>, f64) {
    let q = |exp: &oracle::GoldenExpectation| -> Query {
        let mut q = exp.query.to_query();
        q.nprobe = nprobe;
        q.candidates = DEFAULT_CANDIDATES;
        q.rerank = DEFAULT_RERANK;
        q.k = K;
        q
    };
    let topic: Vec<&oracle::GoldenExpectation> = golden
        .expectations
        .iter()
        .filter(|e| e.query.kind == oracle::KIND_TOPIC)
        .collect();
    // 2 warm passes.
    for _ in 0..2 {
        for exp in &topic {
            engine.search(&q(exp)).unwrap();
        }
    }
    let mut lat = Vec::new();
    let mut rows_scanned = 0usize;
    let mut scan_s = 0.0f64;
    for _ in 0..5 {
        for exp in &topic {
            let query = q(exp);
            let t = Instant::now();
            let res = engine.search(&query).unwrap();
            let dt = t.elapsed().as_secs_f64();
            lat.push(dt * 1000.0);
            rows_scanned += res.counters.rows_scanned_pq;
            scan_s += dt;
        }
    }
    lat.sort_by(|a, b| a.total_cmp(b));
    (lat, rows_scanned as f64 / scan_s.max(1e-9))
}

#[test]
#[ignore]
fn rebaseline_real_v1_at_honest_constants() {
    let corpus = RealCorpus::load_default().unwrap();
    let root = tmp();
    let engine = Engine::init(&root, config(prism_part::format::DEFAULT_BLOCK_SIZE))
        .unwrap()
        .with_plane(corpus.plane());
    let t0 = Instant::now();
    engine
        .ingest(corpus.events.clone(), 1_760_000_000_000)
        .unwrap();
    let ingest_rows_per_sec = corpus.events.len() as f64 / t0.elapsed().as_secs_f64();
    let golden = real_golden(&engine, &corpus);

    // --- storage per-row: the 768d 12x-wider term, stated ---
    let snap = engine.snapshot().unwrap();
    let readers = engine.open_parts(&snap).unwrap();
    let (mut pq_bytes, mut vec_bytes, mut rows_stored) = (0usize, 0usize, 0usize);
    for r in &readers {
        rows_stored += r.manifest.row_count;
        for c in &r.manifest.columns {
            match c.name.as_str() {
                "pq_codes" => pq_bytes += c.storage.logical_bytes() as usize,
                "rerank_vectors" => vec_bytes += c.storage.logical_bytes() as usize,
                _ => {}
            }
        }
    }
    let rows_f = rows_stored.max(1) as f64;

    // --- per-ISA query latency + scan rate at the HONEST constants (worst ISA is the headline) ---
    eprintln!("\n===== RE-BASELINE (real-v1, HONEST constants: nprobe={DEFAULT_NPROBE}, widths 50/50, block 16KiB, restarts=8) =====");
    eprintln!("ISA          p50_ms   p95_ms   p99_ms   scan_rows/s");
    let mut per_isa = Vec::new();
    for isa in kernel::available() {
        kernel::set_isa_ceiling(isa);
        let (lat, scan_rps) = measure(&engine, &golden, DEFAULT_NPROBE);
        kernel::clear_isa_ceiling();
        let (p50, p95, p99) = (pct(&lat, 0.50), pct(&lat, 0.95), pct(&lat, 0.99));
        eprintln!(
            "{:<10}   {p50:>6.3}   {p95:>6.3}   {p99:>6.3}   {scan_rps:>11.0}",
            isa.name()
        );
        per_isa.push(serde_json::json!({"isa":isa.name(),"p50_ms":p50,"p95_ms":p95,"p99_ms":p99,"scan_rows_per_sec":scan_rps}));
    }
    // Worst-ISA headline (slowest p50).
    let worst = per_isa
        .iter()
        .max_by(|a, b| {
            a["p50_ms"]
                .as_f64()
                .unwrap()
                .total_cmp(&b["p50_ms"].as_f64().unwrap())
        })
        .unwrap()
        .clone();

    // --- recall: honest nprobe=11 vs the naive nprobe=6 a reader would pick, on the SAME corpus ---
    let honest = oracle::measure_recall(
        &engine,
        &golden,
        DEFAULT_NPROBE,
        DEFAULT_CANDIDATES,
        DEFAULT_RERANK,
    )
    .unwrap();
    let naive =
        oracle::measure_recall(&engine, &golden, 6, DEFAULT_CANDIDATES, DEFAULT_RERANK).unwrap();
    let naive1 =
        oracle::measure_recall(&engine, &golden, 1, DEFAULT_CANDIDATES, DEFAULT_RERANK).unwrap();
    eprintln!(
        "\nRECALL honest nprobe={DEFAULT_NPROBE}: p1={:.3} mean={:.3} scan={:.4} | naive nprobe=6: p1={:.3} (below 0.8 floor) | nprobe=1: {} boundary zeros",
        honest.p1_recall, honest.mean_recall, honest.mean_scan_fraction, naive.p1_recall,
        naive1.by_kind.iter().find(|c| c.kind == oracle::KIND_BOUNDARY).map(|c| c.zero_recall_queries).unwrap_or(0)
    );

    // --- delta decomposition (median p50 at the default ISA ceiling / scalar for stability) ---
    kernel::set_isa_ceiling(kernel::available()[0]);
    let honest_p50 = pct(&measure(&engine, &golden, DEFAULT_NPROBE).0, 0.50);
    let nprobe6_p50 = pct(&measure(&engine, &golden, 6).0, 0.50);
    kernel::clear_isa_ceiling();
    // Block term: a second store at the old 2 KiB block, same honest nprobe.
    let root2 = tmp();
    let engine2 = Engine::init(&root2, config(2 * 1024))
        .unwrap()
        .with_plane(corpus.plane());
    engine2
        .ingest(corpus.events.clone(), 1_760_000_000_000)
        .unwrap();
    let golden2 = real_golden(&engine2, &corpus);
    kernel::set_isa_ceiling(kernel::available()[0]);
    let block2k_p50 = pct(&measure(&engine2, &golden2, DEFAULT_NPROBE).0, 0.50);
    kernel::clear_isa_ceiling();
    eprintln!(
        "\nDELTA DECOMPOSITION (p50, first ISA): honest(nprobe11,16KiB)={honest_p50:.3}ms | nprobe6,16KiB={nprobe6_p50:.3}ms (nprobe term = +{:.3}ms) | nprobe11,2KiB={block2k_p50:.3}ms (block term = {:+.3}ms)",
        honest_p50 - nprobe6_p50, honest_p50 - block2k_p50
    );

    // --- composed broad-τ threshold cost (issue #8): does broad-τ threshold beat topic top-k? ---
    let topic0 = golden
        .expectations
        .iter()
        .find(|e| e.query.kind == oracle::KIND_TOPIC)
        .unwrap();
    let mk = |threshold: Option<f32>, tau: f32| {
        let mut q = topic0.query.to_query();
        q.nprobe = DEFAULT_NPROBE;
        q.candidates = corpus.events.len();
        q.rerank = corpus.events.len();
        q.k = corpus.events.len();
        q.threshold = threshold.map(|_| tau);
        q
    };
    let cost = |q: &Query| -> (f64, usize, usize) {
        for _ in 0..2 {
            engine.search(q).unwrap();
        }
        let mut best = f64::MAX;
        let (mut hits, mut bytes) = (0, 0);
        for _ in 0..5 {
            let t = Instant::now();
            let r = engine.search(q).unwrap();
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            hits = r.hits.len();
            bytes = r.counters.physical_bytes_read;
        }
        (best, hits, bytes)
    };
    let (topic_ms, topic_hits, topic_bytes) = cost(&{
        let mut q = topic0.query.to_query();
        q.nprobe = DEFAULT_NPROBE;
        q.candidates = DEFAULT_CANDIDATES;
        q.rerank = DEFAULT_RERANK;
        q.k = K;
        q
    });
    let (broad_ms, broad_hits, broad_bytes) = cost(&mk(Some(0.2), 0.2));
    let (narrow_ms, narrow_hits, narrow_bytes) = cost(&mk(Some(0.6), 0.6));
    eprintln!(
        "\nCOMPOSED COST (issue #8, nprobe={DEFAULT_NPROBE} + ε=0.30): topic top-k {topic_ms:.3}ms/{topic_hits}hits/{topic_bytes}B | broad-τ=0.2 {broad_ms:.3}ms/{broad_hits}hits/{broad_bytes}B | narrow-τ=0.6 {narrow_ms:.3}ms/{narrow_hits}hits/{narrow_bytes}B"
    );
    let broad_most_expensive = broad_ms > topic_ms && broad_ms > narrow_ms;
    eprintln!("broad-τ is the most expensive query class: {broad_most_expensive}");

    let receipt = serde_json::json!({
        "corpus_version":"real-v1","constants":{"kmeans_restarts":8,"nprobe":DEFAULT_NPROBE,"candidates":DEFAULT_CANDIDATES,"rerank":DEFAULT_RERANK,"block_size":prism_part::format::DEFAULT_BLOCK_SIZE,"dim":768,"nlist":64,"pq_m":96},
        "ingest_rows_per_sec":ingest_rows_per_sec,
        "storage_bytes_per_row":{"pq_codes":pq_bytes as f64/rows_f,"rerank_vectors":vec_bytes as f64/rows_f,"vec_over_pq_ratio":vec_bytes as f64/pq_bytes.max(1) as f64},
        "per_isa":per_isa,"headline_worst_isa":worst,
        "recall":{"honest_nprobe":DEFAULT_NPROBE,"honest_p1":honest.p1_recall,"honest_mean":honest.mean_recall,"honest_scan_fraction":honest.mean_scan_fraction,"naive_nprobe6_p1":naive.p1_recall,"naive_nprobe1_boundary_zeros":naive1.by_kind.iter().find(|c| c.kind==oracle::KIND_BOUNDARY).map(|c| c.zero_recall_queries).unwrap_or(0)},
        "delta_decomposition":{"honest_p50_ms":honest_p50,"nprobe6_p50_ms":nprobe6_p50,"nprobe_term_ms":honest_p50-nprobe6_p50,"block2k_p50_ms":block2k_p50,"block_term_ms":honest_p50-block2k_p50,"note":"768d term is inherent to the corpus (12x wider exact-vector column); it is the dominant term for any byte-touching metric and cannot be isolated without a 64d real corpus."},
        "composed_cost_issue8":{"topic":{"ms":topic_ms,"hits":topic_hits,"bytes":topic_bytes},"broad_tau_0_2":{"ms":broad_ms,"hits":broad_hits,"bytes":broad_bytes},"narrow_tau_0_6":{"ms":narrow_ms,"hits":narrow_hits,"bytes":narrow_bytes},"broad_tau_is_most_expensive":broad_most_expensive},
    });
    std::fs::write(
        "../../testing/evidence/rebaseline-real-v1.json",
        serde_json::to_string_pretty(&receipt).unwrap(),
    )
    .unwrap();
    eprintln!("\nwrote testing/evidence/rebaseline-real-v1.json");
    eprintln!("==================================================================\n");

    // Sanity gates: honest holds the floor, naive does not (the correctness half of the headline).
    assert!(
        honest.p1_recall >= 0.8,
        "honest nprobe must hold the tail floor"
    );
    assert!(
        naive.p1_recall < 0.8,
        "the naive nprobe=6 must visibly fail the floor on this geometry — that is the point"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&root2);
}
