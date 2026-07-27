//! **S13 dir 2 — re-derive the tail-recall safety pair (`DEFAULT_NPROBE` + `ADAPTIVE_MARGIN`) on the
//! real corpus** (D-074, C-1, C-3/C-6). The safety-critical re-derivation: the whole reason these two
//! constants are chosen on the TAIL (p1/p5, not the mean) is the cluster-boundary query class, whose
//! true neighbours are split across two centroids so a small `nprobe` reaches one and misses the other
//! (the S0/S1 `min recall = 0.000` lesson). The hash corpus's degenerate motifs could not produce real
//! boundaries, so its `nprobe=6` was measured against geometry that flattered it. real-v1's continuous
//! 768d geometry finally has real boundaries — and `boundary_queries.jsonl` puts them in the query set.
//!
//! `#[ignore]` (768d PQ training is heavy — run `--release --ignored --nocapture`). It reuses the exact
//! measurement kernel the hash receipts use (`oracle::sweep_nprobe`, `evidence::sweep_adaptive`) so the
//! only thing that changes is the corpus. It prints the full S1 table (min/p1/p5/zero, by query class)
//! and asserts the shipped constants still clear their tail floor on real-v1.

use prism_engine::realcorpus::RealCorpus;
use prism_engine::{evidence, oracle, Engine};
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::query::{DEFAULT_CANDIDATES, DEFAULT_RERANK};
use prism_types::Query;
use std::path::PathBuf;

static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp() -> PathBuf {
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-np-{}-{}", std::process::id(), n));
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
const P1_FLOOR: f32 = 0.8;

/// Build a real-v1 golden: topic queries (`KIND_TOPIC`) + cluster-boundary queries (`KIND_BOUNDARY`),
/// with exact top-k ground truth by brute force. The boundary class is the point (S1 tail lesson).
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
fn rederive_nprobe_and_adaptive_on_real_v1() {
    let corpus = RealCorpus::load_default().unwrap();
    let root = tmp();
    let engine = Engine::init(&root, config())
        .unwrap()
        .with_plane(corpus.plane());
    engine
        .ingest(corpus.events.clone(), 1_760_000_000_000)
        .unwrap();
    let golden = real_golden(&engine, &corpus);
    let n_topic = corpus.queries.len();
    let n_boundary = corpus.boundary_queries.len();

    // --- nprobe sweep, full S1 discipline ----------------------------------------------------------
    let prov = oracle::sweep_nprobe(
        &engine,
        &golden,
        DEFAULT_CANDIDATES,
        DEFAULT_RERANK,
        P1_FLOOR,
    )
    .unwrap();
    eprintln!(
        "\n===== DEFAULT_NPROBE RE-DERIVATION (real-v1, 768d, nlist=64, {n_topic} topic + {n_boundary} boundary q) ====="
    );
    eprintln!("nprobe  mean    min     p1      p5      zero  scan_frac");
    for r in &prov.sweep {
        if r.nprobe <= 24 || r.nprobe == 64 {
            eprintln!(
                "{:>4}   {:.3}   {:.3}   {:.3}   {:.3}   {:>3}   {:.4}",
                r.nprobe,
                r.mean_recall,
                r.min_recall,
                r.p1_recall,
                r.p5_recall,
                r.zero_recall_queries,
                r.mean_scan_fraction
            );
        }
    }
    eprintln!(
        "CHOSEN nprobe = {} (smallest with p1 recall@{K} >= {P1_FLOOR}); mean {:.3}, scan {:.4}",
        prov.chosen_nprobe, prov.chosen_mean_recall, prov.chosen_scan_fraction
    );

    // The tail lesson, by query class: at nprobe=1 vs the chosen, show where recall actually fails.
    for np in [1usize, prov.chosen_nprobe] {
        let r = oracle::measure_recall(&engine, &golden, np, DEFAULT_CANDIDATES, DEFAULT_RERANK)
            .unwrap();
        eprintln!("  nprobe={np:>2} by-class:");
        for c in &r.by_kind {
            eprintln!(
                "    {:<16} n={:>3}  mean {:.3}  min {:.3}  zero {}",
                c.kind, c.queries, c.mean_recall, c.min_recall, c.zero_recall_queries
            );
        }
    }
    eprintln!(
        "HASH-corpus DEFAULT_NPROBE was 6 (nlist=32). real-v1 chosen above is the shipped basis."
    );

    // --- adaptive margin sweep: starved base below the tail floor, recovered by adaptive probing -----
    // Pick a starved base: the largest nprobe still BELOW the tail floor (so adaptive has a floor to
    // recover). If nprobe=1 already clears the floor (easy geometry), fall back to base 1.
    let starved = prov
        .sweep
        .iter()
        .filter(|r| r.p1_recall < P1_FLOOR)
        .map(|r| r.nprobe)
        .max()
        .unwrap_or(1);
    let shipping = prov.chosen_nprobe;
    eprintln!("\n===== ADAPTIVE_MARGIN RE-DERIVATION (real-v1) =====");
    match evidence::sweep_adaptive(&engine, &golden, "real-v1", starved, shipping, P1_FLOOR) {
        Ok(adapt) => {
            eprintln!(
                "starved base = {} (flat p1 {:.3}), shipping base = {}",
                adapt.starved_base, adapt.flat_starved_p1_recall, adapt.shipping_base
            );
            eprintln!(
                "margin  starved_p1  starved_zero  starved_probes  shipping_p1  shipping_probes"
            );
            for r in &adapt.sweep {
                eprintln!(
                    "{:.2}    {:.3}       {:>3}           {:>6.2}          {:.3}        {:>6.2}",
                    r.margin,
                    r.starved_p1_recall,
                    r.starved_zero_recall_queries,
                    r.starved_mean_probes,
                    r.shipping_p1_recall,
                    r.shipping_mean_probes
                );
            }
            eprintln!(
                "CHOSEN margin = {} (x1000 = {}). HASH-corpus margin was 0.05.",
                adapt.chosen_margin, adapt.chosen_margin_x1000
            );
            eprintln!(
                "RECEIPT_REAL_ADAPTIVE {}",
                serde_json::to_string(&adapt).unwrap()
            );
            // Gate: the benefit-first rule must fire on real geometry (the starved base recovers to
            // the floor), and the shipped ADAPTIVE_MARGIN must equal the margin it selects.
            assert_eq!(
                adapt.selection_basis, "benefit",
                "real-v1 must select the adaptive margin by BENEFIT (starved tail recovered), not the cost-ceiling fallback"
            );
            let margin_x1000 = (prism_types::query::ADAPTIVE_MARGIN * 1000.0).round() as i64;
            assert_eq!(
                adapt.chosen_margin_x1000, margin_x1000,
                "the real-v1 sweep chose margin x1000 {} but ADAPTIVE_MARGIN is x1000 {margin_x1000} — re-derive, do not edit the constant",
                adapt.chosen_margin_x1000
            );
        }
        Err(e) => eprintln!("adaptive sweep did not select a margin: {e}"),
    }
    eprintln!("==================================================================\n");

    // --- exact serialized dumps, so the paired receipts are transcribed from measurement, not typed --
    eprintln!(
        "RECEIPT_REAL_NPROBE {}",
        serde_json::to_string(&prov).unwrap()
    );

    // --- gate: the shipped DEFAULT_NPROBE must be exactly the smallest that holds the tail on real-v1
    let shipped = prism_types::query::DEFAULT_NPROBE;
    for r in &prov.sweep {
        if r.nprobe < shipped {
            assert!(
                r.p1_recall < P1_FLOOR,
                "nprobe={} already clears the p1 floor ({:.3}) on real-v1, so DEFAULT_NPROBE={shipped} is not the smallest",
                r.nprobe, r.p1_recall
            );
        }
        if r.nprobe == shipped {
            assert!(
                r.p1_recall >= P1_FLOOR && r.zero_recall_queries == 0,
                "DEFAULT_NPROBE={shipped} must clear the tail floor on real-v1: p1 {:.3} (>= {P1_FLOOR}?), zero {}",
                r.p1_recall, r.zero_recall_queries
            );
        }
    }
    assert_eq!(
        prov.chosen_nprobe, shipped,
        "the real-v1 sweep chose nprobe {} but DEFAULT_NPROBE is {shipped} — re-derive, do not edit the constant",
        prov.chosen_nprobe
    );

    // A boundary class must actually be harder than topic at nprobe=1, or the golden set is not
    // testing the tail (the S1 self-check, now on real geometry).
    let r1 =
        oracle::measure_recall(&engine, &golden, 1, DEFAULT_CANDIDATES, DEFAULT_RERANK).unwrap();
    let topic1 = r1
        .by_kind
        .iter()
        .find(|c| c.kind == oracle::KIND_TOPIC)
        .map(|c| c.mean_recall)
        .unwrap_or(1.0);
    let bound1 = r1
        .by_kind
        .iter()
        .find(|c| c.kind == oracle::KIND_BOUNDARY)
        .map(|c| c.mean_recall)
        .unwrap_or(1.0);
    assert!(
        bound1 <= topic1,
        "cluster-boundary queries must be at least as hard as topic queries at nprobe=1 (boundary {bound1:.3} vs topic {topic1:.3}); \
         otherwise the boundary set is not exercising the tail"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _: Query = Query::default();
}
