//! **S13 dir 2 — re-derive KMEANS_RESTARTS on the real corpus** (C-1, C-3/C-6, S5).
//!
//! The objective is not inertia (best-by-inertia is not best-by-recall — S5's lesson): it is the
//! smallest restart count whose codebook needs the fewest probes to clear the recall tail, and — the
//! rule that matters — one that begins a **plateau**: a derived nprobe matched by every larger restart
//! count. A plateau is the signature of a method that has stopped depending on its lucky init; picking
//! the single luckiest grid point would reintroduce exactly the init-dependence this constant removes.
//!
//! This is the heaviest re-derivation: it trains a fresh 768d codebook per restart count and derives
//! that codebook's nprobe. To stay affordable it derives nprobe by **early stop** — the smallest probe
//! count clearing the tail floor, scanned upward and stopped at the first hit (identical result to the
//! full sweep, a fraction of the cost). `#[ignore]`, release, long-running.

use prism_engine::realcorpus::RealCorpus;
use prism_engine::{oracle, Engine};
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_quantizer::kmeans::KMEANS_RESTARTS;
use prism_types::query::{DEFAULT_CANDIDATES, DEFAULT_RERANK};
use std::path::PathBuf;
use std::time::Instant;

static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp() -> PathBuf {
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-res-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn config(restarts: usize) -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 768,
        nlist: 64,
        pq_m: 96,
        seed: 42,
        kmeans_restarts: restarts,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        partitions: Default::default(),
        promote: Vec::new(),
    }
}

const K: usize = 10;
const P1_FLOOR: f32 = 0.8;

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

/// The smallest nprobe (1..=nlist) whose codebook clears the tail floor — early-stopped. Identical to
/// the full sweep's `chosen_nprobe`, at a fraction of the cost. Returns `(nprobe, scan_fraction, p1)`.
fn derive_nprobe(engine: &Engine, golden: &oracle::Golden) -> Option<(usize, f64, f32)> {
    for np in 1..=64usize {
        let r =
            oracle::measure_recall(engine, golden, np, DEFAULT_CANDIDATES, DEFAULT_RERANK).unwrap();
        if r.p1_recall >= P1_FLOOR && r.zero_recall_queries == 0 {
            return Some((np, r.mean_scan_fraction, r.p1_recall));
        }
    }
    None
}

#[test]
#[ignore]
fn rederive_kmeans_restarts_on_real_v1() {
    let corpus = RealCorpus::load_default().unwrap();
    let grid = [1usize, 2, 3, 5, 8, 12, 16, 25];

    // (restarts, derived_nprobe, scan_fraction, p1, train_seconds)
    let mut rows: Vec<(usize, usize, f64, f32, f64)> = Vec::new();
    eprintln!("\n===== KMEANS_RESTARTS RE-DERIVATION (real-v1, 768d, nlist=64) =====");
    eprintln!("restarts  derived_nprobe  scan_frac  p1     train_s");
    for &restarts in &grid {
        let root = tmp();
        let t = Instant::now();
        let engine = Engine::init(&root, config(restarts))
            .unwrap()
            .with_plane(corpus.plane());
        engine
            .ingest(corpus.events.clone(), 1_760_000_000_000)
            .unwrap();
        let train_s = t.elapsed().as_secs_f64();
        let golden = real_golden(&engine, &corpus);
        let (np, scan, p1) = derive_nprobe(&engine, &golden)
            .expect("some nprobe must clear the floor at this restart count");
        eprintln!("{restarts:>5}     {np:>10}      {scan:.4}    {p1:.3}   {train_s:.1}");
        rows.push((restarts, np, scan, p1, train_s));
        let _ = std::fs::remove_dir_all(&root);
    }

    // The plateau rule: the smallest restart count whose derived nprobe is matched by every larger one.
    let chosen = (0..rows.len())
        .find(|&i| rows[i..].iter().all(|r| r.1 == rows[i].1))
        .map(|i| rows[i])
        .expect("no plateau: the derived probe count never settled — widen the grid");
    eprintln!(
        "CHOSEN restarts = {} (smallest beginning a plateau at derived nprobe {}, scan {:.4}).",
        chosen.0, chosen.1, chosen.2
    );
    eprintln!(
        "HASH-corpus chose 5 (plateau at nprobe 6). derived_nprobe sequence: {:?}",
        rows.iter().map(|r| r.1).collect::<Vec<_>>()
    );
    eprintln!(
        "RECEIPT_REAL_RESTARTS {}",
        serde_json::to_string(&serde_json::json!({
            "corpus_version": "real-v1",
            "config": {"dim": 768, "nlist": 64, "pq_m": 96, "seed": 42},
            "p1_floor": P1_FLOOR,
            "chosen_restarts": chosen.0,
            "chosen_nprobe": chosen.1,
            "chosen_scan_fraction": chosen.2,
            "sweep": rows.iter().map(|r| serde_json::json!({
                "restarts": r.0, "derived_nprobe": r.1, "mean_scan_fraction": r.2, "p1_recall": r.3, "train_seconds": r.4
            })).collect::<Vec<_>>(),
        })).unwrap()
    );
    eprintln!("==================================================================\n");

    assert_eq!(
        chosen.0, KMEANS_RESTARTS,
        "the real-v1 plateau begins at {} restarts but KMEANS_RESTARTS is {KMEANS_RESTARTS} — re-derive, do not edit the constant",
        chosen.0
    );
}
