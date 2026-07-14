//! The S4 manifest extension (D-020 in action).
//!
//! Partition metadata, per-tenant scoped statistics and promoted columns all land as a
//! **flagged TLV extension on format v3** — not as v4. That is the mechanism v3 shipped in
//! S2 for exactly this moment: the extension section existed before its first user, so that
//! the first user would not also be a format break.
//!
//! The extension is **required** (`EXT_REQUIRED`). An older reader must refuse a part
//! carrying it, and the reason is not ceremony: **a promoted key is removed from the
//! attribute map.** A reader that ignored this extension would decode the map, not find the
//! key, and report it as *absent* — silently returning wrong answers rather than an error.
//! That is precisely the failure the required bit exists to prevent.
//!
//! Every length here is untrusted input and obeys S1's discipline: bounded before allocation,
//! errors naming the byte.

use crate::format::{Cursor, Writer, EXT_REQUIRED};
use crate::partition::{Bucket, PartitionKey};
use prism_types::error::{PrismError, Result};
use prism_types::limits::MAX_ATTRIBUTE_KEY_CARDINALITY;
use serde::{Deserialize, Serialize};

/// Partition + per-tenant stats + promoted columns. Required.
pub const EXT_S4_PARTITION: u16 = 0x8001 | EXT_REQUIRED;

/// Lineage + retention (S5). **Required.**
///
/// Required, and the requirement is not ceremonial. This extension is what says *"the raw
/// bodies in this part are gone"*. A reader that skipped it would see a column full of empty
/// strings and have no way to tell "this event had no body" from "this event's body expired
/// under retention" — and a re-embed migration that could not tell those apart would either
/// dead-letter a partition it should have reported as un-re-embeddable, or, if some future
/// embedder tolerated empty input, write a part full of meaningless vectors and call the
/// migration a success. The whole point of §7 of the generation contract is that the second
/// outcome is unacceptable *silently*, so the format refuses to let a reader be unaware.
pub const EXT_S5_LINEAGE: u16 = 0x8002 | EXT_REQUIRED;

/// What a part remembers about where it came from and what it has lost.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct S5Ext {
    /// The generation whose rows were re-embedded to produce this part, if it was produced by a
    /// migration. Lineage: it makes a migration *enumerable* rather than a matter of faith.
    pub reembedded_from: Option<String>,
    /// The raw bodies are gone — expired under retention policy.
    ///
    /// The rows are still here and still queryable; what is gone is the text they were embedded
    /// from. Which means they can never be re-embedded into a new space, which means any drift
    /// baseline that would have been rebuilt from them **cannot be**, which is exactly when an
    /// alarm goes DEGRADED instead of silent.
    pub bodies_redacted: bool,
    pub redacted_at_ms: i64,
    /// Why. An operator asking "where did my bodies go" deserves a better answer than "policy".
    pub redaction_reason: String,
}

impl S5Ext {
    pub fn is_default(&self) -> bool {
        self == &S5Ext::default()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match &self.reembedded_from {
            None => w.u8(0),
            Some(g) => {
                w.u8(1);
                w.string(g);
            }
        }
        w.u8(u8::from(self.bodies_redacted));
        w.i64(self.redacted_at_ms);
        w.string(&self.redaction_reason);
        w.buf
    }

    pub fn decode(bytes: &[u8]) -> Result<S5Ext> {
        let mut c = Cursor::new(bytes);
        let reembedded_from = if c.u8()? == 1 {
            Some(c.string()?)
        } else {
            None
        };
        let bodies_redacted = c.u8()? != 0;
        let redacted_at_ms = c.i64()?;
        let redaction_reason = c.string()?;
        let e = S5Ext {
            reembedded_from,
            bodies_redacted,
            redacted_at_ms,
            redaction_reason,
        };
        if e.bodies_redacted && e.redaction_reason.is_empty() {
            return Err(PrismError::Corrupt(
                "part says its bodies were redacted but gives no reason; an irreversible \
                 deletion with no recorded cause is not a retention policy, it is data loss"
                    .into(),
            ));
        }
        Ok(e)
    }
}

/// Sanity bound on how many tenants may share one part. A shared bucket holding thousands of
/// tenants would make the per-tenant sections larger than the data.
pub const MAX_TENANTS_PER_PART: usize = 4_096;

/// Per-tenant scoped metadata (directive 3).
///
/// **This is the answer to the shared-bucket seam.** In a shared bucket, part-level metadata
/// describes *the bucket*, not the tenant: a single `time_min`/`time_max` pair, a single cost
/// range, one union attribute-key dictionary, one set of centroid ranges. Every one of those
/// tells tenant A something about tenant B.
///
/// So the metadata a query can *observe* is scoped per tenant, and a query only ever reads
/// its own section. "Does this part contain key X?" is answerable **per tenant** —
/// `TenantStats::has_key` — and a zone map is a zone map *for one tenant*.
///
/// What is **not** hidden, and is documented rather than pretended away
/// (`docs/QUERY-CONTRACT.md` §8, D-030): the union key dictionary and the tenant list are in
/// the manifest bytes, because the dictionary is what decodes the attribute column and the
/// tenant list is what prunes the part. An operator with **raw disk access** to a shared
/// bucket can therefore see which tenants share it and the union of their attribute keys. No
/// query can. A tenant who cannot accept that gets a **dedicated bucket**, and S14's envelope
/// encryption closes it properly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TenantStats {
    pub tenant: String,
    pub rows: usize,
    pub time_min: i64,
    pub time_max: i64,
    pub cost_min: f64,
    pub cost_max: f64,
    pub has_error: bool,
    pub has_success: bool,
    /// The attribute keys **this tenant** uses. Scoped, so `has_key` cannot answer a question
    /// about somebody else's data.
    pub attribute_keys: Vec<String>,
}

impl TenantStats {
    pub fn has_key(&self, key: &str) -> bool {
        self.attribute_keys.iter().any(|k| k == key)
    }

    /// Zone-map check, scoped to this tenant.
    pub fn may_match(&self, from: Option<i64>, to: Option<i64>) -> bool {
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
}

/// An attribute key promoted to a typed top-level column (issue #2, directive 4).
///
/// **Promotion is a versioned, generation-like schema event, never an in-place rewrite.** A
/// part is written either promoting a key or not; existing parts are never touched. So the
/// two representations — typed column and attribute map — **coexist across parts of different
/// ages**, and every reader must dispatch on which one a given part uses.
///
/// The key is *removed from the map* in a part that promotes it. Storing it twice would make
/// promotion cost storage rather than save it, and would leave two sources of truth for one
/// value — which is worse than either.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromotedColumn {
    /// The attribute key this column replaces, e.g. `gen_ai.system`.
    pub key: String,
    /// The logical column name in the part, e.g. `attr.gen_ai.system`.
    pub column: String,
    /// The value type. A promoted column is typed; that is the point.
    pub type_tag: u8,
}

impl PromotedColumn {
    pub fn column_for(key: &str) -> String {
        format!("attr.{key}")
    }
}

/// The decoded S4 extension.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct S4Ext {
    pub partition: Option<PartitionKey>,
    pub tenant_stats: Vec<TenantStats>,
    pub promoted: Vec<PromotedColumn>,
}

impl S4Ext {
    pub fn stats_for(&self, tenant: &str) -> Option<&TenantStats> {
        self.tenant_stats.iter().find(|t| t.tenant == tenant)
    }

    /// Owned lookup, for callers that cannot hold a borrow across the decode.
    pub fn stats_for_owned(&self, tenant: Option<&str>) -> Option<TenantStats> {
        tenant.and_then(|t| self.stats_for(t).cloned())
    }

    pub fn promoted_for(&self, key: &str) -> Option<&PromotedColumn> {
        self.promoted.iter().find(|p| p.key == key)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();

        match &self.partition {
            None => w.u8(0),
            Some(p) => {
                w.u8(1);
                match p.bucket {
                    Bucket::Shared(i) => {
                        w.u8(0);
                        w.u32(i);
                    }
                    Bucket::Dedicated(i) => {
                        w.u8(1);
                        w.u32(i);
                    }
                }
                w.i64(p.window);
                w.string(&p.generation);
            }
        }

        w.len(self.tenant_stats.len());
        for t in &self.tenant_stats {
            w.string(&t.tenant);
            w.u64(t.rows as u64);
            w.i64(t.time_min);
            w.i64(t.time_max);
            w.f64(t.cost_min);
            w.f64(t.cost_max);
            w.u8(u8::from(t.has_error) | (u8::from(t.has_success) << 1));
            w.len(t.attribute_keys.len());
            for k in &t.attribute_keys {
                w.string(k);
            }
        }

        w.len(self.promoted.len());
        for p in &self.promoted {
            w.string(&p.key);
            w.string(&p.column);
            w.u8(p.type_tag);
        }
        w.buf
    }

    pub fn decode(bytes: &[u8]) -> Result<S4Ext> {
        let mut c = Cursor::new(bytes);

        let partition = if c.u8()? == 1 {
            let bucket = match c.u8()? {
                0 => Bucket::Shared(c.u32()?),
                1 => Bucket::Dedicated(c.u32()?),
                other => {
                    return Err(PrismError::Corrupt(format!(
                        "part declares bucket kind {other}, which this build does not know"
                    )))
                }
            };
            let window = c.i64()?;
            let generation = c.string()?;
            Some(PartitionKey {
                bucket,
                window,
                generation,
            })
        } else {
            None
        };

        // A tenant section is at least: 4-byte name len + 8 rows + 8 + 8 + 8 + 8 + 1 + 4.
        let n_tenants = c.read_len(49, "tenant sections")?;
        if n_tenants > MAX_TENANTS_PER_PART {
            return Err(PrismError::Corrupt(format!(
                "part declares {n_tenants} tenant sections, over the {MAX_TENANTS_PER_PART} bound"
            )));
        }
        let mut tenant_stats = Vec::with_capacity(n_tenants);
        for _ in 0..n_tenants {
            let tenant = c.string()?;
            let rows = c.u64()? as usize;
            let time_min = c.i64()?;
            let time_max = c.i64()?;
            let cost_min = c.f64()?;
            let cost_max = c.f64()?;
            let flags = c.u8()?;
            let n_keys = c.read_len(4, "tenant attribute keys")?;
            if n_keys > MAX_ATTRIBUTE_KEY_CARDINALITY {
                return Err(PrismError::Corrupt(format!(
                    "tenant `{tenant}` declares {n_keys} attribute keys, over the \
                     {MAX_ATTRIBUTE_KEY_CARDINALITY} cardinality bound"
                )));
            }
            let mut attribute_keys = Vec::with_capacity(n_keys);
            for _ in 0..n_keys {
                attribute_keys.push(c.string()?);
            }
            if time_min > time_max {
                return Err(PrismError::Corrupt(format!(
                    "tenant `{tenant}` has time_min {time_min} > time_max {time_max}; a zone map \
                     that cannot be true would prune rows that exist"
                )));
            }
            tenant_stats.push(TenantStats {
                tenant,
                rows,
                time_min,
                time_max,
                cost_min,
                cost_max,
                has_error: flags & 1 != 0,
                has_success: flags & 2 != 0,
                attribute_keys,
            });
        }

        let n_promoted = c.read_len(9, "promoted columns")?;
        let mut promoted = Vec::with_capacity(n_promoted);
        for _ in 0..n_promoted {
            promoted.push(PromotedColumn {
                key: c.string()?,
                column: c.string()?,
                type_tag: c.u8()?,
            });
        }

        Ok(S4Ext {
            partition,
            tenant_stats,
            promoted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext() -> S4Ext {
        S4Ext {
            partition: Some(PartitionKey {
                bucket: Bucket::Shared(3),
                window: 1_760_000_000_000,
                generation: "gen0".into(),
            }),
            tenant_stats: vec![
                TenantStats {
                    tenant: "a".into(),
                    rows: 10,
                    time_min: 1,
                    time_max: 9,
                    cost_min: 0.0,
                    cost_max: 1.0,
                    has_error: true,
                    has_success: true,
                    attribute_keys: vec!["k1".into()],
                },
                TenantStats {
                    tenant: "b".into(),
                    rows: 5,
                    time_min: 3,
                    time_max: 4,
                    cost_min: 0.5,
                    cost_max: 0.5,
                    has_error: false,
                    has_success: true,
                    attribute_keys: vec!["k2".into(), "k3".into()],
                },
            ],
            promoted: vec![PromotedColumn {
                key: "gen_ai.system".into(),
                column: "attr.gen_ai.system".into(),
                type_tag: 1,
            }],
        }
    }

    #[test]
    fn it_round_trips() {
        assert_eq!(S4Ext::decode(&ext().encode()).unwrap(), ext());
    }

    #[test]
    fn stats_are_scoped_so_one_tenant_cannot_ask_about_another() {
        // The shared-bucket seam, in one assertion. Tenant `a` can ask whether *its own* rows
        // use key `k1`. It cannot learn anything about `k2` -- that is `b`'s key, and it lives
        // in `b`'s section.
        let e = ext();
        let a = e.stats_for("a").unwrap();
        assert!(a.has_key("k1"));
        assert!(
            !a.has_key("k2"),
            "tenant a can see tenant b's attribute keys"
        );

        // And a zone map is a zone map for ONE tenant. `b`'s rows span 3..4 while the part
        // spans 1..9; a query for `b` outside 3..4 must be able to skip this part even though
        // the part as a whole overlaps.
        let b = e.stats_for("b").unwrap();
        assert!(!b.may_match(Some(5), None));
        assert!(a.may_match(Some(5), None));
    }

    #[test]
    fn the_required_bit_is_set() {
        // A reader that does not know this extension MUST refuse the part: a promoted key is
        // removed from the attribute map, so ignoring the extension would report the key as
        // absent -- silently wrong answers rather than an error.
        assert_ne!(EXT_S4_PARTITION & EXT_REQUIRED, 0);
    }

    #[test]
    fn absurd_counts_are_refused_before_anything_is_allocated() {
        let mut w = Writer::new();
        w.u8(0); // no partition
        w.u32(u32::MAX); // "four billion tenant sections"
        let e = S4Ext::decode(&w.buf).unwrap_err().to_string();
        assert!(e.contains("refusing to allocate"), "{e}");
    }

    #[test]
    fn an_impossible_zone_map_is_refused() {
        // time_min > time_max would prune rows that exist. A zone map that cannot be true is
        // worse than no zone map, because it is *trusted*.
        let mut bad = ext();
        bad.tenant_stats[0].time_min = 100;
        bad.tenant_stats[0].time_max = 0;
        let e = S4Ext::decode(&bad.encode()).unwrap_err().to_string();
        assert!(e.contains("prune rows that exist"), "{e}");
    }

    #[test]
    fn an_unknown_bucket_kind_is_refused_not_guessed() {
        let mut w = Writer::new();
        w.u8(1);
        w.u8(7); // not a bucket kind
        assert!(S4Ext::decode(&w.buf).is_err());
    }
}
