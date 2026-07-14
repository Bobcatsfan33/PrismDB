//! The fault-injection matrix — permanent artifact #3 (docs/PRISM.md, Part II §7.4).
//!
//! For every durability boundary in the write path, we kill a real `prism`
//! process at that exact point (SIGABRT — no destructors, no flushes, no tidying
//! up; that is what a crash is) and then assert the same four things:
//!
//!   1. the store still opens;
//!   2. the live snapshot is the old one or the new one, **never a hybrid**;
//!   3. every part the live snapshot names is still checksum- and
//!      structure-valid;
//!   4. the store still accepts writes and still answers queries.
//!
//! That is the whole promise of "publication is one atomic rename". A crash
//! anywhere before the rename leaves an *orphan* — bytes on disk that no
//! snapshot names, that no reader can see, and that GC (and only GC) will
//! reclaim later. A crash after it leaves the new snapshot, complete.
//!
//! S0 walks the matrix. The 10,000-run randomized campaign is the S1 gate; run
//! it with `testing/faults/campaign.sh`.

use prism_engine::corpus::{self, Kind};
use prism_engine::{tsv, Engine};
use prism_part::faults::KILL_POINTS;
use prism_types::Query;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-fault-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn prism() -> &'static str {
    env!("CARGO_BIN_EXE_prism")
}

/// The state a healthy store is in, from the outside.
struct Health {
    snapshot: String,
    rows: usize,
}

/// Open the store and assert every consistency invariant we can observe.
/// Returns what we found, so the caller can assert on *which* of the two legal
/// states we landed in.
fn assert_healthy(root: &Path) -> Health {
    let engine = Engine::open(root).expect("a crashed store must still open");

    // Invariant 2: the live snapshot references only durable, valid parts.
    // `verify` checksums every byte AND decodes every structure.
    let report = engine
        .catalog()
        .verify()
        .expect("the live snapshot names a part that is not intact");

    let snap = engine.snapshot().unwrap();
    let rows: usize = engine
        .open_parts(&snap)
        .unwrap()
        .iter()
        .map(|r| r.manifest.row_count)
        .sum();

    assert_eq!(report.snapshot_id, snap.snapshot_id);

    Health {
        snapshot: snap.snapshot_id,
        rows,
    }
}

fn write_corpus(dir: &Path, name: &str, kind: Kind, rows: usize, seed: u64, tag: &str) -> PathBuf {
    let events: Vec<_> = corpus::generate(kind, rows, seed)
        .into_iter()
        .map(|mut e| {
            e.event_id = format!("{tag}-{}", e.event_id);
            e
        })
        .collect();
    let path = dir.join(name);
    std::fs::write(&path, tsv::write(&events)).unwrap();
    path
}

fn run(args: &[&str], fault: Option<&str>) -> std::process::Output {
    let mut cmd = Command::new(prism());
    cmd.args(args);
    match fault {
        Some(f) => {
            cmd.env("PRISM_FAULT", f);
        }
        None => {
            cmd.env_remove("PRISM_FAULT");
        }
    }
    cmd.output().expect("failed to run prism")
}

fn died_abnormally(out: &std::process::Output) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        // SIGABRT. Not a clean exit, not a handled error: a crash.
        return out.status.signal() == Some(6);
    }
    #[allow(unreachable_code)]
    {
        !out.status.success()
    }
}

/// Set up a store with one committed batch. This is the "old snapshot" that any
/// crash must be able to fall back to.
fn seeded_store(tag: &str) -> (PathBuf, PathBuf, String, usize) {
    let dir = tmp(tag);
    let root = dir.join("store");
    let first = write_corpus(&dir, "first.tsv", Kind::Uniform, 400, 1, "a");

    let out = run(
        &[
            "init",
            "--path",
            root.to_str().unwrap(),
            "--dim",
            "32",
            "--nlist",
            "8",
            "--pq-m",
            "4",
        ],
        None,
    );
    assert!(out.status.success(), "init failed");
    let out = run(
        &[
            "ingest",
            "--path",
            root.to_str().unwrap(),
            "--file",
            first.to_str().unwrap(),
        ],
        None,
    );
    assert!(out.status.success(), "seed ingest failed");

    let h = assert_healthy(&root);
    (dir, root, h.snapshot, h.rows)
}

#[test]
fn killing_the_writer_at_every_ingest_boundary_leaves_old_or_new_never_hybrid() {
    let ingest_points = [
        "part.after_write_before_fsync",
        "part.after_fsync_before_rename",
        "part.after_rename_before_snapshot",
        "snapshot.after_write_before_current",
        "current.after_rename",
    ];

    for point in ingest_points {
        // A few repeats per point: a crash-consistency bug that only shows up
        // sometimes is still a crash-consistency bug.
        for iteration in 0..3 {
            let (dir, root, old_snapshot, old_rows) = seeded_store("ingest");
            let second = write_corpus(&dir, "second.tsv", Kind::Uniform, 400, 2, "b");

            let out = run(
                &[
                    "ingest",
                    "--path",
                    root.to_str().unwrap(),
                    "--file",
                    second.to_str().unwrap(),
                ],
                Some(point),
            );
            assert!(
                died_abnormally(&out),
                "kill point `{point}` did not actually kill the process \
                 (status {:?}); the fault is not wired up and this test proves nothing",
                out.status
            );

            let h = assert_healthy(&root);

            // The two legal outcomes, and nothing in between.
            let committed = h.rows == old_rows + 400 && h.snapshot != old_snapshot;
            let rolled_back = h.rows == old_rows && h.snapshot == old_snapshot;
            assert!(
                committed || rolled_back,
                "`{point}` iteration {iteration} left a hybrid state: \
                 snapshot {} with {} rows (old was {old_snapshot} with {old_rows})",
                h.snapshot,
                h.rows
            );

            // Only the very last kill point can possibly have committed: every
            // earlier one dies before CURRENT is swapped.
            if point != "current.after_rename" {
                assert!(
                    rolled_back,
                    "`{point}` committed data despite dying before CURRENT was swapped"
                );
            }

            // And the store is not merely readable, it is *usable*: it takes new
            // writes and answers queries.
            let third = write_corpus(&dir, "third.tsv", Kind::Uniform, 100, 3, "c");
            let out = run(
                &[
                    "ingest",
                    "--path",
                    root.to_str().unwrap(),
                    "--file",
                    third.to_str().unwrap(),
                ],
                None,
            );
            assert!(
                out.status.success(),
                "the store would not accept a write after a crash at `{point}`: {}",
                String::from_utf8_lossy(&out.stderr)
            );

            let after = assert_healthy(&root);
            assert_eq!(after.rows, h.rows + 100);

            let engine = Engine::open(&root).unwrap();
            let res = engine
                .search(&Query {
                    text: "tool call timed out retrying".into(),
                    nprobe: 8,
                    ..Default::default()
                })
                .unwrap();
            assert!(!res.hits.is_empty());

            std::fs::remove_dir_all(&dir).ok();
        }
    }
}

#[test]
fn killing_a_merge_before_it_commits_changes_nothing() {
    let (dir, root, _, _) = seeded_store("merge");
    let second = write_corpus(&dir, "second.tsv", Kind::Uniform, 400, 2, "b");
    run(
        &[
            "ingest",
            "--path",
            root.to_str().unwrap(),
            "--file",
            second.to_str().unwrap(),
        ],
        None,
    );

    let before = assert_healthy(&root);
    let engine = Engine::open(&root).unwrap();
    let parts_before = engine.snapshot().unwrap().parts;
    assert_eq!(parts_before.len(), 2);

    let out = run(
        &["merge", "--path", root.to_str().unwrap()],
        Some("merge.after_part_before_commit"),
    );
    assert!(died_abnormally(&out), "the merge kill point did not fire");

    // The merge wrote a whole new part and then died. That part is an orphan:
    // no snapshot names it, so no reader can see it, and the pre-merge parts are
    // exactly as they were.
    let after = assert_healthy(&root);
    assert_eq!(
        after.snapshot, before.snapshot,
        "a killed merge advanced the catalog"
    );
    assert_eq!(after.rows, before.rows);

    let engine = Engine::open(&root).unwrap();
    assert_eq!(engine.snapshot().unwrap().parts, parts_before);

    // Retrying the merge works, and now it commits.
    let out = run(&["merge", "--path", root.to_str().unwrap()], None);
    assert!(out.status.success());
    let merged = assert_healthy(&root);
    assert_eq!(merged.rows, before.rows);
    assert_ne!(merged.snapshot, before.snapshot);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn killing_gc_midway_never_takes_a_live_part_with_it() {
    // GC is the one operation that deletes. If it dies halfway through, the
    // things it had not yet deleted must still be there, and the things it did
    // delete must be things no retained snapshot needed. A half-run GC is
    // allowed to leave garbage; it is not allowed to leave a hole.
    let (dir, root, _, _) = seeded_store("gc");
    let second = write_corpus(&dir, "second.tsv", Kind::Uniform, 400, 2, "b");
    run(
        &[
            "ingest",
            "--path",
            root.to_str().unwrap(),
            "--file",
            second.to_str().unwrap(),
        ],
        None,
    );
    run(&["merge", "--path", root.to_str().unwrap()], None);

    let before = assert_healthy(&root);
    let engine = Engine::open(&root).unwrap();
    let live: Vec<String> = engine.snapshot().unwrap().parts;

    let out = run(
        &["gc", "--path", root.to_str().unwrap(), "--retain", "1"],
        Some("gc.after_first_unlink"),
    );
    assert!(died_abnormally(&out), "the gc kill point did not fire");

    let after = assert_healthy(&root);
    assert_eq!(after.snapshot, before.snapshot);
    assert_eq!(after.rows, before.rows);

    let engine = Engine::open(&root).unwrap();
    for p in &live {
        assert!(
            engine.store.part_dir(p).exists(),
            "a half-finished gc deleted live part {p}"
        );
    }

    // Finishing the job is safe.
    let out = run(
        &["gc", "--path", root.to_str().unwrap(), "--retain", "1"],
        None,
    );
    assert!(out.status.success());
    assert_eq!(assert_healthy(&root).rows, before.rows);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn orphans_left_by_a_crash_are_invisible_to_readers_and_reclaimable_by_gc() {
    let (dir, root, old_snapshot, old_rows) = seeded_store("orphan");
    let second = write_corpus(&dir, "second.tsv", Kind::Uniform, 400, 2, "b");

    // Die after the part is durable and renamed in, but before any snapshot
    // names it. The bytes are on disk, complete and valid -- and invisible.
    let out = run(
        &[
            "ingest",
            "--path",
            root.to_str().unwrap(),
            "--file",
            second.to_str().unwrap(),
        ],
        Some("part.after_rename_before_snapshot"),
    );
    assert!(died_abnormally(&out));

    let h = assert_healthy(&root);
    assert_eq!(h.snapshot, old_snapshot);
    assert_eq!(h.rows, old_rows, "an uncommitted part must not be visible");

    // It really is on disk...
    let on_disk = std::fs::read_dir(root.join("parts")).unwrap().count();
    assert_eq!(on_disk, 2, "expected the orphan part to be sitting there");

    // ...and GC is what removes it. Not the reader, not the next write.
    let out = run(
        &["gc", "--path", root.to_str().unwrap(), "--retain", "1"],
        None,
    );
    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        report["removed_parts"].as_array().unwrap().len(),
        1,
        "gc should have reclaimed exactly the orphan"
    );

    let h = assert_healthy(&root);
    assert_eq!(h.rows, old_rows);
    assert_eq!(std::fs::read_dir(root.join("parts")).unwrap().count(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn every_declared_kill_point_is_reachable() {
    // A kill point nobody ever fires is a kill point that is lying about being
    // tested. Every point in KILL_POINTS must be exercised by some test above;
    // this asserts the list and the coverage have not drifted apart.
    let covered = [
        "part.after_write_before_fsync",
        "part.after_fsync_before_rename",
        "part.after_rename_before_snapshot",
        "snapshot.after_write_before_current",
        "current.after_rename",
        "gc.after_first_unlink",
        "merge.after_part_before_commit",
    ];
    for p in KILL_POINTS {
        assert!(
            covered.contains(p),
            "kill point `{p}` is declared but no fault test drives it"
        );
    }
    assert_eq!(covered.len(), KILL_POINTS.len());
}
