//! Deriving tuned constants from measurement (charter amendment C-1).
//!
//! > *"Every tuned constant must be pinned to committed benchmark evidence, with a
//! > test asserting the constant matches the evidence."*
//!
//! `DEFAULT_NPROBE` got its receipt in S1 (`prism golden sweep`). This is the other
//! one: **`BLOCK_SIZE`**, which was picked in S1 at 64 KiB because 64 KiB is what
//! people pick. Under C-1 that is not good enough, so it is derived here.
//!
//! The trade-off is real and two-sided, which is why it *has* an optimum:
//!
//! * **Bigger blocks** waste bytes on a small ranged read. A centroid range is a few
//!   hundred bytes of PQ codes; fetching it out of a 1 MiB block reads 1 MiB. That is
//!   read amplification, and it is what the whole ranged-read design exists to avoid.
//! * **Smaller blocks** grow the block directory carried in *every manifest* — which
//!   every reader pays on *every open*, including the opens that prune the part away
//!   without reading a single column — and add a 24-byte frame header per block.
//!
//! So the sweep builds a real store at each candidate size, runs the real golden
//! query set against it, and counts the bytes that actually move. No model, no
//! roofline: a store, a query, a number.

use crate::bench::BenchOpts;
use crate::engine::Engine;
use crate::oracle;
use crate::tsv;
use prism_part::store::StoreConfig;
use prism_types::error::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Instant;

/// How much manifest block directory a part may carry per stored row.
///
/// A **policy** constant (charter C-1): not an empirical optimum, a statement about
/// what has to remain true at scale. The directory is read in full on every part
/// open — including the opens that prune the part away without reading a column — so
/// it must stay small enough that a billion-row part is still cheap to *consider*.
/// 4 bytes/row keeps that directory in the low gigabytes at a billion rows.
pub const MANIFEST_BUDGET_BYTES_PER_ROW: f64 = 4.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockSizeRow {
    pub block_size: u32,
    /// Bytes of block directory carried in the manifest. Paid on every open, even
    /// by a query that prunes the part away.
    pub manifest_bytes: usize,
    /// Bytes physically read from column files to answer the golden query set —
    /// frame headers and over-read included, because that is what the disk does.
    pub bytes_read: usize,
    /// bytes actually read / bytes logically wanted. 1.0 would be perfect.
    pub read_amplification: f64,
    /// Total bytes moved: what a reader pays per query, all in.
    pub total_bytes: usize,
    pub query_p50_ms: f64,
    pub recall_at_10: f32,
    /// Manifest block-directory bytes per stored row. The number to extrapolate
    /// with: at 2,000 rows the manifest term is negligible, and at a billion rows it
    /// is not.
    pub manifest_bytes_per_row: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockSizeEvidence {
    pub constant: String,
    pub chosen_block_size: u32,
    pub rule: String,
    pub corpus_rows: usize,
    pub dim: usize,
    pub nlist: usize,
    pub pq_m: usize,
    pub queries: usize,
    pub sweep: Vec<BlockSizeRow>,
    pub note: String,
}

/// Build a store at each candidate block size and measure what queries really cost.
pub fn sweep_block_size(workdir: &Path, corpus_tsv: &Path) -> Result<BlockSizeEvidence> {
    // The range must bracket the answer. A sweep whose winner is its own smallest
    // candidate has not found an optimum, it has hit a wall — so the range extends
    // well below anything anyone would plausibly choose.
    let candidates: [u32; 9] = [
        512,
        1024,
        2 * 1024,
        4 * 1024,
        8 * 1024,
        16 * 1024,
        64 * 1024,
        256 * 1024,
        1024 * 1024,
    ];

    let events = tsv::parse(&std::fs::read_to_string(corpus_tsv)?)?;
    let corpus_rows = events.len();
    let base = BenchOpts::default();

    let mut sweep = Vec::new();

    for bs in candidates {
        let root = workdir.join(format!("bs-{bs}"));
        if root.exists() {
            std::fs::remove_dir_all(&root)?;
        }

        let engine = Engine::init(
            &root,
            StoreConfig {
                format_version: prism_part::store::STORE_VERSION,
                dim: base.dim,
                nlist: base.nlist,
                pq_m: base.pq_m,
                seed: 1234,
                block_size: bs,
            },
        )?;
        engine.ingest(events.clone(), 1_760_000_000_000)?;

        // The manifest cost: the block directory, paid on every open.
        let snap = engine.snapshot()?;
        let readers = engine.open_parts(&snap)?;
        let manifest_bytes: usize = readers
            .iter()
            .map(|r| r.manifest.encode().map(|b| b.len()).unwrap_or(0))
            .sum();

        // The read cost: what the disk actually moves to answer the golden queries.
        let golden = oracle::build(&engine, "zipf", corpus_rows, 1234, 10)?;

        let mut bytes_read = 0usize;
        let mut bytes_wanted = 0usize;
        let mut latencies: Vec<f64> = Vec::new();

        for exp in &golden.expectations {
            let q = exp.query.to_query();
            let t = Instant::now();
            let res = engine.search(&q)?;
            latencies.push(t.elapsed().as_secs_f64() * 1000.0);

            // Logical bytes the plan asked for...
            bytes_wanted += res.counters.pq_bytes_scanned + res.counters.exact_bytes_fetched;
            // ...and the bytes the disk actually moved to satisfy it. Measured by the
            // reader itself, not modelled: whole blocks, frame headers, over-read and
            // all. The gap between the two IS the block size's cost.
            bytes_read += res.counters.physical_bytes_read;
        }

        latencies.sort_by(|a, b| a.total_cmp(b));
        let p50 = latencies[latencies.len() / 2];

        let recall = oracle::measure_recall(
            &engine,
            &golden,
            prism_types::query::DEFAULT_NPROBE,
            base.candidates,
            base.rerank,
        )?;

        sweep.push(BlockSizeRow {
            block_size: bs,
            manifest_bytes,
            bytes_read,
            read_amplification: bytes_read as f64 / bytes_wanted.max(1) as f64,
            total_bytes: bytes_read + manifest_bytes * golden.expectations.len(),
            query_p50_ms: p50,
            recall_at_10: recall.mean_recall,
            manifest_bytes_per_row: manifest_bytes as f64 / corpus_rows.max(1) as f64,
        });

        std::fs::remove_dir_all(&root).ok();
    }

    // --- the rule, and why it is not simply "fewest bytes" ---
    //
    // A naive "minimise total bytes per query" objective picks the *smallest* block
    // in the sweep, every time, and it is wrong — because at a 2,000-row corpus the
    // manifest term is invisible, and the manifest is the term that does not scale.
    //
    // The block directory is read **in full on every part open**, including opens
    // that then prune the part away without touching a column. At 512-byte blocks it
    // costs ~16 bytes per row: a billion-row part would carry a 16 GB directory that
    // every reader must load before it can decide the part is irrelevant. No number
    // of saved read-bytes buys that back.
    //
    // So the objective is constrained: **minimise bytes physically read, subject to
    // the manifest staying under MANIFEST_BUDGET_BYTES_PER_ROW.** The budget is a
    // *policy* constant with a rationale (a manifest must remain openable at a
    // billion rows); the block size is a *tuned* constant derived under it. That
    // split is the whole point of charter amendment C-1 — the empirical question is
    // answered by measurement, and the question measurement cannot answer is answered
    // in prose and reviewed.
    let eligible: Vec<&BlockSizeRow> = sweep
        .iter()
        .filter(|r| r.manifest_bytes_per_row <= MANIFEST_BUDGET_BYTES_PER_ROW)
        .collect();

    let best = eligible
        .iter()
        .min_by(|a, b| {
            a.bytes_read
                .cmp(&b.bytes_read)
                .then(a.block_size.cmp(&b.block_size))
        })
        .expect("no candidate block size fits the manifest budget")
        .to_owned()
        .clone();

    let queries = sweep.len();
    Ok(BlockSizeEvidence {
        constant: "BLOCK_SIZE".to_string(),
        chosen_block_size: best.block_size,
        rule: format!(
            "the block size minimising physically-read bytes across the golden query set \
             (whole blocks, frame headers and block over-read included, as measured by the \
             reader itself), SUBJECT TO the manifest block directory staying under \
             {MANIFEST_BUDGET_BYTES_PER_ROW} bytes per row. The constraint is what stops the \
             sweep collapsing onto its own smallest candidate: the directory is read in full on \
             every part open, including opens that prune the part away, so at 512-byte blocks a \
             billion-row part would carry a ~16 GB directory. Ties go to the smaller block, \
             because smaller blocks localize damage more tightly."
        ),
        corpus_rows,
        dim: base.dim,
        nlist: base.nlist,
        pq_m: base.pq_m,
        queries,
        sweep,
        note: "Measured, not modelled: a real store is built at each candidate size and the real \
               golden query set is run against it. Re-derive with `prism evidence block-size` \
               whenever the corpus, the index geometry, or the column layout changes. The value \
               is a DEFAULT, not a law — every part records its own block size, so a store built \
               at one size stays readable whatever this default later becomes."
            .to_string(),
    })
}
