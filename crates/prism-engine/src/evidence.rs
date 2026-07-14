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
                partitions: Default::default(),
                promote: Vec::new(),
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

// --- candidate width x rerank width, swept JOINTLY (S3, charter C-1) -----------
//
// `DEFAULT_CANDIDATES` and `DEFAULT_RERANK` were classified `policy` in S2, and the
// ledger admitted in writing that they were "empirical questions wearing a policy hat".
// They are now `tuned`, and this is their derivation.
//
// **They are swept jointly, and that is not a stylistic choice.** The two controls
// interact: the candidate width decides *who is allowed to be reranked*, and the rerank
// width decides *how many of those actually are*. A rerank budget of 200 buys nothing if
// only 50 candidates ever entered the heap, and a candidate width of 500 buys nothing if
// only 10 of them are ever scored exactly. An independent single-axis sweep of either one
// measures a cross-section of a surface and reports it as the surface.
//
// `nprobe` is held at its own receipted value throughout. Sweeping three interacting
// controls at once would produce a receipt nobody could interpret, and `nprobe` already
// has one.

/// How many rows the default plan must be able to produce.
///
/// A **policy** constant (charter C-1): not an empirical optimum, a statement about what
/// has to remain true. Measurement cannot see it, and it is the constraint that actually
/// binds.
///
/// **Pagination's result set IS the plan's rerank survivors** (see `docs/QUERY-CONTRACT.md`).
/// A rerank width of 10, with a default page size of 10, means the first page is the entire
/// result set and the cursor is decorative. So the default plan must be able to serve at
/// least five pages at the default page size: `5 x 10 = 50`.
///
/// Without this bound, the sweep below chooses `rerank = 10` — because on the golden corpus
/// the recall floors do not bind *at all*: every point in the grid clears them, since PQ's
/// top-10 already contains the true top-10. That is a property of a synthetic corpus with
/// well-separated motifs, and tuning to it would be overfitting to the easiest thing we own.
pub const MIN_PAGEABLE_ROWS: usize = 50;

/// One point on the (candidates x rerank) surface.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WidthRow {
    pub candidates: usize,
    pub rerank: usize,
    pub mean_recall: f32,
    pub p1_recall: f32,
    pub min_recall: f32,
    pub zero_recall_queries: usize,
    /// The number that costs money: exact vectors are ~32x the size of the codes, and
    /// this is how many of them the plan pulls.
    pub mean_exact_bytes: f64,
    pub mean_physical_bytes: f64,
    pub query_p50_ms: f64,
    /// Does this point clear both tail floors?
    pub meets_floor: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WidthEvidence {
    pub constants: Vec<String>,
    pub chosen_candidates: usize,
    pub chosen_rerank: usize,
    pub held_nprobe: usize,
    pub p1_floor: f32,
    pub corpus_version: String,
    pub queries: usize,
    pub rule: String,
    pub sweep: Vec<WidthRow>,
    pub note: String,
}

/// Sweep the two widths jointly against the frozen golden corpus.
pub fn sweep_widths(
    engine: &Engine,
    golden: &oracle::Golden,
    corpus_version: &str,
    p1_floor: f32,
) -> Result<WidthEvidence> {
    let nprobe = prism_types::query::DEFAULT_NPROBE;
    let candidate_grid = [25usize, 50, 100, 200, 400, 800];
    let rerank_grid = [10usize, 25, 50, 100, 200, 400];

    let mut sweep = Vec::new();

    for &cand in &candidate_grid {
        for &rr in &rerank_grid {
            // A rerank budget wider than the candidate list is not a configuration, it is
            // a misunderstanding: you cannot exactly score rows that never entered the
            // heap. Skipping these keeps the receipt honest rather than padding it with
            // points that are really just `rerank = candidates`.
            if rr > cand {
                continue;
            }

            let report = oracle::measure_recall(engine, golden, nprobe, cand, rr)?;

            // Cost is measured, not modelled.
            let mut exact_bytes = 0usize;
            let mut phys_bytes = 0usize;
            let mut lat: Vec<f64> = Vec::new();
            for exp in &golden.expectations {
                let mut q = exp.query.to_query();
                q.nprobe = nprobe;
                q.candidates = cand;
                q.rerank = rr;
                let t = Instant::now();
                let res = engine.search(&q)?;
                lat.push(t.elapsed().as_secs_f64() * 1000.0);
                exact_bytes += res.counters.exact_bytes_fetched;
                phys_bytes += res.counters.physical_bytes_read;
            }
            lat.sort_by(|a, b| a.total_cmp(b));
            let n = golden.expectations.len().max(1);

            sweep.push(WidthRow {
                candidates: cand,
                rerank: rr,
                mean_recall: report.mean_recall,
                p1_recall: report.p1_recall,
                min_recall: report.min_recall,
                zero_recall_queries: report.zero_recall_queries,
                mean_exact_bytes: exact_bytes as f64 / n as f64,
                mean_physical_bytes: phys_bytes as f64 / n as f64,
                query_p50_ms: lat[lat.len() / 2],
                meets_floor: report.p1_recall >= p1_floor && report.zero_recall_queries == 0,
            });
        }
    }

    // --- the rule, and the constraint that actually binds ---
    //
    // Among the points that clear BOTH tail floors -- the same floors nprobe was chosen
    // against, because a control that quietly relaxes the recall contract is a downgrade
    // and not a tuning choice -- take the smallest rerank width, then the smallest candidate
    // width. Rerank leads because it is the expensive one: an exact vector is ~32x a coded
    // row, so one rerank fetch costs 32 rows of scanning; the candidate heap costs memory,
    // not I/O.
    //
    // But **on this corpus the recall floors do not bind at all**: every point in the grid
    // clears them, because PQ's top-10 already contains the true top-10. Left there, the rule
    // would choose `rerank = 10` -- the hard floor, since you cannot return k=10 hits from
    // fewer than 10 reranked rows -- and that would be overfitting to a synthetic corpus with
    // unusually well-separated motifs.
    //
    // It would also quietly break pagination. The paginated result set IS the rerank survivor
    // set, so rerank=10 with a default page size of 10 makes the first page the whole result
    // and the cursor decorative. That is a real constraint, measurement cannot see it, and so
    // it is stated as policy: MIN_PAGEABLE_ROWS.
    let best = sweep
        .iter()
        .filter(|r| r.meets_floor && r.rerank >= MIN_PAGEABLE_ROWS)
        .min_by(|a, b| {
            a.rerank
                .cmp(&b.rerank)
                .then(a.candidates.cmp(&b.candidates))
        })
        .ok_or_else(|| {
            prism_types::error::PrismError::Invariant(format!(
                "no (candidates, rerank) pair clears the tail floors at nprobe={nprobe}; the \
                 floors or the index have to change"
            ))
        })?
        .clone();

    Ok(WidthEvidence {
        constants: vec!["DEFAULT_CANDIDATES".into(), "DEFAULT_RERANK".into()],
        chosen_candidates: best.candidates,
        chosen_rerank: best.rerank,
        held_nprobe: nprobe,
        p1_floor,
        corpus_version: corpus_version.to_string(),
        queries: golden.expectations.len(),
        rule: format!(
            "swept JOINTLY, because the two controls interact: the candidate width decides who \
             is allowed to be reranked and the rerank width decides how many of them actually \
             are, so an independent single-axis sweep of either measures a cross-section of a \
             surface and reports it as the surface. nprobe is held at its own receipted value \
             ({nprobe}). Among the points clearing BOTH tail floors -- p1 recall@10 >= \
             {p1_floor} AND zero_recall_queries == 0, the same floors nprobe was chosen against, \
             because a control that quietly relaxes the recall contract is a downgrade and not a \
             tuning choice -- take the smallest RERANK width first, then the smallest CANDIDATE \
             width. Rerank leads because it is the expensive one: an exact vector is ~32x a \
             coded row, so one rerank fetch costs 32 rows of scanning; the candidate heap costs \
             memory, not I/O. SUBJECT TO rerank >= {} (MIN_PAGEABLE_ROWS), because the paginated \
             result set IS the rerank survivor set and a rerank width of 10 with a page size of \
             10 makes the cursor decorative. THE RECALL FLOORS DO NOT BIND ON THIS CORPUS -- \
             every point in the grid clears them -- so the pagination bound is what actually \
             selects the value. If a future corpus makes recall bind, this receipt moves.",
            MIN_PAGEABLE_ROWS
        ),
        sweep,
        note: "Measured against the FROZEN golden corpus (charter C-2), whose version is named \
               above. Re-derive with `prism evidence widths` if the corpus version, the index \
               geometry, or the embedder changes -- and note that a new corpus version does not \
               invalidate this receipt, it simply means this receipt describes the corpus it \
               names."
            .to_string(),
    })
}
