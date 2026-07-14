//! S0 integration tests.
//!
//! Each test names the invariant it defends. If you weaken one to make it pass,
//! you have not fixed anything — you have removed the thing that would have told
//! you.

use prism_engine::corpus::{self, Kind};
use prism_engine::model::HashModelPlane;
use prism_engine::{oracle, tsv, Engine};
use prism_part::part::PartReader;
use prism_part::store::{StoreConfig, FORMAT_VERSION};
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
        format_version: FORMAT_VERSION,
        dim,
        nlist,
        pq_m,
        seed: 42,
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
                format_version: FORMAT_VERSION,
                dim: 64,
                nlist: 32,
                pq_m: 8,
                seed: 1234,
            },
        )
        .unwrap_or(engine)
    };

    let corpus_path = repo_root().join("testing/golden/corpus.tsv");
    let events = tsv::parse(&std::fs::read_to_string(&corpus_path).unwrap()).unwrap();
    engine.ingest(events, 1_000).unwrap();

    let golden: oracle::Golden = serde_json::from_slice(
        &std::fs::read(repo_root().join("testing/golden/expected.json")).unwrap(),
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
fn every_corrupt_fixture_is_rejected_with_a_specific_error() {
    let corrupt = repo_root().join("testing/compat/corrupt");

    let cases = [
        ("flipped-byte", "checksum"),
        ("truncated-column", "bytes"),
        ("future-format", "format version"),
        ("mutated-codebook", "hash to its own id"),
        ("bad-offsets", "outside"),
    ];

    for (dir, expect) in cases {
        let path = corrupt.join(dir);
        assert!(path.exists(), "missing corrupt fixture {dir}");

        // Whatever the failure is, it must surface as a specific Corrupt error,
        // and the message must say which byte lied.
        let err = Engine::open(&path)
            .and_then(|e| e.catalog().verify())
            .expect_err(&format!("corrupt fixture `{dir}` was accepted"));

        let msg = err.to_string();
        assert!(
            matches!(err, PrismError::Corrupt(_)),
            "fixture `{dir}` produced {err:?}, not a Corrupt error"
        );
        assert!(
            msg.contains(expect),
            "fixture `{dir}` error did not explain itself: {msg}"
        );
    }
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
