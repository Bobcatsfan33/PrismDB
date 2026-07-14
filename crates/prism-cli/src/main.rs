//! `prism` — the S0 command line.
//!
//! Every command prints JSON to stdout. Search prints its physical-execution
//! counters alongside its hits, because a result you cannot explain the cost of
//! is a result you cannot trust.

mod args;

use args::Args;
use prism_engine::bench::{self, BenchOpts};
use prism_engine::corpus::{self, Kind};
use prism_engine::engine::now_ms;
use prism_engine::model::HashModelPlane;
use prism_engine::{oracle, tsv, Engine};
use prism_part::store::{Store, StoreConfig, FORMAT_VERSION};
use prism_types::error::{PrismError, Result};
use prism_types::Query;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;

const USAGE: &str = r#"prism — the semantic event store

USAGE:
  prism init      --path <dir> [--dim 64] [--nlist 32] [--pq-m 8] [--seed 42]
  prism ingest    --path <dir> --file <events.tsv>
  prism search    --path <dir> --query <text> [--tenant T] [--from MS] [--to MS]
                  [--k 10] [--nprobe 4] [--candidates 200] [--rerank 50]
                  [--group K] [--space model:version] [--exact]
  prism inspect   --path <dir>
  prism verify    --path <dir>
  prism merge     --path <dir>
  prism reembed   --path <dir> --version <v>
  prism rollback  --path <dir> --to <snapshot-id>
  prism gc        --path <dir> [--retain 5] [--dry-run]
  prism bench     [--path <dir>] [--rows 20000] [--kind zipf] [--out baselines.json]
  prism gen-corpus --kind <uniform|zipf|tenant-skew|late|duplicates|edge>
                   --rows <n> [--seed 42] --out <file.tsv>
  prism golden build --path <dir> --out <golden.json> [--k 10]
  prism golden check --path <dir> --golden <golden.json>
                     [--nprobe 4] [--candidates 200] [--rerank 50] [--min-recall 0.9]
  prism kill-points

Four separate query controls, all reported in the counters:
  --k           how many hits to return
  --nprobe      how many centroids to probe   (how much of the data is looked at)
  --candidates  how many rows survive the compressed scan
  --rerank      how many candidates get their exact vector fetched (the fetch budget)
"#;

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 2 || argv[1] == "--help" || argv[1] == "-h" || argv[1] == "help" {
        println!("{USAGE}");
        return;
    }
    if argv[1] == "--version" {
        println!("prism {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    match run(argv) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("prism: {e}");
            std::process::exit(1);
        }
    }
}

fn emit<T: Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn run(argv: Vec<String>) -> Result<()> {
    let a = Args::parse(argv)?;

    match a.command.as_str() {
        "init" => cmd_init(&a),
        "ingest" => cmd_ingest(&a),
        "search" => cmd_search(&a),
        "inspect" => cmd_inspect(&a),
        "verify" => cmd_verify(&a),
        "merge" => cmd_merge(&a),
        "reembed" => cmd_reembed(&a),
        "rollback" => cmd_rollback(&a),
        "gc" => cmd_gc(&a),
        "bench" => cmd_bench(&a),
        "gen-corpus" => cmd_gen_corpus(&a),
        "golden" => cmd_golden(&a),
        "kill-points" => emit(&prism_part::faults::KILL_POINTS),
        other => Err(PrismError::Invalid(format!(
            "unknown command `{other}`\n\n{USAGE}"
        ))),
    }
}

fn path_of(a: &Args) -> Result<PathBuf> {
    Ok(PathBuf::from(a.req("path")?))
}

fn open(a: &Args) -> Result<Engine> {
    Engine::open(&path_of(a)?)
}

fn cmd_init(a: &Args) -> Result<()> {
    let config = StoreConfig {
        format_version: FORMAT_VERSION,
        dim: a.parse_opt("dim", 64usize)?,
        nlist: a.parse_opt("nlist", 32usize)?,
        pq_m: a.parse_opt("pq-m", 8usize)?,
        seed: a.parse_opt("seed", 42u64)?,
    };
    let root = path_of(a)?;
    Engine::init(&root, config.clone())?;
    emit(&serde_json::json!({
        "initialized": root.display().to_string(),
        "config": config,
    }))
}

fn cmd_ingest(a: &Args) -> Result<()> {
    let engine = open(a)?;
    let file = a.req("file")?;
    let text = std::fs::read_to_string(file)?;
    let events = tsv::parse(&text)?;
    let report = engine.ingest(events, now_ms())?;
    emit(&report)
}

fn cmd_search(a: &Args) -> Result<()> {
    let engine = open(a)?;
    let q = Query {
        text: a.req("query")?.to_string(),
        tenant: a.opt("tenant").map(String::from),
        time_from: a.parse_some("from")?,
        time_to: a.parse_some("to")?,
        k: a.parse_opt("k", 10usize)?,
        nprobe: a.parse_opt("nprobe", 4usize)?,
        candidates: a.parse_opt("candidates", 200usize)?,
        rerank: a.parse_opt("rerank", 50usize)?,
        group_k: a.parse_some("group")?,
        space: a.opt("space").map(String::from),
    };

    if a.has("exact") {
        // The oracle, exposed. Brute-force every eligible row: no centroids, no
        // PQ, no candidate list. Slow by design, and the ground truth that the
        // approximate path is measured against.
        let hits = engine.exact_search(&q)?;
        return emit(&serde_json::json!({ "exact": true, "hits": hits }));
    }

    emit(&engine.search(&q)?)
}

fn cmd_inspect(a: &Args) -> Result<()> {
    let engine = open(a)?;
    let snap = engine.snapshot()?;
    let readers = engine.open_parts(&snap)?;

    let parts: Vec<serde_json::Value> = readers
        .iter()
        .map(|r| {
            let m = &r.manifest;
            let bytes: usize = m.columns.iter().map(|c| c.bytes).sum();
            let pq: usize = m
                .columns
                .iter()
                .filter(|c| c.name == "pq_codes")
                .map(|c| c.bytes)
                .sum();
            let exact: usize = m
                .columns
                .iter()
                .filter(|c| c.name == "vectors")
                .map(|c| c.bytes)
                .sum();
            serde_json::json!({
                "part_id": m.part_id,
                "generation_id": m.generation_id,
                "model": format!("{}:{}", m.model_id, m.model_version),
                "rows": m.row_count,
                "centroids_present": m.centroid_ranges.len(),
                "time_min": m.time_min,
                "time_max": m.time_max,
                "tenants": m.tenants,
                "bytes_total": bytes,
                "bytes_pq_scan_tier": pq,
                "bytes_exact_rerank_tier": exact,
            })
        })
        .collect();

    let total_rows: usize = readers.iter().map(|r| r.manifest.row_count).sum();
    emit(&serde_json::json!({
        "snapshot_id": snap.snapshot_id,
        "parent": snap.parent,
        "active_generation": snap.active_generation,
        "rows": total_rows,
        "parts": parts,
        "snapshots_retained": engine.catalog().list_snapshots()?,
    }))
}

fn cmd_verify(a: &Args) -> Result<()> {
    let engine = open(a)?;
    emit(&engine.catalog().verify()?)
}

fn cmd_merge(a: &Args) -> Result<()> {
    let engine = open(a)?;
    emit(&engine.merge(now_ms())?)
}

fn cmd_reembed(a: &Args) -> Result<()> {
    let root = path_of(a)?;
    let version = a.req("version")?;
    let engine = Engine::open(&root)?.with_plane(Arc::new(HashModelPlane::at_version(version)));
    emit(&engine.reembed(version, now_ms())?)
}

fn cmd_rollback(a: &Args) -> Result<()> {
    let engine = open(a)?;
    let to = a.req("to")?;
    let snap = engine.rollback(to, now_ms())?;
    emit(&serde_json::json!({
        "rolled_back_to": to,
        "new_snapshot": snap,
        "data_rewritten_bytes": 0,
    }))
}

fn cmd_gc(a: &Args) -> Result<()> {
    let engine = open(a)?;
    let retain = a.parse_opt("retain", 5usize)?;
    emit(&engine.catalog().gc(retain, a.has("dry-run"))?)
}

fn cmd_bench(a: &Args) -> Result<()> {
    let kind = a.opt("kind").unwrap_or("zipf");
    let opts = BenchOpts {
        rows: a.parse_opt("rows", 20_000usize)?,
        batch: a.parse_opt("batch", 5_000usize)?,
        seed: a.parse_opt("seed", 42u64)?,
        dim: a.parse_opt("dim", 64usize)?,
        nlist: a.parse_opt("nlist", 32usize)?,
        pq_m: a.parse_opt("pq-m", 8usize)?,
        nprobe: a.parse_opt("nprobe", 4usize)?,
        candidates: a.parse_opt("candidates", 200usize)?,
        rerank: a.parse_opt("rerank", 50usize)?,
        kind: Kind::parse(kind)
            .ok_or_else(|| PrismError::Invalid(format!("unknown corpus kind `{kind}`")))?,
    };

    let root = match a.opt("path") {
        Some(p) => PathBuf::from(p),
        None => std::env::temp_dir().join(format!("prism-bench-{}", std::process::id())),
    };

    let baselines = bench::run(&root, &opts)?;

    if let Some(out) = a.opt("out") {
        std::fs::write(out, serde_json::to_string_pretty(&baselines)?)?;
        eprintln!("prism: wrote {out}");
    }
    emit(&baselines)
}

fn cmd_gen_corpus(a: &Args) -> Result<()> {
    let kind_s = a.req("kind")?;
    let kind = Kind::parse(kind_s).ok_or_else(|| {
        PrismError::Invalid(format!(
            "unknown corpus kind `{kind_s}`; known kinds: {}",
            Kind::all().join(", ")
        ))
    })?;
    let rows: usize = a.parse_opt("rows", 10_000usize)?;
    let seed: u64 = a.parse_opt("seed", 42u64)?;
    let out = a.req("out")?;

    let events = corpus::generate(kind, rows, seed);
    std::fs::write(out, tsv::write(&events))?;

    emit(&serde_json::json!({
        "kind": kind_s,
        "rows": events.len(),
        "seed": seed,
        "out": out,
    }))
}

fn cmd_golden(a: &Args) -> Result<()> {
    match a.sub.as_deref() {
        Some("build") => {
            let engine = open(a)?;
            let store = Store::open(&path_of(a)?)?;
            let k = a.parse_opt("k", 10usize)?;
            let golden = oracle::build(
                &engine,
                a.opt("kind").unwrap_or("unknown"),
                0,
                store.config.seed,
                k,
            )?;
            let out = a.req("out")?;
            std::fs::write(out, serde_json::to_string_pretty(&golden)?)?;
            emit(&serde_json::json!({
                "wrote": out,
                "queries": golden.expectations.len(),
                "k": k,
            }))
        }
        Some("check") => {
            let engine = open(a)?;
            let golden_path = a.req("golden")?;
            let golden: oracle::Golden = serde_json::from_slice(&std::fs::read(golden_path)?)?;

            // 1. Has the meaning of the corpus moved?
            oracle::check_drift(&engine, &golden)?;

            // 2. What recall does the approximate path buy, and at what cost?
            let report = oracle::measure_recall(
                &engine,
                &golden,
                a.parse_opt("nprobe", 4usize)?,
                a.parse_opt("candidates", 200usize)?,
                a.parse_opt("rerank", 50usize)?,
            )?;

            let min_recall: f32 = a.parse_opt("min-recall", 0.0f32)?;
            emit(&report)?;
            if report.mean_recall < min_recall {
                return Err(PrismError::Invariant(format!(
                    "mean recall@{} is {:.3}, below the required {:.3}",
                    report.k, report.mean_recall, min_recall
                )));
            }
            Ok(())
        }
        _ => Err(PrismError::Invalid(
            "usage: prism golden <build|check> ...".into(),
        )),
    }
}
