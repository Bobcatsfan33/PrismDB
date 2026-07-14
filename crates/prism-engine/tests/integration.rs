//! S0 integration tests.
//!
//! Each test names the invariant it defends. If you weaken one to make it pass,
//! you have not fixed anything — you have removed the thing that would have told
//! you.

use prism_engine::corpus::{self, Kind};
use prism_engine::model::HashModelPlane;
use prism_engine::{oracle, tsv, Engine};
use prism_part::part::PartReader;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::error::PrismError;
use prism_types::{Event, Query};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp(name: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!(
        "prism-it-{}-{}-{}-{}",
        name,
        std::process::id(),
        n,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn config(dim: usize, nlist: usize, pq_m: usize) -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim,
        nlist,
        pq_m,
        seed: 42,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
    }
}

fn store_with(kind: Kind, rows: usize, seed: u64, name: &str) -> (Engine, PathBuf) {
    let root = tmp(name);
    let engine = Engine::init(&root, config(64, 32, 8)).unwrap();
    engine
        .ingest(corpus::generate(kind, rows, seed), 1_000)
        .unwrap();
    (engine, root)
}

fn q(text: &str) -> Query {
    Query {
        text: text.to_string(),
        ..Default::default()
    }
}

/// `corpus::generate` numbers events from zero, so two corpora built from
/// different seeds carry the *same* event_ids. That is fine for a single corpus
/// and a trap for a multi-batch test: without this, "merge changed the answer"
/// really means "merge correctly deduplicated 400 accidental collisions". When a
/// test wants several distinct batches, it namespaces them.
fn batch(kind: Kind, rows: usize, seed: u64, tag: &str) -> Vec<Event> {
    corpus::generate(kind, rows, seed)
        .into_iter()
        .map(|mut e| {
            e.event_id = format!("{tag}-{}", e.event_id);
            e
        })
        .collect()
}

// ---------------------------------------------------------------- persistence

#[test]
fn data_survives_a_reopen_byte_for_byte() {
    let (engine, root) = store_with(Kind::Uniform, 1000, 1, "reopen");
    let before = engine.search(&q("tool call timed out retrying")).unwrap();
    drop(engine);

    let reopened = Engine::open(&root).unwrap();
    let after = reopened.search(&q("tool call timed out retrying")).unwrap();

    assert_eq!(before.snapshot_id, after.snapshot_id);
    let ids = |r: &prism_types::SearchResult| -> Vec<String> {
        r.hits.iter().map(|h| h.event.event_id.clone()).collect()
    };
    assert_eq!(ids(&before), ids(&after));
    assert_eq!(before.counters, after.counters);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn identical_ingests_produce_identical_parts() {
    // Content addressing is only meaningful if the write path is deterministic.
    let a = tmp("det-a");
    let b = tmp("det-b");
    let ea = Engine::init(&a, config(64, 16, 8)).unwrap();
    let eb = Engine::init(&b, config(64, 16, 8)).unwrap();
    let events = corpus::generate(Kind::Uniform, 500, 9);

    let ra = ea.ingest(events.clone(), 1234).unwrap();
    let rb = eb.ingest(events, 1234).unwrap();

    assert_eq!(
        ra.part_id, rb.part_id,
        "same rows must produce the same part"
    );
    assert_eq!(
        ra.generation_id, rb.generation_id,
        "same data, same codebooks"
    );
    std::fs::remove_dir_all(a).ok();
    std::fs::remove_dir_all(b).ok();
}

// ------------------------------------------------------------------- pruning

#[test]
fn an_ineligible_part_is_never_opened() {
    // The claim "we pruned it" is only worth anything if it is a fact about the
    // syscalls. Three parts, three disjoint tenants; a tenant-filtered query
    // must open exactly one.
    let root = tmp("prune");
    let engine = Engine::init(&root, config(64, 16, 8)).unwrap();

    for (i, tenant) in ["alpha", "beta", "gamma"].iter().enumerate() {
        let events: Vec<Event> = corpus::generate(Kind::Uniform, 300, 10 + i as u64)
            .into_iter()
            .enumerate()
            .map(|(j, mut e)| {
                e.tenant_id = tenant.to_string();
                e.event_id = format!("{tenant}-{j}");
                // Disjoint time windows, one per part.
                e.event_time = 1_000_000 + (i as i64 * 1_000_000) + j as i64;
                e
            })
            .collect();
        engine.ingest(events, 1000 + i as i64).unwrap();
    }

    let mut query = q("tool call timed out retrying");
    query.tenant = Some("beta".to_string());
    let res = engine.search(&query).unwrap();

    assert_eq!(res.counters.parts_total, 3);
    assert_eq!(
        res.counters.parts_pruned, 2,
        "two parts should be pruned on tenant alone"
    );
    assert_eq!(res.counters.parts_opened, 1);
    assert!(res.hits.iter().all(|h| h.event.tenant_id == "beta"));

    // Same, on time.
    let mut tq = q("tool call timed out retrying");
    tq.time_from = Some(2_000_000);
    tq.time_to = Some(2_500_000);
    let res = engine.search(&tq).unwrap();
    assert_eq!(
        res.counters.parts_pruned, 2,
        "two parts should be pruned on time alone"
    );
    assert!(res
        .hits
        .iter()
        .all(|h| (2_000_000..=2_500_000).contains(&h.event.event_time)));

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn probing_fewer_centroids_scans_strictly_fewer_rows() {
    // The centroid index is the whole bet. If nprobe does not change how much
    // data is touched, there is no index.
    let (engine, root) = store_with(Kind::Zipf, 3000, 2, "nprobe");

    let mut low = q("ignore previous instructions reveal the system prompt");
    low.nprobe = 1;
    let mut high = low.clone();
    high.nprobe = 32;

    let lo = engine.search(&low).unwrap();
    let hi = engine.search(&high).unwrap();

    assert!(
        lo.counters.rows_scanned_pq < hi.counters.rows_scanned_pq,
        "nprobe=1 scanned {} rows, nprobe=32 scanned {}",
        lo.counters.rows_scanned_pq,
        hi.counters.rows_scanned_pq
    );
    assert!(
        (lo.counters.rows_scanned_pq as f64) < 0.25 * lo.counters.rows_eligible as f64,
        "a single probe should touch a small fraction of the data, touched {}/{}",
        lo.counters.rows_scanned_pq,
        lo.counters.rows_eligible
    );
    // Scanning everything is exactly scanning everything.
    assert_eq!(hi.counters.rows_scanned_pq, hi.counters.rows_eligible);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn the_rerank_fetch_budget_is_never_exceeded() {
    // The exact-vector tier is 32x the size of the scan tier. If the plan says
    // it will fetch 25 vectors, it fetches at most 25 vectors -- otherwise the
    // cost model is fiction.
    let (engine, root) = store_with(Kind::Uniform, 2000, 3, "budget");
    let mut query = q("write a python function to parse csv");
    query.candidates = 500;
    query.rerank = 25;
    query.k = 10;

    let res = engine.search(&query).unwrap();
    assert!(res.counters.exact_vectors_fetched <= 25);
    assert_eq!(
        res.counters.exact_bytes_fetched,
        res.counters.exact_vectors_fetched * 64 * 4
    );
    assert!(res.counters.rerank_width <= 25);
    assert_eq!(res.hits.len(), 10);
    std::fs::remove_dir_all(root).ok();
}

// -------------------------------------------------------------------- recall

#[test]
fn approximate_search_meets_its_recall_contract_against_the_exact_oracle() {
    let (engine, root) = store_with(Kind::Zipf, 3000, 1234, "recall");
    let golden = oracle::build(&engine, "zipf", 3000, 1234, 10).unwrap();

    let report = oracle::measure_recall(&engine, &golden, 4, 200, 50).unwrap();
    assert!(
        report.mean_recall >= 0.95,
        "mean recall@10 was {:.3} at nprobe=4 (scan fraction {:.3})",
        report.mean_recall,
        report.mean_scan_fraction
    );
    // And it bought that recall by NOT scanning everything -- which is the
    // entire point. Recall of 1.0 at a scan fraction of 1.0 is a full scan.
    assert!(
        report.mean_scan_fraction < 0.5,
        "recall was bought at a scan fraction of {:.3}",
        report.mean_scan_fraction
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn scanning_every_centroid_reproduces_the_exact_answer() {
    // With nprobe = nlist and a candidate width above the row count, the
    // approximate path degenerates into the exact one. If it does not, the
    // difference is a bug, not approximation.
    let (engine, root) = store_with(Kind::Uniform, 800, 5, "exhaustive");

    let mut query = q("connection pool exhausted deadlock");
    query.nprobe = 32;
    query.candidates = 2000;
    query.rerank = 2000;
    query.k = 10;

    let approx = engine.search(&query).unwrap();
    let exact = engine.exact_search(&query).unwrap();

    let a: Vec<&str> = approx
        .hits
        .iter()
        .map(|h| h.event.event_id.as_str())
        .collect();
    let e: Vec<&str> = exact.iter().map(|h| h.event.event_id.as_str()).collect();
    assert_eq!(a, e, "exhaustive probe did not reproduce the oracle");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn the_committed_golden_corpus_still_means_what_it_meant() {
    // Permanent artifact #2. Rebuild the store from the committed corpus and
    // recompute the exact answers. If they moved, something changed the meaning
    // of stored data -- embedder, normalization, or the storage path.
    let root = tmp("golden");
    let engine = Engine::init(&root, config(64, 32, 8)).unwrap();
    // The golden store is built with seed 1234; match it exactly.
    let engine = {
        std::fs::remove_dir_all(&root).ok();
        Engine::init(
            &root,
            StoreConfig {
                format_version: STORE_VERSION,
                dim: 64,
                nlist: 32,
                pq_m: 8,
                seed: 1234,
                block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
            },
        )
        .unwrap_or(engine)
    };

    let corpus_path = repo_root().join("testing/golden/v1/corpus.tsv");
    let events = tsv::parse(&std::fs::read_to_string(&corpus_path).unwrap()).unwrap();
    engine.ingest(events, 1_000).unwrap();

    let golden: oracle::Golden = serde_json::from_slice(
        &std::fs::read(repo_root().join("testing/golden/v1/expected.json")).unwrap(),
    )
    .unwrap();

    oracle::check_drift(&engine, &golden).expect("exact-search drift against the golden corpus");

    let report = oracle::measure_recall(&engine, &golden, 4, 200, 50).unwrap();
    assert!(
        report.mean_recall >= 0.95,
        "recall@10 against the golden corpus was {:.3}",
        report.mean_recall
    );
    std::fs::remove_dir_all(root).ok();
}

// ------------------------------------------------------------- dead-lettering

#[test]
fn unembeddable_events_are_dead_lettered_never_silently_stored() {
    // The rule from Part III §10: an event is never stored without the semantic
    // columns it asked for. Empty bodies, punctuation-only bodies and oversized
    // bodies must be visible in the dead-letter log, not sitting in a part with
    // a meaningless vector.
    let root = tmp("deadletter");
    let engine = Engine::init(&root, config(64, 8, 8)).unwrap();

    let events = corpus::generate(Kind::Edge, 100, 5);
    let offered = events.len();
    let report = engine.ingest(events, 1000).unwrap();

    assert!(
        report.dead_lettered > 0,
        "the edge corpus contains rows that cannot be embedded"
    );
    assert_eq!(
        report.admitted + report.dead_lettered,
        offered,
        "every offered row is either admitted or dead-lettered; none may vanish"
    );

    let dl = std::fs::read_to_string(engine.store.deadletter_path()).unwrap();
    assert_eq!(dl.lines().count(), report.dead_lettered);
    assert!(dl.contains("zero-norm") || dl.contains("empty") || dl.contains("limit"));

    // And the rows that did make it are all really there.
    let snap = engine.snapshot().unwrap();
    let stored: usize = engine
        .open_parts(&snap)
        .unwrap()
        .iter()
        .map(|r| r.manifest.row_count)
        .sum();
    assert_eq!(stored, report.admitted);
    std::fs::remove_dir_all(root).ok();
}

// -------------------------------------------------------------------- merge

#[test]
fn merge_preserves_results_and_reconciles_duplicates_by_the_documented_policy() {
    let root = tmp("merge");
    let engine = Engine::init(&root, config(64, 16, 8)).unwrap();

    // Two distinct batches, plus exactly one deliberate duplicate: the same
    // event_id re-ingested later with a newer event_time and a different body.
    engine
        .ingest(batch(Kind::Uniform, 400, 1, "a"), 1000)
        .unwrap();
    engine
        .ingest(batch(Kind::Uniform, 400, 2, "b"), 2000)
        .unwrap();

    let dup = Event {
        observed_time: 9_999_999_999_999,
        trace_id: String::new(),
        span_id: String::new(),
        attributes: Default::default(),
        idempotency_key: None,
        event_id: "a-e00000001".to_string(), // collides with the first batch
        tenant_id: "t0".to_string(),
        event_time: 9_999_999_999_999, // newer: this one must win
        event_name: "db.error".to_string(),
        cost: 1.0,
        error: true,
        body: "connection pool exhausted while querying the primary".to_string(),
    };
    engine.ingest(vec![dup.clone()], 3000).unwrap();

    let before = engine
        .search(&q("summarize this report in bullet points"))
        .unwrap();
    assert_eq!(before.counters.parts_total, 3);

    let report = engine.merge(4000).unwrap();
    assert_eq!(report.parts_in, 3);
    assert_eq!(report.parts_out, 1, "one generation collapses to one part");
    assert_eq!(report.rows_in, 801);
    assert_eq!(
        report.duplicates_reconciled, 1,
        "exactly one event_id collided; nothing else may be deduplicated"
    );
    assert_eq!(report.rows_out, 800);
    assert_eq!(
        report.rows_out,
        report.rows_in - report.duplicates_reconciled
    );

    // Results survive the merge unchanged.
    let after = engine
        .search(&q("summarize this report in bullet points"))
        .unwrap();
    let ids = |r: &prism_types::SearchResult| -> Vec<String> {
        r.hits.iter().map(|h| h.event.event_id.clone()).collect()
    };
    assert_eq!(ids(&before), ids(&after), "merge changed the answer");
    assert_eq!(after.counters.parts_total, 1);

    // Last write won.
    let mut find = q("connection pool exhausted while querying the primary");
    find.k = 50;
    find.nprobe = 16;
    let hit = engine
        .search(&find)
        .unwrap()
        .hits
        .into_iter()
        .find(|h| h.event.event_id == "a-e00000001")
        .expect("the deduplicated row is still queryable");
    assert_eq!(
        hit.event.event_time, dup.event_time,
        "the newer row must win"
    );
    assert!(hit.event.error);

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn merge_write_amplification_is_reported() {
    let root = tmp("amp");
    let engine = Engine::init(&root, config(64, 16, 8)).unwrap();
    for i in 0..4 {
        engine
            .ingest(
                batch(Kind::Uniform, 250, i, &format!("b{i}")),
                1000 + i as i64,
            )
            .unwrap();
    }
    let r = engine.merge(5000).unwrap();
    assert_eq!(
        r.duplicates_reconciled, 0,
        "these batches share no event_ids"
    );
    assert!(r.bytes_read > 0 && r.bytes_written > 0);
    // Compacting four parts into one rewrites roughly the same bytes: no data is
    // dropped and none is duplicated. Amplification far from 1.0 here would mean
    // the merge is either losing rows or writing them twice.
    assert!(
        (0.9..1.1).contains(&r.write_amplification),
        "write amplification of {} is implausible for a lossless compaction",
        r.write_amplification
    );
    std::fs::remove_dir_all(root).ok();
}

// ------------------------------------------------------ generations, invariant 9

#[test]
fn a_query_refuses_to_merge_scores_across_embedding_spaces() {
    // Invariant 9. Build a store that genuinely holds two embedding spaces, then
    // prove the engine will not silently compare their scores -- and will not
    // silently drop one of them either.
    let root = tmp("spaces");
    let engine = Engine::init(&root, config(64, 16, 8)).unwrap();
    engine
        .ingest(corpus::generate(Kind::Uniform, 400, 1), 1000)
        .unwrap();

    let snap_v1 = engine.snapshot().unwrap();
    let part_v1 = snap_v1.parts[0].clone();

    // Migrate to model version 2. The new snapshot holds only v2 parts.
    let engine2 = Engine::open(&root)
        .unwrap()
        .with_plane(Arc::new(HashModelPlane::at_version("2")));
    let re = engine2.reembed("2", 2000).unwrap();
    assert_ne!(re.old_generation, re.new_generation);
    assert_eq!(re.old_model, "hash-embedder:1");
    assert_eq!(re.new_model, "hash-embedder:2");

    // Now force the mixed state the invariant exists for: a snapshot naming
    // parts from both spaces at once.
    let snap_v2 = engine2.snapshot().unwrap();
    let mixed = engine2
        .catalog()
        .commit(
            &snap_v2,
            vec![part_v1.clone(), snap_v2.parts[0].clone()],
            snap_v2.next_seq + 1,
            snap_v2.active_generation.clone(),
            3000,
        )
        .unwrap();
    assert_eq!(mixed.parts.len(), 2);

    // Unqualified: refuse. Loudly.
    let err = engine2.search(&q("tool call timed out")).unwrap_err();
    match err {
        PrismError::Invariant(m) => {
            assert!(m.contains("embedding space"), "unhelpful message: {m}");
            assert!(m.contains("hash-embedder:1") && m.contains("hash-embedder:2"));
        }
        other => panic!("expected an invariant violation, got {other:?}"),
    }

    // Qualified: answer, from exactly one space.
    let mut qq = q("tool call timed out");
    qq.space = Some("hash-embedder:2".to_string());
    let res = engine2.search(&qq).unwrap();
    assert_eq!(
        res.counters.parts_pruned, 1,
        "the other space must be pruned, visibly"
    );
    assert_eq!(res.generations.len(), 1);
    assert!(!res.hits.is_empty());

    let mut q1 = q("tool call timed out");
    q1.space = Some("hash-embedder:1".to_string());
    let res1 = engine2.search(&q1).unwrap();
    assert_eq!(res1.generations.len(), 1);
    assert_ne!(res1.generations[0], res.generations[0]);

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn rollback_is_a_catalog_write_not_a_data_rewrite() {
    let root = tmp("rollback");
    let engine = Engine::init(&root, config(64, 16, 8)).unwrap();
    engine
        .ingest(corpus::generate(Kind::Uniform, 300, 1), 1000)
        .unwrap();

    let original = engine.snapshot().unwrap();
    let before = engine.search(&q("plan the steps and call tools")).unwrap();

    let engine2 = Engine::open(&root)
        .unwrap()
        .with_plane(Arc::new(HashModelPlane::at_version("2")));
    engine2.reembed("2", 2000).unwrap();

    // The old part was never touched. Byte for byte, it is still there.
    let old_part = PartReader::open(&engine.store.part_dir(&original.parts[0])).unwrap();
    old_part
        .verify()
        .expect("the pre-migration part is still checksum-valid");

    let new_id = engine2.rollback(&original.snapshot_id, 3000).unwrap();
    assert_ne!(
        new_id, original.snapshot_id,
        "rollback moves forward, it does not rewind history"
    );

    let after = Engine::open(&root)
        .unwrap()
        .search(&q("plan the steps and call tools"))
        .unwrap();
    let ids = |r: &prism_types::SearchResult| -> Vec<String> {
        r.hits.iter().map(|h| h.event.event_id.clone()).collect()
    };
    assert_eq!(
        ids(&before),
        ids(&after),
        "rollback did not restore the old answer"
    );
    std::fs::remove_dir_all(root).ok();
}

// ----------------------------------------------------------------------- gc

#[test]
fn gc_never_removes_a_referenced_part() {
    let root = tmp("gc-safe");
    let engine = Engine::init(&root, config(64, 16, 8)).unwrap();
    for i in 0..3 {
        engine
            .ingest(corpus::generate(Kind::Uniform, 200, i), 1000 + i as i64)
            .unwrap();
    }

    let snap = engine.snapshot().unwrap();
    let live: Vec<String> = snap.parts.clone();

    let report = engine.catalog().gc(5, false).unwrap();
    for p in &live {
        assert!(
            !report.removed_parts.contains(p),
            "gc removed part {p}, which the live snapshot references"
        );
        assert!(engine.store.part_dir(p).exists());
    }

    // And the store still answers.
    engine.catalog().verify().unwrap();
    assert!(!engine
        .search(&q("mfa challenge failed"))
        .unwrap()
        .hits
        .is_empty());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn gc_reclaims_parts_that_no_retained_snapshot_names() {
    let root = tmp("gc-reclaim");
    let engine = Engine::init(&root, config(64, 16, 8)).unwrap();
    for i in 0..3 {
        engine
            .ingest(corpus::generate(Kind::Uniform, 200, i), 1000 + i as i64)
            .unwrap();
    }
    let pre_merge: Vec<String> = engine.snapshot().unwrap().parts;
    engine.merge(5000).unwrap();

    // With retention 1, the pre-merge parts are unreachable and reclaimable.
    let report = engine.catalog().gc(1, false).unwrap();
    for p in &pre_merge {
        assert!(
            report.removed_parts.contains(p),
            "the merged-away part {p} should be reclaimed"
        );
        assert!(!engine.store.part_dir(p).exists());
    }
    engine.catalog().verify().unwrap();
    assert!(!engine
        .search(&q("mfa challenge failed"))
        .unwrap()
        .hits
        .is_empty());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_dry_run_gc_removes_nothing() {
    let root = tmp("gc-dry");
    let engine = Engine::init(&root, config(64, 16, 8)).unwrap();
    engine
        .ingest(corpus::generate(Kind::Uniform, 200, 1), 1000)
        .unwrap();
    engine
        .ingest(corpus::generate(Kind::Uniform, 200, 2), 2000)
        .unwrap();
    engine.merge(3000).unwrap();

    let dry = engine.catalog().gc(1, true).unwrap();
    assert!(
        !dry.removed_parts.is_empty(),
        "there is something to reclaim"
    );
    for p in &dry.removed_parts {
        assert!(engine.store.part_dir(p).exists(), "a dry run deleted {p}");
    }
    std::fs::remove_dir_all(root).ok();
}

// ------------------------------------------------- format compatibility (#1)

#[test]
fn todays_build_opens_the_committed_v1_fixture() {
    // Permanent artifact #1. The day this fails, either the format changed
    // without a version bump, or a compatibility promise was quietly broken.
    let fixture = repo_root().join("testing/compat/v1");
    let engine = Engine::open(&fixture).expect("the v1 fixture must open");

    let report = engine
        .catalog()
        .verify()
        .expect("every v1 byte still checksums");
    assert_eq!(report.parts_verified, 1);

    let mut query = q("tool call timed out retrying");
    query.nprobe = 8;
    let res = engine.search(&query).unwrap();
    assert!(
        !res.hits.is_empty(),
        "the v1 fixture must still answer queries"
    );
    assert_eq!(res.hits[0].event.event_name, "tool.retry");
}

#[test]
fn todays_build_opens_every_released_format() {
    // The compatibility corpus, doing the only job it has. Three released formats,
    // three sets of committed bytes, and today's build opens and answers from all of
    // them. v1 and v2 are NEVER regenerated -- their whole value is that nothing
    // since has touched them.
    for (version, legacy) in [(1u32, true), (2, true), (3, false)] {
        let fixture = repo_root().join(format!("testing/compat/v{version}"));
        let engine = Engine::open(&fixture)
            .unwrap_or_else(|e| panic!("the v{version} fixture must open: {e}"));

        engine
            .catalog()
            .verify()
            .unwrap_or_else(|e| panic!("every v{version} byte must still checksum: {e}"));

        let snap = engine.snapshot().unwrap();
        let parts = engine.open_parts(&snap).unwrap();
        assert_eq!(parts[0].manifest.format_version, version);
        assert_eq!(parts[0].is_legacy(), legacy);

        // Every part, of every version, declares its rerank tier (D-003-resolved) --
        // v1 and v2 by synthesis, because they had no choice but float32/exact.
        assert_eq!(parts[0].manifest.rerank.describe(), "float32/exact");

        // Only v3 carries attributes and trace context. A v1/v2 part not having them
        // is history, not corruption, and a reader that cannot tell the difference
        // will condemn perfectly good data.
        assert_eq!(parts[0].manifest.has_attributes(), version >= 3);
        assert_eq!(parts[0].manifest.has_trace_context(), version >= 3);

        let mut query = q("tool call timed out retrying");
        query.nprobe = 8;
        let res = engine.search(&query).unwrap();
        assert!(
            !res.hits.is_empty(),
            "the v{version} fixture must still answer"
        );
        assert_eq!(res.hits[0].event.event_name, "tool.retry");
    }
}

#[test]
fn the_v3_fixture_carries_bounded_attributes() {
    let fixture = repo_root().join("testing/compat/v3");
    let engine = Engine::open(&fixture).expect("the v3 fixture must open");

    let snap = engine.snapshot().unwrap();
    let parts = engine.open_parts(&snap).unwrap();
    let m = &parts[0].manifest;

    // The key dictionary lives in the MANIFEST, not a column -- so "does this part
    // hold any row with key X?" is answerable without opening a single column file.
    // And it is bounded, which is the limit that protects the shape of the data.
    assert!(m.attribute_keys.len() <= prism_types::limits::MAX_ATTRIBUTE_KEY_CARDINALITY);

    // The TLV extension section ships before its first user, on purpose: the first
    // user must not also be a format break.
    assert!(m.extensions.is_empty());
    assert_eq!(m.reserved, [0u64; 4]);
}

#[test]
fn every_corrupt_fixture_is_rejected_with_a_specific_error() {
    // Twelve ways a v2 part can lie, and five ways a v1 part can. Each must be
    // refused with an error that says *which byte lied* — an operator woken at
    // 3am cannot act on the word "corrupt".
    let cases: [(&str, &str, &str); 17] = [
        // --- v3: the manifest itself ---
        ("corrupt-v3", "bad-magic", "not a part file"),
        ("corrupt-v3", "bad-header-crc", "header failed checksum"),
        ("corrupt-v3", "bad-body-crc", "body failed checksum"),
        ("corrupt-v3", "future-format", "format version"),
        ("corrupt-v3", "unknown-feature", "feature bits"),
        // --- v3: things the manifest declares that we cannot honour ---
        ("corrupt-v3", "unknown-codec", "codec id 99"),
        (
            "corrupt-v3",
            "unknown-rerank-encoding",
            "rerank encoding id 2",
        ),
        // --- v3: a length no bytes could back (the S1 allocation gate) ---
        ("corrupt-v3", "absurd-length", "refusing to allocate"),
        // --- v3: the stored bytes ---
        ("corrupt-v3", "block-checksum", "block 0 failed checksum"),
        ("corrupt-v3", "truncated-column", "is truncated"),
        ("corrupt-v3", "bad-offsets", "outside"),
        ("corrupt-v3", "mutated-codebook", "hash to its own id"),
        // --- v1: still readable, so still refusable ---
        ("corrupt", "flipped-byte", "checksum"),
        ("corrupt", "truncated-column", "bytes"),
        ("corrupt", "future-format", "format version"),
        ("corrupt", "mutated-codebook", "hash to its own id"),
        ("corrupt", "bad-offsets", "outside"),
    ];

    for (family, dir, expect) in cases {
        let path = repo_root().join("testing/compat").join(family).join(dir);
        assert!(path.exists(), "missing corrupt fixture {family}/{dir}");

        let err = Engine::open(&path)
            .and_then(|e| e.catalog().verify())
            .expect_err(&format!("corrupt fixture `{family}/{dir}` was accepted"));

        let msg = err.to_string();
        assert!(
            matches!(err, PrismError::Corrupt(_)),
            "fixture `{family}/{dir}` produced {err:?}, not a Corrupt error"
        );
        assert!(
            msg.contains(expect),
            "fixture `{family}/{dir}` did not explain itself.\n  wanted: {expect}\n  got:    {msg}"
        );
    }
}

#[test]
fn fsck_condemns_a_bad_part_without_a_catalog_and_reports_every_wound() {
    // The offline validator answers one question about a directory of bytes,
    // needing no engine and no catalog: is this a part, and is it intact?
    let good = repo_root().join("testing/compat/v2");
    let reports = prism_part::fsck::fsck(&good).unwrap();
    assert_eq!(reports.len(), 1);
    assert!(
        reports[0].ok,
        "a healthy part was condemned: {:?}",
        reports[0].findings
    );
    assert!(reports[0].blocks_checked > 0);
    assert_eq!(reports[0].rerank_encoding.as_deref(), Some("float32/exact"));

    let bad = repo_root().join("testing/compat/corrupt-v2/block-checksum");
    let reports = prism_part::fsck::fsck(&bad).unwrap();
    assert!(!reports[0].ok);
    assert!(
        reports[0].findings.iter().any(|f| f.kind == "block"),
        "a damaged block should be reported as a block finding: {:?}",
        reports[0].findings
    );
}

#[test]
fn a_v1_part_is_migrated_forward_by_a_merge_not_by_a_rewrite() {
    // The compatibility promise has two halves. Today's build must *read*
    // yesterday's parts — and it must have a way to move them forward that does
    // not violate immutability. That way is the merge: read the old immutable
    // part, write a new immutable part, swap the catalog. The v1 bytes are never
    // touched, which is why a rollback is still possible afterwards.
    let root = tmp("migrate");
    copy_tree(&repo_root().join("testing/compat/v1"), &root);

    let engine = Engine::open(&root).unwrap();
    let before = engine.snapshot().unwrap();
    let old_parts = before.parts.clone();
    assert!(
        engine.open_parts(&before).unwrap()[0].is_legacy(),
        "the fixture should start out as a v1 part"
    );

    let mut query = q("tool call timed out retrying");
    query.nprobe = 8;
    let hits_before: Vec<String> = engine
        .search(&query)
        .unwrap()
        .hits
        .iter()
        .map(|h| h.event.event_id.clone())
        .collect();

    // No new data: a legacy part is reason enough for a merge to run, because the
    // merge is the migration. Nothing else about the store changes, so the
    // answers must come out bit-for-bit identical.
    let report = engine.merge(6_000).unwrap();
    assert_eq!(report.parts_out, 1);
    assert_eq!(
        report.parts_migrated, 1,
        "the merge did not report a migration"
    );
    assert_eq!(report.duplicates_reconciled, 0);
    assert_eq!(report.rows_in, report.rows_out);

    // Everything is v2 now...
    let after = engine.snapshot().unwrap();
    let parts = engine.open_parts(&after).unwrap();
    assert!(
        parts.iter().all(|p| !p.is_legacy()),
        "the merge did not migrate the v1 part"
    );
    assert_eq!(parts[0].manifest.rerank.describe(), "float32/exact");

    // ...the v1 bytes are still sitting there, untouched and still valid...
    for p in &old_parts {
        let old = PartReader::open(&engine.store.part_dir(p)).unwrap();
        assert!(old.is_legacy());
        old.verify()
            .expect("the v1 part was mutated by the migration");
    }

    // ...and the answers are identical. A migration that changes an answer is not
    // a migration, it is a data-loss incident with good PR.
    let hits_after: Vec<String> = engine
        .search(&query)
        .unwrap()
        .hits
        .iter()
        .map(|h| h.event.event_id.clone())
        .collect();
    assert_eq!(
        hits_before, hits_after,
        "migrating v1 -> v2 changed the answer"
    );

    // And the store still verifies end to end after the format moved under it.
    engine.catalog().verify().unwrap();

    std::fs::remove_dir_all(root).ok();
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.path().is_dir() {
            copy_tree(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

// -------------------------------------------------- the recall tail (S1)

#[test]
fn the_default_nprobe_still_matches_its_committed_provenance() {
    // PRISM.md Part I §5.3: "No magic constants in docs or defaults without
    // benchmark provenance." DEFAULT_NPROBE is derived from a sweep on the golden
    // corpus, and `testing/evidence/nprobe.json` is the receipt. This
    // test is what stops the constant from drifting away from its evidence: if
    // someone edits one without re-deriving the other, CI says so.
    let prov: serde_json::Value = serde_json::from_slice(
        &std::fs::read(repo_root().join("testing/evidence/nprobe.json")).unwrap(),
    )
    .unwrap();

    let chosen = prov["chosen_nprobe"].as_u64().unwrap() as usize;
    assert_eq!(
        chosen,
        prism_types::query::DEFAULT_NPROBE,
        "DEFAULT_NPROBE is {} but the committed sweep chose {chosen}. One of them is stale — \
         re-derive with `prism golden sweep`, do not just edit the constant.",
        prism_types::query::DEFAULT_NPROBE
    );

    // And the receipt must actually justify the choice: the chosen probe count
    // clears the tail floor, and every smaller one does not.
    let floor = prov["p1_floor"].as_f64().unwrap();
    for row in prov["sweep"].as_array().unwrap() {
        let np = row["nprobe"].as_u64().unwrap() as usize;
        let p1 = row["p1_recall"].as_f64().unwrap();
        if np < chosen {
            assert!(
                p1 < floor,
                "nprobe={np} already clears the p1 floor ({p1:.3} >= {floor}), so {chosen} is \
                 not the smallest probe count that does"
            );
        }
        if np == chosen {
            assert!(
                p1 >= floor,
                "the chosen nprobe does not clear its own floor"
            );
        }
    }
}

#[test]
fn cluster_boundary_queries_are_what_expose_the_recall_tail() {
    // The finding this whole sprint's recall work exists for. At nprobe=1, queries
    // aimed at the *middle* of a cluster are answered perfectly — and queries that
    // sit *between* two clusters fail outright, because their true neighbours are
    // split across two centroids and one probe reaches only one of them.
    //
    // A benchmark that only asks easy questions reports a mean of 0.90 and calls
    // it a day. This test insists the hard class exists, and that the default
    // probe count actually fixes it.
    let root = tmp("boundary");
    let engine = Engine::init(
        &root,
        StoreConfig {
            format_version: STORE_VERSION,
            dim: 64,
            nlist: 32,
            pq_m: 8,
            seed: 1234,
            block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        },
    )
    .unwrap();
    let events = tsv::parse(
        &std::fs::read_to_string(repo_root().join("testing/golden/v1/corpus.tsv")).unwrap(),
    )
    .unwrap();
    engine.ingest(events, 1_000).unwrap();

    let golden: oracle::Golden = serde_json::from_slice(
        &std::fs::read(repo_root().join("testing/golden/v1/expected.json")).unwrap(),
    )
    .unwrap();

    let by = |r: &oracle::RecallReport, kind: &str| -> oracle::ClassRecall {
        r.by_kind.iter().find(|c| c.kind == kind).unwrap().clone()
    };

    // One probe: the easy class is perfect, the hard class is not.
    let one = oracle::measure_recall(&engine, &golden, 1, 200, 50).unwrap();
    let topic = by(&one, oracle::KIND_TOPIC);
    let boundary = by(&one, oracle::KIND_BOUNDARY);

    assert_eq!(
        topic.zero_recall_queries, 0,
        "topic queries should be easy even at nprobe=1"
    );
    assert!(
        boundary.zero_recall_queries > 0,
        "no cluster-boundary query failed at nprobe=1 — the golden set is not actually testing \
         the boundary case, and the tail metric is measuring nothing"
    );
    assert!(
        one.mean_recall > 0.85,
        "the mean is supposed to look *reassuring* here; that is the entire point"
    );
    assert_eq!(
        one.p1_recall, 0.0,
        "the tail is supposed to be catastrophic here"
    );

    // The derived default: nobody is left behind.
    let dflt = oracle::measure_recall(
        &engine,
        &golden,
        prism_types::query::DEFAULT_NPROBE,
        200,
        50,
    )
    .unwrap();
    assert_eq!(
        dflt.zero_recall_queries, 0,
        "the default probe count still fails {} queries entirely",
        dflt.zero_recall_queries
    );
    assert!(
        dflt.p1_recall >= 0.8,
        "p1 recall {} is below the floor",
        dflt.p1_recall
    );
    assert!(
        dflt.mean_scan_fraction < 0.25,
        "the default buys its tail by scanning {:.1}% of the data",
        dflt.mean_scan_fraction * 100.0
    );

    std::fs::remove_dir_all(root).ok();
}

// ------------------------------------------------------------------ grouping

#[test]
fn semantic_grouping_returns_real_exemplar_events_and_aggregates() {
    let (engine, root) = store_with(Kind::Uniform, 2000, 7, "group");

    let mut query = q("the agent hit an error");
    query.nprobe = 16;
    query.rerank = 100;
    query.k = 1;
    query.group_k = Some(4);

    let res = engine.search(&query).unwrap();
    let clusters = res.clusters.expect("grouping was requested");
    assert!(!clusters.is_empty());

    let total: usize = clusters.iter().map(|c| c.count).sum();
    assert_eq!(
        total, res.counters.rerank_width,
        "every survivor lands in exactly one group"
    );

    // Groups are ordered biggest-first, and each exemplar is a real event that
    // is really a member of its own group.
    for w in clusters.windows(2) {
        assert!(w[0].count >= w[1].count);
    }
    for c in &clusters {
        assert!(
            c.member_ids.contains(&c.exemplar.event_id),
            "the exemplar must be a member"
        );
        assert!(!c.exemplar.body.is_empty());
        assert!((0.0..=1.0).contains(&c.error_rate));
        assert!(c.avg_cost >= 0.0);
        assert_eq!(c.member_ids.len(), c.count);
    }
    std::fs::remove_dir_all(root).ok();
}

// ------------------------------------------------------------- late + skew

#[test]
fn time_pruning_follows_event_time_not_arrival_order() {
    let root = tmp("late");
    let engine = Engine::init(&root, config(64, 16, 8)).unwrap();
    let events = corpus::generate(Kind::Late, 600, 3);

    let oldest = events.iter().map(|e| e.event_time).min().unwrap();
    engine.ingest(events, 1000).unwrap();

    // A window that only contains the late arrivals.
    let mut query = q("tool call timed out retrying");
    query.time_from = Some(oldest);
    query.time_to = Some(oldest + 1000);
    query.nprobe = 16;

    let res = engine.search(&query).unwrap();
    for h in &res.hits {
        assert!(
            (oldest..=oldest + 1000).contains(&h.event.event_time),
            "a row outside the window came back"
        );
    }
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn tenant_skew_does_not_break_the_filter() {
    let (engine, root) = store_with(Kind::TenantSkew, 2000, 11, "skew");
    let mut query = q("write a python function to parse csv");
    query.tenant = Some("t3".to_string());
    query.nprobe = 32;

    let res = engine.search(&query).unwrap();
    assert!(res.hits.iter().all(|h| h.event.tenant_id == "t3"));
    // The mask ran inside the scan: far fewer rows passed it than were scanned.
    assert!(res.counters.rows_passing_filter < res.counters.rows_scanned_pq);
    std::fs::remove_dir_all(root).ok();
}

// -------------------------------------------------------------- boundaries

#[test]
fn malformed_queries_are_refused_not_guessed_at() {
    let (engine, root) = store_with(Kind::Uniform, 200, 1, "badq");
    let mut bad = q("");
    assert!(engine.search(&bad).is_err());
    bad = q("hello");
    bad.k = 0;
    assert!(engine.search(&bad).is_err());
    bad = q("hello");
    bad.nprobe = 0;
    assert!(engine.search(&bad).is_err());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn an_empty_store_answers_with_nothing_rather_than_failing() {
    let root = tmp("empty");
    let engine = Engine::init(&root, config(64, 16, 8)).unwrap();
    let res = engine.search(&q("anything at all")).unwrap();
    assert!(res.hits.is_empty());
    assert_eq!(res.counters.parts_total, 0);
    std::fs::remove_dir_all(root).ok();
}
