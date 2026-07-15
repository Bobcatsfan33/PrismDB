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
use prism_part::store::{Store, StoreConfig, STORE_VERSION};
use prism_types::error::{PrismError, Result};
use prism_types::Query;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;

const USAGE: &str = r#"prism — the semantic event store

USAGE:
  prism init      --path <dir> [--dim 64] [--nlist 32] [--pq-m 8] [--seed 42]
                  [--buckets 16] [--time-window-ms 86400000]
                  [--dedicated tenantA,tenantB]   tenants with a bucket to themselves
                  [--promote gen_ai.system,...]   attribute keys as typed columns
  prism ingest    --path <dir> --file <events.tsv>
  prism ingest-source --path <dir> --file <events.jsonl> --source <name> [--max N]
                  poll a source, admit, ack, publish, THEN advance its offset
  prism ingest-otlp   --path <dir> --file <otlp.json> [--tenant fallback]
                  map OTel GenAI spans (semconv pinned) into events
  prism recover   --path <dir>
                  replay every acknowledged-but-unpublished batch from the WAL
  prism evidence block-size --out <file.json> [--corpus <tsv>]
                  derive the default block size from measurement (charter C-1)
  prism search    --path <dir> --query <text> [--tenant T] [--from MS] [--to MS]
                  [--k 10] [--nprobe 4] [--candidates 200] [--rerank 50]
                  [--group K] [--space model:version] [--exact]
  prism sql       --path <dir> --tenant <T> --query "SELECT ..." [--cursor TOK]
                  the SAME door as `search`, reached through SQL. Tenant policy is
                  injected BELOW the parser and is not expressible in the statement.
  prism inspect   --path <dir>
  prism verify    --path <dir>
  prism fsck      --path <dir|part-dir>   offline format validator; needs no catalog
  prism merge     --path <dir>
  prism reembed   --path <dir> --version <v>
  prism rollback  --path <dir> --to <snapshot-id>
  prism gc        --path <dir> [--retain 5] [--dry-run]
  prism bench     [--path <dir>] [--rows 20000] [--kind zipf] [--out baselines.json]
  prism gen-corpus --kind <uniform|zipf|tenant-skew|late|duplicates|edge>
                   --rows <n> [--seed 42] [--format tsv|jsonl] --out <file>
                   tsv is the S0 slice; jsonl carries attributes and trace context
  prism golden build --path <dir> --out <golden.json> [--k 10]
  prism golden check --path <dir> --golden <golden.json>
                     [--nprobe N] [--candidates 200] [--rerank 50]
                     [--min-recall 0.9] [--min-p1 0.8]
  prism golden sweep --path <dir> --golden <golden.json> --out <provenance.json>
                     [--p1-floor 0.8]      derive the default nprobe from the tail
  prism generation status  --path <dir>
  prism generation create  --path <dir> [--version <v>]  train a candidate; changes nothing
                           no --version retrains codebooks in the SAME embedding space;
                           --version moves to a NEW space, which needs a bridge or a
                           finished migration before a query can span both
  prism generation canary  --path <dir> --gen <id> [--partitions 1]
  prism generation compare --path <dir> --gen <id> --golden <golden.json>
  prism generation promote --path <dir> --gen <id>      new writes encode under it
  prism generation migrate --path <dir> --gen <id> [--partitions N]
  prism generation retire  --path <dir> --gen <id>      refused while a snapshot names it
  prism baseline build   --path <dir> --tenant <T> --gen <id>
  prism baseline refresh --path <dir>   rebuild every baseline; DEGRADE what cannot be
  prism drift check      --path <dir> --tenant <T> [--from ms] [--to ms]
                         exits non-zero if an alarm is DEGRADED -- an alarm that is not
                         running is an incident, not a quiet day
  prism redact    --path <dir> --before <ms> --reason "<why>"
                  expire raw bodies. The rows stay queryable and can NEVER be
                  re-embedded again, so any baseline needing them goes DEGRADED.
  prism bridge declare --path <dir> --from <space> --to <space> --validation "<why>"
                  the only way to answer across two embedding spaces. Fuses RANKS,
                  never scores (invariant 9).
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
        "ingest-source" => cmd_ingest_source(&a),
        "ingest-otlp" => cmd_ingest_otlp(&a),
        "recover" => cmd_recover(&a),
        "evidence" => cmd_evidence(&a),
        "search" => cmd_search(&a),
        "sql" => cmd_sql(&a),
        "inspect" => cmd_inspect(&a),
        "verify" => cmd_verify(&a),
        "fsck" => cmd_fsck(&a),
        "merge" => cmd_merge(&a),
        "reembed" => cmd_reembed(&a),
        "rollback" => cmd_rollback(&a),
        "gc" => cmd_gc(&a),
        "bench" => cmd_bench(&a),
        "gen-corpus" => cmd_gen_corpus(&a),
        "golden" => cmd_golden(&a),
        "generation" => cmd_generation(&a),
        "baseline" => cmd_baseline(&a),
        "drift" => cmd_drift(&a),
        "redact" => cmd_redact(&a),
        "bridge" => cmd_bridge(&a),
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
    a.allow(&[
        "path",
        "dim",
        "nlist",
        "pq-m",
        "seed",
        "block-size",
        "buckets",
        "kmeans-restarts",
        "time-window-ms",
        "dedicated",
        "promote",
    ])?;
    let config = StoreConfig {
        format_version: STORE_VERSION,
        dim: a.parse_opt("dim", 64usize)?,
        nlist: a.parse_opt("nlist", 32usize)?,
        pq_m: a.parse_opt("pq-m", 8usize)?,
        seed: a.parse_opt("seed", 42u64)?,
        kmeans_restarts: a
            .parse_opt("kmeans-restarts", prism_quantizer::kmeans::KMEANS_RESTARTS)?,
        block_size: a.parse_opt("block-size", prism_part::format::DEFAULT_BLOCK_SIZE)?,
        partitions: prism_part::partition::PartitionScheme {
            buckets: a.parse_opt("buckets", prism_part::partition::DEFAULT_BUCKETS)?,
            time_window_ms: a.parse_opt(
                "time-window-ms",
                prism_part::partition::DEFAULT_TIME_WINDOW_MS,
            )?,
            dedicated: a
                .opt("dedicated")
                .map(|s| {
                    s.split(',')
                        .filter(|t| !t.is_empty())
                        .enumerate()
                        .map(|(i, t)| (t.to_string(), i as u32))
                        .collect()
                })
                .unwrap_or_default(),
        },
        promote: a
            .opt("promote")
            .map(|s| {
                s.split(',')
                    .filter(|k| !k.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
    };
    let root = path_of(a)?;
    Engine::init(&root, config.clone())?;
    emit(&serde_json::json!({
        "initialized": root.display().to_string(),
        "config": config,
    }))
}

/// The S0 loader: no admission boundary, no quotas, no skew check.
///
/// Distinct from `ingest-source`, which is the S2 path and *does* enforce all of those.
/// Loading a corpus whose event times are a fixed historical epoch through the admission
/// boundary would -- correctly -- dead-letter every row for lateness.
fn cmd_ingest(a: &Args) -> Result<()> {
    let engine = open(a)?;
    let file = a.req("file")?;
    let text = std::fs::read_to_string(file)?;

    let events = if file.ends_with(".jsonl") || a.opt("format") == Some("jsonl") {
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<prism_types::Event>(l).map_err(PrismError::from))
            .collect::<Result<Vec<_>>>()?
    } else {
        tsv::parse(&text)?
    };

    let report = engine.ingest(events, now_ms())?;
    emit(&report)
}

fn cmd_search(a: &Args) -> Result<()> {
    a.allow(&[
        "path",
        "query",
        "tenant",
        "from",
        "to",
        "k",
        "nprobe",
        "candidates",
        "rerank",
        "group",
        "space",
        "exact",
        "no-adaptive",
    ])?;
    let engine = open(a)?;
    let q = Query {
        text: a.req("query")?.to_string(),
        tenant: a.opt("tenant").map(String::from),
        time_from: a.parse_some("from")?,
        time_to: a.parse_some("to")?,
        k: a.parse_opt("k", 10usize)?,
        nprobe: a.parse_opt("nprobe", prism_types::query::DEFAULT_NPROBE)?,
        candidates: a.parse_opt("candidates", prism_types::query::DEFAULT_CANDIDATES)?,
        rerank: a.parse_opt("rerank", prism_types::query::DEFAULT_RERANK)?,
        group_k: a.parse_some("group")?,
        predicate: None,
        space: a.opt("space").map(String::from),
        // Adaptive probing is on unless explicitly disabled. `--no-adaptive` pins the flat base
        // nprobe, which is what the receipts measure (the floor adaptive probing sits on).
        adaptive: !a.has("no-adaptive"),
        adaptive_margin: None,
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

fn cmd_ingest_source(a: &Args) -> Result<()> {
    use prism_engine::source::{FileSource, Source};
    use prism_engine::Ingestor;

    let root = path_of(a)?;
    let mut ing = Ingestor::open(Engine::open(&root)?)?;

    let file = PathBuf::from(a.req("file")?);
    let name = a.req("source")?;
    let max: usize = a.parse_opt("max", 10_000usize)?;

    let src = FileSource::new(name, &file, &root.join("sources"))?;
    let before = src.committed_offset()?;
    let report = ing.poll_and_ingest(&src, max, now_ms())?;

    emit(&serde_json::json!({
        "offered": report.offered,
        "published": report.published,
        "duplicates_suppressed": report.duplicates_suppressed,
        "dead_lettered": report.dead_lettered,
        "by_reason": report.by_reason,
        "part_id": report.part_id,
        "snapshot_id": report.snapshot_id,
        "wal_record": report.wal_record,
        "source": name,
        "source_offset_before": before,
        "source_offset_after": report.source_offset_after,
    }))
}

fn cmd_ingest_otlp(a: &Args) -> Result<()> {
    use prism_engine::otlp;
    use prism_engine::Ingestor;

    let root = path_of(a)?;
    let mut ing = Ingestor::open(Engine::open(&root)?)?;

    let json = std::fs::read_to_string(a.req("file")?)?;
    let now = now_ms();
    let events = otlp::parse(&json, a.opt("tenant").unwrap_or("default"), now)?;
    let mapped = events.len();

    let report = ing.ingest(events, None, None, now)?;
    emit(&serde_json::json!({
        "semconv_version": otlp::SEMCONV_VERSION,
        "mapping_version": otlp::MAPPING_VERSION,
        "spans_mapped": mapped,
        "published": report.published,
        "duplicates_suppressed": report.duplicates_suppressed,
        "dead_lettered": report.dead_lettered,
        "by_reason": report.by_reason,
        "snapshot_id": report.snapshot_id,
    }))
}

/// Replay every acknowledged-but-unpublished batch.
///
/// These events were **acked**: a producer has been told they are safe, and they are
/// not queryable yet. That promise is the only thing standing between us and silent
/// data loss.
fn cmd_recover(a: &Args) -> Result<()> {
    use prism_engine::Ingestor;

    let root = path_of(a)?;
    let mut ing = Ingestor::open(Engine::open(&root)?)?;
    let reports = ing.recover(now_ms())?;

    let events: usize = reports.iter().map(|r| r.published).sum();
    emit(&serde_json::json!({
        "recovered_batches": reports.len(),
        "recovered_events": events,
        "snapshot_id": ing.engine.snapshot()?.snapshot_id,
    }))
}

/// Derive a tuned constant from measurement, and leave a receipt (charter C-1).
fn cmd_evidence(a: &Args) -> Result<()> {
    match a.sub.as_deref() {
        Some("block-size") => {
            let corpus = PathBuf::from(a.opt("corpus").unwrap_or("testing/golden/v1/corpus.tsv"));
            let work = std::env::temp_dir().join(format!("prism-evidence-{}", std::process::id()));
            std::fs::create_dir_all(&work)?;

            let ev = prism_engine::evidence::sweep_block_size(&work, &corpus)?;
            std::fs::remove_dir_all(&work).ok();

            if let Some(out) = a.opt("out") {
                std::fs::write(out, serde_json::to_string_pretty(&ev)?)?;
                eprintln!("prism: wrote {out}");
            }
            emit(&ev)
        }
        Some("adaptive") => {
            // ADAPTIVE_MARGIN (issue #1). Demonstrated at a starved base; monotone at shipping.
            let root = std::env::temp_dir().join(format!("prism-adaptive-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read("testing/golden/MANIFEST.json")?)?;
            let version = manifest["current"].as_str().unwrap_or("v1").to_string();
            let engine = Engine::init(
                &root,
                StoreConfig {
                    format_version: STORE_VERSION,
                    dim: 64,
                    nlist: 32,
                    pq_m: 8,
                    seed: 1234,
                    kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
                    block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
                    partitions: Default::default(),
                    promote: Vec::new(),
                },
            )?;
            engine.ingest(
                tsv::parse(&std::fs::read_to_string(format!(
                    "testing/golden/{version}/corpus.tsv"
                ))?)?,
                1_760_000_000_000,
            )?;
            let golden = prism_engine::oracle::build(&engine, &version, 2000, 1234, 10)?;
            let ev = prism_engine::evidence::sweep_adaptive(
                &engine,
                &golden,
                &version,
                a.parse_opt("starved-base", 2usize)?,
                prism_types::query::DEFAULT_NPROBE,
                a.parse_opt("p1-floor", 0.8f32)?,
            )?;
            std::fs::remove_dir_all(&root).ok();
            if let Some(out) = a.opt("out") {
                std::fs::write(out, serde_json::to_string_pretty(&ev)?)?;
            }
            emit(&ev)
        }
        Some("restarts") => {
            // KMEANS_RESTARTS. Tuned the moment it was chosen by measurement (C-1).
            let root = std::env::temp_dir().join(format!("prism-restarts-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read("testing/golden/MANIFEST.json")?)?;
            let version = manifest["current"].as_str().unwrap_or("v1").to_string();
            let ev = prism_engine::evidence::sweep_restarts(
                &root,
                std::path::Path::new(&format!("testing/golden/{version}/corpus.tsv")),
                &version,
                a.parse_opt("p1-floor", 0.8f32)?,
            )?;
            let _ = std::fs::remove_dir_all(&root);
            if let Some(out) = a.opt("out") {
                std::fs::write(out, serde_json::to_string_pretty(&ev)?)?;
            }
            emit(&ev)
        }
        Some("widths") => {
            // DEFAULT_CANDIDATES x DEFAULT_RERANK, swept jointly. They interact, so there
            // is no honest single-axis sweep of either one.
            let root = std::env::temp_dir().join(format!("prism-widths-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);

            let manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read("testing/golden/MANIFEST.json")?)?;
            let version = manifest["current"].as_str().unwrap_or("v1").to_string();

            let engine = Engine::init(
                &root,
                StoreConfig {
                    format_version: STORE_VERSION,
                    dim: 64,
                    nlist: 32,
                    pq_m: 8,
                    seed: 1234,
                    kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
                    block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
                    partitions: Default::default(),
                    promote: Vec::new(),
                },
            )?;
            let corpus = format!("testing/golden/{version}/corpus.tsv");
            engine.ingest(
                tsv::parse(&std::fs::read_to_string(&corpus)?)?,
                1_760_000_000_000,
            )?;

            let golden: oracle::Golden = serde_json::from_slice(&std::fs::read(format!(
                "testing/golden/{version}/expected.json"
            ))?)?;

            let ev = prism_engine::evidence::sweep_widths(
                &engine,
                &golden,
                &version,
                a.parse_opt("p1-floor", 0.8f32)?,
            )?;
            std::fs::remove_dir_all(&root).ok();

            if let Some(out) = a.opt("out") {
                std::fs::write(out, serde_json::to_string_pretty(&ev)?)?;
                eprintln!("prism: wrote {out}");
            }
            emit(&ev)
        }
        _ => Err(PrismError::Invalid(
            "usage: prism evidence <block-size|widths> --out <file.json>".into(),
        )),
    }
}

/// The SQL door.
///
/// It compiles to the same `Query` the direct path takes and calls the same executor. The
/// tenant comes from `--tenant` (standing in for the authorization layer) and is injected
/// by the binder, beneath the statement, where nothing the user writes can reach it.
fn cmd_sql(a: &Args) -> Result<()> {
    a.allow(&["path", "tenant", "query", "cursor"])?;
    let engine = open(a)?;
    let session = prism_sql::Session {
        tenant: a.req("tenant")?.to_string(),
    };
    let plan = prism_sql::compile(a.req("query")?, &session)?;
    let res = engine.run_sql(&plan, a.opt("cursor"))?;
    emit(&res)
}

fn cmd_inspect(a: &Args) -> Result<()> {
    let engine = open(a)?;
    let snap = engine.snapshot()?;
    let readers = engine.open_parts(&snap)?;

    let parts: Vec<serde_json::Value> = readers
        .iter()
        .map(|r| {
            let m = &r.manifest;
            let bytes: usize = m
                .columns
                .iter()
                .map(|c| c.storage.logical_bytes() as usize)
                .sum();
            let pq: usize = m
                .columns
                .iter()
                .filter(|c| c.name == "pq_codes")
                .map(|c| c.storage.logical_bytes() as usize)
                .sum();
            let exact: usize = m
                .columns
                .iter()
                .filter(|c| c.name == "rerank_vectors")
                .map(|c| c.storage.logical_bytes() as usize)
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

/// The offline format validator.
///
/// Deliberately does not open the engine, the catalog, or a generation: an
/// operator holding a suspicious object out of a backup must be able to condemn
/// it without standing a database up first. Exits non-zero if anything is wrong,
/// so it composes into a shell pipeline.
fn cmd_fsck(a: &Args) -> Result<()> {
    let path = path_of(a)?;
    let reports = prism_part::fsck::fsck(&path)?;
    let bad = reports.iter().filter(|r| !r.ok).count();

    emit(&serde_json::json!({
        "path": path.display().to_string(),
        "parts": reports.len(),
        "healthy": reports.len() - bad,
        "damaged": bad,
        "reports": reports,
    }))?;

    if bad > 0 {
        return Err(PrismError::Corrupt(format!(
            "{bad} of {} parts failed validation",
            reports.len()
        )));
    }
    Ok(())
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
        block_size: a.parse_opt("block-size", prism_part::format::DEFAULT_BLOCK_SIZE)?,
        rows: a.parse_opt("rows", 20_000usize)?,
        batch: a.parse_opt("batch", 5_000usize)?,
        seed: a.parse_opt("seed", 42u64)?,
        dim: a.parse_opt("dim", 64usize)?,
        nlist: a.parse_opt("nlist", 32usize)?,
        pq_m: a.parse_opt("pq-m", 8usize)?,
        nprobe: a.parse_opt("nprobe", prism_types::query::DEFAULT_NPROBE)?,
        candidates: a.parse_opt("candidates", prism_types::query::DEFAULT_CANDIDATES)?,
        rerank: a.parse_opt("rerank", prism_types::query::DEFAULT_RERANK)?,
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

    // TSV is the S0 slice of the event model and carries no attributes, no trace context
    // and no observed_time. JSONL carries the whole event -- which is what you want if you
    // are going to query the things S2 added.
    let format = a.opt("format").unwrap_or("tsv");
    let text = match format {
        "tsv" => tsv::write(&events),
        "jsonl" => events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n"),
        other => {
            return Err(PrismError::Invalid(format!(
                "unknown format `{other}`; use tsv or jsonl"
            )))
        }
    };
    std::fs::write(out, text)?;

    emit(&serde_json::json!({
        "kind": kind_s,
        "format": format,
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
                a.parse_opt("nprobe", prism_types::query::DEFAULT_NPROBE)?,
                a.parse_opt("candidates", prism_types::query::DEFAULT_CANDIDATES)?,
                a.parse_opt("rerank", prism_types::query::DEFAULT_RERANK)?,
            )?;
            emit(&report)?;

            // Two floors, and the tail one is the one that matters. A mean floor
            // alone would have waved S0's `min recall = 0.000` straight through.
            let min_recall: f32 = a.parse_opt("min-recall", 0.0f32)?;
            if report.mean_recall < min_recall {
                return Err(PrismError::Invariant(format!(
                    "mean recall@{} is {:.3}, below the required {:.3}",
                    report.k, report.mean_recall, min_recall
                )));
            }
            let min_p1: f32 = a.parse_opt("min-p1", 0.0f32)?;
            if report.p1_recall < min_p1 {
                return Err(PrismError::Invariant(format!(
                    "p1 recall@{} is {:.3}, below the required {:.3} (the mean is {:.3} — which \
                     is exactly how a tail failure hides)",
                    report.k, report.p1_recall, min_p1, report.mean_recall
                )));
            }
            Ok(())
        }

        // Derive the default nprobe rather than picking one, and leave a receipt.
        Some("sweep") => {
            let engine = open(a)?;
            let golden: oracle::Golden = serde_json::from_slice(&std::fs::read(a.req("golden")?)?)?;

            let prov = oracle::sweep_nprobe(
                &engine,
                &golden,
                a.parse_opt("candidates", prism_types::query::DEFAULT_CANDIDATES)?,
                a.parse_opt("rerank", prism_types::query::DEFAULT_RERANK)?,
                a.parse_opt("p1-floor", 0.8f32)?,
            )?;

            if let Some(out) = a.opt("out") {
                std::fs::write(out, serde_json::to_string_pretty(&prov)?)?;
                eprintln!("prism: wrote {out}");
            }
            emit(&prov)
        }

        _ => Err(PrismError::Invalid(
            "usage: prism golden <build|check|sweep> ...".into(),
        )),
    }
}

// --- S5: the generation lifecycle -------------------------------------------------

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn cmd_generation(a: &Args) -> Result<()> {
    let engine = open(a)?;
    match a.sub.as_deref() {
        Some("status") => {
            a.allow(&["path"])?;
            emit(&engine.migration_status()?)
        }
        Some("create") => {
            a.allow(&["path", "version"])?;
            let g = engine.generation_create(a.opt("version"), now())?;
            eprintln!(
                "prism: candidate generation {} created. It encodes nothing and answers nothing \
                 until you canary or promote it.",
                g.generation_id
            );
            emit(&g.generation_id)
        }
        Some("canary") => {
            a.allow(&["path", "gen", "partitions"])?;
            let r = engine.generation_canary(
                a.req("gen")?,
                a.parse_opt("partitions", 1usize)?,
                now(),
            )?;
            emit(&r)
        }
        Some("compare") => {
            a.allow(&["path", "gen", "golden", "k"])?;
            let golden: prism_engine::oracle::Golden =
                serde_json::from_slice(&std::fs::read(a.req("golden")?)?)?;
            emit(&engine.generation_compare(a.req("gen")?, &golden)?)
        }
        Some("promote") => {
            a.allow(&["path", "gen"])?;
            let s = engine.generation_promote(a.req("gen")?, now())?;
            emit(&s)
        }
        Some("migrate") => {
            a.allow(&["path", "gen", "partitions"])?;
            let r = engine.generation_migrate(
                a.req("gen")?,
                a.parse_some::<usize>("partitions")?,
                now(),
            )?;
            emit(&r)
        }
        Some("retire") => {
            a.allow(&["path", "gen"])?;
            let s = engine.generation_retire(a.req("gen")?, now())?;
            emit(&s)
        }
        _ => Err(PrismError::Invalid(
            "usage: prism generation <status|create|canary|compare|promote|migrate|retire>".into(),
        )),
    }
}

fn cmd_baseline(a: &Args) -> Result<()> {
    let engine = open(a)?;
    match a.sub.as_deref() {
        Some("build") => {
            a.allow(&["path", "tenant", "gen"])?;
            let b = engine.baseline_build(a.req("tenant")?, a.req("gen")?, now())?;
            emit(&b.baseline_id)
        }
        Some("refresh") => {
            a.allow(&["path"])?;
            let set = engine.baselines_refresh(now())?;
            emit(&set)
        }
        _ => Err(PrismError::Invalid(
            "usage: prism baseline <build|refresh>".into(),
        )),
    }
}

fn cmd_drift(a: &Args) -> Result<()> {
    a.allow(&["path", "tenant", "from", "to"])?;
    let engine = open(a)?;
    let r = engine.drift_check(
        a.req("tenant")?,
        a.parse_some::<i64>("from")?,
        a.parse_some::<i64>("to")?,
    )?;
    emit(&r)?;

    // An alarm that is not running is an incident, not a quiet day. Exiting zero here would
    // let a cron job treat "we stopped watching" as "nothing to see".
    if r.is_degraded() {
        for d in &r.degraded {
            eprintln!(
                "prism: DEGRADED -- tenant `{}`, generation {}: {} rows are NOT being watched.\n  {}",
                d.tenant, d.generation_id, d.rows_unwatched, d.reason
            );
        }
        return Err(PrismError::Invalid(
            "drift check is DEGRADED: at least one alarm is not running".into(),
        ));
    }
    Ok(())
}

fn cmd_redact(a: &Args) -> Result<()> {
    a.allow(&["path", "before", "reason"])?;
    let engine = open(a)?;
    let n = engine.redact_bodies(a.parse_opt::<i64>("before", 0)?, a.req("reason")?, now())?;
    eprintln!(
        "prism: redacted the bodies of {n} parts. Those rows can never be re-embedded again; any \
         drift baseline that needed them is now DEGRADED, not silent."
    );
    emit(&n)
}

fn cmd_bridge(a: &Args) -> Result<()> {
    match a.sub.as_deref() {
        Some("declare") => {
            a.allow(&["path", "from", "to", "validation"])?;
            let engine = open(a)?;
            let s =
                engine.bridge_declare(a.req("from")?, a.req("to")?, a.req("validation")?, now())?;
            emit(&s)
        }
        _ => Err(PrismError::Invalid(
            "usage: prism bridge declare ...".into(),
        )),
    }
}
