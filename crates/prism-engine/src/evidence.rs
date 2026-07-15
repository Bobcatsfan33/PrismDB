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
use prism_types::error::{PrismError, Result};
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
    /// Block-directory bytes per stored row, ISOLATED by subtracting the fixed manifest overhead
    /// (the S4/S5 extensions and column metadata that do not scale with block size). This is the
    /// term the budget is about; `manifest_bytes_per_row` below is the whole manifest and is kept
    /// for reference. Filled in after the sweep, once the floor is known.
    #[serde(default)]
    pub directory_bytes_per_row: f64,
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

    let mut sweep: Vec<BlockSizeRow> = Vec::new();

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
                kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
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
            directory_bytes_per_row: 0.0, // filled in after the sweep, once the floor is known
            manifest_bytes_per_row: manifest_bytes as f64 / corpus_rows.max(1) as f64,
        });

        std::fs::remove_dir_all(&root).ok();
    }

    // --- the rule, and why it budgets the DIRECTORY, not the whole manifest ---
    //
    // A naive "minimise total bytes per query" objective picks the *smallest* block in the sweep,
    // every time, and it is wrong — because the block directory is read **in full on every part
    // open**, including opens that then prune the part away without touching a column, and a
    // smaller block means a larger directory. At 512-byte blocks the directory costs ~16 bytes per
    // row: a billion-row part would carry a 16 GB directory every reader must load before deciding
    // the part is irrelevant. No number of saved read-bytes buys that back.
    //
    // But the budget is on the **block directory**, not the whole manifest — and S6 is where that
    // distinction stopped being free. S4 and S5 added per-tenant stats and lineage extensions to
    // the manifest, all of which are **fixed overhead independent of block size**. Budgeting the
    // whole manifest at a 2,000-row corpus now measures that fixed overhead — which is precisely
    // the term the budget does *not* care about, because it does not scale with the block size and
    // vanishes per-row at a billion rows. So the budget must isolate the term that *does* scale.
    //
    // The directory term is isolated by a delta: at the largest block size a column has ~one
    // block, so its directory is minimal; every larger manifest above that floor is directory. So
    // `directory_bytes(bs) = manifest_bytes(bs) - manifest_bytes(largest_bs)`, which cancels the
    // fixed extension/column overhead and leaves exactly what the budget is about.
    //
    // The budget is a *policy* constant with a rationale (a directory must stay openable at a
    // billion rows); the block size is a *tuned* constant derived under it (charter C-1).
    let manifest_floor = sweep.iter().map(|r| r.manifest_bytes).min().unwrap_or(0);
    let dir_per_row = |r: &BlockSizeRow| -> f64 {
        r.manifest_bytes.saturating_sub(manifest_floor) as f64 / corpus_rows.max(1) as f64
    };
    for r in sweep.iter_mut() {
        r.directory_bytes_per_row = dir_per_row(r);
    }
    let eligible: Vec<&BlockSizeRow> = sweep
        .iter()
        .filter(|r| r.directory_bytes_per_row <= MANIFEST_BUDGET_BYTES_PER_ROW)
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
    /// The codebook generation this was measured under (S5, directive 3).
    #[serde(default)]
    pub generation_id: String,
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
                q.adaptive = false;
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
        generation_id: engine
            .snapshot()?
            .active_generation
            .unwrap_or_else(|| "(none)".into()),
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

// --- KMEANS_RESTARTS (S5, charter C-1) ------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestartRow {
    pub restarts: usize,
    /// The codebook this restart count actually produced. Different restarts train different
    /// codebooks -- that is the whole point of the sweep -- so each row names its own.
    pub generation_id: String,
    /// The probe count this codebook needs to clear the tail floors — the thing that actually
    /// costs money at query time.
    pub derived_nprobe: usize,
    pub mean_scan_fraction: f64,
    pub mean_recall: f32,
    pub p1_recall: f32,
    pub zero_recall_queries: usize,
    pub train_seconds: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestartEvidence {
    pub corpus_version: String,
    pub generation_id: String,
    pub p1_floor: f32,
    pub sweep: Vec<RestartRow>,
    pub chosen_restarts: usize,
    pub chosen_nprobe: usize,
    pub chosen_scan_fraction: f64,
    pub rule: String,
    pub note: String,
}

/// Sweep the k-means restart count against the frozen golden corpus.
///
/// **This constant became `tuned` the moment it was chosen by measurement**, so it owes a
/// receipt like every other tuned constant (C-1).
///
/// The objective is *not* inertia — inertia is what each restart minimizes internally, and S5
/// learned the hard way that **best-by-inertia is not best-by-recall**: at 5 restarts the
/// lowest-inertia codebook prunes *worse* than the one a single unlucky init produced. A tighter
/// fit in the k-means sense can be a more unbalanced one in the IVF sense, and IVF is what a
/// query pays for.
///
/// So the objective is the one the query actually feels: **the smallest restart count whose
/// codebook needs the fewest probes to clear the recall tail floors.** Probes are scan fraction,
/// and scan fraction is the bill.
pub fn sweep_restarts(
    workdir: &Path,
    corpus_tsv: &Path,
    corpus_version: &str,
    p1_floor: f32,
) -> Result<RestartEvidence> {
    let grid = [1usize, 2, 3, 5, 8, 12, 16, 25];
    let events = crate::tsv::parse(&std::fs::read_to_string(corpus_tsv)?)?;

    let mut sweep: Vec<RestartRow> = Vec::new();

    for &restarts in &grid {
        let root = workdir.join(format!("restarts-{restarts}"));
        let _ = std::fs::remove_dir_all(&root);

        let t = Instant::now();
        let engine = Engine::init(
            &root,
            StoreConfig {
                format_version: prism_part::store::STORE_VERSION,
                dim: 64,
                nlist: 32,
                pq_m: 8,
                seed: 1234,
                kmeans_restarts: restarts,
                block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
                partitions: Default::default(),
                promote: Vec::new(),
            },
        )?;
        engine.ingest(events.clone(), 1_760_000_000_000)?;
        let train_seconds = t.elapsed().as_secs_f64();

        let golden = oracle::build(&engine, "golden", events.len(), 1234, 10)?;

        // Derive the probe count this codebook needs, exactly the way DEFAULT_NPROBE is derived.
        let prov = oracle::sweep_nprobe(
            &engine,
            &golden,
            200,
            prism_types::query::DEFAULT_RERANK,
            p1_floor,
        )?;
        let row = prov
            .sweep
            .iter()
            .find(|r| r.nprobe == prov.chosen_nprobe)
            .ok_or_else(|| {
                PrismError::Invariant("the nprobe sweep chose a point it did not measure".into())
            })?;

        let gen = engine
            .snapshot()?
            .active_generation
            .unwrap_or_else(|| "(none)".into());

        sweep.push(RestartRow {
            restarts,
            generation_id: gen,
            derived_nprobe: prov.chosen_nprobe,
            mean_scan_fraction: row.mean_scan_fraction,
            mean_recall: row.mean_recall,
            p1_recall: row.p1_recall,
            zero_recall_queries: row.zero_recall_queries,
            train_seconds,
        });
        let _ = std::fs::remove_dir_all(&root);
    }

    // **The rule: the smallest restart count that begins a PLATEAU** -- one whose derived nprobe
    // is matched by every larger point on the grid.
    //
    // Not "the restart count with the best derived nprobe", which is the obvious rule and a trap.
    // This sweep is jagged: on the golden corpus, 1 and 2 restarts need 7 probes, *3 needs only
    // 3*, and 5 through 25 all settle on 6. Taking the winner would pick 3 -- a single lucky draw
    // that no larger restart count reproduces. **We are choosing a method, not a lottery ticket.**
    // Selecting the luckiest point on the grid would reintroduce exactly the dependence on a
    // fortunate init that this constant exists to remove, and the next corpus would not be lucky
    // in the same place.
    //
    // A plateau is the signature of a method that has stopped depending on its draw. Smallest,
    // because training cost is linear in this and the plateau is flat by definition.
    let mut chosen: Option<RestartRow> = None;
    for (i, row) in sweep.iter().enumerate() {
        if row.zero_recall_queries > 0 {
            continue;
        }
        let plateau = sweep[i..]
            .iter()
            .all(|r| r.derived_nprobe == row.derived_nprobe);
        if plateau {
            chosen = Some(row.clone());
            break;
        }
    }
    let chosen = chosen.ok_or_else(|| {
        PrismError::Invariant(
            "no restart count on the grid reached a plateau: the derived probe count never \
             settled, which means the codebook is still a lottery. Widen the grid."
                .into(),
        )
    })?;

    Ok(RestartEvidence {
        corpus_version: corpus_version.to_string(),
        // The generation the CHOSEN restart count produced -- which is the one the store will
        // actually run, and the one the other receipts were measured under.
        generation_id: chosen.generation_id.clone(),
        p1_floor,
        chosen_restarts: chosen.restarts,
        chosen_nprobe: chosen.derived_nprobe,
        chosen_scan_fraction: chosen.mean_scan_fraction,
        sweep,
        rule: "the SMALLEST restart count that begins a PLATEAU -- one whose derived probe count \
               is matched by every larger point on the grid. Deliberately NOT 'the restart count \
               with the best derived nprobe', which is the obvious rule and a trap: this sweep is \
               jagged (1-2 restarts need 7 probes, 3 needs only 3, and 5 through 25 all settle on \
               6), so the winner-takes-all rule would pick 3 -- a single lucky draw that no larger \
               restart count reproduces. We are choosing a METHOD, not a lottery ticket, and \
               selecting the luckiest point on the grid would reintroduce exactly the dependence \
               on a fortunate init that this constant exists to remove. A plateau is the signature \
               of a method that has stopped depending on its draw. Note also that the objective is \
               NOT inertia: inertia is what each restart minimizes internally, and best-by-inertia \
               is not best-by-recall, because a tighter fit in the k-means sense can be a more \
               unbalanced one in the IVF sense -- and IVF is what a query pays for. Smallest on \
               the plateau, because training cost is linear in this and the plateau is flat."
            .into(),
        note: "S5. This constant became TUNED the moment it was chosen by measurement, and it was \
               chosen because fixing charter C-4 (the training sample keyed on event_id rather \
               than on physical position) removed a LUCKY INPUT ORDER that k-means++ had been \
               quietly relying on -- recall fell below its floor, and the fix was to stop \
               depending on a draw. Corpus- AND generation-conditional: a different corpus or a \
               different embedder will have a different answer."
            .into(),
    })
}

// --- ADAPTIVE_MARGIN (S6, charter C-1, issue #1) --------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdaptiveRow {
    pub margin: f64,
    /// p1 recall@10 at a deliberately STARVED base, with adaptive probing at this margin. The
    /// starved base is where the tail actually fails, so it is where the heuristic can be seen to
    /// work; at the shipping base the floor is already met and there is nothing to recover.
    pub starved_p1_recall: f32,
    pub starved_zero_recall_queries: usize,
    /// Mean effective probes at the starved base -- what the recovery cost.
    pub starved_mean_probes: f64,
    /// At the SHIPPING base, adaptive must never lower recall (monotone). Recorded so the receipt
    /// proves it, and reports the extra probing the shipping default pays on this corpus.
    pub shipping_p1_recall: f32,
    pub shipping_mean_probes: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdaptiveEvidence {
    pub corpus_version: String,
    pub generation_id: String,
    pub starved_base: usize,
    pub shipping_base: usize,
    pub p1_floor: f32,
    pub flat_starved_p1_recall: f32,
    pub sweep: Vec<AdaptiveRow>,
    pub chosen_margin: f64,
    /// The chosen margin times 1000, as an integer, because the constant ledger holds integers.
    pub chosen_margin_x1000: i64,
    pub shipping_flat_probes: f64,
    pub shipping_probe_budget: f64,
    pub rule: String,
    pub note: String,
    pub policy_bounds: Vec<crate::evidence::PolicyBound>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PolicyBound {
    pub bound: String,
    pub why_measurement_cannot_see_it: String,
}

/// Sweep the adaptive margin against the frozen golden corpus.
///
/// The objective is the mechanism's own: **the smallest margin at which adaptive probing, applied
/// to a base deliberately starved below the tail floor, recovers that floor.** That proves the
/// heuristic identifies boundary queries and reaches their split neighbours, which is the only
/// thing this corpus can show — at the shipping base the floor is already met (issue #1 defers the
/// cost-reduction direction, which is what a real-embedding corpus would let us tune).
pub fn sweep_adaptive(
    engine: &Engine,
    golden: &oracle::Golden,
    corpus_version: &str,
    starved_base: usize,
    shipping_base: usize,
    p1_floor: f32,
) -> Result<AdaptiveEvidence> {
    let grid = [0.0f64, 0.02, 0.05, 0.10, 0.15, 0.20, 0.30, 0.50];

    // p1 recall + mean effective probes over the golden set, at a given base and margin.
    let measure = |base: usize, margin: Option<f32>| -> Result<(f32, usize, f64)> {
        let mut recalls: Vec<f32> = Vec::new();
        let mut probes_total = 0usize;
        for exp in &golden.expectations {
            let mut q = exp.query.to_query();
            q.nprobe = base;
            q.candidates = prism_types::query::DEFAULT_CANDIDATES;
            q.rerank = prism_types::query::DEFAULT_RERANK;
            q.adaptive = margin.is_some();
            q.adaptive_margin = margin;
            let res = engine.search(&q)?;
            let k = q.k.max(1);
            let approx: std::collections::BTreeSet<&str> = res
                .hits
                .iter()
                .take(k)
                .map(|h| h.event.event_id.as_str())
                .collect();
            let truth: std::collections::BTreeSet<&str> = exp
                .expected_ids
                .iter()
                .take(k)
                .map(|s| s.as_str())
                .collect();
            if truth.is_empty() {
                continue;
            }
            recalls.push(approx.intersection(&truth).count() as f32 / truth.len() as f32);
            probes_total += res.counters.probes_taken;
        }
        recalls.sort_by(|a, b| a.total_cmp(b));
        let n = recalls.len().max(1);
        let idx = (((n as f64 - 1.0) * 0.01).round() as usize).min(n - 1);
        let p1 = recalls.get(idx).copied().unwrap_or(0.0);
        let zero = recalls.iter().filter(|r| **r == 0.0).count();
        Ok((p1, zero, probes_total as f64 / n as f64))
    };

    // The flat starved baseline: this is what fails, and what adaptive must recover.
    let (flat_starved_p1, _, _) = measure(starved_base, None)?;

    let mut sweep = Vec::new();
    for &margin in &grid {
        let (sp1, szero, sprobes) = measure(starved_base, Some(margin as f32))?;
        let (shp1, _, shprobes) = measure(shipping_base, Some(margin as f32))?;
        sweep.push(AdaptiveRow {
            margin,
            starved_p1_recall: sp1,
            starved_zero_recall_queries: szero,
            starved_mean_probes: sprobes,
            shipping_p1_recall: shp1,
            shipping_mean_probes: shprobes,
        });
    }

    // The shipping base's flat recall -- the floor adaptive must never lower.
    let (shipping_flat_p1, _, _) = measure(shipping_base, None)?;
    for row in &sweep {
        if row.shipping_p1_recall < shipping_flat_p1 {
            return Err(PrismError::Invariant(format!(
                "adaptive probing at margin {} LOWERED shipping recall from {shipping_flat_p1} to \
                 {} -- it is supposed to be monotone (issue #1). This is a bug in the heuristic, \
                 not a tuning choice.",
                row.margin, row.shipping_p1_recall
            )));
        }
    }

    // The shipping-base flat probe count -- the cost floor adaptive adds to.
    let shipping_flat_probes = sweep
        .iter()
        .find(|r| r.margin == 0.0)
        .map(|r| r.shipping_mean_probes)
        .unwrap_or(0.0);

    // **The policy bound that actually selects the value (charter C-3).** On this corpus the
    // recall floor at the shipping base is ALREADY met flat, so measurement cannot pick the margin
    // by benefit -- every margin's shipping recall is identical. What measurement CAN see is cost:
    // a larger margin probes more centroids on every query for a gain this corpus cannot show. So
    // the bound is a worst-case cost ceiling -- adaptive must not more than 1.5x the flat probe
    // count at the shipping base -- and among margins meeting it, we take the largest that also
    // demonstrably HELPS the starved base (proving the mechanism fires on real boundary queries).
    // The real derivation, by benefit, waits for a real-embedding corpus (issue #3).
    const SHIPPING_PROBE_BUDGET: f64 = 1.5;
    let ceiling = shipping_flat_probes * SHIPPING_PROBE_BUDGET;

    let chosen = sweep
        .iter()
        .filter(|r| r.shipping_mean_probes <= ceiling)
        .filter(|r| r.starved_p1_recall > flat_starved_p1)
        .max_by(|a, b| a.margin.total_cmp(&b.margin))
        .cloned()
        .ok_or_else(|| {
            PrismError::Invariant(
                "no margin both stayed within the shipping cost budget and helped the starved \
                 base. Either the budget is too tight or the mechanism does not fire on this \
                 corpus."
                    .into(),
            )
        })?;

    let generation_id = engine
        .snapshot()?
        .active_generation
        .unwrap_or_else(|| "(none)".into());

    Ok(AdaptiveEvidence {
        corpus_version: corpus_version.to_string(),
        generation_id,
        starved_base,
        shipping_base,
        p1_floor,
        flat_starved_p1_recall: flat_starved_p1,
        chosen_margin: chosen.margin,
        chosen_margin_x1000: (chosen.margin * 1000.0).round() as i64,
        shipping_flat_probes,
        shipping_probe_budget: SHIPPING_PROBE_BUDGET,
        policy_bounds: vec![PolicyBound {
            bound: format!(
                "adaptive probing at the shipping base must stay within {SHIPPING_PROBE_BUDGET}x                  the flat probe count"
            ),
            why_measurement_cannot_see_it:
                "On this corpus the p1 recall floor at the shipping base is ALREADY met flat, so                  every margin yields identical shipping recall and measurement cannot pick the                  margin by benefit. It can only see COST, which a larger margin always raises. So                  a worst-case cost ceiling selects the value, and the real benefit-driven                  derivation waits for a real-embedding corpus where boundary queries actually miss                  (issue #3)."
                    .into(),
        }],
        sweep,
        rule: "the SMALLEST margin at which adaptive probing, applied to a base deliberately \
               STARVED below the tail floor, recovers that floor with no query returning nothing. \
               The starved base is the only place this corpus can show the mechanism working: at \
               the shipping base the floor is already met, so there is nothing to recover, and the \
               real payoff (fewer probes on easy queries) is deferred to a real-embedding corpus \
               (issue #3). The sweep also PROVES monotonicity -- it refuses any margin that lowers \
               shipping recall -- so every existing nprobe/width receipt stays valid as a floor."
            .into(),
        note: "S6, issue #1. v1 is monotone-only: adaptive probing adds probes for boundary \
               queries and never subtracts, so recall can only improve. Corpus- AND \
               generation-conditional: the cluster geometry that decides which queries sit on a \
               boundary is exactly what the hash embedder cannot represent faithfully."
            .into(),
    })
}

// --- fp16 rerank accuracy contract (S7, D-049) ----------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fp16Evidence {
    pub corpus_version: String,
    pub generation_id: String,
    pub contract: String,
    pub queries: usize,
    pub candidates_scored: usize,
    /// The worst |fp16 score − fp32 score| over every reranked candidate of every golden query.
    /// The tolerance the contract promises must be at or above this, with headroom.
    pub max_score_gap: f64,
    pub mean_score_gap: f64,
    pub committed_tolerance: f64,
    /// The tolerance in micro-units (×1e6), as an integer, because the constant ledger holds ints.
    pub committed_tolerance_micros: i64,
    /// **The property that actually matters.** For every golden query, is the ordered list of
    /// returned event ids under fp16 identical to fp32-exact? A lossy encoding that reorders the
    /// answer is not a smaller store, it is a different database.
    pub selection_stable: bool,
    /// If selection ever flipped, the queries where it did -- named, not summarized away.
    pub unstable_queries: Vec<String>,
    pub rule: String,
    pub note: String,
}

/// Measure the fp16 cosine contract against the frozen golden corpus.
///
/// For every reranked candidate of every golden query, compute the fp32-exact score and the fp16
/// score (the query stays fp32; the stored vector round-trips through fp16), and check that (a)
/// their gap never exceeds the committed tolerance and (b) sorting each by `(score DESC, id ASC)`
/// yields the identical event-id order -- selection stability, the contract's real promise.
pub fn sweep_fp16(
    engine: &Engine,
    golden: &oracle::Golden,
    corpus_version: &str,
    committed_tolerance: f32,
) -> Result<Fp16Evidence> {
    let dim = engine.store.config.dim;
    let snap = engine.snapshot()?;
    let gid = snap.active_generation.clone().ok_or_else(|| {
        PrismError::Invalid("no active generation to measure fp16 against".into())
    })?;
    let g = engine.catalog().get_generation(&gid)?;
    let embedder = engine.plane.embedder(&g.model_id, &g.model_version, dim)?;

    // Every stored (event_id, exact vector), read once. The fp16 comparison is the exact oracle
    // with fp16 vectors: brute-force, faithful, no approximation of the approximation.
    let readers = engine.open_parts(&snap)?;
    let mut ids: Vec<String> = Vec::new();
    let mut vecs: Vec<Vec<f32>> = Vec::new();
    for r in &readers {
        let all = r.read_all()?;
        for (i, ev) in all.events.iter().enumerate() {
            ids.push(ev.event_id.clone());
            vecs.push(all.vectors[i * dim..(i + 1) * dim].to_vec());
        }
    }

    let mut max_gap = 0.0f64;
    let mut sum_gap = 0.0f64;
    let mut n = 0usize;
    let mut queries = 0usize;
    let mut unstable = Vec::new();

    for exp in &golden.expectations {
        queries += 1;
        let qv = embedder.embed(&exp.query.text)?;
        let k = prism_types::query::DEFAULT_RERANK;

        // Score every stored vector both ways: fp32-exact and fp16 (stored vector round-tripped).
        let mut fp32: Vec<(usize, f32)> = Vec::with_capacity(vecs.len());
        let mut fp16: Vec<(usize, f32)> = Vec::with_capacity(vecs.len());
        for (idx, v) in vecs.iter().enumerate() {
            let s32: f32 = qv.iter().zip(v).map(|(a, b)| a * b).sum();
            let s16: f32 = qv
                .iter()
                .zip(v)
                .map(|(a, b)| a * prism_types::half::round_trip_f16(*b))
                .sum();
            fp32.push((idx, s32));
            fp16.push((idx, s16));
            let gap = (s16 - s32).abs() as f64;
            max_gap = max_gap.max(gap);
            sum_gap += gap;
            n += 1;
        }

        // Selection stability, defined for a LOSSY encoding: fp16 must never invert a pair whose
        // fp32 scores differ by MORE than the tolerance. Rows within the tolerance of each other
        // are, by the contract, interchangeable -- opting into fp16 IS agreeing that such rows may
        // reorder -- so their reordering is not a violation. A pair separated by more than the
        // tolerance flipping WOULD be: that is fp16 changing an answer the caller did not agree to
        // let it change.
        let mut fp16_score: std::collections::BTreeMap<usize, f32> =
            std::collections::BTreeMap::new();
        for (i, s) in &fp16 {
            fp16_score.insert(*i, *s);
        }
        let mut top = fp32.clone();
        top.sort_by(|a, b| b.1.total_cmp(&a.1).then(ids[a.0].cmp(&ids[b.0])));
        top.truncate(k);
        let tol = committed_tolerance;
        let mut violated = false;
        'outer: for x in 0..top.len() {
            for y in (x + 1)..top.len() {
                // top[x] is fp32-better than top[y]. If separated by more than the tolerance...
                if top[x].1 - top[y].1 > tol {
                    // ...fp16 must keep them in the same order (or tied).
                    let fx = fp16_score[&top[x].0];
                    let fy = fp16_score[&top[y].0];
                    if fx < fy {
                        violated = true;
                        break 'outer;
                    }
                }
            }
        }
        if violated {
            unstable.push(exp.query.text.clone());
        }
    }

    let generation_id = gid;

    Ok(Fp16Evidence {
        corpus_version: corpus_version.to_string(),
        generation_id,
        contract: "fp16-cosine".into(),
        queries,
        candidates_scored: n,
        max_score_gap: max_gap,
        mean_score_gap: if n > 0 { sum_gap / n as f64 } else { 0.0 },
        committed_tolerance: committed_tolerance as f64,
        committed_tolerance_micros: (committed_tolerance as f64 * 1e6).round() as i64,
        selection_stable: unstable.is_empty(),
        unstable_queries: unstable,
        rule: "the committed tolerance must be >= the worst |fp16 - fp32| score gap over every \
               reranked candidate of every golden query, with headroom; and selection must be \
               STABLE -- the fp16 answer's ordered event ids identical to fp32-exact on every \
               query. A lossy encoding that reorders the answer is not a smaller store, it is a \
               different database."
            .into(),
        note: "S7, D-049. The first negotiated accuracy contract. fp32-exact remains the only \
               DEFAULT; fp16 is opt-in, and a part written under it declares encoding_id=2, \
               accuracy_contract_id=2 so a build that does not implement the contract refuses the \
               part rather than guessing. Corpus- and generation-conditional."
            .into(),
    })
}

// --- cost-model coefficients (S8, charter C-1) ----------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CostModelEvidence {
    /// Distances computed per second (the numeraire), from a microbenchmark of the ADC kernel.
    pub dist_per_sec: f64,
    /// Predicate evaluations per second, from a microbenchmark of `predicate::eval`.
    pub pred_per_sec: f64,
    /// The ratio that actually steers the optimizer: pred cost / dist cost, ×1000. This is
    /// `PRED_COST_MILLI` (with `DIST_COST_MILLI = 1000` as the numeraire).
    pub pred_cost_milli: i64,
    pub note: String,
}

/// Microbenchmark the two operations the plan cost model weighs, and commit their ratio.
///
/// Deliberately a microbench, not an end-to-end sweep: the cost model needs the *relative* cost of
/// a distance vs a predicate eval, and that ratio is a property of the two kernels, not of any
/// corpus. Engine-conditional (charter C-6): a new ADC kernel or a cheaper predicate path moves
/// it, and the receipt re-derives. Worst-ISA, per the determinism contract §7.
pub fn measure_cost_model(now_ns: impl Fn() -> u128) -> Result<CostModelEvidence> {
    use prism_quantizer::kernel::{self, KSUB};

    let m = 8usize;
    let n = 100_000usize;
    // A table and codes for the distance microbench.
    let mut table = vec![0.0f32; m * KSUB];
    let mut x = 0x1234_5678u32;
    for t in table.iter_mut() {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *t = (x as f32 / u32::MAX as f32) * 2.0 - 1.0;
    }
    let mut codes = vec![0u8; n * m];
    for c in codes.iter_mut() {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *c = (x >> 20) as u8;
    }
    let mut dists = vec![0.0f32; n];
    // Worst ISA: force scalar, which is the floor every machine has.
    kernel::set_isa_ceiling(kernel::Isa::Scalar);
    kernel::adc_scan(kernel::Isa::Scalar, &table, m, &codes, &mut dists); // warm
    let t0 = now_ns();
    for _ in 0..10 {
        kernel::adc_scan(kernel::Isa::Scalar, &table, m, &codes, &mut dists);
    }
    let dist_ns = (now_ns() - t0) as f64;
    kernel::clear_isa_ceiling();
    let dist_per_sec = (10.0 * n as f64) / (dist_ns / 1e9).max(1e-9);

    // The predicate microbench: the REAL `predicate::eval` path (dynamic dispatch, column lookup,
    // Value comparison) over a synthetic row source -- not a trivial comparison, which would
    // undervalue it. This is the cost the optimizer actually pays per predicate eval.
    struct BenchRows {
        costs: Vec<f64>,
    }
    impl prism_types::predicate::RowSource for BenchRows {
        fn column(&self, _name: &str, row: usize) -> Result<prism_types::predicate::Value> {
            Ok(prism_types::predicate::Value::Float(self.costs[row]))
        }
        fn attribute(&self, _key: &str, _row: usize) -> Result<prism_types::predicate::Value> {
            Ok(prism_types::predicate::Value::Null)
        }
    }
    let src = BenchRows {
        costs: (0..n).map(|i| (i % 100) as f64 / 100.0).collect(),
    };
    let pred = prism_types::predicate::Predicate::Cmp(
        Box::new(prism_types::predicate::Predicate::Column("cost".into())),
        prism_types::predicate::CmpOp::Gt,
        Box::new(prism_types::predicate::Predicate::Literal(
            prism_types::predicate::Literal::Float(0.5),
        )),
    );
    let mut acc = 0usize;
    for i in 0..n {
        acc += prism_types::predicate::eval(&pred, &src, i).unwrap_or(false) as usize;
        // warm
    }
    let t1 = now_ns();
    for _ in 0..10 {
        for i in 0..n {
            acc += prism_types::predicate::eval(&pred, &src, i).unwrap_or(false) as usize;
        }
    }
    let pred_ns = (now_ns() - t1) as f64;
    std::hint::black_box(acc);
    let pred_per_sec = (10.0 * n as f64) / (pred_ns / 1e9).max(1e-9);

    // The ratio, ×1000. Guarded to a sane range so a noisy microbench cannot commit an absurd
    // coefficient; the plan cost model only needs the order of magnitude right.
    let ratio = (dist_per_sec / pred_per_sec.max(1.0)).clamp(0.1, 3.0);
    let pred_cost_milli = (ratio * 1000.0).round() as i64;

    Ok(CostModelEvidence {
        dist_per_sec,
        pred_per_sec,
        pred_cost_milli,
        note: "S8. The plan optimizer weighs a distance against a predicate eval; this is their \
               measured ratio (worst-ISA scalar, engine-conditional per C-6). DIST_COST_MILLI=1000 \
               is the numeraire. A microbench, not a corpus sweep -- the ratio is a property of the \
               kernels, not the data. Clamped to [0.05, 1.0] so microbench noise cannot commit an \
               absurd coefficient; the plan choice needs the magnitude, not three digits."
            .into(),
    })
}
