//! **S13 dir 3 — the measured 768d cost worksheet** (storage contract §7, issue #3).
//!
//! The cost-per-billion-events worksheet was a PROJECTION at 768d (the measured numbers were dim=64
//! test config; the 768d column was extrapolated linearly). The real-embedding corpus finally makes it
//! MEASURED: a real store at the shipped 768d config (pq_m=96, restarts=8, block 16 KiB), the two-tier
//! byte breakdown read off the manifest, scaled to per-million and per-billion. The projection is kept
//! alongside for comparison — the honest test of a projection is the measurement it predicted.
//!
//! Backend-conditional (on-object bytes; request/egress unchanged, priced by the operator). `#[ignore]`,
//! release. Writes `testing/evidence/cost-worksheet-real-v1.json`.

use prism_engine::realcorpus::RealCorpus;
use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn tmp() -> PathBuf {
    let p = std::env::temp_dir().join(format!("prism-ws-{}", std::process::id()));
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

#[test]
#[ignore]
fn measured_768d_cost_worksheet() {
    let corpus = RealCorpus::load_default().unwrap();
    let root = tmp();
    let engine = Engine::init(&root, config())
        .unwrap()
        .with_plane(corpus.plane());
    engine
        .ingest(corpus.events.clone(), 1_760_000_000_000)
        .unwrap();
    let snap = engine.snapshot().unwrap();
    let readers = engine.open_parts(&snap).unwrap();

    // Per-column logical bytes, summed across parts, then per-row.
    let mut by_col: BTreeMap<String, usize> = BTreeMap::new();
    let mut rows = 0usize;
    for r in &readers {
        rows += r.manifest.row_count;
        for c in &r.manifest.columns {
            *by_col.entry(c.name.clone()).or_default() += c.storage.logical_bytes() as usize;
        }
    }
    let rows_f = rows.max(1) as f64;

    // Two tiers + everything else.
    let pq = *by_col.get("pq_codes").unwrap_or(&0) as f64 / rows_f; // HOT
    let vec = *by_col.get("rerank_vectors").unwrap_or(&0) as f64 / rows_f; // COLD
    let other: f64 = by_col
        .iter()
        .filter(|(k, _)| k.as_str() != "pq_codes" && k.as_str() != "rerank_vectors")
        .map(|(_, v)| *v as f64)
        .sum::<f64>()
        / rows_f;
    let total = pq + vec + other;

    // Scale a per-row byte figure to GB per N events.
    let gb = |bytes_per_row: f64, n: f64| bytes_per_row * n / 1e9;

    eprintln!(
        "\n===== MEASURED 768d COST WORKSHEET (real-v1, pq_m=96, restarts=8, block 16KiB) ====="
    );
    eprintln!("rows={rows}");
    eprintln!("column bytes/row:");
    for (k, v) in &by_col {
        eprintln!("  {k:<18} {:>8.1}", *v as f64 / rows_f);
    }
    eprintln!(
        "TIERS  hot(pq)={pq:.1}  cold(exact vec)={vec:.1}  scalars_and_text={other:.1}  total={total:.1} B/row"
    );
    eprintln!(
        "per BILLION events: hot={:.0}GB cold={:.0}GB other={:.0}GB total={:.0}GB",
        gb(pq, 1e9),
        gb(vec, 1e9),
        gb(other, 1e9),
        gb(total, 1e9)
    );

    // The projection this is testing (from cost-worksheet.json projected_at_768d, at pq_m=8):
    // cold = 3072 B/vec (dim*4), hot(pq_m=8) = 8 B/vec, scalars ~284 B/row. Note the SHIPPED config is
    // pq_m=96, not 8, so the hot tier is 12x the projection's — the projection under-counted the hot
    // tier by assuming pq_m=8. The cold tier (3072) is exact. State both.
    eprintln!("PROJECTION (cost-worksheet.json, pq_m=8): cold=3072 hot=8 scalars~284 B/row");
    eprintln!("  → the projection assumed pq_m=8; the SHIPPED config is pq_m=96, so measured hot is ~{:.0}x the projected hot. Cold matches (3072).", pq / 8.0);

    let receipt = serde_json::json!({
        "_what": "MEASURED 768d cost worksheet (S13 dir 3, issue #3). Two-tier on-object byte footprint at the SHIPPED config, measured on the real-v1 corpus, scaled to per-billion events. Backend-conditional (bytes are backend-invariant; request/egress priced by the operator). The projection is retained alongside for comparison.",
        "backend": "local-object-store", "backend_conditional": true,
        "shipped_config": {"dim": 768, "pq_m": 96, "nlist": 64, "kmeans_restarts": 8, "block_size": prism_part::format::DEFAULT_BLOCK_SIZE},
        "corpus": "real-v1 (all-mpnet 768d, agent-telemetry text)", "rows": rows,
        "measured_bytes_per_row": {
            "hot_pq_codes": pq, "cold_exact_vectors": vec, "scalars_and_text": other, "total": total,
            "by_column": by_col.iter().map(|(k, v)| (k.clone(), *v as f64 / rows_f)).collect::<BTreeMap<_,_>>(),
        },
        "per_billion_events_GB": {"hot": gb(pq, 1e9), "cold": gb(vec, 1e9), "scalars_and_text": gb(other, 1e9), "total": gb(total, 1e9)},
        "per_million_events_GB": {"hot": gb(pq, 1e6), "cold": gb(vec, 1e6), "scalars_and_text": gb(other, 1e6), "total": gb(total, 1e6)},
        "projection_comparison": {
            "projected_at_768d_pq_m8": {"cold": 3072, "hot": 8, "scalars_approx": 284, "source": "cost-worksheet.json projected_at_768d"},
            "measured_vs_projected": {
                "cold_matches": (vec - 3072.0).abs() < 1.0,
                "hot_ratio_measured_over_projected": pq / 8.0,
                "note": "The projection assumed pq_m=8 (the test config's PQ granularity); the SHIPPED 768d config uses pq_m=96 (768/96 = 8-dim subquantizers), so the measured hot tier is ~12x the projected. The COLD tier (exact float32 vectors) matches the projection exactly (768*4 = 3072 B/vec) — the projection got the dominant term right. The scalars_and_text term is now MEASURED on realistic agent-telemetry text, not assumed."
            }
        },
        "note": "The cold tier (exact vectors) dominates at ~3 KB/vec — ~3 TB per billion events — and is why PrismDB tiers it cold and scans the hot PQ codes at bandwidth. This is the number the README quotes, now MEASURED at 768d, not projected."
    });
    std::fs::write(
        "../../testing/evidence/cost-worksheet-real-v1.json",
        serde_json::to_string_pretty(&receipt).unwrap(),
    )
    .unwrap();
    eprintln!("wrote testing/evidence/cost-worksheet-real-v1.json");
    eprintln!("==================================================================\n");

    // Sanity: the cold tier is the exact float32 vectors (dim*4), and it dominates.
    assert!(
        (vec - 3072.0).abs() < 1.0,
        "cold tier must be dim*4 = 3072 B/vec, measured {vec}"
    );
    assert!(
        vec > pq,
        "the cold exact-vector tier must dominate the hot PQ tier"
    );
    let _ = std::fs::remove_dir_all(&root);
}
