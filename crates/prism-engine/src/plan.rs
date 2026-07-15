//! Physical execution strategies (S8) — three ways to run one logical query.
//!
//! A semantic query with a scalar predicate can be executed three ways, and they are **three
//! physical strategies for one logical query** ([docs/QUERY-CONTRACT.md](../../../docs/QUERY-CONTRACT.md)
//! §9): plan-invariance is [D-033](../../../docs/DECISIONS.md) in its plan edition.
//!
//! - **Interleaved** — the fused scan: evaluate the predicate inline, per row, as the distances
//!   stream by. The balanced default.
//! - **Scalar-first** — filter first: evaluate the predicate columnar, then compute a distance
//!   only for the survivors. Wins when the predicate is *selective* — a handful of survivors, so
//!   most distances are never computed.
//! - **Semantic-first** — distance first: compute every probed distance, but evaluate the
//!   predicate *only* for rows near enough to enter the selection. Wins when the predicate is
//!   *not* selective — the distance does the narrowing, and the predicate is barely consulted.
//!
//! **All three offer the identical passing rows, ranked by the identical distance, to the
//! identical bounded top-k.** They differ only in *when* the predicate is evaluated relative to
//! the distance — which changes the *work*, never the *set*. So the plan may cost differently; it
//! may not answer differently. That is provable by construction, and the plan-invariance gate
//! proves it on every gate query.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};

/// Which physical strategy runs a query's candidate scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    Interleaved,
    ScalarFirst,
    SemanticFirst,
}

impl Strategy {
    pub fn name(self) -> &'static str {
        match self {
            Strategy::Interleaved => "interleaved",
            Strategy::ScalarFirst => "scalar-first",
            Strategy::SemanticFirst => "semantic-first",
        }
    }

    pub fn parse(s: &str) -> Option<Strategy> {
        match s {
            "interleaved" => Some(Strategy::Interleaved),
            "scalar-first" => Some(Strategy::ScalarFirst),
            "semantic-first" => Some(Strategy::SemanticFirst),
            _ => None,
        }
    }

    pub const ALL: [Strategy; 3] = [
        Strategy::Interleaved,
        Strategy::ScalarFirst,
        Strategy::SemanticFirst,
    ];
}

// --- test-only forced-plan override (mirrors the S7 route override) --------------------------

static FORCED_PLAN: AtomicU8 = AtomicU8::new(0); // 0 = none (optimizer decides)

/// Force a strategy globally (test only). The plan-invariance gate flips this to prove every
/// strategy answers identically, and flips it *between pages* to prove a cursor need not pin it.
pub fn set_forced_plan(plan: Option<Strategy>) {
    FORCED_PLAN.store(
        match plan {
            None => 0,
            Some(Strategy::Interleaved) => 1,
            Some(Strategy::ScalarFirst) => 2,
            Some(Strategy::SemanticFirst) => 3,
        },
        Ordering::SeqCst,
    );
}

pub fn forced_plan_override() -> Option<Strategy> {
    match FORCED_PLAN.load(Ordering::SeqCst) {
        1 => Some(Strategy::Interleaved),
        2 => Some(Strategy::ScalarFirst),
        3 => Some(Strategy::SemanticFirst),
        _ => None,
    }
}

/// The strategy the optimizer chose, and why — carried into `EXPLAIN` (§14).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanChoice {
    pub strategy: Strategy,
    pub reason: String,
    /// The estimated fraction of probed rows the predicate admits. The whole decision turns on it.
    pub estimated_selectivity: f64,
}

// --- the cost model (S8) ---------------------------------------------------------------------
//
// Coefficients derive from the committed S6/S7 artifacts (directive 2). The unit is a "work
// point"; the ratio between the two weights is what matters, not their absolute scale, so the
// cost is a deterministic function of the query's *actual counters* -- no wall-clock noise.

/// The cost of computing one ADC distance, in work points ×1000.
///
/// **Policy** (C-1): the numeraire. Cost is measured in units of one ADC distance, so this is 1.0
/// by definition. The microbench in `testing/evidence/cost-model.json` measures it in absolute
/// terms (worst-ISA distances/sec) for documentation; the plan choice only needs the *ratio* to
/// [`PRED_COST_MILLI`].
pub const DIST_COST_MILLI: u64 = 1000; // 1.0 work point, the numeraire

/// The cost of one general-predicate evaluation, relative to a distance.
///
/// **Policy** (C-1), informed by measurement but not bound to a noisy timing. The honest surprise
/// (`testing/evidence/cost-model.json`): a real `predicate::eval` -- interpreted per row, with
/// dynamic dispatch over the predicate tree and boxed `Value` comparisons -- costs *as much as or
/// more than* an ADC distance, which is SIMD-batched. So the predicate is the expensive per-row
/// operation, which is why "distance first, predicate lazily" (semantic-first) tends to win. The
/// microbench measured ~2.7x on this machine; we commit a stable in-magnitude value (equal to a
/// distance) rather than an exact ratio that would drift with the hardware -- a committed constant
/// bound to a stopwatch is a flaky gate. Engine-conditional (C-6): a compiled predicate path (a
/// future sprint) would slash it, and it re-derives.
pub const PRED_COST_MILLI: u64 = 1000;

/// **Worst-cell regret bound (policy, C-3).** The chosen plan must be within this fraction of the
/// best fixed plan's actual cost in **every** cell of the selectivity matrix, not on average. An
/// optimizer that wins on average by losing badly in one cell is worse than a fixed heuristic for
/// the customer stuck in that cell. 15% is the declared bound: wide enough that a crude
/// selectivity estimate (directive 7) can meet it, tight enough that a plainly-wrong choice fails.
/// Measurement cannot pick this -- it is a statement about how much regret we are willing to ship.
pub const PLAN_REGRET_BOUND_PCT: u64 = 15;

/// The actual cost of a query, in work points, from its real counters. Deterministic: the same
/// query always has the same cost, because the counters are exact. This is the currency both the
/// optimizer's estimate and the regret gate's measurement are denominated in.
pub fn actual_cost(distances_computed: usize, predicate_evals: usize) -> u64 {
    distances_computed as u64 * DIST_COST_MILLI + predicate_evals as u64 * PRED_COST_MILLI
}

/// The estimated cost of running `strategy` over `probed_rows` at an estimated `selectivity`
/// (fraction of probed rows the predicate admits) and a candidate width `cap`.
///
/// This mirrors what each strategy actually does (see the module docs): interleaved distances and
/// predicates every probed row; scalar-first predicates every row but distances only survivors;
/// semantic-first distances every row but predicates only rows that could enter (~`cap`).
pub fn estimate_cost(strategy: Strategy, probed_rows: usize, selectivity: f64, cap: usize) -> u64 {
    let n = probed_rows as f64;
    let sel = selectivity.clamp(1e-4, 1.0);
    let survivors = (n * sel).ceil();
    // Semantic-first evaluates the predicate only for rows that could enter the bounded heap -- but
    // the heap gates on *fullness*, and it fills only after `cap` **passing** rows, which takes
    // ~`cap / sel` probed rows. Until then every row admits and its predicate runs. So the
    // predicate saving materializes only when the predicate is NOT selective (heap fills fast); a
    // selective predicate keeps the heap starved and the predicate running. Modelling `cap` instead
    // of `cap / sel` was the bug the worst-cell regret gate caught.
    let admittable = n.min((cap as f64 / sel).ceil());
    let d = DIST_COST_MILLI as f64;
    let p = PRED_COST_MILLI as f64;
    let cost = match strategy {
        Strategy::Interleaved => n * d + n * p,
        Strategy::ScalarFirst => survivors * d + n * p,
        Strategy::SemanticFirst => n * d + admittable * p,
    };
    cost.round() as u64
}
