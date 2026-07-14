//! Outer partitioning (S4): `tenant-bucket × event-time window × generation`.
//!
//! > *"partitioning makes isolation and retention **structural** (cross-tenant reads are
//! > physically impossible, deletes/TTL are partition drops)"* — PRISM.md, Part III §9
//!
//! **"Physically impossible" is only a claim until it is an I/O property.** So S4 defines it
//! as one:
//!
//! > A query's execution trace never touches a byte range belonging to another tenant's
//! > partition.
//!
//! That has a sharp consequence, and it is the reason this module exists rather than living
//! in the part manifest. Until now, pruning read *every part's manifest* to decide which
//! parts to skip — which means a tenant-A query already touched bytes belonging to tenant
//! B's parts, and a corrupt part anywhere would break a query everywhere.
//!
//! So the partition key is carried in the **catalog snapshot**, above the parts. Pruning
//! happens against catalog metadata, and a part outside the query's partitions is never
//! opened, never checksummed, never read. Fill another tenant's partitions with unreadable
//! garbage and a tenant-A query still answers correctly — **because it never looked.** That
//! is the strongest form of the test, and it is the S4 gate.
//!
//! Every length and count decoded here is untrusted input, and obeys S1's discipline: bounds
//! checked before allocation, errors naming the byte.

use prism_types::error::{PrismError, Result};
use prism_types::hash::sha256;
use serde::{Deserialize, Serialize};

/// How many shared buckets a store hashes small tenants into.
///
/// A **policy** constant (C-1). Not a measured optimum: a statement about the granularity at
/// which small tenants share physical parts. More buckets means better isolation and more,
/// smaller parts; fewer means the opposite. Large tenants get a *dedicated* bucket and never
/// share, which is the escape hatch for anyone who cannot accept co-tenancy at all
/// (see D-030 and `docs/QUERY-CONTRACT.md` §8).
pub const DEFAULT_BUCKETS: u32 = 16;

/// Milliseconds per time partition. A **policy** constant: a day.
///
/// Time windows make retention a partition *drop* rather than a scan-and-delete, and they
/// are the unit at which a merge is allowed to combine parts. A day is the granularity most
/// telemetry retention policies are actually written in.
pub const DEFAULT_TIME_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// Which physical bucket a tenant's rows live in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
pub enum Bucket {
    /// This tenant has the bucket to itself. No co-tenancy, no shared metadata, nothing to
    /// leak. The answer for anyone who cannot accept a shared bucket.
    Dedicated(u32),
    /// Small tenants hashed together. Rows are separated by a fused mask; *metadata* is
    /// separated by per-tenant sections in the manifest (D-030).
    Shared(u32),
}

impl Bucket {
    pub fn id(&self) -> u32 {
        match self {
            Bucket::Dedicated(i) | Bucket::Shared(i) => *i,
        }
    }

    pub fn is_shared(&self) -> bool {
        matches!(self, Bucket::Shared(_))
    }

    /// A stable directory-safe name. Dedicated buckets are namespaced apart from shared ones
    /// so a tenant can be promoted to a dedicated bucket without colliding with a hash slot.
    pub fn dir(&self) -> String {
        match self {
            Bucket::Dedicated(i) => format!("d{i}"),
            Bucket::Shared(i) => format!("s{i}"),
        }
    }
}

/// How a store maps tenants onto buckets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionScheme {
    pub buckets: u32,
    pub time_window_ms: i64,
    /// Tenants with a bucket to themselves.
    #[serde(default)]
    pub dedicated: std::collections::BTreeMap<String, u32>,
}

impl Default for PartitionScheme {
    fn default() -> Self {
        PartitionScheme {
            buckets: DEFAULT_BUCKETS,
            time_window_ms: DEFAULT_TIME_WINDOW_MS,
            dedicated: Default::default(),
        }
    }
}

impl PartitionScheme {
    pub fn validate(&self) -> Result<()> {
        if self.buckets == 0 {
            return Err(PrismError::Invalid("buckets must be positive".into()));
        }
        if self.time_window_ms <= 0 {
            return Err(PrismError::Invalid(
                "time_window_ms must be positive".into(),
            ));
        }
        Ok(())
    }

    /// Which bucket a tenant lives in.
    ///
    /// Hashed with SHA-256, not with a fast non-cryptographic hash. A tenant must not be able
    /// to *choose* which bucket they land in by choosing their id — co-tenancy is our
    /// decision, not theirs, and an attacker who can steer themselves into a chosen victim's
    /// bucket has turned a metadata question into a targeting one.
    pub fn bucket_of(&self, tenant: &str) -> Bucket {
        if let Some(i) = self.dedicated.get(tenant) {
            return Bucket::Dedicated(*i);
        }
        let h = sha256(tenant.as_bytes());
        let n = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
        Bucket::Shared(n % self.buckets)
    }

    /// The time window an event belongs to. Keyed on `event_time` — always. Agent telemetry
    /// is late by nature, and keying on arrival would smear one trace across partitions.
    pub fn window_of(&self, event_time: i64) -> i64 {
        event_time.div_euclid(self.time_window_ms) * self.time_window_ms
    }
}

/// The outer partition key: `tenant-bucket × event-time window × semantic generation`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
pub struct PartitionKey {
    pub bucket: Bucket,
    /// Inclusive start of the time window.
    pub window: i64,
    pub generation: String,
}

impl PartitionKey {
    /// A directory-safe, sortable name.
    pub fn dir(&self) -> String {
        format!(
            "b={}/w={:020}/g={}",
            self.bucket.dir(),
            self.window,
            self.generation
        )
    }
}

/// What the **catalog** knows about a part — enough to prune it without opening it.
///
/// This is the whole point. Every field here is one a query needs to decide *whether to
/// look*, and none of it requires reading a single byte of the part.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartRef {
    pub part_id: String,
    pub partition: PartitionKey,
    pub rows: usize,
    /// The tenants whose rows are in this part. In a dedicated bucket this is exactly one.
    pub tenants: Vec<String>,
    /// Zone map, at partition granularity.
    pub time_min: i64,
    pub time_max: i64,
}

impl PartRef {
    /// Can a query for `tenant` over `[from, to]` possibly need this part?
    ///
    /// **Conservative, and it must stay that way.** It may say yes and contribute nothing; it
    /// may never say no to a part that holds a match. Pruning that can lose a row is not
    /// pruning, it is sampling — which is why `pruning_never_produces_a_false_negative` is a
    /// property test over randomized metadata rather than a handful of examples.
    pub fn may_match(&self, tenant: &str, from: Option<i64>, to: Option<i64>) -> bool {
        if !self.tenants.iter().any(|t| t == tenant) {
            return false;
        }
        if let Some(f) = from {
            if self.time_max < f {
                return false;
            }
        }
        if let Some(t) = to {
            if self.time_min > t {
                return false;
            }
        }
        true
    }

    /// Structural validation. Every field arrived from a file a stranger may have edited.
    pub fn validate(&self) -> Result<()> {
        if self.part_id.is_empty() {
            return Err(PrismError::Corrupt(
                "catalog part entry has no part id".into(),
            ));
        }
        if self.time_min > self.time_max {
            return Err(PrismError::Corrupt(format!(
                "catalog entry for part {} has time_min {} > time_max {}",
                self.part_id, self.time_min, self.time_max
            )));
        }
        if self.tenants.is_empty() {
            return Err(PrismError::Corrupt(format!(
                "catalog entry for part {} names no tenants; it could never be pruned in or out",
                self.part_id
            )));
        }
        if matches!(self.partition.bucket, Bucket::Dedicated(_)) && self.tenants.len() > 1 {
            return Err(PrismError::Corrupt(format!(
                "part {} is in a DEDICATED bucket but names {} tenants; a dedicated bucket that \
                 holds two tenants is not dedicated, and every isolation claim resting on it is \
                 false",
                self.part_id,
                self.tenants.len()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tenant_cannot_choose_its_own_bucket() {
        // Hashed with SHA-256 on purpose. If a tenant could steer themselves into a chosen
        // victim's bucket by picking their id, a metadata question would become a targeting
        // one.
        let s = PartitionScheme::default();
        let a = s.bucket_of("acme");
        assert_eq!(a, s.bucket_of("acme"), "bucket assignment must be stable");

        // Spread: not everything in one bucket.
        let used: std::collections::BTreeSet<u32> = (0..200)
            .map(|i| s.bucket_of(&format!("tenant-{i}")).id())
            .collect();
        assert!(
            used.len() > 8,
            "buckets are badly distributed: {}",
            used.len()
        );
    }

    #[test]
    fn a_dedicated_tenant_never_shares() {
        let mut s = PartitionScheme::default();
        s.dedicated.insert("whale".into(), 0);
        assert_eq!(s.bucket_of("whale"), Bucket::Dedicated(0));
        assert!(!s.bucket_of("whale").is_shared());
        // And a dedicated bucket id cannot collide with a shared one: different namespaces.
        assert_ne!(Bucket::Dedicated(0).dir(), Bucket::Shared(0).dir());
    }

    #[test]
    fn time_windows_key_on_event_time_and_handle_the_epoch_edges() {
        let s = PartitionScheme::default();
        let day = DEFAULT_TIME_WINDOW_MS;
        assert_eq!(s.window_of(0), 0);
        assert_eq!(s.window_of(day - 1), 0);
        assert_eq!(s.window_of(day), day);
        // Negative event times (pre-1970) must floor, not truncate toward zero -- otherwise
        // two different instants land in the same window from opposite sides of the epoch.
        assert_eq!(s.window_of(-1), -day);
    }

    #[test]
    fn pruning_never_says_no_to_a_part_it_should_have_opened() {
        let p = PartRef {
            part_id: "p1".into(),
            partition: PartitionKey {
                bucket: Bucket::Shared(3),
                window: 0,
                generation: "g".into(),
            },
            rows: 10,
            tenants: vec!["a".into(), "b".into()],
            time_min: 100,
            time_max: 200,
        };
        assert!(p.may_match("a", None, None));
        assert!(p.may_match("a", Some(150), Some(150)));
        assert!(p.may_match("a", Some(200), None));
        assert!(p.may_match("a", None, Some(100)));
        // Genuinely disjoint.
        assert!(!p.may_match("a", Some(201), None));
        assert!(!p.may_match("a", None, Some(99)));
        // Another tenant, ever.
        assert!(!p.may_match("c", None, None));
    }

    #[test]
    fn a_dedicated_bucket_holding_two_tenants_is_refused() {
        // If this were accepted, every isolation claim resting on dedicated buckets would be
        // false, and nothing else in the system would notice.
        let p = PartRef {
            part_id: "p1".into(),
            partition: PartitionKey {
                bucket: Bucket::Dedicated(0),
                window: 0,
                generation: "g".into(),
            },
            rows: 2,
            tenants: vec!["a".into(), "b".into()],
            time_min: 0,
            time_max: 1,
        };
        assert!(p.validate().is_err());
    }
}
