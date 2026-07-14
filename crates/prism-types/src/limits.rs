//! Admission limits (S2).
//!
//! > *"`attributes` is where formats go to die — bound it before it exists."*
//!
//! Every limit here is enforced at the admission boundary, and every violation is
//! **dead-lettered with a named reason** — never silently truncated. A truncated
//! attribute map is a lie that nobody will ever catch, because the event is still
//! there, still queryable, and simply missing the field that would have explained
//! the incident.
//!
//! The contract these implement is [`docs/INGESTION-CONTRACT.md`](../../../docs/INGESTION-CONTRACT.md).
//! Where the two disagree, the document is right and this file is a bug.
//!
//! These are **policy** constants, not tuned ones (charter amendment C-1). A cap
//! of 64 attribute keys is not the measured optimum number of attribute keys; it
//! is a decision about what we are willing to accept. Each therefore owes a
//! rationale rather than a benchmark, and the rationale is §5 of the contract.

use serde::{Deserialize, Serialize};

// --- the size of one event ---------------------------------------------------

/// Distinct attribute keys on a single event.
pub const MAX_ATTRIBUTE_KEYS: usize = 64;

/// Bytes in one attribute key.
pub const MAX_ATTRIBUTE_KEY_BYTES: usize = 128;

/// Bytes in one attribute value.
pub const MAX_ATTRIBUTE_VALUE_BYTES: usize = 4 * 1024;

/// Total attribute bytes (keys + values) on a single event.
pub const MAX_ATTRIBUTES_BYTES: usize = 16 * 1024;

// --- the shape of the data ---------------------------------------------------

/// **The one that actually matters.**
///
/// The four limits above bound the size of one *event*. Only this one bounds the
/// shape of the *dataset*.
///
/// A tenant emitting `user_id_<uuid>` as an attribute **key** produces a key
/// dictionary the size of their traffic, and every part carries it. That is how a
/// columnar format dies: not with a single huge event, but with ten million tiny
/// ones that each introduce a column nobody will ever query.
///
/// So attribute keys are a bounded dictionary per partition. When it is full, an
/// event introducing a *new* key is refused at admission — not absorbed, not
/// spilled to a side table, not silently dropped from the map. The tenant is told
/// they are emitting unbounded key cardinality, because that is a bug in their
/// instrumentation and one they can only fix if we tell them about it.
///
/// Attribute *values* are unbounded in cardinality. A `session_id` value is fine
/// and normal; a `session_id` key is not.
pub const MAX_ATTRIBUTE_KEY_CARDINALITY: usize = 512;

// --- time ---------------------------------------------------------------------

/// How far in the **past** an `event_time` may be, relative to `observed_time`.
///
/// Beyond this the partition it belongs to may already have been merged, tiered
/// or expired by retention, and admitting it would resurrect a closed partition.
pub const MAX_LATENESS_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// How far into the **future** an `event_time` may be.
///
/// A clock-skewed producer emitting timestamps months ahead poisons zone maps and
/// retention *forever*: one event with `event_time = 2099` makes its partition
/// immortal, and no retention policy will ever reclaim it.
pub const MAX_SKEW_AHEAD_MS: i64 = 60 * 60 * 1000;

// --- quotas -------------------------------------------------------------------

/// Per-tenant admission quotas, enforced **before** any GPU time is spent.
///
/// Over-quota events are *rejected*, not queued. An unbounded queue is not a
/// quota — it is a slower way to run out of memory.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Quota {
    pub events_per_sec: u64,
    pub bytes_per_sec: u64,
    /// Bytes a single tenant may have in flight in one batch.
    pub in_flight_bytes: u64,
}

impl Default for Quota {
    fn default() -> Self {
        Quota {
            events_per_sec: 10_000,
            bytes_per_sec: 32 * 1024 * 1024,
            in_flight_bytes: 64 * 1024 * 1024,
        }
    }
}

/// How long the idempotency index remembers a key.
///
/// The honest limit: beyond this window a replayed key is no longer recognised
/// and is admitted as a new event, becoming a duplicate row that merge reconciles
/// by last-write-wins on `event_time` (D-012). Two mechanisms, one seam, both
/// documented — see §2 of the ingestion contract.
pub const IDEMPOTENCY_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// A hard cap on the idempotency index, so it cannot grow without bound even if
/// the window never rolls over.
pub const IDEMPOTENCY_MAX_ENTRIES: usize = 1_000_000;

/// Every way an event can fail admission.
///
/// A string would have been easier and would have rotted. This is an enum because
/// a dead-letter reason is an **API**: an operator greps for it, an alert fires on
/// it, and a producer fixes their instrumentation because of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    // --- schema ---
    MissingEventId,
    MissingTenantId,
    EmptyBody,
    BodyTooLarge,
    InvalidCost,

    // --- attributes (directive 1) ---
    TooManyAttributeKeys,
    AttributeKeyTooLong,
    AttributeValueTooLong,
    AttributesTooLarge,
    /// The partition's key dictionary is full and this event introduces a new key.
    AttributeKeyCardinalityExceeded,

    // --- time ---
    EventTimeTooLate,
    EventTimeInFuture,

    // --- identity ---
    /// The same key, with *different* content. Not a replay: a conflict. We refuse
    /// rather than silently rewrite history under a reused id.
    IdempotencyConflict,

    // --- capacity ---
    QuotaExceeded,

    // --- the semantic columns could not be produced ---
    EmbeddingFailed,
}

impl RejectReason {
    /// The stable string an operator greps for.
    pub fn as_str(&self) -> &'static str {
        match self {
            RejectReason::MissingEventId => "missing_event_id",
            RejectReason::MissingTenantId => "missing_tenant_id",
            RejectReason::EmptyBody => "empty_body",
            RejectReason::BodyTooLarge => "body_too_large",
            RejectReason::InvalidCost => "invalid_cost",
            RejectReason::TooManyAttributeKeys => "too_many_attribute_keys",
            RejectReason::AttributeKeyTooLong => "attribute_key_too_long",
            RejectReason::AttributeValueTooLong => "attribute_value_too_long",
            RejectReason::AttributesTooLarge => "attributes_too_large",
            RejectReason::AttributeKeyCardinalityExceeded => "attribute_key_cardinality_exceeded",
            RejectReason::EventTimeTooLate => "event_time_too_late",
            RejectReason::EventTimeInFuture => "event_time_in_future",
            RejectReason::IdempotencyConflict => "idempotency_conflict",
            RejectReason::QuotaExceeded => "quota_exceeded",
            RejectReason::EmbeddingFailed => "embedding_failed",
        }
    }
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_reasons_have_stable_distinct_names() {
        // These strings are an API. An operator alerts on them; a duplicate or a
        // rename is a silently broken runbook.
        let all = [
            RejectReason::MissingEventId,
            RejectReason::MissingTenantId,
            RejectReason::EmptyBody,
            RejectReason::BodyTooLarge,
            RejectReason::InvalidCost,
            RejectReason::TooManyAttributeKeys,
            RejectReason::AttributeKeyTooLong,
            RejectReason::AttributeValueTooLong,
            RejectReason::AttributesTooLarge,
            RejectReason::AttributeKeyCardinalityExceeded,
            RejectReason::EventTimeTooLate,
            RejectReason::EventTimeInFuture,
            RejectReason::IdempotencyConflict,
            RejectReason::QuotaExceeded,
            RejectReason::EmbeddingFailed,
        ];
        let mut seen = std::collections::BTreeSet::new();
        for r in all {
            assert!(!r.as_str().is_empty());
            assert!(seen.insert(r.as_str()), "duplicate reason {}", r.as_str());
        }
        assert_eq!(seen.len(), 15);
    }

    // These read the constants through `black_box` so they are checked as *relations
    // between the limits* at test time, not folded away as trivially-true constants at
    // compile time. If someone edits one cap in isolation, these fail — which is the
    // point: the caps only make sense together.
    fn v(x: usize) -> usize {
        std::hint::black_box(x)
    }

    #[test]
    fn the_cardinality_cap_is_the_one_that_bounds_the_dataset() {
        // A single event cannot on its own exhaust the partition-wide key dictionary.
        // If it could, one malformed event would lock out a whole partition — turning
        // a producer's bug into our outage.
        assert!(v(MAX_ATTRIBUTE_KEYS) < v(MAX_ATTRIBUTE_KEY_CARDINALITY));
    }

    #[test]
    fn per_event_caps_are_mutually_consistent() {
        // One maximal value must fit...
        assert!(v(MAX_ATTRIBUTE_VALUE_BYTES) < v(MAX_ATTRIBUTES_BYTES));
        // ...and the per-key caps together must be *able* to exceed the total, or the
        // total cap would never bind and would be decoration.
        assert!(
            v(MAX_ATTRIBUTE_KEYS) * (v(MAX_ATTRIBUTE_KEY_BYTES) + v(MAX_ATTRIBUTE_VALUE_BYTES))
                > v(MAX_ATTRIBUTES_BYTES)
        );
    }
}
