use crate::error::{PrismError, Result};
use serde::{Deserialize, Serialize};

/// Admission limit on the raw body. Oversized bodies are dead-lettered, never
/// silently truncated (Part III §10: an event is never stored without the
/// semantic columns it asked for, and admission failures must be *visible*).
pub const MAX_BODY_BYTES: usize = 1 << 20; // 1 MiB

/// The S0 slice of the logical event model (Part III §9).
///
/// The full model adds `observed_time`, `attributes`, `trace_id`/`span_id` and
/// promoted attribute columns; those arrive with the OTel GenAI mapping in S2.
/// `cost` and `error` are carried now because they are what semantic `GROUP BY`
/// aggregates over, and the flagship query is not demonstrable without them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub event_id: String,
    pub tenant_id: String,
    /// Epoch milliseconds.
    pub event_time: i64,
    pub event_name: String,
    pub cost: f64,
    pub error: bool,
    /// The text that gets embedded.
    pub body: String,
}

impl Event {
    /// Validate at the boundary. Never trust external data.
    pub fn validate(&self) -> Result<()> {
        if self.event_id.is_empty() {
            return Err(PrismError::Invalid("event_id is empty".into()));
        }
        if self.tenant_id.is_empty() {
            return Err(PrismError::Invalid("tenant_id is empty".into()));
        }
        if !self.cost.is_finite() {
            return Err(PrismError::Invalid(format!(
                "cost is not finite: {}",
                self.cost
            )));
        }
        if self.cost < 0.0 {
            return Err(PrismError::Invalid(format!(
                "cost is negative: {}",
                self.cost
            )));
        }
        if self.body.len() > MAX_BODY_BYTES {
            return Err(PrismError::Invalid(format!(
                "body is {} bytes, limit is {MAX_BODY_BYTES}",
                self.body.len()
            )));
        }
        if self.body.trim().is_empty() {
            return Err(PrismError::Invalid(
                "body is empty; it would produce a zero-norm embedding".into(),
            ));
        }
        Ok(())
    }
}

/// An event that failed admission, with the reason, for the dead-letter log.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeadLetter {
    pub reason: String,
    pub stage: String,
    pub event: Event,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev() -> Event {
        Event {
            event_id: "e1".into(),
            tenant_id: "t1".into(),
            event_time: 1,
            event_name: "llm.call".into(),
            cost: 0.01,
            error: false,
            body: "hello".into(),
        }
    }

    #[test]
    fn valid_event_passes() {
        assert!(ev().validate().is_ok());
    }

    #[test]
    fn empty_body_is_rejected_not_stored() {
        let mut e = ev();
        e.body = "   ".into();
        assert!(matches!(e.validate(), Err(PrismError::Invalid(_))));
    }

    #[test]
    fn oversized_body_is_rejected() {
        let mut e = ev();
        e.body = "x".repeat(MAX_BODY_BYTES + 1);
        assert!(matches!(e.validate(), Err(PrismError::Invalid(_))));
    }

    #[test]
    fn nonfinite_cost_is_rejected() {
        let mut e = ev();
        e.cost = f64::NAN;
        assert!(matches!(e.validate(), Err(PrismError::Invalid(_))));
    }
}
