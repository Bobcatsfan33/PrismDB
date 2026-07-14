use crate::attributes::{self, Attributes};
use crate::error::{PrismError, Result};
use crate::limits::{RejectReason, MAX_LATENESS_MS, MAX_SKEW_AHEAD_MS};
use serde::{Deserialize, Serialize};

/// Admission limit on the raw body. Oversized bodies are dead-lettered, never
/// silently truncated (Part III §10: an event is never stored without the
/// semantic columns it asked for, and admission failures must be *visible*).
pub const MAX_BODY_BYTES: usize = 1 << 20; // 1 MiB

/// The logical event model (Part III §9), as of S2.
///
/// The two timestamps are not interchangeable and confusing them is how a
/// telemetry system quietly loses the ability to answer questions about the past:
///
/// * `event_time` — when the thing **happened**. Set by the producer. This is what
///   partitions, zone maps, retention and every time predicate key on. Agent
///   telemetry is late by nature — a trace is flushed minutes after the span it
///   describes — so keying on arrival would smear a single trace across partitions
///   and make time pruning worthless.
/// * `observed_time` — when **we received it**. Set at the admission boundary. Used
///   for lag measurement and for the skew check, and for nothing else.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub event_id: String,
    pub tenant_id: String,
    /// When it happened. Epoch milliseconds.
    pub event_time: i64,
    /// When we received it. Epoch milliseconds.
    pub observed_time: i64,
    pub event_name: String,
    pub cost: f64,
    pub error: bool,
    /// The text that gets embedded.
    pub body: String,

    /// W3C trace context. Empty when the producer sent none.
    #[serde(default)]
    pub trace_id: String,
    #[serde(default)]
    pub span_id: String,

    /// Bounded, typed attributes. See `docs/INGESTION-CONTRACT.md` §5.
    #[serde(default)]
    pub attributes: Attributes,

    /// What an idempotency key covers: `(tenant_id, idempotency_key)`, where the
    /// key defaults to `event_id`. Stored, because "why was this suppressed?" is a
    /// question an operator will ask and deserves an answer to.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

impl Event {
    /// The key this event is deduplicated under.
    pub fn dedup_key(&self) -> (&str, &str) {
        (
            &self.tenant_id,
            self.idempotency_key.as_deref().unwrap_or(&self.event_id),
        )
    }

    /// The hash of everything that makes this event *this event*.
    ///
    /// Deliberately excludes `observed_time` and `idempotency_key`: a replay of the
    /// same event necessarily arrives at a different instant, and that must not
    /// make it look like a *conflict*. It also excludes `event_id` — the id is the
    /// identity, and hashing the identity into the content would make every event
    /// trivially unique and the conflict check useless.
    pub fn content_hash(&self) -> String {
        use crate::hash::{content_id, sha256};
        let mut buf = Vec::new();
        buf.extend_from_slice(self.tenant_id.as_bytes());
        buf.extend_from_slice(&self.event_time.to_le_bytes());
        buf.extend_from_slice(self.event_name.as_bytes());
        buf.extend_from_slice(&self.cost.to_bits().to_le_bytes());
        buf.push(u8::from(self.error));
        buf.extend_from_slice(self.body.as_bytes());
        buf.extend_from_slice(self.trace_id.as_bytes());
        buf.extend_from_slice(self.span_id.as_bytes());
        for (k, v) in &self.attributes {
            buf.extend_from_slice(k.as_bytes());
            buf.push(v.type_tag());
            buf.extend_from_slice(v.as_display().as_bytes());
        }
        let _ = sha256(&buf);
        content_id(&buf)
    }

    /// Roughly what this event costs, for quota accounting.
    pub fn byte_size(&self) -> usize {
        self.event_id.len()
            + self.tenant_id.len()
            + self.event_name.len()
            + self.body.len()
            + self.trace_id.len()
            + self.span_id.len()
            + attributes::byte_size(&self.attributes)
    }

    /// Schema validation: everything checkable without knowing anything about the
    /// store's state. Cardinality, quotas, idempotency and skew are *admission*
    /// concerns and live in `prism_engine::admission`, because each needs to know
    /// something this event does not.
    pub fn validate(&self) -> std::result::Result<(), (RejectReason, String)> {
        if self.event_id.is_empty() {
            return Err((RejectReason::MissingEventId, "event_id is empty".into()));
        }
        if self.tenant_id.is_empty() {
            return Err((RejectReason::MissingTenantId, "tenant_id is empty".into()));
        }
        if !self.cost.is_finite() {
            return Err((
                RejectReason::InvalidCost,
                format!("cost is not finite: {}", self.cost),
            ));
        }
        if self.cost < 0.0 {
            return Err((
                RejectReason::InvalidCost,
                format!("cost is negative: {}", self.cost),
            ));
        }
        if self.body.len() > MAX_BODY_BYTES {
            return Err((
                RejectReason::BodyTooLarge,
                format!(
                    "body is {} bytes, limit is {MAX_BODY_BYTES}",
                    self.body.len()
                ),
            ));
        }
        if self.body.trim().is_empty() {
            return Err((
                RejectReason::EmptyBody,
                "body is empty; it would produce a zero-norm embedding".into(),
            ));
        }
        attributes::validate(&self.attributes)?;
        Ok(())
    }

    /// The skew check. An event outside the accepted window is **dead-lettered,
    /// never clamped** — silently rewriting a producer's timestamp is falsifying
    /// their data, and they will believe us.
    pub fn check_skew(&self, now_ms: i64) -> std::result::Result<(), (RejectReason, String)> {
        let lateness = now_ms - self.event_time;
        if lateness > MAX_LATENESS_MS {
            return Err((
                RejectReason::EventTimeTooLate,
                format!(
                    "event_time is {lateness} ms behind observed_time, past the {MAX_LATENESS_MS} ms \
                     lateness bound; its partition may already have been merged, tiered or expired"
                ),
            ));
        }
        if -lateness > MAX_SKEW_AHEAD_MS {
            return Err((
                RejectReason::EventTimeInFuture,
                format!(
                    "event_time is {} ms in the future, past the {MAX_SKEW_AHEAD_MS} ms bound; a \
                     clock-skewed producer would make this partition immortal",
                    -lateness
                ),
            ));
        }
        Ok(())
    }

    /// Kept for callers that want a hard error rather than a reason.
    pub fn validate_or_err(&self) -> Result<()> {
        self.validate()
            .map_err(|(r, d)| PrismError::Invalid(format!("{r}: {d}")))
    }
}

/// An event that failed admission, with the reason, for the dead-letter log.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeadLetter {
    /// The stable, greppable reason string. This is an API.
    pub reason: String,
    pub detail: String,
    pub stage: String,
    pub event: Event,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attributes::AttrValue;

    fn ev() -> Event {
        Event {
            event_id: "e1".into(),
            tenant_id: "t1".into(),
            event_time: 1_000_000,
            observed_time: 1_000_000,
            event_name: "llm.call".into(),
            cost: 0.01,
            error: false,
            body: "hello".into(),
            trace_id: String::new(),
            span_id: String::new(),
            attributes: Attributes::new(),
            idempotency_key: None,
        }
    }

    #[test]
    fn valid_event_passes() {
        ev().validate().unwrap();
    }

    #[test]
    fn empty_body_is_rejected_not_stored() {
        let mut e = ev();
        e.body = "   ".into();
        assert_eq!(e.validate().unwrap_err().0, RejectReason::EmptyBody);
    }

    #[test]
    fn oversized_body_is_rejected() {
        let mut e = ev();
        e.body = "x".repeat(MAX_BODY_BYTES + 1);
        assert_eq!(e.validate().unwrap_err().0, RejectReason::BodyTooLarge);
    }

    #[test]
    fn nonfinite_cost_is_rejected() {
        let mut e = ev();
        e.cost = f64::NAN;
        assert_eq!(e.validate().unwrap_err().0, RejectReason::InvalidCost);
    }

    #[test]
    fn the_dedup_key_defaults_to_the_event_id_and_is_tenant_scoped() {
        let e = ev();
        assert_eq!(e.dedup_key(), ("t1", "e1"));

        let mut k = ev();
        k.idempotency_key = Some("batch-7".into());
        assert_eq!(k.dedup_key(), ("t1", "batch-7"));

        // Two tenants may use the same key for different events. One tenant must
        // never be able to suppress another's event by guessing an id.
        let mut other = ev();
        other.tenant_id = "t2".into();
        assert_ne!(e.dedup_key(), other.dedup_key());
    }

    #[test]
    fn a_replay_has_the_same_content_hash_even_though_it_arrived_later() {
        // This is the whole distinction between a replay and a conflict. A retry
        // necessarily arrives at a different instant; if observed_time were hashed,
        // every retry would look like a conflict and the system would reject
        // exactly the producers who are behaving correctly.
        let a = ev();
        let mut b = ev();
        b.observed_time += 60_000;
        b.idempotency_key = Some("anything".into());
        assert_eq!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn a_changed_body_or_attribute_is_a_different_content_hash() {
        let a = ev();
        let mut b = ev();
        b.body = "goodbye".into();
        assert_ne!(a.content_hash(), b.content_hash());

        let mut c = ev();
        c.attributes.insert("k".into(), AttrValue::Int(1));
        assert_ne!(a.content_hash(), c.content_hash());

        let mut d = ev();
        d.event_time += 1;
        assert_ne!(a.content_hash(), d.content_hash());
    }

    #[test]
    fn a_late_event_within_the_window_is_admitted_and_beyond_it_is_not() {
        let e = ev();
        e.check_skew(e.event_time + MAX_LATENESS_MS).unwrap();

        let err = e
            .check_skew(e.event_time + MAX_LATENESS_MS + 1)
            .unwrap_err();
        assert_eq!(err.0, RejectReason::EventTimeTooLate);
    }

    #[test]
    fn a_clock_skewed_future_event_is_refused_not_clamped() {
        let e = ev();
        e.check_skew(e.event_time - MAX_SKEW_AHEAD_MS).unwrap();

        let err = e
            .check_skew(e.event_time - MAX_SKEW_AHEAD_MS - 1)
            .unwrap_err();
        assert_eq!(err.0, RejectReason::EventTimeInFuture);

        // And the event still says what the producer said it said.
        assert_eq!(e.event_time, ev().event_time);
    }
}
