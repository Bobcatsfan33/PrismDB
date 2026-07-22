//! **The threshold overfetch margin ε is MEASURED, not guessed (S12 item 1).** A similarity
//! threshold `τ` on the exact cosine score translates to a squared-L2 bound `l2² ≤ 2(1−τ)` (unit
//! vectors), but the candidate phase only has the PQ **approximation** of `l2²`, which is the exact
//! distance ± quantization error. To let *every* row that clears `τ` survive the candidate phase
//! (PQ-approximately), the candidate bound is relaxed to `PQ ≤ 2(1−τ) + ε`, and `ε` is a **measured**
//! high quantile of that quantization error — the quantile IS the recall contract for threshold
//! queries ([D-074](../../../docs/DECISIONS.md), C-3).
//!
//! This test measures the error distribution on the golden corpus for the reference generation and
//! asserts the receipted `ε` (`testing/evidence/pq-margin.json`) still bounds its p999 — a C-6 guard
//! that a codebook change re-derives the margin. `ε` is **corpus- and generation-conditional**.

use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::vector::l2_sq;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-pqm-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn config() -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 64,
        nlist: 16,
        pq_m: 8,
        seed: 9,
        kmeans_restarts: 1,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        partitions: Default::default(),
        promote: Vec::new(),
    }
}

/// Measure the PQ quantization error |adc(q, code) − l2²(q, exact)| over representative (query, row)
/// pairs on the golden corpus, and report the p999 — the overfetch margin ε.
#[test]
fn the_pq_error_p999_is_the_receipted_overfetch_margin() {
    let root = tmp("margin");
    let engine = Engine::init(&root, config()).unwrap();
    engine
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 3000, 5),
            1_760_000_000_000,
        )
        .unwrap();

    let snap = engine.snapshot().unwrap();
    let gen_id = snap.active_generation.clone().unwrap();
    let g = engine.catalog().get_generation(&gen_id).unwrap();
    let m = config().pq_m;

    let readers = engine.open_parts(&snap).unwrap();

    // Collect (exact_vector, code) for every row, across all parts.
    let mut rows: Vec<(Vec<f32>, Vec<u8>)> = Vec::new();
    for r in &readers {
        for cr in &r.manifest.centroid_ranges {
            let codes = r.read_pq_range(cr).unwrap();
            let idx: Vec<usize> = (cr.first_row..cr.first_row + cr.row_count).collect();
            let exact = r.read_vectors_for_rows(&idx).unwrap();
            for (i, v) in exact.into_iter().enumerate() {
                rows.push((v, codes[i * m..(i + 1) * m].to_vec()));
            }
        }
    }
    assert!(rows.len() > 1000, "need a real sample");

    // Queries are realistic vectors: a stride sample of the stored (normalized) vectors. For each
    // query, the quantization error against every row is |adc − true l2²|.
    let mut errors: Vec<f32> = Vec::new();
    let stride = (rows.len() / 60).max(1);
    for qi in (0..rows.len()).step_by(stride) {
        let q = &rows[qi].0;
        let table = g.pq.adc_table(q).unwrap();
        for (exact, code) in &rows {
            let adc = table.distance(code);
            let truth = l2_sq(q, exact);
            errors.push((adc - truth).abs());
        }
    }

    errors.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| errors[(((errors.len() - 1) as f64) * q).round() as usize];
    let p999 = p(0.999);
    let nonzero = errors.iter().filter(|&&e| e > 0.0).count();
    eprintln!(
        "MEASURED PQ error (dim=64, pq_m=8, golden Zipf): n={} nonzero={} p50={:.8} p99={:.8} p999={:.8} max={:.8}",
        errors.len(),
        nonzero,
        p(0.5),
        p(0.99),
        p999,
        errors[errors.len() - 1]
    );

    // The receipt: ε must bound the measured p999 (with headroom), or the recall contract has drifted
    // and the margin must be re-derived (C-6). Loaded from the committed receipt.
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read("../../testing/evidence/pq-margin.json").unwrap())
            .unwrap();
    let epsilon = receipt["epsilon"].as_f64().unwrap() as f32;
    assert!(
        p999 <= epsilon,
        "measured p999 {p999} exceeds the receipted ε {epsilon}; the codebook changed — re-derive \
         the margin (C-6) and update testing/evidence/pq-margin.json"
    );

    let _ = std::fs::remove_dir_all(&root);
}
