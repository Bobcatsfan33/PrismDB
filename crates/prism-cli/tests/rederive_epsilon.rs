//! **S13 dir 2 — re-derive the threshold overfetch margin ε on the real corpus** (D-074, C-3/C-6).
//!
//! `#[ignore]` benchmark (768d PQ training is heavy — run `--release --ignored`). Measures the PQ
//! quantization error `|adc − true l2²|` over the real queries × the real-v1 rows, exactly as
//! `pq_margin.rs` does for the hash corpus, and prints the p999 — the new ε. The hash-corpus number is
//! retained (its own test); this is the paired real-v1 series.

use prism_engine::model::ModelPlane;
use prism_engine::realcorpus::RealCorpus;
use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::vector::l2_sq;
use prism_types::Query;
use std::path::PathBuf;

static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp() -> PathBuf {
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-eps-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

const PQ_M: usize = 96; // 768 / 96 = 8-dim subvectors, matching the hash corpus's 8-dim granularity

fn config() -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 768,
        nlist: 64,
        pq_m: PQ_M,
        seed: 42,
        kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        partitions: Default::default(),
        promote: Vec::new(),
    }
}

#[test]
#[ignore]
fn rederive_threshold_epsilon_on_real_v1() {
    let corpus = RealCorpus::load_default().unwrap();
    let plane = corpus.plane();
    let root = tmp();
    let engine = Engine::init(&root, config())
        .unwrap()
        .with_plane(plane.clone());
    engine
        .ingest(corpus.events.clone(), 1_760_000_000_000)
        .unwrap();

    let snap = engine.snapshot().unwrap();
    let gen_id = snap.active_generation.clone().unwrap();
    let g = engine.catalog().get_generation(&gen_id).unwrap();

    // (exact_vector, code) for every stored row.
    let readers = engine.open_parts(&snap).unwrap();
    let mut rows: Vec<(Vec<f32>, Vec<u8>)> = Vec::new();
    for r in &readers {
        for cr in &r.manifest.centroid_ranges {
            let codes = r.read_pq_range(cr).unwrap();
            let idx: Vec<usize> = (cr.first_row..cr.first_row + cr.row_count).collect();
            let exact = r.read_vectors_for_rows(&idx).unwrap();
            for (i, v) in exact.into_iter().enumerate() {
                rows.push((v, codes[i * PQ_M..(i + 1) * PQ_M].to_vec()));
            }
        }
    }
    assert!(rows.len() > 1000);

    // The queries are the REAL corpus queries (a realistic query distribution), embedded via the
    // committed vectors. For each, the quantization error against every row is |adc − true l2²|.
    let embedder = plane.default_embedder(768).unwrap();
    let mut errors: Vec<f32> = Vec::new();
    for q in &corpus.queries {
        let qv = embedder.embed(&q.text).unwrap();
        let table = g.pq.adc_table(&qv).unwrap();
        for (exact, code) in &rows {
            errors.push((table.distance(code) - l2_sq(&qv, exact)).abs());
        }
    }
    errors.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| errors[(((errors.len() - 1) as f64) * q).round() as usize];
    let p999 = p(0.999);

    eprintln!("\n===== ε RE-DERIVATION (real-v1, all-mpnet 768d, pq_m={PQ_M}) =====");
    eprintln!(
        "n={} p50={:.8} p90={:.8} p99={:.8} p999={:.8} max={:.8}",
        errors.len(),
        p(0.50),
        p(0.90),
        p(0.99),
        p999,
        errors[errors.len() - 1]
    );
    eprintln!("HASH-corpus ε was 1e-6 (p999 6e-7). real-v1 p999 above is the shipped ε basis.");
    eprintln!("==================================================================\n");

    // The receipt: the shipped ε must still bound the measured real-v1 p999. If the codebook or corpus
    // drifts and this fails, re-derive ε (C-6) and update pq-margin.json + the constant.
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read("../../testing/evidence/pq-margin.json").unwrap())
            .unwrap();
    let epsilon = receipt["epsilon"].as_f64().unwrap() as f32;
    assert!(
        p999 <= epsilon,
        "real-v1 p999 {p999} exceeds the receipted ε {epsilon}; re-derive the margin (C-6)"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// **The cost of ε=0.30 (S13 dir 2, addition 1).** ε is a recall margin; its price is *overfetch* —
/// the relaxed candidate bound `l2² ≤ 2(1−τ) + ε` admits rows that the exact τ then rejects. This
/// measures the overfetch ratio (candidates admitted / rows that actually clear τ) across selectivities
/// on real-v1, and shows the S9 state-budget refusal firing when the margin admits past its budget. ε is
/// NOT tightened below the receipted p999 to make the number look better — the quantile is the recall
/// contract; a severe overfetch is a finding to file, not a knob to turn.
#[test]
#[ignore]
fn epsilon_overfetch_cost_on_real_v1() {
    let corpus = RealCorpus::load_default().unwrap();
    let root = tmp();
    let engine = Engine::init(&root, config())
        .unwrap()
        .with_plane(corpus.plane());
    engine
        .ingest(corpus.events.clone(), 1_760_000_000_000)
        .unwrap();
    let snap = engine.snapshot().unwrap();
    let total = corpus.events.len();

    // Disarmed: measure the SHIPPED ε=0.30 as it bites on natural geometry (no injection).
    prism_engine::search::inject_threshold_margin(None, None);
    eprintln!("\n===== ε=0.30 OVERFETCH COST (real-v1, {total} rows, shipped margin) =====");
    eprintln!("  τ    queries  qualify(med)  overfetch_med  overfetch_max  admit%(med)");
    for tau in [0.20f32, 0.30, 0.40, 0.50, 0.60, 0.70, 0.80] {
        let mut ratios: Vec<f64> = Vec::new();
        let mut admit_frac: Vec<f64> = Vec::new();
        let mut quals: Vec<usize> = Vec::new();
        let mut zero_qual = 0usize;
        for q in &corpus.queries {
            let query = Query {
                text: q.text.clone(),
                k: total,   // no width cap: keep every row clearing exact τ
                nprobe: 64, // = nlist: scan all cells, so admitted is the true relaxed-bound count
                candidates: total,
                rerank: total,
                threshold: Some(tau),
                ..Default::default()
            };
            let r = engine.search_at(&snap, &query).unwrap();
            let qualifying = r.hits.len();
            let admitted = r.counters.candidates_considered;
            admit_frac.push(admitted as f64 / total as f64);
            if qualifying == 0 {
                zero_qual += 1;
            } else {
                quals.push(qualifying);
                ratios.push(admitted as f64 / qualifying as f64);
            }
        }
        ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
        admit_frac.sort_by(|a, b| a.partial_cmp(b).unwrap());
        quals.sort_unstable();
        let med = |v: &[f64]| {
            if v.is_empty() {
                f64::NAN
            } else {
                v[v.len() / 2]
            }
        };
        let medq = if quals.is_empty() {
            0
        } else {
            quals[quals.len() / 2]
        };
        eprintln!(
            "{tau:.2}   {:>3}({}·0q)   {:>10}   {:>12.2}   {:>12.2}   {:>9.1}%",
            corpus.queries.len(),
            zero_qual,
            medq,
            med(&ratios),
            ratios.last().copied().unwrap_or(f64::NAN),
            med(&admit_frac) * 100.0
        );
    }
    eprintln!(
        "overfetch_med/max = candidates admitted by the relaxed bound / rows clearing exact τ."
    );
    eprintln!(
        "admit% = fraction of the whole corpus the candidate phase kept (scan-all nprobe).\n"
    );

    // The S9 state-budget refusal still fires when the margin admits past the budget. Inject a small
    // budget and a broad τ; the query is refused by name, never answered short.
    prism_engine::search::inject_threshold_margin(None, Some(200));
    let broad = Query {
        text: corpus.queries[0].text.clone(),
        k: total,
        nprobe: 64,
        candidates: total,
        rerank: total,
        threshold: Some(0.1),
        ..Default::default()
    };
    let refused = engine.search_at(&snap, &broad);
    prism_engine::search::inject_threshold_margin(None, None);
    let refused_ok = refused
        .as_ref()
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    eprintln!(
        "state-budget refusal (budget=200, τ=0.1): {}",
        if refused.is_err() {
            format!("REFUSED — {refused_ok}")
        } else {
            "did NOT fire".into()
        }
    );
    eprintln!("note: real-v1 is {total} rows; the production 100k state budget never fires at this scale.");
    eprintln!(
        "The refusal is SCALE-DEPENDENT: at N rows a broad-τ query admits admit%·N, refusing past"
    );
    eprintln!("THRESHOLD_STATE_BUDGET. The overfetch RATIO above is scale-free and is the number to file.");
    eprintln!("=====================================================================\n");
    assert!(
        refused.is_err() && refused_ok.contains("state budget"),
        "the S9 state-budget refusal must still fire under the shipped margin when admits exceed budget"
    );
    let _ = std::fs::remove_dir_all(&root);
}
