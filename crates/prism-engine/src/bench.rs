//! The baseline report (S0 gate).
//!
//! A machine-generated, checked-in `baselines.json`. Not a marketing number: a
//! regression tripwire and an honesty device. Three rules it exists to enforce:
//!
//!   * **Two-tier cost, always.** PQ bytes and exact-vector bytes are reported
//!     separately, because the compressed scan tier is small and the rerank tier
//!     is 32x larger, and quoting only the first is how a storage claim becomes
//!     a lie (Part I §5.2).
//!   * **Recall is priced.** Every recall number carries the scan fraction it
//!     was bought at. Recall alone is meaningless — you can always have 1.0 by
//!     scanning everything.
//!   * **No rooflines.** Every number here was measured end to end, including
//!     the filter, the heap, the rerank fetch and the materialization.
//!
//! The numbers are machine-dependent. Compare them against a run on the *same*
//! machine; the point is the shape and the direction, not the absolute value.

use crate::corpus::{self, Kind};
use crate::engine::Engine;
use crate::oracle;
use prism_part::store::StoreConfig;
use prism_types::error::Result;
use prism_types::Query;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Instant;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Baselines {
    pub prism_version: String,
    pub format_version: u32,
    pub os: String,
    pub arch: String,
    /// Debug builds are several times slower. Recorded so nobody compares across
    /// profiles by accident.
    pub profile: String,

    pub corpus_kind: String,
    pub rows_offered: usize,
    pub rows_admitted: usize,
    pub rows_dead_lettered: usize,
    pub dim: usize,
    pub nlist: usize,
    pub pq_m: usize,

    pub ingest_rows_per_sec: f64,
    pub ingest_seconds: f64,

    /// Time to open every part manifest in the snapshot — the cost pruning pays
    /// before it can eliminate anything.
    pub part_open_ms: f64,
    pub parts: usize,

    pub query_p50_ms: f64,
    pub query_p95_ms: f64,
    pub query_max_ms: f64,
    /// Rows of *compressed codes* scanned per second, end to end.
    pub scan_rows_per_sec: f64,

    pub recall_at_10: f32,
    /// The tail. A mean without a minimum is how S0 shipped a configuration that
    /// returned *nothing* for five queries and called it 90% accurate.
    pub min_recall_at_10: f32,
    pub p1_recall_at_10: f32,
    pub p5_recall_at_10: f32,
    pub zero_recall_queries: usize,
    pub recall_queries: usize,
    pub nprobe: usize,
    pub candidates: usize,
    pub rerank: usize,
    /// Fraction of eligible rows the centroid index made us touch.
    pub mean_scan_fraction: f64,

    // --- two-tier storage, reported separately, always ---
    pub bytes_per_row_total: f64,
    pub bytes_per_row_pq: f64,
    pub bytes_per_row_exact_vectors: f64,
    pub bytes_per_row_scalars_and_text: f64,
    /// exact-vector bytes / PQ bytes. The number that decides the storage bill.
    pub rerank_tier_multiple: f64,

    pub merge_write_amplification: f64,
    pub merge_parts_in: usize,
    pub merge_parts_out: usize,
    pub merge_duplicates_reconciled: usize,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

pub struct BenchOpts {
    pub block_size: u32,
    pub rows: usize,
    pub batch: usize,
    pub seed: u64,
    pub dim: usize,
    pub nlist: usize,
    pub pq_m: usize,
    pub nprobe: usize,
    pub candidates: usize,
    pub rerank: usize,
    pub kind: Kind,
}

impl Default for BenchOpts {
    fn default() -> Self {
        BenchOpts {
            block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
            rows: 20_000,
            batch: 5_000,
            seed: 42,
            dim: 64,
            nlist: 32,
            pq_m: 8,
            nprobe: prism_types::query::DEFAULT_NPROBE,
            candidates: prism_types::query::DEFAULT_CANDIDATES,
            rerank: prism_types::query::DEFAULT_RERANK,
            kind: Kind::Zipf,
        }
    }
}

/// Build a store from scratch, ingest, query, measure, merge, and report.
pub fn run(root: &Path, opts: &BenchOpts) -> Result<Baselines> {
    if root.exists() {
        std::fs::remove_dir_all(root)?;
    }

    let engine = Engine::init(
        root,
        StoreConfig {
            format_version: prism_part::store::STORE_VERSION,
            dim: opts.dim,
            nlist: opts.nlist,
            pq_m: opts.pq_m,
            seed: opts.seed,
            block_size: opts.block_size,
            partitions: Default::default(),
            promote: Vec::new(),
        },
    )?;

    // --- ingest ---
    let events = corpus::generate(opts.kind, opts.rows, opts.seed);
    let offered = events.len();
    let mut admitted = 0usize;
    let mut dead = 0usize;

    let t0 = Instant::now();
    for (i, chunk) in events.chunks(opts.batch).enumerate() {
        let rep = engine.ingest(chunk.to_vec(), 1_760_000_000_000 + i as i64)?;
        admitted += rep.admitted;
        dead += rep.dead_lettered;
    }
    let ingest_seconds = t0.elapsed().as_secs_f64();

    // --- part open cost ---
    let snap = engine.snapshot()?;
    let t1 = Instant::now();
    let readers = engine.open_parts(&snap)?;
    let part_open_ms = t1.elapsed().as_secs_f64() * 1000.0;

    // --- two-tier storage accounting ---
    let mut pq_bytes = 0usize;
    let mut vec_bytes = 0usize;
    let mut other_bytes = 0usize;
    let mut rows_stored = 0usize;
    for r in &readers {
        rows_stored += r.manifest.row_count;
        for c in &r.manifest.columns {
            match c.name.as_str() {
                "pq_codes" => pq_bytes += c.storage.logical_bytes() as usize,
                "rerank_vectors" => vec_bytes += c.storage.logical_bytes() as usize,
                _ => other_bytes += c.storage.logical_bytes() as usize,
            }
        }
    }
    let rows_f = rows_stored.max(1) as f64;

    // --- query latency + scan rate ---
    let golden = oracle::build(
        &engine,
        &format!("{:?}", opts.kind).to_lowercase(),
        offered,
        opts.seed,
        10,
    )?;

    let mut latencies: Vec<f64> = Vec::new();
    let mut rows_scanned_total = 0usize;
    let mut scan_seconds = 0.0f64;

    for exp in &golden.expectations {
        let mut q: Query = exp.query.to_query();
        q.nprobe = opts.nprobe;
        q.candidates = opts.candidates;
        q.rerank = opts.rerank;
        q.k = 10;

        // Two warm passes, then five measured. A cold-cache number belongs in
        // S11 where the cache is the thing being measured; here it would just be
        // noise.
        for _ in 0..2 {
            engine.search(&q)?;
        }
        for _ in 0..5 {
            let t = Instant::now();
            let res = engine.search(&q)?;
            let dt = t.elapsed().as_secs_f64();
            latencies.push(dt * 1000.0);
            scan_seconds += dt;
            rows_scanned_total += res.counters.rows_scanned_pq;
        }
    }
    latencies.sort_by(|a, b| a.total_cmp(b));

    let recall =
        oracle::measure_recall(&engine, &golden, opts.nprobe, opts.candidates, opts.rerank)?;

    // --- merge ---
    let merge = engine.merge(1_760_000_100_000)?;

    Ok(Baselines {
        prism_version: env!("CARGO_PKG_VERSION").to_string(),
        format_version: prism_part::store::STORE_VERSION,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
        .to_string(),

        corpus_kind: format!("{:?}", opts.kind).to_lowercase(),
        rows_offered: offered,
        rows_admitted: admitted,
        rows_dead_lettered: dead,
        dim: opts.dim,
        nlist: opts.nlist,
        pq_m: opts.pq_m,

        ingest_rows_per_sec: admitted as f64 / ingest_seconds.max(1e-9),
        ingest_seconds,

        part_open_ms,
        parts: readers.len(),

        query_p50_ms: percentile(&latencies, 0.50),
        query_p95_ms: percentile(&latencies, 0.95),
        query_max_ms: latencies.last().copied().unwrap_or(0.0),
        scan_rows_per_sec: rows_scanned_total as f64 / scan_seconds.max(1e-9),

        recall_at_10: recall.mean_recall,
        min_recall_at_10: recall.min_recall,
        p1_recall_at_10: recall.p1_recall,
        p5_recall_at_10: recall.p5_recall,
        zero_recall_queries: recall.zero_recall_queries,
        recall_queries: recall.queries,
        nprobe: opts.nprobe,
        candidates: opts.candidates,
        rerank: opts.rerank,
        mean_scan_fraction: recall.mean_scan_fraction,

        bytes_per_row_total: (pq_bytes + vec_bytes + other_bytes) as f64 / rows_f,
        bytes_per_row_pq: pq_bytes as f64 / rows_f,
        bytes_per_row_exact_vectors: vec_bytes as f64 / rows_f,
        bytes_per_row_scalars_and_text: other_bytes as f64 / rows_f,
        rerank_tier_multiple: vec_bytes as f64 / pq_bytes.max(1) as f64,

        merge_write_amplification: merge.write_amplification,
        merge_parts_in: merge.parts_in,
        merge_parts_out: merge.parts_out,
        merge_duplicates_reconciled: merge.duplicates_reconciled,
    })
}
