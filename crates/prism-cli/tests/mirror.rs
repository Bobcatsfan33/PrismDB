//! **The S11 catalog-mirror gates (D-069).** The object store is a *mirror*, not a master: local
//! `CURRENT` is the single-writer authority, the mirror lags it and never leads. These gates prove
//! the two properties that make that safe — **split-brain is detected, not tolerated** (two writers
//! racing the mirror: one wins, the other halts named), and **the mirror is a real recovery target**
//! (a local catalog lost to disk failure is restored from the highest verified mirror snapshot, and
//! the restored store answers byte-identically). The rename→mirror crash/heal gate lives in the
//! cross-process fault harness (`tests/faults.rs`); these two run in-process.

use prism_engine::storage::object::{CachedObjectStore, LocalObjectStore};
use prism_engine::storage::CACHE_QUOTA_BYTES;
use prism_engine::{Engine, Ingestor};
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Query;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-mirror-{}-{}-{}", tag, std::process::id(), n));
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

fn top_ids(engine: &Engine) -> Vec<String> {
    engine
        .search(&Query {
            text: "the tool call timed out retrying".into(),
            k: 15,
            tenant: Some("t1".into()),
            rerank: 40,
            ..Default::default()
        })
        .unwrap()
        .hits
        .iter()
        .map(|h| h.event.event_id.clone())
        .collect()
}

/// **Gate (ii): two writers racing the mirror trip split-brain detection.** Two engines share one
/// object-store backend. The first publishes snapshot `s1` to the mirror; the second, publishing a
/// *different* `s1`, finds the key taken by bytes that are not its own and **halts with the named
/// split-brain condition** — detection, not tolerance (D-069).
#[test]
fn two_writers_racing_the_mirror_trip_split_brain_detection() {
    let shared = tmp("shared-store");
    std::fs::create_dir_all(&shared).unwrap();
    let backend = Arc::new(LocalObjectStore::new(shared));
    let cold = || Arc::new(CachedObjectStore::new(backend.clone(), CACHE_QUOTA_BYTES));

    let root_a = tmp("writer-a");
    let a = Engine::init(&root_a, config()).unwrap().with_cold(cold());
    a.ingest(
        prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 800, 1),
        1_760_000_000_000,
    )
    .unwrap();

    // A second writer, its own local store, the SAME mirror. Its first snapshot is also `s00000001`
    // but names different parts, so it cannot be A's — the CAS conflict is split-brain.
    let root_b = tmp("writer-b");
    let b = Engine::init(&root_b, config()).unwrap().with_cold(cold());
    let err = b
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 800, 2),
            1_760_000_000_000,
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("split-brain"),
        "the second writer must halt with the named split-brain condition, got: {err}"
    );

    let _ = std::fs::remove_dir_all(&root_a);
    let _ = std::fs::remove_dir_all(&root_b);
}

/// **Gate (iii): the disaster drill.** Ingest through the full WAL path (D-068), then delete the
/// local catalog entirely — `CURRENT` and every snapshot file — as a disk failure would. Recovery
/// restores the highest verified snapshot from the mirror and replays the WAL, and the restored
/// store answers **byte-identically** to before the deletion. This drill is the mirror's reason to
/// exist; it is a permanent gate.
#[test]
fn a_lost_local_catalog_is_recovered_from_the_mirror() {
    // The mirror lives on a backend that OUTLIVES the local catalog — modelled here as a separate
    // local directory (a real deployment points it at object storage).
    let mirror_dir = tmp("mirror-store");
    std::fs::create_dir_all(&mirror_dir).unwrap();
    let backend = Arc::new(LocalObjectStore::new(mirror_dir));

    let root = tmp("victim");
    let engine =
        Engine::init(&root, config())
            .unwrap()
            .with_cold(Arc::new(CachedObjectStore::new(
                backend.clone(),
                CACHE_QUOTA_BYTES,
            )));
    let mut ing = Ingestor::open(engine).unwrap();
    ing.ingest(
        prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 2000, 5),
        None,
        None,
        1_760_000_000_000,
    )
    .unwrap();

    let golden = top_ids(&ing.engine);
    assert!(!golden.is_empty());
    let snap_before = ing.engine.snapshot().unwrap().snapshot_id;

    // Disk failure: the local catalog is gone. Parts, the cold tier, and the WAL survive; the
    // authority that names them does not.
    std::fs::remove_file(root.join("catalog/CURRENT")).ok();
    std::fs::remove_dir_all(root.join("catalog/snapshots")).ok();
    // The store now reads as empty — the catalog authority is lost.
    let broken = Engine::open(&root)
        .unwrap()
        .with_cold(Arc::new(CachedObjectStore::new(
            backend.clone(),
            CACHE_QUOTA_BYTES,
        )));
    assert!(
        broken.snapshot().unwrap().parts.is_empty(),
        "precondition: with the catalog deleted the store must read empty before recovery"
    );

    // Recovery: restore the highest verified snapshot from the mirror, then replay the WAL.
    let restored = broken.recover_catalog_from_mirror().unwrap();
    assert_eq!(
        restored.as_deref(),
        Some(snap_before.as_str()),
        "recovery must restore the highest mirror snapshot"
    );
    let mut ing = Ingestor::open(broken).unwrap();
    ing.recover(1_760_000_000_001).unwrap();

    // The restored store answers byte-identically to before the disaster.
    assert_eq!(
        top_ids(&ing.engine),
        golden,
        "the recovered store answered differently — the mirror is not a faithful recovery target"
    );
    assert_eq!(ing.engine.snapshot().unwrap().snapshot_id, snap_before);

    let _ = std::fs::remove_dir_all(&root);
}
