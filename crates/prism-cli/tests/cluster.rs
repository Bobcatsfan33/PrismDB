//! **The S9 gate: `semantic_cluster` is a deterministic function of the data (C-7).**
//!
//! Every assertion here cites the clause it exercises:
//! - identical clusters/exemplars/aggregates across layouts and forced plan/route flips
//!   ([determinism contract §13](../../../docs/DETERMINISM-CONTRACT.md));
//! - `ARI ≥ 0.8` against ground-truth labels on the frozen corpus, including the adversarial
//!   shapes, with the no-structure corpus asserted **low-confidence** not confidently-wrong
//!   ([query contract §17](../../../docs/QUERY-CONTRACT.md), directive 5);
//! - the aggregate is bounded before it runs — a `k` over the cap is a named refusal (§17);
//! - exemplars are a C-4 bounded selection on the exact score (§15).

use prism_engine::cluster::{ClusterRequest, Confidence, MAX_SEMANTIC_K};
use prism_engine::cluster_corpus::{self, Shape};
use prism_engine::Engine;
use prism_part::partition::PartitionScheme;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Event;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

/// Generator parameters the frozen corpus was created with (C-2). Changing any of these is a new
/// corpus version, not an edit.
const CORPUS_ROWS: usize = 1600;
const CORPUS_SEED: u64 = 9;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn frozen(shape: Shape) -> Vec<Event> {
    let path = repo_root().join(format!("testing/cluster/v1/{}.tsv", shape.name()));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("frozen cluster corpus missing: {}", path.display()));
    prism_engine::tsv::parse(&text).unwrap()
}

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!(
        "prism-cluster-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn config(window_ms: i64) -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 64,
        nlist: 32,
        pq_m: 8,
        seed: 1234,
        kmeans_restarts: 1,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        partitions: PartitionScheme {
            buckets: 16,
            time_window_ms: window_ms,
            dedicated: Default::default(),
        },
        promote: Vec::new(),
    }
}

fn store_of(events: &[Event], window: i64) -> (Engine, PathBuf) {
    let root = tmp("s");
    let engine = Engine::init(&root, config(window)).unwrap();
    for (i, chunk) in events.chunks(200).enumerate() {
        engine
            .ingest(chunk.to_vec(), 1_760_000_000_000 + i as i64)
            .unwrap();
    }
    (engine, root)
}

fn ari_of_frozen(shape: Shape, k: usize) -> f64 {
    let events = frozen(shape);
    let (engine, root) = store_of(&events, 24 * 60 * 60 * 1000);
    let mut req = ClusterRequest::new("alpha", k);
    req.with_assignments = true;
    let r = engine.semantic_cluster(&req).unwrap();

    // Map each row's assignment and its ground-truth label into parallel integer vectors.
    let truth_by_id: std::collections::BTreeMap<&str, usize> = events
        .iter()
        .map(|e| (e.event_id.as_str(), cluster_corpus::true_label(e)))
        .collect();
    let mut predicted = Vec::new();
    let mut truth = Vec::new();
    for (id, cid) in &r.assignments {
        predicted.push(*cid);
        truth.push(truth_by_id[id.as_str()]);
    }
    let _ = std::fs::remove_dir_all(&root);
    cluster_corpus::adjusted_rand_index(&predicted, &truth)
}

/// **The ARI floor holds on the easy and the adversarial shapes** (query contract §17, directive
/// 5), read from the **frozen** corpus (C-2), not regenerated.
#[test]
fn clustering_recovers_the_true_labels() {
    let k = cluster_corpus::TRUE_CLUSTERS;
    for shape in [Shape::Balanced, Shape::Zipf, Shape::Overlap] {
        let ari = ari_of_frozen(shape, k);
        assert!(
            ari >= 0.8,
            "ARI on the frozen {} corpus was {ari:.3}, below the 0.8 floor",
            shape.name()
        );
    }
}

/// **On uniform noise the honest answer is low confidence** — not `k` confident clusters
/// (query contract §17, the no-structure corpus).
#[test]
fn no_structure_is_reported_low_confidence() {
    let events = frozen(Shape::Noise);
    let (engine, root) = store_of(&events, 24 * 60 * 60 * 1000);
    let r = engine
        .semantic_cluster(&ClusterRequest::new("alpha", 8))
        .unwrap();
    assert_eq!(
        r.confidence,
        Confidence::Low,
        "uniform noise clustered with quality {:.3} but was not flagged low-confidence",
        r.quality
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// **Identical clusters, exemplars, and aggregates across two physical layouts** (C-7,
/// determinism contract §13): the same logical rows partitioned differently must cluster the same.
#[test]
fn clustering_is_layout_invariant() {
    let events = frozen(Shape::Zipf);
    let k = cluster_corpus::TRUE_CLUSTERS;

    let fingerprint = |engine: &Engine| -> Vec<(usize, usize, String, u64)> {
        let r = engine
            .semantic_cluster(&ClusterRequest::new("alpha", k))
            .unwrap();
        r.clusters
            .iter()
            .map(|c| {
                (
                    c.cluster_id,
                    c.count,
                    c.exemplar.event_id.clone(),
                    c.avg_cost.to_bits(),
                )
            })
            .collect()
    };

    let (e1, r1) = store_of(&events, i64::MAX / 4); // one giant time window: few parts
    let (e2, r2) = store_of(&events, 24 * 60 * 60 * 1000); // daily windows: many parts
    let a = fingerprint(&e1);
    let b = fingerprint(&e2);
    assert_eq!(
        a, b,
        "the same logical rows clustered differently under a different layout — the answer is a \
         function of the data, not the layout (C-7)"
    );
    let _ = std::fs::remove_dir_all(&r1);
    let _ = std::fs::remove_dir_all(&r2);
}

/// **The clustering is invisible to the physical plan and route** (C-7, determinism §13): forcing
/// a plan or route flip does not move a cluster, an exemplar, or an aggregate. The aggregate does
/// not run through the search plan/route at all, so this holds by construction — and the gate
/// asserts it, as the directive requires, rather than assuming it.
#[test]
fn clustering_is_plan_and_route_invariant() {
    let events = frozen(Shape::Balanced);
    let (engine, root) = store_of(&events, 24 * 60 * 60 * 1000);
    let k = cluster_corpus::TRUE_CLUSTERS;
    let fp = |engine: &Engine| -> Vec<(usize, usize, String, u64, u64)> {
        engine
            .semantic_cluster(&ClusterRequest::new("alpha", k))
            .unwrap()
            .clusters
            .iter()
            .map(|c| {
                (
                    c.cluster_id,
                    c.count,
                    c.exemplar.event_id.clone(),
                    c.avg_cost.to_bits(),
                    c.error_rate.to_bits(),
                )
            })
            .collect()
    };

    let reference = fp(&engine);
    for plan in [
        prism_engine::plan::Strategy::ScalarFirst,
        prism_engine::plan::Strategy::SemanticFirst,
    ] {
        prism_engine::plan::set_forced_plan(Some(plan));
        assert_eq!(
            fp(&engine),
            reference,
            "a forced plan flip moved the clustering"
        );
        prism_engine::plan::set_forced_plan(None);
    }
    prism_engine::gpu::set_forced_route(Some(prism_engine::gpu::Route::GpuReference));
    assert_eq!(
        fp(&engine),
        reference,
        "a forced route flip moved the clustering"
    );
    prism_engine::gpu::set_forced_route(None);
    let _ = std::fs::remove_dir_all(&root);
}

/// **`k` over the cap is a named refusal, never a silent clamp** (query contract §17).
#[test]
fn an_oversized_k_is_refused_by_name() {
    let events = frozen(Shape::Balanced);
    let (engine, root) = store_of(&events, 24 * 60 * 60 * 1000);
    let err = engine
        .semantic_cluster(&ClusterRequest::new("alpha", MAX_SEMANTIC_K + 1))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("MAX_SEMANTIC_K") && err.contains(&(MAX_SEMANTIC_K + 1).to_string()),
        "the refusal did not name the limit: {err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// **The committed corpus has not moved** (C-2): every frozen shape matches the SHA-256 in
/// `MANIFEST.json`. A drift check compares committed bytes; it never regenerates.
#[test]
fn the_frozen_cluster_corpus_still_means_what_it_meant() {
    let dir = repo_root().join("testing/cluster/v1");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("MANIFEST.json")).unwrap()).unwrap();
    let shapes = &manifest["shapes"];
    for shape in Shape::ALL {
        let bytes = std::fs::read(dir.join(format!("{}.tsv", shape.name()))).unwrap();
        let got = prism_types::hash::content_id(&bytes);
        let want = shapes[shape.name()]["sha256"].as_str().unwrap();
        assert_eq!(
            got,
            want,
            "the frozen {} corpus has changed in place — a corpus change is a new version, not an \
             edit (C-2)",
            shape.name()
        );
    }
}

/// Freeze the corpus (run once, `--ignored`): writes the four shapes and a `MANIFEST.json` with a
/// SHA-256 of each. Refuses to overwrite an existing `v1`, exactly as `new-golden-corpus.sh` does
/// for the golden corpus (C-2). Regeneration is a deliberate, reviewed step — never automatic.
#[test]
#[ignore]
fn freeze_cluster_corpus() {
    let dir = repo_root().join("testing/cluster/v1");
    assert!(
        !dir.join("MANIFEST.json").exists(),
        "testing/cluster/v1 already exists — a corpus change is a NEW version, never an overwrite \
         (C-2). Delete by hand and re-review only with intent."
    );
    std::fs::create_dir_all(&dir).unwrap();
    let mut shapes = serde_json::Map::new();
    for shape in Shape::ALL {
        let events = cluster_corpus::generate(shape, CORPUS_ROWS, CORPUS_SEED);
        let text = prism_engine::tsv::write(&events);
        let bytes = text.into_bytes();
        let sha = prism_types::hash::content_id(&bytes);
        std::fs::write(dir.join(format!("{}.tsv", shape.name())), &bytes).unwrap();
        shapes.insert(
            shape.name().to_string(),
            serde_json::json!({ "sha256": sha, "bytes": bytes.len() }),
        );
    }
    let manifest = serde_json::json!({
        "current": "v1",
        "created_in_sprint": "S9",
        "generator": {
            "rows": CORPUS_ROWS,
            "seed": CORPUS_SEED,
            "true_clusters": cluster_corpus::TRUE_CLUSTERS,
        },
        "why": "Labeled synthetic clusters — the ground-truth oracle for ARI (no sklearn; labels \
                are the exact answer). Four shapes: balanced, zipf (unequal sizes), overlap \
                (touching boundaries), noise (no structure -> low confidence).",
        "shapes": shapes,
    });
    std::fs::write(
        dir.join("MANIFEST.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    eprintln!("froze testing/cluster/v1 ({} shapes)", Shape::ALL.len());
}
