//! Training-sample selection (S5).
//!
//! > *"codebooks trained from stratified/reservoir samples (never just the first batch)"*
//! > — PRISM.md, S5
//!
//! **A codebook defines the meaning of every byte encoded under it.** So the question "which
//! rows train it" is not a tuning detail — it decides what the store's bytes *say*.
//!
//! Two rules, and the second one is the one that was broken:
//!
//! 1. **Never the first batch.** A codebook trained on the first batch bakes that batch's
//!    distribution into the meaning of everything written afterwards.
//!
//! 2. **Never a position.** The sample is chosen by **`event_id`** — the row's logical identity
//!    — and never by where the row sits. The old reservoir sampled by *index into a vector
//!    built by reading parts in catalog order*, so the same rows, laid out differently, trained
//!    a **different codebook**: different centroids, different PQ codes, a different meaning for
//!    every byte in the store. That is [D-033](../../docs/DECISIONS.md)'s disease in the one
//!    place it would have been hardest to ever notice, and charter **C-4** now forbids the whole
//!    class.
//!
//! The mechanism is **bottom-k by hash**: score every row with `sha256(seed ‖ event_id)` and
//! keep the smallest. It is a reservoir that does not care what order it sees the rows in — the
//! chosen set is a pure function of the *set* of rows, which is exactly the property a reservoir
//! keyed on arrival position cannot have.
//!
//! Strata are **tenants**, because a store whose loudest tenant emits a hundred times the rows of
//! everyone else would otherwise get a codebook that describes that one tenant — and every other
//! tenant's recall would quietly pay for it.
//!
//! **Tenants, and deliberately not partitions.** The first version stratified by partition
//! (tenant-bucket × time window), and the layout gate caught it within the hour: a time window is
//! a *store configuration*, so two stores holding identical rows with different window sizes got
//! different strata, different samples, and **different codebooks**. That is C-4 again, one level
//! up — the strata themselves have to be a logical property of the data, or the whole exercise of
//! keying the sample on `event_id` buys nothing. A tenant is a fact about a row. A time window is
//! a fact about a config file.

use prism_types::error::{PrismError, Result};
use prism_types::hash::sha256;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Cap on how many vectors train a codebook.
///
/// **Policy** (C-1): not a measured optimum. It is a bound on how long training may take and
/// how much memory it may hold, chosen so a codebook can be trained on a laptop. Raising it
/// buys a marginally better-fitted codebook at a linear cost in time; the recall receipts
/// measure whether the fit is good enough, and they say it is.
pub const TRAIN_SAMPLE_MAX: usize = 50_000;

/// The share of the sample reserved for *equal* allocation across strata before the rest is
/// allocated proportionally.
///
/// **Policy** (C-1). Purely proportional allocation is not stratification — it reproduces
/// exactly the imbalance it was supposed to protect against, because a stratum with 1% of the
/// rows gets 1% of the sample whether you stratify or not. A floor is what makes a small
/// tenant's geometry visible to the codebook at all. A quarter is a deliberate compromise: most
/// of the sample still follows the data, but no stratum is invisible.
pub const STRATUM_FLOOR_SHARE: usize = 4;

/// One row offered to training. Borrowed — training samples are large and copying them twice
/// to decide which ones to copy once is silly.
pub struct SampleRow<'a> {
    /// The stratum this row belongs to. In practice the partition key. Owned, because it is
    /// derived rather than borrowed from anything that outlives the call.
    pub stratum: String,
    /// The row's **logical identity**. The whole point.
    pub event_id: &'a str,
    pub vector: &'a [f32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StratumStat {
    pub stratum: String,
    pub rows: usize,
    pub sampled: usize,
}

/// How a codebook's training sample was chosen. Recorded in the generation, like everything
/// else here — a codebook you cannot account for is a codebook you cannot defend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleProvenance {
    /// Named so a future reader knows exactly what was done, without reading this file.
    pub strategy: String,
    pub seed: u64,
    pub max: usize,
    pub rows_offered: usize,
    pub rows_sampled: usize,
    pub strata: Vec<StratumStat>,
    /// The snapshot the sample was drawn from. A sample is a statement about a store at an
    /// instant, and the instant is part of the statement.
    pub snapshot_id: String,
    /// True only for the bootstrap generation, which had nothing but the first batch to learn
    /// from. Honest, and exactly what the generation lifecycle exists to replace.
    pub provisional: bool,
}

/// `sha256(seed ‖ event_id)`, folded to 64 bits. The row's ticket in the lottery.
///
/// Cryptographic, not a fast hash, for the same reason bucket assignment is: an id must not be
/// able to *steer itself* into (or out of) the codebook's training set.
fn ticket(seed: u64, event_id: &str) -> u64 {
    let mut buf = Vec::with_capacity(8 + event_id.len());
    buf.extend_from_slice(&seed.to_le_bytes());
    buf.extend_from_slice(event_id.as_bytes());
    let h = sha256(&buf);
    u64::from_le_bytes([h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]])
}

/// Allocate `max` sample slots across strata: an equal floor first, then proportional.
///
/// Deterministic in the *sorted* stratum names, so allocation cannot depend on the order the
/// strata were discovered in — which would be the layout, again, wearing a different hat.
fn allocate(sizes: &BTreeMap<String, usize>, max: usize) -> BTreeMap<String, usize> {
    let s = sizes.len();
    let total: usize = sizes.values().sum();
    let mut quota: BTreeMap<String, usize> = BTreeMap::new();
    if s == 0 || total == 0 || max == 0 {
        return quota;
    }

    // The floor: an equal share of STRATUM_FLOOR_SHARE^-1 of the sample, capped by what the
    // stratum actually has.
    let floor_pool = max / STRATUM_FLOOR_SHARE;
    let floor = (floor_pool / s).max(1);
    let mut used = 0usize;
    for (name, &rows) in sizes {
        let q = floor.min(rows);
        quota.insert(name.clone(), q);
        used += q;
    }

    // The rest, proportional to stratum size, largest-remainder so the parts sum to the whole.
    let rest = max.saturating_sub(used);
    if rest > 0 {
        let mut remainders: Vec<(u128, &String)> = Vec::new();
        for (name, &rows) in sizes {
            let headroom = rows - quota[name];
            let exact = (rest as u128) * (rows as u128);
            let share = ((exact / total as u128) as usize).min(headroom);
            *quota.get_mut(name).unwrap() += share;
            used += share;
            if quota[name] < rows {
                remainders.push((exact % total as u128, name));
            }
        }
        // Hand out what integer division dropped, biggest remainder first; ties on the stratum
        // name, because a tie broken on iteration order is a tie broken on the layout.
        remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        let mut left = max.saturating_sub(used);
        for (_, name) in remainders {
            if left == 0 {
                break;
            }
            if quota[name] < sizes[name] {
                *quota.get_mut(name).unwrap() += 1;
                left -= 1;
            }
        }
    }
    quota
}

/// Choose the training sample. Returns the flattened vectors and the provenance.
///
/// **The chosen set is a pure function of the set of rows offered.** Not of their order, not of
/// which parts they came from, not of how the ingest was batched. `rows` may be handed to this
/// function in any order at all and the answer will not move — which is the property C-4 is
/// about, and the property a test holds it to.
pub fn stratified_sample(
    rows: &[SampleRow],
    max: usize,
    seed: u64,
    snapshot_id: &str,
    provisional: bool,
) -> Result<(Vec<f32>, SampleProvenance)> {
    if rows.is_empty() {
        return Err(PrismError::Invalid(
            "cannot train a codebook on zero vectors".into(),
        ));
    }
    let dim = rows[0].vector.len();
    if let Some(bad) = rows.iter().find(|r| r.vector.len() != dim) {
        return Err(PrismError::Invariant(format!(
            "training rows disagree about dimension: {} vs {} (event {})",
            dim,
            bad.vector.len(),
            bad.event_id
        )));
    }

    // Bucket by stratum, and score every row by its identity.
    let mut by_stratum: BTreeMap<String, Vec<(u64, &str, usize)>> = BTreeMap::new();
    for (i, r) in rows.iter().enumerate() {
        by_stratum.entry(r.stratum.clone()).or_default().push((
            ticket(seed, r.event_id),
            r.event_id,
            i,
        ));
    }

    let sizes: BTreeMap<String, usize> = by_stratum
        .iter()
        .map(|(k, v)| (k.clone(), v.len()))
        .collect();
    let quota = allocate(&sizes, max.min(rows.len()));

    let mut chosen: Vec<usize> = Vec::new();
    let mut strata: Vec<StratumStat> = Vec::new();
    for (name, mut tickets) in by_stratum {
        // Bottom-k by ticket. Ties on the event id — two rows cannot share one, so this is a
        // total order on identity and the sample is unique.
        tickets.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
        let q = *quota.get(&name).unwrap_or(&0);
        let rows_in = tickets.len();
        for (_, _, i) in tickets.into_iter().take(q) {
            chosen.push(i);
        }
        strata.push(StratumStat {
            stratum: name,
            rows: rows_in,
            sampled: q,
        });
    }

    // Emit in a deterministic order. k-means seeds itself from the first vectors it is handed,
    // so the *order* of the sample is as load-bearing as its membership: hand the same rows to
    // training in two different orders and you get two different codebooks. Sorted by ticket,
    // which is a function of identity and nothing else.
    chosen.sort_by(|&a, &b| {
        ticket(seed, rows[a].event_id)
            .cmp(&ticket(seed, rows[b].event_id))
            .then(rows[a].event_id.cmp(rows[b].event_id))
    });

    let mut flat = Vec::with_capacity(chosen.len() * dim);
    for i in &chosen {
        flat.extend_from_slice(rows[*i].vector);
    }

    let prov = SampleProvenance {
        strategy: "stratified bottom-k by sha256(seed || event_id), partition strata, equal \
                   floor then proportional"
            .into(),
        seed,
        max,
        rows_offered: rows.len(),
        rows_sampled: chosen.len(),
        strata,
        snapshot_id: snapshot_id.to_string(),
        provisional,
    };
    Ok((flat, prov))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize, strata: usize) -> (Vec<String>, Vec<String>, Vec<Vec<f32>>) {
        let ids: Vec<String> = (0..n).map(|i| format!("e{i:06}")).collect();
        let st: Vec<String> = (0..n).map(|i| format!("s{}", i % strata)).collect();
        let vs: Vec<Vec<f32>> = (0..n).map(|i| vec![i as f32, (i * 2) as f32]).collect();
        (ids, st, vs)
    }

    fn view<'a>(
        ids: &'a [String],
        st: &'a [String],
        vs: &'a [Vec<f32>],
        order: &[usize],
    ) -> Vec<SampleRow<'a>> {
        order
            .iter()
            .map(|&i| SampleRow {
                stratum: st[i].clone(),
                event_id: &ids[i],
                vector: &vs[i],
            })
            .collect()
    }

    /// **The C-4 property, on the thing that matters most.**
    ///
    /// Offer the same rows in a different order — which is all a different layout *is*, as far
    /// as training can tell — and the sample must not move by one vector. If it moves, the
    /// codebook moves; if the codebook moves, every byte in the store means something else.
    #[test]
    fn the_sample_does_not_depend_on_the_order_the_rows_arrive_in() {
        let (ids, st, vs) = rows(5_000, 7);

        let forward: Vec<usize> = (0..5_000).collect();
        let backward: Vec<usize> = (0..5_000).rev().collect();
        // A "layout": grouped by stratum, as a partitioned store would hand them over.
        let mut grouped: Vec<usize> = (0..5_000).collect();
        grouped.sort_by_key(|&i| (i % 7, i));

        let a = stratified_sample(&view(&ids, &st, &vs, &forward), 500, 9, "s1", false).unwrap();
        let b = stratified_sample(&view(&ids, &st, &vs, &backward), 500, 9, "s1", false).unwrap();
        let c = stratified_sample(&view(&ids, &st, &vs, &grouped), 500, 9, "s1", false).unwrap();

        assert_eq!(a.0, b.0, "reversing the input changed the training sample");
        assert_eq!(a.0, c.0, "regrouping the input changed the training sample");
        assert_eq!(a.1.rows_sampled, b.1.rows_sampled);
        assert!(a.1.rows_sampled <= 500);
    }

    /// A small stratum must not be invisible to the codebook just because a loud one exists.
    #[test]
    fn a_loud_stratum_does_not_crowd_out_a_quiet_one() {
        let mut ids = Vec::new();
        let mut st = Vec::new();
        let mut vs = Vec::new();
        for i in 0..10_000 {
            ids.push(format!("loud{i:06}"));
            st.push("loud".to_string());
            vs.push(vec![1.0, i as f32]);
        }
        for i in 0..50 {
            ids.push(format!("quiet{i:06}"));
            st.push("quiet".to_string());
            vs.push(vec![2.0, i as f32]);
        }
        let order: Vec<usize> = (0..ids.len()).collect();
        let (_, prov) =
            stratified_sample(&view(&ids, &st, &vs, &order), 400, 3, "s1", false).unwrap();

        let quiet = prov.strata.iter().find(|s| s.stratum == "quiet").unwrap();
        // Purely proportional allocation would give the quiet stratum 400 * 50/10050 = 1 row.
        // The floor is what makes its geometry visible at all.
        assert!(
            quiet.sampled >= 40,
            "the quiet stratum got {} of 400 sample slots; purely proportional allocation is not \
             stratification, it reproduces exactly the imbalance it claims to fix",
            quiet.sampled
        );
        assert!(prov.rows_sampled <= 400);
    }

    #[test]
    fn the_sample_is_bounded_and_never_exceeds_what_was_offered() {
        let (ids, st, vs) = rows(37, 5);
        let order: Vec<usize> = (0..37).collect();
        let (flat, prov) =
            stratified_sample(&view(&ids, &st, &vs, &order), 1_000, 1, "s1", false).unwrap();
        assert_eq!(
            prov.rows_sampled, 37,
            "asking for more than exists must give all of it"
        );
        assert_eq!(flat.len(), 37 * 2);
    }

    #[test]
    fn training_on_nothing_is_an_error_not_an_empty_codebook() {
        assert!(stratified_sample(&[], 10, 1, "s1", false).is_err());
    }
}
