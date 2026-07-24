//! **S12 increment 3, item 2 — write-path fencing, proved by the zombie test** ([D-076](../../../docs/DECISIONS.md)).
//!
//! One node owns a shard's write path at a time, by a monotonic ownership **epoch** recorded in the
//! object store (`catalog/OWNER-<epoch>`, create-only CAS). A writer acquires the next epoch on
//! start; the catalog commit **fences on the write path** — it refuses, by name, to publish under an
//! epoch a later acquisition has superseded.
//!
//! The gate is the zombie: pause a real `prism` writer **mid-publication** (after its parts are
//! written, before the commit), let a second `prism` process — the restart — acquire a higher epoch
//! and publish, then resume the zombie. Its in-flight publish must fail with the **named** fencing
//! refusal, not commit: the store holds the restart's data, `verify` passes (no torn catalog), and
//! the row count proves the zombie's batch never landed (no duplicate parts).
//!
//! This is single-node ownership done properly (D-076). Cross-node failover — a *different* node
//! taking a dead node's shard — is a named deferral (it needs a remote-durable admission log so
//! D-068's ack contract is not silently weakened), not something improvised under a chaos harness.

use prism_engine::corpus::{self, Kind};
use prism_engine::{tsv, Engine};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-fence-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn prism() -> &'static str {
    env!("CARGO_BIN_EXE_prism")
}

/// A corpus TSV the CLI can ingest, its event ids tagged so batches are distinguishable.
fn write_corpus(dir: &Path, name: &str, rows: usize, seed: u64, tag: &str) -> PathBuf {
    let events: Vec<_> = corpus::generate(Kind::Uniform, rows, seed)
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

fn run(args: &[&str]) -> std::process::Output {
    Command::new(prism())
        .args(args)
        .env_remove("PRISM_FAULT_PAUSE")
        .output()
        .expect("failed to run prism")
}

fn wait_for(path: &Path, timeout: Duration) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < timeout,
            "timed out waiting for `{}` — the writer never reached the pause point",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn rows(root: &Path) -> usize {
    let engine = Engine::open(root).unwrap();
    let snap = engine.snapshot().unwrap();
    engine
        .open_parts(&snap)
        .unwrap()
        .iter()
        .map(|r| r.manifest.row_count)
        .sum()
}

#[test]
fn a_zombie_writer_resuming_after_a_restart_took_over_is_fenced_not_committed() {
    let dir = tmp("zombie");
    let root = dir.join("store");
    let root_s = root.to_str().unwrap();
    let pause_dir = dir.join("pause");
    std::fs::create_dir_all(&pause_dir).unwrap();

    let seed = write_corpus(&dir, "seed.tsv", 200, 1, "seed");
    let batch1 = write_corpus(&dir, "batch1.tsv", 100, 2, "b1");
    let batch2 = write_corpus(&dir, "batch2.tsv", 100, 3, "b2");

    // Establish the store: init + one committed batch (ownership epoch 1).
    assert!(
        run(&["init", "--path", root_s]).status.success(),
        "init failed"
    );
    assert!(
        run(&["ingest", "--path", root_s, "--file", seed.to_str().unwrap()])
            .status
            .success(),
        "seed ingest failed"
    );

    // Writer 1 (the soon-to-be zombie): ingest batch1, but pause at the commit fence — after its
    // parts are written, before the catalog commit. It acquires ownership epoch 2, then freezes.
    let zombie = Command::new(prism())
        .args([
            "ingest",
            "--path",
            root_s,
            "--file",
            batch1.to_str().unwrap(),
        ])
        .env("PRISM_FAULT_PAUSE", "publish.before_commit_fence")
        .env("PRISM_FAULT_PAUSE_DIR", pause_dir.to_str().unwrap())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the zombie writer");

    // Wait until it is frozen mid-publication.
    wait_for(&pause_dir.join("paused"), Duration::from_secs(30));

    // The restart takes over: a fresh writer ingests batch2, acquires epoch 3, and commits. It must
    // succeed — it is the current owner.
    let restart = run(&[
        "ingest",
        "--path",
        root_s,
        "--file",
        batch2.to_str().unwrap(),
    ]);
    assert!(
        restart.status.success(),
        "the restart (current owner) must publish successfully: {}",
        String::from_utf8_lossy(&restart.stderr)
    );

    // Resume the zombie: on the way out of the pause it fences on its now-stale epoch 2.
    std::fs::write(pause_dir.join("resume"), b"go").unwrap();
    let out = zombie
        .wait_with_output()
        .expect("failed to await the zombie");

    // The zombie's publish must FAIL — fenced, by name — never commit.
    assert!(
        !out.status.success(),
        "the zombie's stale-epoch publish committed instead of being fenced"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("write fenced") && err.contains("D-076"),
        "the fence must be a NAMED refusal: {err}"
    );
    // A refusal, not a crash: fencing is a clean error exit, never a torn write (SIGABRT).
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_ne!(
            out.status.signal(),
            Some(6),
            "fencing must be a clean refusal, not an abort"
        );
    }

    // The store is intact and holds the RESTART's data, not the zombie's. `verify` passes (no torn
    // catalog, no dangling references), and the row count proves batch1 never landed: seed(200) +
    // batch2(100) = 300, not 400.
    let engine = Engine::open(&root).expect("the store must still open");
    engine
        .catalog()
        .verify()
        .expect("the live snapshot must be intact after a fenced zombie");
    assert_eq!(
        rows(&root),
        300,
        "the store must hold seed + the restart's batch only — the fenced zombie's batch must not \
         have landed (no duplicate parts, no torn catalog)"
    );
}
