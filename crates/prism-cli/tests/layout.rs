//! **Charter C-4, as a permanent gate: the answer is a function of the data, not of the layout.**
//!
//! > *"any bounded or truncating selection breaks ties on logical row identity (event_id), never
//! > on physical position. Audit all existing bounded structures and bind each to a
//! > layout-variant test. Add layout-variant golden fixtures: one frozen logical corpus,
//! > materialized under >=3 partitionings/batchings; every golden query must answer identically
//! > across all variants. This gate is permanent."*
//!
//! One **frozen logical corpus** (`testing/golden/v1/corpus.tsv`, immutable under charter C-2),
//! materialized four different ways: different time windows, different ingest batchings, before
//! and after a merge. The stores are byte-different on disk and *logically identical*. Every
//! golden query must return byte-identical ordered results from all of them.
//!
//! This exists because the layout is the one thing an engineer can change without meaning to. A
//! merge runs. A window is retuned. A batch size doubles. None of those is a decision about
//! *answers* — and every one of them silently was, until D-033.
//!
//! What the gate has already caught, by class:
//!
//! - **The candidate heap** broke score ties on `(part, row)` (D-033). Recall fell 1.00 → 0.60
//!   under repartitioning, and probing harder did nothing, because the rows were never being
//!   missed — they were being outvoted by their addresses.
//! - **The training sample** was a reservoir keyed on *index into a vector built by reading parts
//!   in catalog order*. Same rows, different layout, **different codebook** — and a codebook is
//!   the meaning of every byte encoded under it. Now keyed on `event_id`.
//! - **Merge duplicate reconciliation** broke `event_time` ties with *"the later part wins"*.
//!   Now the content hash wins, which is a total order on the data.

use prism_engine::corpus::{self, Kind};
use prism_engine::{oracle, tsv, Engine};
use prism_part::partition::PartitionScheme;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::{Event, Query};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-c4-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// One logical corpus. Frozen bytes, charter C-2 — this is not regenerated, it is *read*.
fn frozen_corpus() -> Vec<Event> {
    let text = std::fs::read_to_string(repo_root().join("testing/golden/v1/corpus.tsv"))
        .expect("the frozen golden corpus");
    tsv::parse(&text).expect("the frozen golden corpus parses")
}

fn golden() -> oracle::Golden {
    let bytes = std::fs::read(repo_root().join("testing/golden/v1/expected.json"))
        .expect("the frozen golden expectations");
    serde_json::from_slice(&bytes).expect("golden json")
}

/// A materialization of the same logical corpus: a physical layout.
struct Layout {
    name: &'static str,
    window_ms: i64,
    batch: usize,
    merge: bool,
}

const LAYOUTS: &[Layout] = &[
    // Everything in one window, one ingest: the simplest possible store.
    Layout {
        name: "single-part",
        window_ms: i64::MAX / 4,
        batch: usize::MAX,
        merge: false,
    },
    // A part per day, one ingest.
    Layout {
        name: "daily",
        window_ms: 24 * 60 * 60 * 1000,
        batch: usize::MAX,
        merge: false,
    },
    // A part per day, but ingested in small batches — so a window has SEVERAL parts, published
    // in an order that has nothing to do with event time. This is the layout that broke D-033.
    Layout {
        name: "daily-batched",
        window_ms: 24 * 60 * 60 * 1000,
        batch: 97,
        merge: false,
    },
    // Per-hour partitions, batched, then merged — rows physically moved between parts.
    Layout {
        name: "hourly-merged",
        window_ms: 60 * 60 * 1000,
        batch: 250,
        merge: true,
    },
];

/// **What a bootstrap codebook can and cannot promise.**
///
/// The bootstrap generation is trained on the **first batch**, because on the first batch the
/// first batch is all that exists. So its codebook is a function of *arrival order* — and no
/// amount of C-4 discipline can change that, because you cannot train on data that has not
/// arrived. Ingest the same corpus in one batch or in twenty and you get different codebooks,
/// therefore different PQ codes, therefore different *approximate* answers.
///
/// That is precisely what `provisional: true` means on a bootstrap generation, and precisely
/// what the generation lifecycle exists to resolve: `generation create` trains from a stratified
/// sample of the **whole store**, keyed on `event_id` and stratified by *tenant* — all logical
/// properties — so the settled codebook is a function of the data and nothing else.
///
/// So the gate is in two halves, and both are strict:
///
/// 1. The **exact** path — which uses no codebook at all — is layout-invariant **always**, even
///    on a provisional store. That is C-4 on the data path, with nothing to hide behind.
/// 2. The **approximate** path is layout-invariant **once the store is settled**, which is the
///    state any store that has run a migration is in, and the state the lifecycle exists to
///    reach.
///
/// Anything weaker would be a gate that passes by being vague. Anything stronger would be a
/// promise the arrow of time does not allow.
fn settle(engine: &Engine) {
    match engine.generation_create(None, 9_000_000) {
        Ok(g) => {
            engine
                .generation_promote(&g.generation_id, 9_000_001)
                .unwrap();
            engine
                .generation_migrate(&g.generation_id, None, 9_000_002)
                .unwrap();
        }
        // Already settled, and it says so honestly. A store ingested in ONE batch trained its
        // bootstrap on every row it has -- so the whole-store training reproduces a
        // byte-identical codebook, the content address is the same, and it is not a new
        // generation. Refusing to migrate a store onto the codebook it is already using is the
        // correct behaviour, not an obstacle to be worked around.
        Err(e) if e.to_string().contains("byte-identical") => {}
        Err(e) => panic!("settling failed: {e}"),
    }
}

fn build(l: &Layout, events: &[Event]) -> (Engine, PathBuf) {
    let root = tmp(l.name);
    let engine = Engine::init(
        &root,
        StoreConfig {
            format_version: STORE_VERSION,
            dim: 64,
            nlist: 32,
            pq_m: 8,
            seed: 1234,
            // ONE restart, not the shipping five. Layout-invariance must hold for *any*
            // deterministic training configuration -- it is a statement about whether the
            // codebook depends on where the rows sit, not about how good the codebook is. The
            // shipping restart count is what `testing/evidence/kmeans-restarts.json` is for.
            // Training dominates this test's cost and it runs four stores through a full
            // migration; paying 5x for a property that does not depend on the 5x is a tax on
            // every CI run, especially the debug one.
            kmeans_restarts: 1,
            block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
            partitions: PartitionScheme {
                buckets: 16,
                time_window_ms: l.window_ms,
                dedicated: Default::default(),
            },
            promote: Vec::new(),
        },
    )
    .unwrap();

    let batch = l.batch.min(events.len()).max(1);
    for (i, chunk) in events.chunks(batch).enumerate() {
        engine
            .ingest(chunk.to_vec(), 1_760_000_000_000 + i as i64)
            .unwrap();
    }
    if l.merge {
        engine.merge(1_760_000_500_000).unwrap();
    }
    (engine, root)
}

/// **The permanent gate.**
///
/// Four physical materializations of one frozen logical corpus. Every golden query — every
/// topic query, every deliberately-nasty cluster-boundary query — must come back byte-identical
/// from all four, ids *and* scores, in order.
#[test]
fn every_golden_query_answers_identically_under_every_layout() {
    let events = frozen_corpus();
    let g = golden();

    /// (layout, part count, one ordered answer per golden query)
    type Answers = (&'static str, usize, Vec<Vec<(String, f32)>>);
    let mut answers: Vec<Answers> = Vec::new();

    for l in LAYOUTS {
        let (engine, _root) = build(l, &events);
        let parts = engine.snapshot().unwrap().parts.len();
        settle(&engine);

        let mut per_query = Vec::new();
        for exp in &g.expectations {
            let q = exp.query.to_query();
            let hits = engine.search(&q).unwrap().hits;
            per_query.push(
                hits.iter()
                    .map(|h| (h.event.event_id.clone(), h.score))
                    .collect::<Vec<_>>(),
            );
        }
        answers.push((l.name, parts, per_query));
    }

    // If every layout produced the same physical shape, this test is testing nothing.
    let shapes: std::collections::BTreeSet<usize> = answers.iter().map(|a| a.1).collect();
    assert!(
        shapes.len() >= 3,
        "the layouts produced only {} distinct part counts ({:?}); a layout-variant gate whose \
         variants are not actually different is a gate that cannot fail",
        shapes.len(),
        shapes
    );

    let (base_name, base_parts, base) = &answers[0];
    for (name, parts, rows) in &answers[1..] {
        for (qi, (b, r)) in base.iter().zip(rows).enumerate() {
            assert_eq!(
                b,
                r,
                "golden query #{qi} (`{}`) answered DIFFERENTLY under two layouts of the SAME \
                 logical corpus.\n\n  {base_name} ({base_parts} parts): {:?}\n  {name} ({parts} \
                 parts): {:?}\n\nThe rows are identical. Only where they are stored differs. An \
                 answer that depends on that is not an answer -- see charter C-4 and D-033.",
                g.expectations[qi].query.text,
                b.iter().take(5).collect::<Vec<_>>(),
                r.iter().take(5).collect::<Vec<_>>(),
            );
        }
    }
}

/// **A codebook is the meaning of every byte encoded under it — so it must not depend on the
/// layout either.**
///
/// This is the C-4 property applied to the thing S5 is built out of. The training sample used to
/// be a reservoir keyed on *position*, over vectors read from parts in catalog order. Two stores
/// with identical rows therefore trained *different codebooks*: different centroids, different
/// PQ codes, a different meaning for every byte. Nothing would have told you.
#[test]
fn the_same_rows_train_the_same_codebook_under_every_layout() {
    let events = frozen_corpus();

    let mut books: Vec<(&str, String)> = Vec::new();
    for l in LAYOUTS {
        let (engine, _root) = build(l, &events);

        // The BOOTSTRAP codebook is trained on the first batch and is therefore a function of
        // arrival order -- unavoidably, and that is what `provisional` means. Settle the store,
        // which is what the lifecycle is for: `create` trains from a stratified sample of the
        // whole store, keyed on event_id and stratified by TENANT. Every one of those is a
        // logical property of the data, so the settled codebook is a function of the data.
        settle(&engine);

        let snap = engine.snapshot().unwrap();
        let gid = snap.active_generation.clone().unwrap();
        let g = engine.catalog().get_generation(&gid).unwrap();

        // The generation id IS the content hash of the codebooks. If the ids match, the coarse
        // centroids and every PQ sub-quantizer are byte-identical.
        books.push((l.name, g.generation_id.clone()));
    }

    let (first_name, first) = &books[0];
    for (name, id) in &books[1..] {
        assert_eq!(
            first,
            id,
            "the same {} rows trained a DIFFERENT codebook under layout `{name}` than under \
             `{first_name}`.\n\nA generation id is the content hash of its codebooks, so this \
             means the centroids moved. A codebook defines what every PQ byte in the store MEANS \
             -- if it depends on how the rows happened to be batched, then so does the meaning of \
             the data. Charter C-4: the training sample is keyed on event_id, never on position.",
            events.len(),
        );
    }
}

/// The bounded selections in the *aggregate* path, which nobody looks at.
///
/// `GROUP BY` clusters the rerank survivors and picks an **exemplar** for each group — "the most
/// central actual event, because nobody can read an average". That is a bounded selection: it
/// picks one row out of many, and ties in centrality are entirely possible when bodies repeat.
/// If it broke ties on physical position, the exemplar a customer is shown would change when the
/// store was merged, and the group they clicked into would be a different group.
#[test]
fn cluster_exemplars_and_membership_do_not_depend_on_the_layout() {
    let events = frozen_corpus();

    /// (layout, per-cluster: count, exemplar id, member ids)
    type Clusters = (&'static str, Vec<(usize, String, Vec<String>)>);
    let mut runs: Vec<Clusters> = Vec::new();
    for l in LAYOUTS {
        let (engine, _root) = build(l, &events);
        settle(&engine);
        let q = Query {
            text: "the tool call timed out".into(),
            k: 20,
            group_k: Some(4),
            ..Default::default()
        };
        let clusters = engine.search(&q).unwrap().clusters.unwrap();
        runs.push((
            l.name,
            clusters
                .iter()
                .map(|c| (c.count, c.exemplar.event_id.clone(), c.member_ids.clone()))
                .collect(),
        ));
    }

    let (base_name, base) = &runs[0];
    for (name, r) in &runs[1..] {
        assert_eq!(
            base, r,
            "semantic grouping produced different clusters under `{name}` than under \
             `{base_name}`, for identical rows. The exemplar is what a human actually reads; if \
             it moves when the store is merged, the product moved."
        );
    }
}

/// Merge duplicate reconciliation used to break `event_time` ties with *"the later part wins"* —
/// a tie broken on physical position, which charter C-4 forbids outright.
///
/// Two copies of one event, same id, same `event_time`, different bodies. Which one survives a
/// merge must be a function of their *content*, not of which part each happened to land in.
#[test]
fn a_merge_reconciles_duplicates_the_same_way_regardless_of_which_part_they_landed_in() {
    let mut a = corpus::generate(Kind::Zipf, 40, 3);
    for (i, e) in a.iter_mut().enumerate() {
        e.tenant_id = "alpha".into();
        e.event_id = format!("e{i:04}");
        e.event_time = 1_760_000_000_000;
    }

    // The same duplicate pair, offered to the store in the two possible orders. Under the old
    // rule ("the later part wins") these two stores would disagree about which body survived.
    let mut dup_x = a[0].clone();
    dup_x.body = "the tool call timed out after 30s".into();
    let mut dup_y = a[0].clone();
    dup_y.body = "connection reset by peer".into();

    let survivors: Vec<String> = [(dup_x.clone(), dup_y.clone()), (dup_y, dup_x)]
        .iter()
        .map(|(first, second)| {
            let root = tmp("dedup");
            let engine = Engine::init(
                &root,
                StoreConfig {
                    format_version: STORE_VERSION,
                    dim: 64,
                    nlist: 8,
                    pq_m: 8,
                    seed: 5,
                    // ONE restart, not the shipping five. Layout-invariance must hold for *any*
                    // deterministic training configuration -- it is a statement about whether the
                    // codebook depends on where the rows sit, not about how good the codebook is. The
                    // shipping restart count is what `testing/evidence/kmeans-restarts.json` is for.
                    // Training dominates this test's cost and it runs four stores through a full
                    // migration; paying 5x for a property that does not depend on the 5x is a tax on
                    // every CI run, especially the debug one.
                    kmeans_restarts: 1,
                    block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
                    partitions: PartitionScheme::default(),
                    promote: Vec::new(),
                },
            )
            .unwrap();
            // Two ingests => two parts in the same partition, in a known order.
            let mut batch = a[1..].to_vec();
            batch.push(first.clone());
            engine.ingest(batch, 1_760_000_000_000).unwrap();
            engine
                .ingest(vec![second.clone()], 1_760_000_001_000)
                .unwrap();
            engine.merge(1_760_000_002_000).unwrap();

            let q = Query {
                text: "anything".into(),
                k: 100,
                ..Default::default()
            };
            let hits = engine.exact_search(&q).unwrap();
            hits.into_iter()
                .find(|h| h.event.event_id == "e0000")
                .expect("the surviving copy")
                .event
                .body
        })
        .collect();

    assert_eq!(
        survivors[0], survivors[1],
        "the same duplicate pair reconciled to a DIFFERENT survivor depending on which part each \
         copy landed in.\n\n  offered x-then-y: {:?}\n  offered y-then-x: {:?}\n\nLast-write-wins \
         by event_time is the policy; when event_times tie, the winner must be decided by the \
         CONTENT, not by an address (charter C-4).",
        survivors[0], survivors[1]
    );
}

/// **The exact path uses no codebook at all — so it is layout-invariant *always*, provisional
/// store or not.**
///
/// This is C-4 on the data path with nothing to hide behind. Approximate answers can hide a
/// layout dependence inside a codebook and blame the approximation; the exact oracle cannot. If
/// two stores holding identical rows disagree here, the disagreement is in the *selection*, the
/// *ordering*, or the *reconciliation* — and every one of those is a bug of the class D-033
/// belonged to.
#[test]
fn the_exact_oracle_is_layout_invariant_even_before_the_store_is_settled() {
    let events = frozen_corpus();
    let g = golden();

    /// (layout, one ordered answer per golden query)
    type ExactAnswers = (&'static str, Vec<Vec<(String, f32)>>);
    let mut answers: Vec<ExactAnswers> = Vec::new();

    for l in LAYOUTS {
        // NOT settled. These stores have provisional codebooks that genuinely differ from each
        // other -- and it must not matter, because the exact path never reads one.
        let (engine, _root) = build(l, &events);

        let mut per_query = Vec::new();
        for exp in &g.expectations {
            let hits = engine.exact_search(&exp.query.to_query()).unwrap();
            per_query.push(
                hits.iter()
                    .map(|h| (h.event.event_id.clone(), h.score))
                    .collect::<Vec<_>>(),
            );
        }
        answers.push((l.name, per_query));
    }

    let (base_name, base) = &answers[0];
    for (name, rows) in &answers[1..] {
        for (qi, (b, r)) in base.iter().zip(rows).enumerate() {
            assert_eq!(
                b, r,
                "the EXACT answer to golden query #{qi} (`{}`) changed with the layout: \
                 `{base_name}` vs `{name}`. The exact path does not use a codebook, so this is \
                 not approximation error -- it is the store answering a question about where its \
                 rows are stored.",
                g.expectations[qi].query.text
            );
        }
    }
}
