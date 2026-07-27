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
use std::path::PathBuf;

fn tmp() -> PathBuf {
    let p = std::env::temp_dir().join(format!("prism-eps-{}", std::process::id()));
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
        kmeans_restarts: 5, // the current receipted value; restarts is re-derived after ε
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
    let embedder = plane.default_embedder(768);
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
