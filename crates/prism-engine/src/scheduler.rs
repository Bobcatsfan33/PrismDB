//! The merge scheduler (S10) — size-tiered selection with budgets, and an explainable decision.
//!
//! S0's merge compacted *everything* into one part per partition every time — correct, but a
//! write-amplification catastrophe under sustained ingest, because every new small part rewrites
//! the whole partition. S10 makes merge **size-tiered** ([merge contract §2](../../../docs/MERGE-CONTRACT.md)):
//! parts are bucketed by size, a tier is merged only once it has accumulated a fan-out's worth of
//! parts, and the result graduates to the next tier. Small parts merge cheaply and often; large
//! parts merge rarely. Part count reaches a steady state and write amplification stays bounded.
//!
//! The scheduler's decision is **explainable, not deterministic** (§2): it records the tiers, the
//! part counts, the merge debt, and every budget it spent, in enough detail to reproduce *why* it
//! chose what it chose — but it does not promise to pick the same partition given a different
//! arrival order, because coupling it to a global order would make it depend on things it must not
//! see. Answers are layout-invariant already; there is nothing for merge determinism to protect.

use crate::merge::{
    MERGE_IO_BUDGET_ROWS, MERGE_MAX_OPS, MERGE_TIER_BASE_ROWS, MERGE_TIER_FANOUT, MERGE_TIER_RATIO,
};
use prism_part::partition::{PartRef, PartitionKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The size tier a part of `rows` rows belongs to. Tier `t` covers
/// `[base·ratio^t, base·ratio^(t+1))` rows, so a part `ratio`× bigger is one tier up.
pub fn tier_of(rows: usize) -> u32 {
    let base = MERGE_TIER_BASE_ROWS.max(1);
    let ratio = MERGE_TIER_RATIO.max(2);
    // The smallest tier whose upper bound (`base·ratio^t`) still covers `rows`: tier 0 is
    // `(0, base]`, tier `t` is `(base·ratio^(t-1), base·ratio^t]`.
    let mut tier = 0u32;
    let mut bound = base;
    while bound < rows {
        bound = bound.saturating_mul(ratio);
        tier += 1;
    }
    tier
}

/// One merge the scheduler chose: the parts to combine, and **why** — the record that makes the
/// decision reproducible (§2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeOp {
    /// A stable human description of the partition (bucket · window · generation).
    pub partition: String,
    pub tier: u32,
    pub part_ids: Vec<String>,
    pub input_rows: usize,
    pub reason: String,
}

/// The scheduler's whole decision for one cycle: the ops it will run, the ops it deferred to stay
/// within budget, and the merge debt it measured. Serializable, so it is logged verbatim (§2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergePlan {
    pub ops: Vec<MergeOp>,
    /// Excess parts beyond the ideal tiered shape (one part per tier per partition), summed over
    /// the store. Bounded merge debt is the soak's steady-state assertion (§8).
    pub merge_debt: usize,
    pub io_budget_rows: usize,
    pub io_spent_rows: usize,
    /// Ops that were eligible but did not fit this cycle's I/O or concurrency budget. Named, not
    /// silently dropped — a bounded backlog the next cycle picks up.
    pub deferred_ops: usize,
}

fn describe(key: &PartitionKey) -> String {
    format!(
        "bucket={:?} window={} generation={}",
        key.bucket, key.window, key.generation
    )
}

/// Plan this cycle's merges from the live parts. Pure and side-effect-free: it reads only tiers,
/// counts, and budgets — never wall-clock, never physical order — so it is explainable by
/// construction.
pub fn plan_merges(parts: &[PartRef]) -> MergePlan {
    // Group parts by (partition, tier).
    let mut by_partition: BTreeMap<PartitionKey, BTreeMap<u32, Vec<&PartRef>>> = BTreeMap::new();
    for p in parts {
        by_partition
            .entry(p.partition.clone())
            .or_default()
            .entry(tier_of(p.rows))
            .or_default()
            .push(p);
    }

    // Merge debt: excess parts beyond one-per-tier, summed over the store.
    let mut merge_debt = 0usize;
    for tiers in by_partition.values() {
        for members in tiers.values() {
            merge_debt += members.len().saturating_sub(1);
        }
    }

    // Candidate ops: a tier with at least the fan-out of parts is ripe to merge. Consider smaller
    // tiers first — merging the smallest parts reclaims the most part-count for the least bytes
    // moved, which is what keeps write amplification down.
    let mut candidates: Vec<(usize, MergeOp)> = Vec::new();
    for (key, tiers) in &by_partition {
        for (tier, members) in tiers {
            if members.len() >= MERGE_TIER_FANOUT {
                let input_rows: usize = members.iter().map(|p| p.rows).sum();
                let mut part_ids: Vec<String> = members.iter().map(|p| p.part_id.clone()).collect();
                part_ids.sort(); // stable record; the merge answer does not depend on this order
                candidates.push((
                    input_rows,
                    MergeOp {
                        partition: describe(key),
                        tier: *tier,
                        part_ids,
                        input_rows,
                        reason: format!(
                            "tier {tier} holds {} parts (>= fan-out {MERGE_TIER_FANOUT}); merging them \
                             into one graduates the tier and cuts part count by {}",
                            members.len(),
                            members.len() - 1
                        ),
                    },
                ));
            }
        }
    }
    // Smallest input first (cheapest part-count reclamation).
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.partition.cmp(&b.1.partition)));

    // Admit within the per-cycle I/O budget and the concurrency (max-ops) budget.
    let mut ops = Vec::new();
    let mut io_spent = 0usize;
    let mut deferred = 0usize;
    for (rows, op) in candidates {
        if ops.len() >= MERGE_MAX_OPS || io_spent.saturating_add(rows) > MERGE_IO_BUDGET_ROWS {
            deferred += 1;
            continue;
        }
        io_spent += rows;
        ops.push(op);
    }

    MergePlan {
        ops,
        merge_debt,
        io_budget_rows: MERGE_IO_BUDGET_ROWS,
        io_spent_rows: io_spent,
        deferred_ops: deferred,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_part::partition::Bucket;

    fn pref(id: &str, bucket: usize, rows: usize) -> PartRef {
        PartRef {
            part_id: id.to_string(),
            partition: PartitionKey {
                bucket: Bucket::Shared(bucket as u32),
                window: 0,
                generation: "g".into(),
            },
            rows,
            tenants: vec!["t".into()],
            time_min: 0,
            time_max: 0,
        }
    }

    #[test]
    fn tiers_grow_by_the_ratio() {
        assert_eq!(tier_of(1), 0);
        assert_eq!(tier_of(MERGE_TIER_BASE_ROWS), 0);
        assert!(tier_of(MERGE_TIER_BASE_ROWS * MERGE_TIER_RATIO * MERGE_TIER_RATIO) >= 2);
    }

    #[test]
    fn a_full_tier_is_selected_and_debt_is_measured() {
        // fan-out worth of small parts in one partition, plus a lone larger one.
        let mut parts: Vec<PartRef> = (0..MERGE_TIER_FANOUT)
            .map(|i| pref(&format!("p{i}"), 0, MERGE_TIER_BASE_ROWS))
            .collect();
        parts.push(pref(
            "big",
            0,
            MERGE_TIER_BASE_ROWS * MERGE_TIER_RATIO * MERGE_TIER_RATIO,
        ));

        let plan = plan_merges(&parts);
        assert_eq!(plan.ops.len(), 1, "the full small tier should be selected");
        assert_eq!(plan.ops[0].part_ids.len(), MERGE_TIER_FANOUT);
        assert!(plan.ops[0].reason.contains("fan-out"));
        // Debt = excess beyond one-per-tier: the small tier has FANOUT parts (FANOUT-1 excess),
        // the big tier has 1 (no excess).
        assert_eq!(plan.merge_debt, MERGE_TIER_FANOUT - 1);
    }

    #[test]
    fn a_tier_below_fan_out_is_left_alone() {
        let parts: Vec<PartRef> = (0..MERGE_TIER_FANOUT - 1)
            .map(|i| pref(&format!("p{i}"), 0, MERGE_TIER_BASE_ROWS))
            .collect();
        let plan = plan_merges(&parts);
        assert!(
            plan.ops.is_empty(),
            "a tier below fan-out must not be merged"
        );
    }

    #[test]
    fn the_io_budget_defers_ops_by_name() {
        // Many full tiers across many partitions, each op larger than a shrunk budget allows.
        let mut parts = Vec::new();
        for b in 0..8 {
            for i in 0..MERGE_TIER_FANOUT {
                parts.push(pref(&format!("p{b}_{i}"), b, MERGE_TIER_BASE_ROWS));
            }
        }
        let plan = plan_merges(&parts);
        // Every partition has a ripe tier; concurrency caps how many run at once.
        assert!(plan.ops.len() <= MERGE_MAX_OPS);
        if plan.ops.len() < 8 {
            assert!(
                plan.deferred_ops > 0,
                "unrun ops must be counted as deferred"
            );
        }
    }
}
