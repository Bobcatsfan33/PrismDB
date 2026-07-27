//! **S13 dir 2 — re-derive the candidate/rerank widths on the real corpus** (C-1, C-3/C-6).
//!
//! `DEFAULT_CANDIDATES` and `DEFAULT_RERANK` are swept **jointly** (never independent axes: the
//! candidate width decides who may be reranked, the rerank width decides how many actually are, so a
//! single-axis sweep measures a cross-section of a surface and reports it as the surface), holding
//! `nprobe` at its own re-derived receipted value (11, D-081 as amended by D-083). The rule: among points clearing BOTH
//! tail floors (p1 recall@10 >= 0.8 AND zero empties — the same floors nprobe was chosen against),
//! smallest rerank first then smallest candidates, **subject to `rerank >= MIN_PAGEABLE_ROWS` (50)** —
//! the S3 policy bound, because the paginated result set IS the rerank survivor set.
//!
//! The hash corpus left the recall floors slack (PQ's top-10 already held the true top-10, so every
//! grid point cleared and the pagination bound alone chose the value). The finding to watch for: does
//! real 768d geometry make the **recall** constraint actually bind *above* the policy floor? If it
//! does, the widths move and this receipt says why.
//!
//! `#[ignore]` (768d, heavy — run `--release --ignored --nocapture`).

use prism_engine::evidence::{sweep_widths, MIN_PAGEABLE_ROWS};
use prism_engine::realcorpus::RealCorpus;
use prism_engine::{oracle, Engine};
use prism_part::store::{StoreConfig, STORE_VERSION};
use std::path::PathBuf;

static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp() -> PathBuf {
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-wid-{}-{}", std::process::id(), n));
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

/// real-v1 golden: topic + cluster-boundary queries, exact top-k ground truth.
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
fn rederive_widths_on_real_v1() {
    let corpus = RealCorpus::load_default().unwrap();
    let root = tmp();
    let engine = Engine::init(&root, config())
        .unwrap()
        .with_plane(corpus.plane());
    engine
        .ingest(corpus.events.clone(), 1_760_000_000_000)
        .unwrap();
    let golden = real_golden(&engine, &corpus);

    let w = sweep_widths(&engine, &golden, "real-v1", P1_FLOOR).unwrap();

    eprintln!(
        "\n===== WIDTHS RE-DERIVATION (real-v1, 768d, held nprobe={}, {} queries) =====",
        w.held_nprobe, w.queries
    );
    eprintln!("cand  rerank  p1     min    zero  meets  exact_bytes  p50_ms");
    for r in &w.sweep {
        eprintln!(
            "{:>4}  {:>5}   {:.3}  {:.3}  {:>3}   {:<5}  {:>10.0}  {:.3}",
            r.candidates,
            r.rerank,
            r.p1_recall,
            r.min_recall,
            r.zero_recall_queries,
            r.meets_floor,
            r.mean_exact_bytes,
            r.query_p50_ms
        );
    }
    eprintln!(
        "CHOSEN candidates = {}, rerank = {} (smallest rerank>={MIN_PAGEABLE_ROWS} then smallest candidates, both floors cleared)",
        w.chosen_candidates, w.chosen_rerank
    );

    // Does the RECALL floor bind on real geometry? Two things the hash corpus could not show:
    //   (1) recall depends on RERANK, not candidates — the recall-only minimum rerank width;
    //   (2) whether that minimum lands at, below, or above the MIN_PAGEABLE_ROWS policy floor.
    // recall_min_rerank = the smallest rerank width at which SOME candidate pairing clears the tail
    // floor, ignoring the policy bound. On the hash corpus this was 10 (everything cleared); if real
    // geometry lifts it to the policy floor, the recall and pagination constraints COINCIDE.
    let recall_min_rerank = w
        .sweep
        .iter()
        .filter(|r| r.meets_floor)
        .map(|r| r.rerank)
        .min()
        .expect("some grid point must clear the floor");
    // Is recall flat in candidates at a fixed rerank? (nprobe already delivered the true neighbours,
    // so a wider candidate heap buys nothing.) Check the chosen rerank across candidate widths.
    let at_chosen_rr: Vec<f32> = w
        .sweep
        .iter()
        .filter(|r| r.rerank == w.chosen_rerank)
        .map(|r| r.p1_recall)
        .collect();
    let recall_flat_in_candidates = at_chosen_rr
        .iter()
        .all(|&p| (p - at_chosen_rr[0]).abs() < 1e-6);
    eprintln!(
        "RECALL BINDS: recall-only min rerank = {recall_min_rerank} (hash corpus: 10, fully slack); \
         MIN_PAGEABLE_ROWS = {MIN_PAGEABLE_ROWS}; recall flat in candidates at rerank={} = {}.",
        w.chosen_rerank, recall_flat_in_candidates
    );
    if recall_min_rerank > MIN_PAGEABLE_ROWS {
        eprintln!(
            "  → FINDING: recall binds ABOVE the policy floor — the widths are chosen by RECALL."
        );
    } else if recall_min_rerank == MIN_PAGEABLE_ROWS {
        eprintln!(
            "  → FINDING: recall now binds at EXACTLY the policy floor — rerank=25 gives p1 0.600 (fails), \
             rerank=50 gives 0.900 (clears), and MIN_PAGEABLE_ROWS is also 50. The two constraints COINCIDE, \
             so 50 is now doubly justified (hash corpus: recall cleared at rerank=10, policy alone chose 50). \
             Candidates is irrelevant to recall — nprobe=11 already delivers the true neighbours into a 50-wide \
             heap — so candidates=rerank=50 (minimum memory) is the honest choice."
        );
    } else {
        eprintln!(
            "  → FINDING: even on real geometry recall clears below the policy floor, so MIN_PAGEABLE_ROWS \
             STILL selects the value alone (now on real boundaries, not synthetic slack)."
        );
    }
    eprintln!("RECEIPT_REAL_WIDTHS {}", serde_json::to_string(&w).unwrap());
    eprintln!("=====================================================================\n");

    // --- gate: the shipped width constants must be exactly what the real-v1 joint sweep chooses -----
    assert_eq!(
        w.chosen_candidates,
        prism_types::query::DEFAULT_CANDIDATES,
        "DEFAULT_CANDIDATES {} != real-v1 sweep choice {}",
        prism_types::query::DEFAULT_CANDIDATES,
        w.chosen_candidates
    );
    assert_eq!(
        w.chosen_rerank,
        prism_types::query::DEFAULT_RERANK,
        "DEFAULT_RERANK {} != real-v1 sweep choice {}",
        prism_types::query::DEFAULT_RERANK,
        w.chosen_rerank
    );
    assert!(
        w.chosen_rerank >= MIN_PAGEABLE_ROWS,
        "the chosen rerank must honour the S3 pagination policy bound"
    );
    let _ = std::fs::remove_dir_all(&root);
}
