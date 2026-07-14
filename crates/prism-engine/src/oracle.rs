//! The exact-search golden corpus — permanent artifact #2 (Part II §7.4).
//!
//! Approximation is a *tested contract*, not a hope. So we keep a fixed corpus, a
//! fixed set of queries, and the exact top-k for each, computed by brute force
//! over every stored vector. Two things then become checkable in CI forever:
//!
//!   1. **Drift.** Recompute the exact answers. If they changed, something in the
//!      embedder, the normalization, or the storage path changed the meaning of
//!      the data. That is a bug even if every other test passes.
//!   2. **Recall, with its tail.** Run the approximate path against the same
//!      queries and measure how much of the truth it found, at a stated `nprobe`
//!      and a stated scan cost.
//!
//! **The tail is the point.** S0 reported mean recall@10 of ~0.90 at `nprobe=1`
//! and called it good; the *minimum* was 0.000, because one query's true
//! neighbours all lived in a centroid we never probed. A mean cannot see that. So
//! every recall report here carries `min`, `p1` and `p5` alongside the mean, and
//! the query set deliberately includes **cluster-boundary queries** — queries
//! built to sit between two motifs, whose neighbours are split across centroids —
//! because a benchmark that only asks easy questions is a benchmark that lies.

use crate::corpus;
use crate::engine::Engine;
use prism_types::error::{PrismError, Result};
use prism_types::Query;
use serde::{Deserialize, Serialize};

/// What kind of question this is. Reported separately, because the aggregate
/// hides which class of query is actually failing.
pub const KIND_TOPIC: &str = "topic";
pub const KIND_BOUNDARY: &str = "cluster-boundary";
pub const KIND_HYBRID: &str = "hybrid";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoldenQuery {
    pub text: String,
    pub kind: String,
    pub tenant: Option<String>,
    pub time_from: Option<i64>,
    pub time_to: Option<i64>,
    pub k: usize,
}

impl GoldenQuery {
    pub fn to_query(&self) -> Query {
        Query {
            text: self.text.clone(),
            tenant: self.tenant.clone(),
            time_from: self.time_from,
            time_to: self.time_to,
            k: self.k,
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoldenExpectation {
    pub query: GoldenQuery,
    /// The exact top-k, in order. Ground truth.
    pub expected_ids: Vec<String>,
    pub expected_scores: Vec<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Golden {
    pub corpus_kind: String,
    pub corpus_rows: usize,
    pub corpus_seed: u64,
    pub dim: usize,
    pub nlist: usize,
    pub pq_m: usize,
    pub expectations: Vec<GoldenExpectation>,
}

const BASE_TIME: i64 = 1_760_000_000_000;

/// The standard query set.
///
/// Three classes, on purpose:
///
/// * **topic** — a query aimed at the middle of one behavioural motif. The easy
///   case, and the only case S0 measured.
/// * **cluster-boundary** — half of one motif's vocabulary and half of another's.
///   The query vector lands between two centroids, so its true neighbours are
///   split, and a small `nprobe` will reach one of them and miss the other. This
///   is the class that produced `min recall = 0.000`, and it is why the default
///   `nprobe` is derived from a tail floor rather than picked.
/// * **hybrid** — meaning plus a scalar predicate, which is the shape PrismDB
///   exists for.
pub fn standard_queries(k: usize) -> Vec<GoldenQuery> {
    let mut qs: Vec<GoldenQuery> = Vec::new();
    let n_topics = corpus::topic_count();

    let mk = |text: String, kind: &str| GoldenQuery {
        text,
        kind: kind.to_string(),
        tenant: None,
        time_from: None,
        time_to: None,
        k,
    };

    // --- topic queries: every phrase of every motif ---
    for t in 0..n_topics {
        for phrase in corpus::topic_phrases(t) {
            qs.push(mk(phrase.to_string(), KIND_TOPIC));
        }
    }

    // --- cluster-boundary queries: every unordered pair of motifs ---
    //
    // Take the first half of a phrase from motif A and the second half of a
    // phrase from motif B. The result is a real sentence fragment that belongs
    // to neither cluster and sits between both.
    for a in 0..n_topics {
        for b in (a + 1)..n_topics {
            let pa = corpus::topic_phrases(a)[0];
            let pb = corpus::topic_phrases(b)[0];
            let wa: Vec<&str> = pa.split_whitespace().collect();
            let wb: Vec<&str> = pb.split_whitespace().collect();
            let half_a = wa[..wa.len().div_ceil(2)].join(" ");
            let half_b = wb[wb.len() / 2..].join(" ");
            qs.push(mk(format!("{half_a} {half_b}"), KIND_BOUNDARY));

            // And the mirror image, so neither motif is systematically favoured
            // by being the one that contributes the leading words.
            let half_b0 = wb[..wb.len().div_ceil(2)].join(" ");
            let half_a1 = wa[wa.len() / 2..].join(" ");
            qs.push(mk(format!("{half_b0} {half_a1}"), KIND_BOUNDARY));
        }
    }

    // --- hybrid queries: the same meaning, constrained ---
    for t in 0..n_topics {
        let phrase = corpus::topic_phrases(t)[0];

        let mut tenant_q = mk(phrase.to_string(), KIND_HYBRID);
        tenant_q.tenant = Some(format!("t{}", t % 5));
        qs.push(tenant_q);

        let mut time_q = mk(phrase.to_string(), KIND_HYBRID);
        time_q.time_from = Some(BASE_TIME);
        time_q.time_to = Some(BASE_TIME + 800 * 1000);
        qs.push(time_q);
    }

    qs
}

/// Compute ground truth by exact brute-force scan.
pub fn build(engine: &Engine, kind: &str, rows: usize, seed: u64, k: usize) -> Result<Golden> {
    let mut expectations = Vec::new();
    for gq in standard_queries(k) {
        let hits = engine.exact_search(&gq.to_query())?;
        expectations.push(GoldenExpectation {
            expected_ids: hits.iter().map(|h| h.event.event_id.clone()).collect(),
            expected_scores: hits.iter().map(|h| h.score).collect(),
            query: gq,
        });
    }
    Ok(Golden {
        corpus_kind: kind.to_string(),
        corpus_rows: rows,
        corpus_seed: seed,
        dim: engine.store.config.dim,
        nlist: engine.store.config.nlist,
        pq_m: engine.store.config.pq_m,
        expectations,
    })
}

/// Recall for one class of query.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClassRecall {
    pub kind: String,
    pub queries: usize,
    pub mean_recall: f32,
    pub min_recall: f32,
    pub zero_recall_queries: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecallReport {
    pub queries: usize,
    pub k: usize,
    pub nprobe: usize,
    pub candidates: usize,
    pub rerank: usize,

    pub mean_recall: f32,
    /// The tail. A mean of 0.9 with a min of 0.0 is not a 0.9 system — it is a
    /// system that fails completely on some queries, and the mean is hiding it.
    pub min_recall: f32,
    pub p1_recall: f32,
    pub p5_recall: f32,
    /// How many queries the approximate path missed *entirely*.
    pub zero_recall_queries: usize,

    /// The scan cost the recall was bought at: the fraction of eligible rows the
    /// centroid index made us touch. Reporting recall without this is reporting
    /// half a result — you can always have recall 1.0 by scanning everything.
    pub mean_scan_fraction: f64,

    pub by_kind: Vec<ClassRecall>,
    pub worst_queries: Vec<(String, String, f32)>,
}

/// The p-th percentile of a sorted-ascending sample, by nearest-rank.
fn percentile(sorted: &[f32], p: f64) -> f32 {
    if sorted.is_empty() {
        return 1.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).floor() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Re-derive the exact answers and confirm they still match the committed golden
/// file. A mismatch means the *meaning* of the corpus moved.
pub fn check_drift(engine: &Engine, golden: &Golden) -> Result<()> {
    for exp in &golden.expectations {
        let hits = engine.exact_search(&exp.query.to_query())?;
        let ids: Vec<String> = hits.iter().map(|h| h.event.event_id.clone()).collect();
        if ids != exp.expected_ids {
            return Err(PrismError::Invariant(format!(
                "exact-search drift on query `{}`:\n  golden:   {:?}\n  computed: {:?}\n\
                 The embedder, normalization, or storage path changed what this corpus means.",
                exp.query.text, exp.expected_ids, ids
            )));
        }
    }
    Ok(())
}

/// Measure the approximate path against the golden truth, tail included.
pub fn measure_recall(
    engine: &Engine,
    golden: &Golden,
    nprobe: usize,
    candidates: usize,
    rerank: usize,
) -> Result<RecallReport> {
    let k = golden.expectations.first().map(|e| e.query.k).unwrap_or(10);

    let mut per_query: Vec<(String, String, f32)> = Vec::new();
    let mut scan_fraction_total = 0.0f64;

    for exp in &golden.expectations {
        let mut q = exp.query.to_query();
        q.nprobe = nprobe;
        q.candidates = candidates;
        q.rerank = rerank;

        let res = engine.search(&q)?;
        let approx: std::collections::BTreeSet<&str> = res
            .hits
            .iter()
            .take(k)
            .map(|h| h.event.event_id.as_str())
            .collect();

        let truth: Vec<&str> = exp
            .expected_ids
            .iter()
            .take(k)
            .map(|s| s.as_str())
            .collect();
        let r = if truth.is_empty() {
            1.0
        } else {
            truth.iter().filter(|id| approx.contains(*id)).count() as f32 / truth.len() as f32
        };

        scan_fraction_total += if res.counters.rows_eligible == 0 {
            0.0
        } else {
            res.counters.rows_scanned_pq as f64 / res.counters.rows_eligible as f64
        };

        per_query.push((exp.query.kind.clone(), exp.query.text.clone(), r));
    }

    let n = per_query.len().max(1);
    let mut sorted: Vec<f32> = per_query.iter().map(|(_, _, r)| *r).collect();
    sorted.sort_by(|a, b| a.total_cmp(b));

    let mut by_kind: Vec<ClassRecall> = Vec::new();
    for kind in [KIND_TOPIC, KIND_BOUNDARY, KIND_HYBRID] {
        let group: Vec<f32> = per_query
            .iter()
            .filter(|(kk, _, _)| kk == kind)
            .map(|(_, _, r)| *r)
            .collect();
        if group.is_empty() {
            continue;
        }
        by_kind.push(ClassRecall {
            kind: kind.to_string(),
            queries: group.len(),
            mean_recall: group.iter().sum::<f32>() / group.len() as f32,
            min_recall: group.iter().cloned().fold(f32::MAX, f32::min),
            zero_recall_queries: group.iter().filter(|r| **r == 0.0).count(),
        });
    }

    // The five worst queries, named. "Recall is 0.87" is a number; "these five
    // queries returned nothing useful, and here they are" is a bug report.
    let mut worst = per_query.clone();
    worst.sort_by(|a, b| a.2.total_cmp(&b.2));
    worst.truncate(5);

    Ok(RecallReport {
        queries: per_query.len(),
        k,
        nprobe,
        candidates,
        rerank,
        mean_recall: sorted.iter().sum::<f32>() / n as f32,
        min_recall: sorted.first().copied().unwrap_or(1.0),
        p1_recall: percentile(&sorted, 0.01),
        p5_recall: percentile(&sorted, 0.05),
        zero_recall_queries: sorted.iter().filter(|r| **r == 0.0).count(),
        mean_scan_fraction: scan_fraction_total / n as f64,
        by_kind,
        worst_queries: worst,
    })
}

// --- deriving the default nprobe, with a receipt --------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SweepRow {
    pub nprobe: usize,
    pub mean_recall: f32,
    pub p5_recall: f32,
    pub p1_recall: f32,
    pub min_recall: f32,
    pub zero_recall_queries: usize,
    pub mean_scan_fraction: f64,
}

/// The provenance of the default `nprobe`.
///
/// PRISM.md Part I §5.3 is explicit: *"`nlist`/`nprobe` are outputs of recall,
/// skew, filter selectivity and latency targets… No magic constants in docs or
/// defaults without benchmark provenance."* This struct is that provenance,
/// committed to `testing/golden/nprobe-provenance.json`, and a test asserts the
/// constant in the code still matches it. The default cannot drift from its
/// receipt without CI noticing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NprobeProvenance {
    pub corpus: String,
    pub corpus_rows: usize,
    pub corpus_seed: u64,
    pub queries: usize,
    pub k: usize,
    pub dim: usize,
    pub nlist: usize,
    pub pq_m: usize,
    pub candidates: usize,
    pub rerank: usize,
    /// The rule: the smallest `nprobe` whose **p1** recall clears this floor.
    /// Chosen on the tail, not the mean, because the mean was what hid the
    /// `min recall = 0.000` failure in S0.
    pub p1_floor: f32,
    pub chosen_nprobe: usize,
    pub chosen_mean_recall: f32,
    pub chosen_p1_recall: f32,
    pub chosen_scan_fraction: f64,
    pub sweep: Vec<SweepRow>,
    pub note: String,
}

/// Sweep every probe count and pick the smallest one that holds the tail.
pub fn sweep_nprobe(
    engine: &Engine,
    golden: &Golden,
    candidates: usize,
    rerank: usize,
    p1_floor: f32,
) -> Result<NprobeProvenance> {
    let nlist = engine.store.config.nlist;
    let mut sweep = Vec::new();

    for nprobe in 1..=nlist {
        let r = measure_recall(engine, golden, nprobe, candidates, rerank)?;
        sweep.push(SweepRow {
            nprobe,
            mean_recall: r.mean_recall,
            p5_recall: r.p5_recall,
            p1_recall: r.p1_recall,
            min_recall: r.min_recall,
            zero_recall_queries: r.zero_recall_queries,
            mean_scan_fraction: r.mean_scan_fraction,
        });
    }

    let chosen = sweep
        .iter()
        .find(|r| r.p1_recall >= p1_floor)
        .ok_or_else(|| {
            PrismError::Invariant(format!(
                "no probe count up to nlist={nlist} holds p1 recall@{} at or above {p1_floor}; \
                 the index cannot meet the floor on this corpus and the floor or the index has \
                 to change",
                golden.expectations.first().map(|e| e.query.k).unwrap_or(10)
            ))
        })?
        .clone();

    Ok(NprobeProvenance {
        corpus: golden.corpus_kind.clone(),
        corpus_rows: golden.corpus_rows,
        corpus_seed: golden.corpus_seed,
        queries: golden.expectations.len(),
        k: golden.expectations.first().map(|e| e.query.k).unwrap_or(10),
        dim: golden.dim,
        nlist: golden.nlist,
        pq_m: golden.pq_m,
        candidates,
        rerank,
        p1_floor,
        chosen_nprobe: chosen.nprobe,
        chosen_mean_recall: chosen.mean_recall,
        chosen_p1_recall: chosen.p1_recall,
        chosen_scan_fraction: chosen.mean_scan_fraction,
        sweep,
        note: "The default nprobe is the smallest probe count whose p1 recall@k clears the floor \
               on the golden corpus, at the reference configuration recorded above. It is chosen \
               on the tail rather than the mean because the mean hid a total failure on \
               cluster-boundary queries in S0. Re-derive with `prism golden sweep` whenever the \
               corpus, the embedder, or the index geometry changes. A different nlist or a \
               different corpus will have a different answer; this number is not universal, it \
               is provenanced."
            .to_string(),
    })
}

/// Recall of one approximate result against one exact result. Re-exported so
/// tests can assert on a single query without building a golden file.
pub use crate::search::recall_at_k as recall;
