//! **S13 dir 2 — re-derive the fp16 rerank tolerance on the real corpus** (C-1, C-3/C-6, S7/D-049).
//!
//! A part may store its rerank vectors in fp16. The contract: for every reranked candidate of every
//! golden query, the fp16 cosine score differs from the fp32-exact score by no more than
//! `FP16_COSINE_TOLERANCE`, and — the property that actually matters — the ordered answer is stable
//! (a lossy encoding that reorders the answer is a different database, not a smaller one). The
//! tolerance is a conservative bound at or above the worst gap. This re-verifies it on real 768d
//! all-mpnet vectors: the gap is geometry-dependent, but the VALUE holds with huge headroom across both
//! corpora, so the tolerance is classified geometry-**STABLE** (re-verified per corpus, not re-derived,
//! and not tightened to a single corpus). It is **not** codebook-dependent — fp16 rounds the raw stored
//! vectors, so unlike nprobe/ε/widths it is not downstream of `KMEANS_RESTARTS` (C-8).
//!
//! `#[ignore]` (768d ingest) but cheap once ingested — brute-force scoring, no nprobe sweep.

use prism_engine::evidence::sweep_fp16;
use prism_engine::realcorpus::RealCorpus;
use prism_engine::{oracle, Engine};
use prism_part::format::FP16_COSINE_TOLERANCE;
use prism_part::store::{StoreConfig, STORE_VERSION};
use std::path::PathBuf;

static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp() -> PathBuf {
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-fp16-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn config() -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 768,
        nlist: 64,
        pq_m: 96,
        seed: 42,
        kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
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

#[test]
#[ignore]
fn rederive_fp16_tolerance_on_real_v1() {
    let corpus = RealCorpus::load_default().unwrap();
    let root = tmp();
    let engine = Engine::init(&root, config())
        .unwrap()
        .with_plane(corpus.plane());
    engine
        .ingest(corpus.events.clone(), 1_760_000_000_000)
        .unwrap();
    let golden = real_golden(&engine, &corpus);

    let ev = sweep_fp16(&engine, &golden, "real-v1", FP16_COSINE_TOLERANCE).unwrap();

    eprintln!("\n===== FP16 TOLERANCE RE-DERIVATION (real-v1, 768d all-mpnet, restarts=8) =====");
    eprintln!(
        "queries={} candidates_scored={}  max_gap={:.8}  mean_gap={:.8}  committed_tolerance={:.6}",
        ev.queries,
        ev.candidates_scored,
        ev.max_score_gap,
        ev.mean_score_gap,
        ev.committed_tolerance
    );
    eprintln!(
        "selection_stable={}  headroom = tolerance/max_gap = {:.1}x",
        ev.selection_stable,
        ev.committed_tolerance / ev.max_score_gap.max(1e-12)
    );
    eprintln!("HASH-corpus max_gap was ~4.6e-4 (tolerance 2e-3). real-v1 max_gap above is the real number.");
    eprintln!("RECEIPT_REAL_FP16 {}", serde_json::to_string(&ev).unwrap());
    eprintln!("==================================================================\n");

    // --- gate: the committed tolerance must still bound the measured real-v1 gap, and selection is
    // stable (the answer order never flips beyond the tolerance). ---
    assert!(
        ev.max_score_gap <= ev.committed_tolerance as f64,
        "real-v1 fp16 max score gap {:.8} exceeds the committed tolerance {:.6} — re-derive the tolerance (C-6)",
        ev.max_score_gap, ev.committed_tolerance
    );
    assert!(
        ev.selection_stable,
        "fp16 reordered the answer on real-v1 beyond the tolerance — the encoding changed the database, not just its size: {:?}",
        ev.unstable_queries
    );
    let _ = std::fs::remove_dir_all(&root);
}
