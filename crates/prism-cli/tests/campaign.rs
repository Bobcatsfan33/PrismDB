//! The randomized kill/reopen campaign — the S1 acceptance gate.
//!
//! > *"10,000 randomized kill/reopen runs yield old-or-new snapshot, never
//! > hybrid."* — PRISM.md, Part IV, S1
//!
//! The fault *matrix* (`faults.rs`) walks each durability boundary a few times
//! and runs on every commit. This is the long version, and it is a different kind
//! of test: it does not know where the bug is, it just kills the writer at a
//! randomly chosen boundary over and over, on a store that keeps accumulating
//! real history, and insists that after every single death the store is one of
//! exactly two things — the old snapshot, or the new one.
//!
//! Nothing in between. Not "mostly fine". Not "recoverable with a repair tool".
//!
//! Ignored by default because it takes minutes. Run it with:
//!
//! ```text
//! PRISM_CAMPAIGN_RUNS=10000 cargo test --release -p prism-cli --test campaign -- --ignored --nocapture
//! ```

use prism_engine::corpus::{self, Kind};
use prism_engine::{tsv, Engine};
use prism_part::faults::KILL_POINTS;
use prism_types::rng::Rng;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

const BATCH: usize = 100;
/// Rebuild the store periodically so a single run's part count — and therefore
/// the cost of verifying it — stays bounded. Each store still lives through
/// dozens of crashes before it is retired.
const STORE_LIFETIME: usize = 100;

fn prism() -> &'static str {
    env!("CARGO_BIN_EXE_prism")
}

fn run(args: &[&str], fault: Option<&str>) -> std::process::Output {
    let mut cmd = Command::new(prism());
    cmd.args(args);
    match fault {
        Some(f) => cmd.env("PRISM_FAULT", f),
        None => cmd.env_remove("PRISM_FAULT"),
    };
    cmd.output().expect("failed to run prism")
}

/// Open the store and report the only two things that matter: which snapshot is
/// live, and how many rows are visible. Panics if the store will not open or
/// does not verify — which is itself the failure the campaign hunts for.
fn observe(root: &Path) -> (String, usize) {
    let engine = Engine::open(root).expect("a crashed store must still open");
    engine
        .catalog()
        .verify()
        .expect("the live snapshot names a part that is not intact");
    let snap = engine.snapshot().unwrap();
    let rows = engine
        .open_parts(&snap)
        .unwrap()
        .iter()
        .map(|r| r.manifest.row_count)
        .sum();
    (snap.snapshot_id, rows)
}

fn fresh_store(dir: &Path, i: usize) -> (PathBuf, String, usize) {
    let root = dir.join(format!("store{i}"));
    let _ = std::fs::remove_dir_all(&root);
    let out = run(
        &[
            "init",
            "--path",
            root.to_str().unwrap(),
            "--dim",
            "16",
            "--nlist",
            "4",
            "--pq-m",
            "4",
        ],
        None,
    );
    assert!(out.status.success(), "init failed");

    let seed = dir.join("seed.tsv");
    let out = run(
        &[
            "ingest",
            "--path",
            root.to_str().unwrap(),
            "--file",
            seed.to_str().unwrap(),
        ],
        None,
    );
    assert!(out.status.success(), "seed ingest failed");

    let (snap, rows) = observe(&root);
    (root, snap, rows)
}

#[test]
#[ignore = "long-running; the S1 gate. PRISM_CAMPAIGN_RUNS=10000"]
fn randomized_kill_reopen_campaign_never_yields_a_hybrid_snapshot() {
    let runs: usize = std::env::var("PRISM_CAMPAIGN_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);

    let dir = std::env::temp_dir().join(format!("prism-campaign-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Two corpora with disjoint ids, so a commit is unambiguously +BATCH rows.
    let mk = |name: &str, seed: u64, tag: &str| {
        let events: Vec<_> = corpus::generate(Kind::Uniform, BATCH, seed)
            .into_iter()
            .map(|mut e| {
                e.event_id = format!("{tag}-{}", e.event_id);
                e
            })
            .collect();
        std::fs::write(dir.join(name), tsv::write(&events)).unwrap();
    };
    mk("seed.tsv", 1, "s");

    let mut rng = Rng::new(0xC0FFEE);
    let started = Instant::now();

    let (mut root, mut snapshot, mut rows) = fresh_store(&dir, 0);
    let mut committed = 0usize;
    let mut rolled_back = 0usize;
    let mut by_point: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();

    for i in 1..=runs {
        if i % STORE_LIFETIME == 0 {
            let (r, s, n) = fresh_store(&dir, i);
            root = r;
            snapshot = s;
            rows = n;
        }

        // Each run gets its own batch, so an id can never collide with a batch a
        // previous crash half-wrote.
        mk("batch.tsv", 100 + i as u64, &format!("b{i}"));

        let point = KILL_POINTS[rng.below(KILL_POINTS.len())];
        *by_point.entry(point).or_default() += 1;

        // Drive whichever operation actually reaches that kill point.
        let out = if point.starts_with("gc.") {
            run(
                &["gc", "--path", root.to_str().unwrap(), "--retain", "1"],
                Some(point),
            )
        } else if point.starts_with("merge.") {
            // A merge needs something to merge; if there is only one part this is
            // a no-op and the kill point is simply not reached, which is fine.
            run(&["merge", "--path", root.to_str().unwrap()], Some(point))
        } else {
            let f = dir.join("batch.tsv");
            run(
                &[
                    "ingest",
                    "--path",
                    root.to_str().unwrap(),
                    "--file",
                    f.to_str().unwrap(),
                ],
                Some(point),
            )
        };

        // The kill may not have fired (a merge with nothing to merge, say). Either
        // way, the store must be consistent afterwards -- that is the invariant,
        // and it does not care whether we actually managed to kill anything.
        let _ = out;

        let (new_snapshot, new_rows) = observe(&root);

        let legal = if point.starts_with("gc.") || point.starts_with("merge.") {
            // Reclamation and compaction never change what is visible.
            new_rows == rows
        } else {
            new_rows == rows || new_rows == rows + BATCH
        };

        assert!(
            legal,
            "run {i} at `{point}`: HYBRID STATE — {rows} rows before, {new_rows} after \
             (snapshot {snapshot} -> {new_snapshot})"
        );

        if new_rows != rows || new_snapshot != snapshot {
            committed += 1;
        } else {
            rolled_back += 1;
        }
        snapshot = new_snapshot;
        rows = new_rows;

        if i % 500 == 0 {
            println!(
                "  {i}/{runs}  ({:.0}s)  committed={committed} rolled_back={rolled_back}",
                started.elapsed().as_secs_f64()
            );
        }
    }

    println!(
        "\ncampaign: {runs} runs in {:.0}s",
        started.elapsed().as_secs_f64()
    );
    println!("  crashes that committed:    {committed}");
    println!("  crashes that rolled back:  {rolled_back}");
    println!("  kill points exercised:");
    for (p, n) in &by_point {
        println!("    {n:>6}  {p}");
    }
    println!("\n  hybrid snapshots: 0");

    // Every kill point must actually have been hit, or the campaign was not
    // random over the thing it claims to be random over.
    for p in KILL_POINTS {
        assert!(
            by_point.contains_key(p),
            "kill point `{p}` was never selected in {runs} runs"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}
