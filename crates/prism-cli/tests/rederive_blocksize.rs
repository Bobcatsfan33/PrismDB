//! **S13 dir 2 — re-derive DEFAULT_BLOCK_SIZE on the real corpus** (C-1, C-3/C-6).
//!
//! Block size is a bytes-moved trade-off, not a geometry constant: a bigger block over-reads on a
//! small ranged fetch (read amplification), a smaller block grows the block directory the manifest
//! carries and every reader pays on every open. The rule (same as `evidence::sweep_block_size`):
//! minimise physically-read bytes across the golden query set, **subject to** the block directory
//! staying under `MANIFEST_BUDGET_BYTES_PER_ROW` (4 bytes/row) — the policy bound that stops the sweep
//! collapsing onto its smallest candidate. Ties go to the smaller block.
//!
//! Why re-measure on real-v1: the exact-rerank vector column at 768d is **12× larger** than the hash
//! corpus's 64d, so the read-amplification profile — the whole point of the trade-off — is different.
//! It is classified geometry-STABLE (it turns on column sizes and row counts, not embedding geometry,
//! so it is not downstream of `KMEANS_RESTARTS`), but the honest value is the one measured at the real
//! column layout. `#[ignore]`, release, heavy (9 stores built at 768d).

use prism_engine::realcorpus::RealCorpus;
use prism_engine::{oracle, Engine};
use prism_part::store::{StoreConfig, STORE_VERSION};
use std::path::PathBuf;

static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp() -> PathBuf {
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-bs-{}-{}", std::process::id(), n));
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
const MANIFEST_BUDGET_BYTES_PER_ROW: f64 = 4.0;

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
fn rederive_block_size_on_real_v1() {
    let corpus = RealCorpus::load_default().unwrap();
    let candidates: [u32; 9] = [512, 1024, 2048, 4096, 8192, 16384, 65536, 262144, 1048576];
    let rows = corpus.events.len();

    // (block_size, manifest_bytes, bytes_read)
    let mut sweep: Vec<(u32, usize, usize)> = Vec::new();
    for &bs in &candidates {
        let root = tmp();
        let engine = Engine::init(&root, config(bs))
            .unwrap()
            .with_plane(corpus.plane());
        engine
            .ingest(corpus.events.clone(), 1_760_000_000_000)
            .unwrap();
        let snap = engine.snapshot().unwrap();
        let readers = engine.open_parts(&snap).unwrap();
        let manifest_bytes: usize = readers
            .iter()
            .map(|r| r.manifest.encode().map(|b| b.len()).unwrap_or(0))
            .sum();
        let golden = real_golden(&engine, &corpus);
        let mut bytes_read = 0usize;
        for exp in &golden.expectations {
            let res = engine.search(&exp.query.to_query()).unwrap();
            bytes_read += res.counters.physical_bytes_read;
        }
        sweep.push((bs, manifest_bytes, bytes_read));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Isolate the directory term: manifest bytes above the largest-block floor (which cancels the
    // fixed S4/S5 extension + column overhead that does not scale with block size).
    let manifest_floor = sweep.iter().map(|r| r.1).min().unwrap();
    let dir_per_row = |m: usize| (m.saturating_sub(manifest_floor)) as f64 / rows as f64;

    eprintln!("\n===== DEFAULT_BLOCK_SIZE RE-DERIVATION (real-v1, 768d, {rows} rows) =====");
    eprintln!("block_size  manifest_bytes  dir_bytes/row  bytes_read  eligible");
    for &(bs, m, br) in &sweep {
        let dpr = dir_per_row(m);
        eprintln!(
            "{bs:>9}   {m:>12}   {dpr:>11.3}   {br:>10}   {}",
            dpr <= MANIFEST_BUDGET_BYTES_PER_ROW
        );
    }

    let best = sweep
        .iter()
        .filter(|&&(_, m, _)| dir_per_row(m) <= MANIFEST_BUDGET_BYTES_PER_ROW)
        .min_by(|a, b| a.2.cmp(&b.2).then(a.0.cmp(&b.0)))
        .copied()
        .expect("no block size fits the manifest budget");
    eprintln!(
        "CHOSEN block_size = {} (min bytes_read {} within dir budget {}/row, ties to smaller).",
        best.0, best.2, MANIFEST_BUDGET_BYTES_PER_ROW
    );
    eprintln!(
        "HASH-corpus chose 2048 (dim 64, 2000 rows). real-v1 column is 12x wider (768d exact vectors)."
    );
    eprintln!(
        "RECEIPT_REAL_BLOCKSIZE {}",
        serde_json::to_string(&serde_json::json!({
            "corpus_version": "real-v1", "dim": 768, "nlist": 64, "pq_m": 96,
            "kmeans_restarts": prism_quantizer::kmeans::KMEANS_RESTARTS, "corpus_rows": rows,
            "manifest_budget_bytes_per_row": MANIFEST_BUDGET_BYTES_PER_ROW,
            "chosen_block_size": best.0,
            "sweep": sweep.iter().map(|&(bs, m, br)| serde_json::json!({
                "block_size": bs, "manifest_bytes": m, "directory_bytes_per_row": dir_per_row(m),
                "bytes_read": br, "eligible": dir_per_row(m) <= MANIFEST_BUDGET_BYTES_PER_ROW
            })).collect::<Vec<_>>(),
        }))
        .unwrap()
    );
    eprintln!("==================================================================\n");

    assert_eq!(
        best.0,
        prism_part::format::DEFAULT_BLOCK_SIZE,
        "the real-v1 sweep chose block size {} but DEFAULT_BLOCK_SIZE is {} — re-derive, do not edit the constant",
        best.0,
        prism_part::format::DEFAULT_BLOCK_SIZE
    );
}
