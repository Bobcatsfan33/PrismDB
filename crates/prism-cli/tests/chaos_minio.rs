//! **S12 increment 3, item 2 — durability chaos on real `prism` processes over MinIO** ([D-076](../../../docs/DECISIONS.md),
//! [D-069](../../../docs/DECISIONS.md), [S10](../../../docs/MERGE-CONTRACT.md)).
//!
//! Each shard is a real `prism` process whose cold tier — cold parts, catalog mirror, and write-
//! ownership epochs — lives in its own MinIO bucket. Chaos hits **one** shard; the binding property is
//! **containment**: a bystander shard answers unchanged throughout, and the victim ends
//! **correct-or-named**, never silent, never hybrid.
//!
//! Scenario 1 (here): a writer is crashed **at every publication boundary** (the kill-point matrix);
//! a same-node restart re-acquires a higher ownership epoch, replays the WAL, and heals the catalog
//! against the MinIO mirror; the victim's answer is **byte-identical** to old-or-new; and the
//! bystander is untouched.
//!
//! Runs only when `PRISM_S3_ENDPOINT` is set (MinIO in CI, digest-pinned; a local 09-07 for the fast
//! loop); skips otherwise.

use prism_engine::storage::object::CachedObjectStore;
use prism_engine::storage::s3::{S3Config, S3ObjectStore};
use prism_engine::storage::sigv4::Credentials;
use prism_engine::storage::CACHE_QUOTA_BYTES;
use prism_engine::{corpus, Engine};
use prism_types::Query;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static N: AtomicU64 = AtomicU64::new(0);

fn prism() -> &'static str {
    env!("CARGO_BIN_EXE_prism")
}

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-chaos-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn base_cfg() -> Option<S3Config> {
    let endpoint = std::env::var("PRISM_S3_ENDPOINT").ok()?;
    Some(S3Config {
        endpoint,
        region: std::env::var("PRISM_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
        bucket: "unused".into(),
        credentials: Credentials {
            access_key: std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| "minioadmin".into()),
            secret_key: std::env::var("AWS_SECRET_ACCESS_KEY")
                .unwrap_or_else(|_| "minioadmin".into()),
            session_token: std::env::var("AWS_SESSION_TOKEN")
                .ok()
                .filter(|value| !value.trim().is_empty()),
        },
        fixed_amz_date: None,
    })
}

fn died_abnormally(out: &Output) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        return out.status.signal() == Some(6); // SIGABRT
    }
    #[allow(unreachable_code)]
    {
        !out.status.success()
    }
}

/// The publication boundaries a crash must survive old-or-new. Every kill point on the durable ingest
/// path from the WAL append through the catalog commit and its mirror — the matrix, not one point.
const PUBLICATION_KILL_POINTS: &[&str] = &[
    "wal.after_append_before_fsync",
    "ingest.after_embed_before_part",
    "part.after_write_before_fsync",
    "part.after_fsync_before_rename",
    "part.after_rename_before_snapshot",
    "publish.after_upload_before_verify",
    "publish.after_verify_before_reference",
    "snapshot.after_write_before_current",
    "current.after_rename",
    "mirror.after_rename_before_mirror",
    "ingest.after_publish_before_offset_commit",
];

/// One shard: a local store root and its own MinIO bucket, driven by real `prism` processes.
struct Shard {
    root: PathBuf,
    cfg: S3Config,
}

impl Shard {
    fn new(base: &S3Config, tag: &str) -> Shard {
        let mut cfg = base.clone();
        cfg.bucket = format!(
            "prism-chaos-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        S3ObjectStore::new(cfg.clone())
            .create_bucket()
            .expect("create shard bucket");
        let root = tmp(tag);
        let shard = Shard { root, cfg };
        shard.cli(&["init", "--path", shard.root_s()], None);
        shard
    }

    fn root_s(&self) -> &str {
        self.root.to_str().unwrap()
    }

    /// Run a `prism` subcommand against this shard, with its MinIO bucket in the environment and an
    /// optional injected fault. Returns the raw process output (the caller asserts on it).
    fn cli(&self, args: &[&str], fault: Option<&str>) -> Output {
        let mut cmd = Command::new(prism());
        cmd.args(args)
            .env("PRISM_S3_ENDPOINT", &self.cfg.endpoint)
            .env("PRISM_S3_REGION", &self.cfg.region)
            .env("PRISM_S3_BUCKET", &self.cfg.bucket)
            .env("AWS_ACCESS_KEY_ID", &self.cfg.credentials.access_key)
            .env("AWS_SECRET_ACCESS_KEY", &self.cfg.credentials.secret_key)
            .env("PRISM_ALLOW_INSECURE_S3", "true")
            .env_remove("PRISM_FAULT");
        if let Some(f) = fault {
            cmd.env("PRISM_FAULT", f);
        }
        cmd.output().expect("failed to run prism")
    }

    /// Ingest a source file through the WAL path (durable ack, then publish). `fault` crashes it.
    fn ingest_source(&self, source: &str, file: &str, fault: Option<&str>) -> Output {
        self.cli(
            &[
                "ingest-source",
                "--path",
                self.root_s(),
                "--source",
                source,
                "--file",
                file,
            ],
            fault,
        )
    }

    fn recover(&self) -> Output {
        self.cli(&["recover", "--path", self.root_s()], None)
    }

    /// Ingest a JSONL file through the **S0 loader** (`ingest`), optionally with a disk-full fault
    /// injected at a space-guard point (`PRISM_FAULT_ENOSPC`) — a **returned** error, not a crash, so
    /// it exercises the graceful S10 degrade-and-recover path.
    fn ingest_enospc(&self, file: &str, enospc: Option<&str>) -> Output {
        let mut cmd = Command::new(prism());
        cmd.args([
            "ingest",
            "--path",
            self.root_s(),
            "--file",
            file,
            "--format",
            "jsonl",
        ])
        .env("PRISM_S3_ENDPOINT", &self.cfg.endpoint)
        .env("PRISM_S3_REGION", &self.cfg.region)
        .env("PRISM_S3_BUCKET", &self.cfg.bucket)
        .env("AWS_ACCESS_KEY_ID", &self.cfg.credentials.access_key)
        .env("AWS_SECRET_ACCESS_KEY", &self.cfg.credentials.secret_key)
        .env("PRISM_ALLOW_INSECURE_S3", "true")
        .env_remove("PRISM_FAULT")
        .env_remove("PRISM_FAULT_ENOSPC");
        if let Some(p) = enospc {
            cmd.env("PRISM_FAULT_ENOSPC", p);
        }
        cmd.output().expect("failed to run prism")
    }

    /// Simulate local disk loss: drop the local catalog (CURRENT + snapshot files). The parts, the
    /// WAL, and the MinIO mirror survive — the S11 disaster-drill starting point.
    fn delete_local_catalog(&self) {
        let _ = std::fs::remove_file(self.root.join("catalog/CURRENT"));
        let _ = std::fs::remove_dir_all(self.root.join("catalog/snapshots"));
    }

    /// Recover a lost local catalog from the MinIO mirror, then replay the WAL — the disaster path.
    /// The mirrored snapshot carries the `applied_wal_record` marker (D-077), so the WAL replay is
    /// exactly-once even though the recovery is from the mirror, not the local truth.
    fn recover_from_mirror(&self) {
        let engine = self.engine();
        engine
            .recover_catalog_from_mirror()
            .expect("recover catalog from mirror");
        let mut ing = prism_engine::Ingestor::open(self.engine()).expect("open ingestor");
        ing.recover(prism_engine::engine::now_ms())
            .expect("wal replay");
    }

    /// An in-process, MinIO-backed engine over this shard's store — for assertions (search, verify,
    /// row count) that read the same cold tier the CLI wrote.
    fn engine(&self) -> Engine {
        let backend = Arc::new(S3ObjectStore::new(self.cfg.clone()));
        Engine::open(&self.root)
            .expect("open shard")
            .with_cold(Arc::new(CachedObjectStore::new(backend, CACHE_QUOTA_BYTES)))
    }

    /// A byte-exact fingerprint of a query's answer: (event_id, score bits), the property that must be
    /// invariant to the physical recovery path.
    fn search_fp(&self) -> Vec<(String, u32)> {
        let engine = self.engine();
        let snap = engine.snapshot().unwrap();
        let q = Query {
            text: QUERY.into(),
            k: 20,
            rerank: 40,
            nprobe: 8,
            ..Default::default()
        };
        engine
            .search_at(&snap, &q)
            .unwrap()
            .hits
            .iter()
            .map(|h| (h.event.event_id.clone(), h.score.to_bits()))
            .collect()
    }

    fn rows(&self) -> usize {
        let engine = self.engine();
        let snap = engine.snapshot().unwrap();
        engine
            .open_parts(&snap)
            .unwrap()
            .iter()
            .map(|r| r.manifest.row_count)
            .sum()
    }

    fn verify(&self) {
        self.engine()
            .catalog()
            .verify()
            .expect("the live snapshot must be intact");
    }

    /// The highest write-ownership epoch recorded in this shard's MinIO bucket (D-076).
    fn owner_epoch(&self) -> u64 {
        let store = S3ObjectStore::new(self.cfg.clone());
        prism_engine::storage::ownership::highest_epoch(&store).unwrap()
    }
}

const QUERY: &str = "the tool call timed out retrying";

/// A JSONL feed the WAL source (`ingest-source`) reads — one whole event per line, its id tagged so
/// batches are distinguishable. If `matching`, every event's body is exactly the query text, so the
/// batch dominates the top-k the moment it lands — which makes "did the answer change?" a sharp,
/// deterministic question the byte-identical assertion can rest on.
fn write_corpus(
    dir: &Path,
    name: &str,
    rows: usize,
    seed: u64,
    tag: &str,
    matching: bool,
) -> String {
    // The WAL admission path runs the event-time skew check (unlike the S0 loader), so the events
    // need a recent `event_time` or they are dead-lettered as too-late against the real clock. One
    // minute ago is well inside the 7-day lateness bound and stable across shards (the files are
    // written once and reused).
    let recent = prism_engine::engine::now_ms() - 60_000;
    let lines: Vec<String> = corpus::generate(corpus::Kind::Uniform, rows, seed)
        .into_iter()
        .map(|mut e| {
            e.event_id = format!("{tag}-{}", e.event_id);
            e.event_time = recent;
            e.observed_time = recent;
            if matching {
                e.body = QUERY.into();
            }
            serde_json::to_string(&e).unwrap()
        })
        .collect();
    let path = dir.join(name);
    std::fs::write(&path, lines.join("\n")).unwrap();
    path.to_str().unwrap().to_string()
}

#[test]
fn scenario_1_crash_at_every_publication_boundary_recovers_byte_identical_and_contained() {
    let Some(base) = base_cfg() else {
        eprintln!("skipping MinIO chaos: PRISM_S3_ENDPOINT is not set");
        return;
    };

    let fixtures = tmp("fix");
    let seed_tsv = write_corpus(&fixtures, "seed.jsonl", 200, 1, "seed", false);
    let batch_tsv = write_corpus(&fixtures, "batch.jsonl", 100, 2, "batch", true);

    // A bystander shard, seeded once. It is queried across every victim crash and must never move.
    let bystander = Shard::new(&base, "bystander");
    assert!(
        bystander
            .ingest_source("seed", &seed_tsv, None)
            .status
            .success(),
        "bystander seed failed"
    );
    let ref_bystander = bystander.search_fp();

    // The two legal victim answers, computed once on a clean reference shard: seed only (rolled back)
    // and seed+batch (committed). Same seed → same content-addressed generation → comparable scores;
    // the answer is layout-invariant, so a recovered store matches one of these to the bit.
    let ref_shard = Shard::new(&base, "ref");
    assert!(ref_shard
        .ingest_source("seed", &seed_tsv, None)
        .status
        .success());
    let ref_before = ref_shard.search_fp();
    assert!(ref_shard
        .ingest_source("batch", &batch_tsv, None)
        .status
        .success());
    let ref_after = ref_shard.search_fp();
    assert_ne!(
        ref_before, ref_after,
        "the batch must change the answer, or the test proves nothing"
    );

    for point in PUBLICATION_KILL_POINTS {
        // A fresh victim per boundary (independent iterations, like the fault harness).
        let victim = Shard::new(&base, "victim");
        assert!(
            victim
                .ingest_source("seed", &seed_tsv, None)
                .status
                .success(),
            "{point}: victim seed failed"
        );
        assert_eq!(
            victim.search_fp(),
            ref_before,
            "{point}: seeded victim disagrees with reference"
        );
        let epoch_seeded = victim.owner_epoch();

        // Crash the victim mid-publication of the batch.
        let out = victim.ingest_source("batch", &batch_tsv, Some(point));
        assert!(
            died_abnormally(&out),
            "{point}: expected a crash (SIGABRT), got {:?}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );

        // CONTAINMENT: the bystander, on its own bucket, is untouched by the victim's crash.
        assert_eq!(
            bystander.search_fp(),
            ref_bystander,
            "{point}: the victim's crash changed a bystander shard's answer — not contained"
        );

        // Same-node restart: recover re-acquires a higher epoch, replays the WAL, heals the mirror.
        let rec = victim.recover();
        assert!(
            rec.status.success(),
            "{point}: recover failed: {}",
            String::from_utf8_lossy(&rec.stderr)
        );
        assert!(
            victim.owner_epoch() > epoch_seeded,
            "{point}: the restart did not re-acquire a higher ownership epoch"
        );

        // CORRECT-OR-NAMED, never hybrid: verify passes and the answer is byte-identical to old-or-new.
        victim.verify();
        let n = victim.rows();
        assert!(
            n == 200 || n == 300,
            "{point}: hybrid row count {n} (expected 200 rolled-back or 300 committed)"
        );
        let fp = victim.search_fp();
        assert!(
            fp == ref_before || fp == ref_after,
            "{point}: recovered answer is neither the old nor the new state — not byte-identical"
        );

        // And the bystander is still unmoved after the victim recovered.
        assert_eq!(
            bystander.search_fp(),
            ref_bystander,
            "{point}: recovery disturbed a bystander shard"
        );
    }
}

/// The three kill points **after** the local commit, where the publication is already reflected in a
/// snapshot: a crash here that also loses the local catalog must still recover **exactly-once** from
/// the MinIO mirror, because the `applied_wal_record` marker rides the mirrored snapshot (D-077).
const CRASH_AFTER_PUBLISH_KILL_POINTS: &[&str] = &[
    "current.after_rename",
    "mirror.after_rename_before_mirror",
    "ingest.after_publish_before_offset_commit",
];

#[test]
fn scenario_1_disaster_recovery_from_the_mirror_is_exactly_once() {
    let Some(base) = base_cfg() else {
        eprintln!("skipping MinIO chaos: PRISM_S3_ENDPOINT is not set");
        return;
    };

    let fixtures = tmp("dr-fix");
    let seed_tsv = write_corpus(&fixtures, "seed.jsonl", 200, 1, "seed", false);
    let batch_tsv = write_corpus(&fixtures, "batch.jsonl", 100, 2, "batch", true);

    let ref_shard = Shard::new(&base, "dr-ref");
    assert!(ref_shard
        .ingest_source("seed", &seed_tsv, None)
        .status
        .success());
    let ref_before = ref_shard.search_fp();
    assert!(ref_shard
        .ingest_source("batch", &batch_tsv, None)
        .status
        .success());
    let ref_after = ref_shard.search_fp();

    for point in CRASH_AFTER_PUBLISH_KILL_POINTS {
        let victim = Shard::new(&base, "dr");
        assert!(victim
            .ingest_source("seed", &seed_tsv, None)
            .status
            .success());

        // Crash mid-publication of the batch — at a point after the local commit.
        let out = victim.ingest_source("batch", &batch_tsv, Some(point));
        assert!(died_abnormally(&out), "{point}: expected a crash");

        // Disaster: the local catalog is lost. Recover from the MinIO mirror + WAL.
        victim.delete_local_catalog();
        victim.recover_from_mirror();

        // Exactly-once: no double-publish through the mirror path either. verify passes; the answer is
        // byte-identical to old-or-new; the row count is never the hybrid 400.
        victim.verify();
        let n = victim.rows();
        assert!(
            n == 200 || n == 300,
            "{point}: mirror recovery produced {n} rows (a double-publish is 400)"
        );
        let fp = victim.search_fp();
        assert!(
            fp == ref_before || fp == ref_after,
            "{point}: mirror-recovered answer is neither the old nor the new state"
        );
    }
}

/// The disk-full guard points (S10): the writes a full disk can interrupt without corrupting the store.
const SPACE_GUARDS: &[&str] = &["part.columns", "catalog.snapshot", "catalog.current"];

#[test]
fn scenario_2_per_node_enospc_is_named_isolated_and_recovers_unaided() {
    let Some(base) = base_cfg() else {
        eprintln!("skipping MinIO chaos: PRISM_S3_ENDPOINT is not set");
        return;
    };

    let fixtures = tmp("enospc-fix");
    let seed = write_corpus(&fixtures, "seed.jsonl", 200, 1, "seed", false);
    let batch = write_corpus(&fixtures, "batch.jsonl", 100, 2, "batch", false);

    // A neighbour on its own bucket, queried across every disk-full event on the victim.
    let bystander = Shard::new(&base, "byst");
    assert!(
        bystander.ingest_enospc(&seed, None).status.success(),
        "bystander seed failed"
    );
    let byst_rows = bystander.rows();

    for point in SPACE_GUARDS {
        let victim = Shard::new(&base, "victim");
        assert!(
            victim.ingest_enospc(&seed, None).status.success(),
            "{point}: seed failed"
        );
        assert_eq!(victim.rows(), 200, "{point}: seed row count");

        // Disk full mid-operation: the ingest degrades **by name**, never a crash, never a hybrid.
        let out = victim.ingest_enospc(&batch, Some(point));
        assert!(
            !out.status.success(),
            "{point}: a disk-full ingest must fail, not silently succeed"
        );
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_ne!(
                out.status.signal(),
                Some(6),
                "{point}: ENOSPC is a named refusal, not an abort"
            );
        }
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("out of disk space"),
            "{point}: the degradation must be named: {err}"
        );

        // Old-or-new: the store is unchanged (old), and `verify` passes — no torn write.
        victim.verify();
        assert_eq!(victim.rows(), 200, "{point}: ENOSPC left a hybrid store");

        // CONTAINMENT: the neighbour on its own disk/bucket is untouched.
        assert_eq!(
            bystander.rows(),
            byst_rows,
            "{point}: a disk-full event on one shard disturbed a neighbour — not isolated"
        );

        // Recovery is unaided: with space back (no fault), a retry completes.
        assert!(
            victim.ingest_enospc(&batch, None).status.success(),
            "{point}: the store did not accept writes once space returned"
        );
        assert_eq!(
            victim.rows(),
            300,
            "{point}: the retry after ENOSPC did not land the batch"
        );
    }
}

#[test]
fn scenario_3_a_shard_restarts_from_the_mirror_while_the_cluster_serves() {
    let Some(base) = base_cfg() else {
        eprintln!("skipping MinIO chaos: PRISM_S3_ENDPOINT is not set");
        return;
    };

    let fixtures = tmp("restart-fix");
    let seed_v = write_corpus(&fixtures, "seedv.jsonl", 200, 1, "victim", true);
    let seed_b1 = write_corpus(&fixtures, "seedb1.jsonl", 150, 3, "b1", true);
    let seed_b2 = write_corpus(&fixtures, "seedb2.jsonl", 150, 4, "b2", true);

    let victim = Shard::new(&base, "rv");
    let b1 = Shard::new(&base, "rb1");
    let b2 = Shard::new(&base, "rb2");
    assert!(victim.ingest_source("s", &seed_v, None).status.success());
    assert!(b1.ingest_source("s", &seed_b1, None).status.success());
    assert!(b2.ingest_source("s", &seed_b2, None).status.success());

    let ref_v = victim.search_fp();
    let ref_b1 = b1.search_fp();
    let ref_b2 = b2.search_fp();
    assert!(
        !ref_v.is_empty() && !ref_b1.is_empty(),
        "each shard must answer its own tenants"
    );

    // The victim node restarts with a lost local catalog — the S11 disaster start, now per-shard and
    // live. As it goes down, the other shards keep serving, unchanged.
    victim.delete_local_catalog();
    assert_eq!(
        b1.search_fp(),
        ref_b1,
        "b1 was disturbed as the victim went down"
    );
    assert_eq!(
        b2.search_fp(),
        ref_b2,
        "b2 was disturbed as the victim went down"
    );

    // The victim recovers from the MinIO mirror + WAL while the cluster serves.
    victim.recover_from_mirror();

    // The restarted shard is back byte-identical; the bystanders never moved (containment).
    victim.verify();
    assert_eq!(
        victim.search_fp(),
        ref_v,
        "the restarted shard did not recover byte-identical"
    );
    assert_eq!(
        b1.search_fp(),
        ref_b1,
        "b1 was disturbed by the victim's restart"
    );
    assert_eq!(
        b2.search_fp(),
        ref_b2,
        "b2 was disturbed by the victim's restart"
    );
}
