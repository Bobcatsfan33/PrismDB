//! The constant ledger (charter amendment C-1).
//!
//! > **Every tuned constant must be pinned to committed benchmark evidence, with a
//! > test asserting the constant still matches that evidence.**
//!
//! This module enumerates every constant that steers behaviour, and classifies each
//! one. `testing/evidence/registry.json` is the committed ledger, and
//! `every_tuned_constant_matches_its_committed_evidence` asserts — **in both
//! directions** — that the code and the ledger agree.
//!
//! Two kinds, and the distinction is load-bearing:
//!
//! * **`tuned`** — derived from measurement. A different measurement would have
//!   produced a different value. It owes **evidence**: a committed file, a named key
//!   inside it, and the rule by which that key was chosen.
//! * **`policy`** — a deliberate decision about behaviour, not an empirical optimum.
//!   A cap of 64 attribute keys is not "the measured best number of attribute keys";
//!   it is a statement about what we are willing to accept. It owes a **rationale**,
//!   pointed at prose, and the test enforces that the pointer resolves.
//!
//! The distinction exists to be abuse-proof. Without it, every inconvenient constant
//! gets reclassified as policy to escape the evidence requirement — so `policy` still
//! has to point at an argument, and an argument is reviewable.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Tuned,
    Policy,
}

#[derive(Clone, Debug)]
pub struct Constant {
    pub name: &'static str,
    pub value: i64,
    pub kind: Kind,
}

/// Every constant that steers behaviour. A new one that is not here will fail
/// `every_tuned_constant_matches_its_committed_evidence`, which is the point.
pub fn constants() -> Vec<Constant> {
    use prism_types::limits as L;
    vec![
        // --- tuned: derived from measurement, owes evidence ---
        Constant {
            name: "DEFAULT_NPROBE",
            value: prism_types::query::DEFAULT_NPROBE as i64,
            kind: Kind::Tuned,
        },
        Constant {
            // The *default* for new parts. `format::BLOCK_SIZE` is a different thing
            // -- the fixed size every v2 part was written at, which is history and
            // cannot be tuned.
            name: "BLOCK_SIZE",
            value: prism_part::format::DEFAULT_BLOCK_SIZE as i64,
            kind: Kind::Tuned,
        },
        // Swept JOINTLY (S3): they interact, so neither has an honest single-axis sweep.
        Constant {
            name: "DEFAULT_CANDIDATES",
            value: prism_types::query::DEFAULT_CANDIDATES as i64,
            kind: Kind::Tuned,
        },
        Constant {
            name: "DEFAULT_RERANK",
            value: prism_types::query::DEFAULT_RERANK as i64,
            kind: Kind::Tuned,
        },
        // S5. Became *tuned* the moment it was chosen by measurement -- and it had to be,
        // because fixing charter C-4 removed a lucky input order that k-means++ had been
        // quietly relying on.
        Constant {
            name: "KMEANS_RESTARTS",
            value: prism_quantizer::kmeans::KMEANS_RESTARTS as i64,
            kind: Kind::Tuned,
        },
        // S6, issue #1. Tuned, but selected by a policy cost bound on this corpus (the recall
        // floor is already met), so the receipt carries its C-3 bound and a corpus-conditional
        // tag. Stored x1000 because the ledger holds integers and the margin is 0.05.
        Constant {
            name: "ADAPTIVE_MARGIN_X1000",
            value: (prism_types::query::ADAPTIVE_MARGIN * 1000.0).round() as i64,
            kind: Kind::Tuned,
        },
        // S7, D-049. The fp16 rerank accuracy contract's tolerance. Tuned: measured as >= 2x the
        // worst fp16-vs-fp32 score gap on the golden corpus, with headroom. Stored in micro-units.
        Constant {
            name: "FP16_COSINE_TOLERANCE_MICROS",
            value: (prism_part::format::FP16_COSINE_TOLERANCE as f64 * 1e6).round() as i64,
            kind: Kind::Tuned,
        },
        // --- policy: a decision about behaviour, owes a rationale ---
        // S5. `TRAIN_SAMPLE_MAX` steered behaviour from S0 and was never registered -- an
        // existing hole in the ledger, found by the C-4 audit and closed here.
        Constant {
            name: "TRAIN_SAMPLE_MAX",
            value: crate::sample::TRAIN_SAMPLE_MAX as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "STRATUM_FLOOR_SHARE",
            value: crate::sample::STRATUM_FLOOR_SHARE as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "BASELINE_CLUSTERS",
            value: crate::drift::BASELINE_CLUSTERS as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "BASELINE_QUANTILE_PCT",
            value: crate::drift::BASELINE_QUANTILE_PCT as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "DRIFT_FIRE_MULTIPLE",
            value: crate::drift::DRIFT_FIRE_MULTIPLE as i64,
            kind: Kind::Policy,
        },
        // S8. The plan cost model. Policy, informed by the cost-model microbench (documented in
        // testing/evidence/cost-model.json), not bound to a noisy timing.
        Constant {
            name: "DIST_COST_MILLI",
            value: crate::plan::DIST_COST_MILLI as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "PRED_COST_MILLI",
            value: crate::plan::PRED_COST_MILLI as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "PLAN_REGRET_BOUND_PCT",
            value: crate::plan::PLAN_REGRET_BOUND_PCT as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "MIN_PAGEABLE_ROWS",
            value: crate::evidence::MIN_PAGEABLE_ROWS as i64,
            kind: Kind::Policy,
        },
        // S6, issue #1. The hard ceiling on adaptive probing -- a worst-case cost bound.
        Constant {
            name: "ADAPTIVE_MAX_NPROBE",
            value: prism_types::query::ADAPTIVE_MAX_NPROBE as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "MAX_STATEMENT_BYTES",
            value: prism_sql::limits::MAX_STATEMENT_BYTES as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "MAX_EXPR_DEPTH",
            value: prism_sql::limits::MAX_EXPR_DEPTH as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "MAX_IN_LIST",
            value: prism_sql::limits::MAX_IN_LIST as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "MAX_TOKENS",
            value: prism_sql::limits::MAX_TOKENS as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "MAX_ATTRIBUTE_KEYS",
            value: L::MAX_ATTRIBUTE_KEYS as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "MAX_ATTRIBUTE_KEY_BYTES",
            value: L::MAX_ATTRIBUTE_KEY_BYTES as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "MAX_ATTRIBUTE_VALUE_BYTES",
            value: L::MAX_ATTRIBUTE_VALUE_BYTES as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "MAX_ATTRIBUTES_BYTES",
            value: L::MAX_ATTRIBUTES_BYTES as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "MAX_ATTRIBUTE_KEY_CARDINALITY",
            value: L::MAX_ATTRIBUTE_KEY_CARDINALITY as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "MAX_LATENESS_MS",
            value: L::MAX_LATENESS_MS,
            kind: Kind::Policy,
        },
        Constant {
            name: "MAX_SKEW_AHEAD_MS",
            value: L::MAX_SKEW_AHEAD_MS,
            kind: Kind::Policy,
        },
        Constant {
            name: "IDEMPOTENCY_WINDOW_MS",
            value: L::IDEMPOTENCY_WINDOW_MS,
            kind: Kind::Policy,
        },
        Constant {
            name: "IDEMPOTENCY_MAX_ENTRIES",
            value: L::IDEMPOTENCY_MAX_ENTRIES as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "MAX_BODY_BYTES",
            value: prism_types::event::MAX_BODY_BYTES as i64,
            kind: Kind::Policy,
        },
        Constant {
            name: "MAX_EMBED_INPUT_BYTES",
            value: prism_types::MAX_EMBED_INPUT_BYTES as i64,
            kind: Kind::Policy,
        },
    ]
}

/// One row of `testing/evidence/registry.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub name: String,
    pub value: i64,
    pub kind: Kind,
    /// `tuned` only: the committed file that justifies the value.
    #[serde(default)]
    pub evidence: Option<String>,
    /// `tuned` only: the key inside that file which *is* the value.
    #[serde(default)]
    pub evidence_key: Option<String>,
    /// `tuned` only: the rule by which the evidence chose this value.
    #[serde(default)]
    pub rule: Option<String>,
    /// `policy` only: prose that argues for it.
    #[serde(default)]
    pub rationale: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Registry {
    pub constants: Vec<RegistryEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_constant_is_classified_and_no_name_repeats() {
        let mut seen = std::collections::BTreeSet::new();
        for c in constants() {
            assert!(seen.insert(c.name), "duplicate constant {}", c.name);
        }
        assert!(constants().iter().any(|c| c.kind == Kind::Tuned));
        assert!(constants().iter().any(|c| c.kind == Kind::Policy));
    }
}
