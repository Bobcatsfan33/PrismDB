//! **S13 directive 1 — the frozen real-embedding corpus is intact and the engine searches its
//! geometry** (`testing/corpus/real-v1/`, `scripts/gen-real-corpus.py`).
//!
//! Two things are proven: (1) the committed artifact matches its manifest byte-for-byte (C-2 — a
//! golden corpus is frozen, and a drift check compares committed bytes, never regenerated output); and
//! (2) the engine ingests it with the **replayed real embeddings** and a query's top-k cluster by
//! topic — a genuine continuous geometry the hash embedder's degenerate motifs could never produce.
//! Nothing here runs a model or the network; the embeddings are data.

use prism_engine::realcorpus::RealCorpus;
use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Query;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);
const TS: i64 = 1_760_000_000_000;

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-real-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn config() -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 768,
        nlist: 64,
        pq_m: 96, // 768 / 96 = 8-dim subvectors
        seed: 42,
        // The gate proves the geometry, not the tuned codebook, so one restart keeps 768d training
        // cheap; the C-3/C-6 re-derivation sweeps restarts properly.
        kmeans_restarts: 1,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        partitions: Default::default(),
        promote: Vec::new(),
    }
}

#[test]
fn the_frozen_real_corpus_is_intact_and_the_engine_searches_its_geometry() {
    let dir = RealCorpus::default_dir();

    // (1) C-2: every committed file matches its manifest sha256, byte for byte.
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("MANIFEST.json")).unwrap()).unwrap();
    for (file, want) in manifest["sha256"].as_object().unwrap() {
        let bytes = std::fs::read(dir.join(file)).unwrap();
        let got: String = prism_types::hash::sha256(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            &got,
            want.as_str().unwrap(),
            "{file} has drifted from its manifest sha256 — the frozen corpus was regenerated, not compared (C-2)"
        );
    }
    assert_eq!(manifest["dim"].as_u64(), Some(768));
    assert_eq!(
        manifest["model"].as_str(),
        Some("sentence-transformers/all-mpnet-base-v2")
    );

    let corpus = RealCorpus::load(&dir).unwrap();
    assert_eq!(corpus.events.len(), 3000);
    assert!(corpus.queries.len() >= 40);

    // (2) Ingest with the replayed real embeddings and search.
    let root = tmp("real");
    let engine = Engine::init(&root, config())
        .unwrap()
        .with_plane(corpus.plane());
    engine.ingest(corpus.events.clone(), TS).unwrap();
    let snap = engine.snapshot().unwrap();

    // A query's top-k should be predominantly its OWN topic — semantic clustering, the point of a real
    // embedding space. (A body maps to the topic it was rendered for.)
    let topic_of: HashMap<&str, &str> = corpus
        .events
        .iter()
        .map(|e| (e.body.as_str(), e.event_name.as_str()))
        .collect();
    let mut total = 0usize;
    let mut same_topic = 0usize;
    for q in &corpus.queries {
        let query = Query {
            text: q.text.clone(),
            k: 10,
            nprobe: 32,
            ..Default::default()
        };
        let r = engine.search_at(&snap, &query).unwrap();
        assert!(
            !r.hits.is_empty(),
            "a real-embedding query returned nothing: {}",
            q.text
        );
        for h in &r.hits {
            total += 1;
            if topic_of.get(h.event.body.as_str()) == Some(&q.topic.as_str()) {
                same_topic += 1;
            }
        }
    }
    let precision = same_topic as f64 / total as f64;
    assert!(
        precision > 0.5,
        "real-embedding top-k should cluster by topic (got {precision:.2} same-topic across {} hits) — \
         the geometry is the point of S13's corpus",
        total
    );
    eprintln!("real-corpus top-10 same-topic precision: {precision:.3} over {total} hits");
}
