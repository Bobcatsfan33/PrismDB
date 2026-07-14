//! The admission boundary (S2).
//!
//! Everything that decides whether an event is *allowed in* happens here, and it
//! happens **before a single GPU cycle is spent embedding it**. That ordering is
//! not an optimization; it is the whole point of an admission boundary. A quota
//! enforced after embedding is a quota that has already been paid for.
//!
//! Implements [`docs/INGESTION-CONTRACT.md`](../../../docs/INGESTION-CONTRACT.md)
//! §5 (attributes), §6 (quotas and starvation) and the skew rules of §4. Where the
//! contract and this file disagree, the contract is right and this file is a bug.

use prism_types::attributes::Attributes;
use prism_types::event::{DeadLetter, Event};
use prism_types::limits::{Quota, RejectReason, MAX_ATTRIBUTE_KEY_CARDINALITY};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The bounded attribute key dictionary for a partition (directive 1).
///
/// This is the object that stops a columnar format from dying. A tenant emitting
/// `user_id_<uuid>` as an attribute **key** would otherwise grow a dictionary the
/// size of their traffic, carried in every manifest, forever.
///
/// When it is full, an event introducing a *new* key is **refused** — not
/// absorbed, not spilled, not silently stripped of the offending key. The tenant
/// is told, because unbounded key cardinality is a bug in their instrumentation
/// and one they can only fix if somebody tells them about it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KeyDictionary {
    keys: BTreeSet<String>,
}

impl KeyDictionary {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn contains(&self, k: &str) -> bool {
        self.keys.contains(k)
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.keys.iter()
    }

    /// Would admitting these attributes overflow the dictionary?
    ///
    /// Checked *before* anything is inserted, so a rejected event leaves the
    /// dictionary exactly as it found it. An event that is refused must not be
    /// able to poison the partition for the next one.
    pub fn check(&self, attrs: &Attributes) -> Result<(), (RejectReason, String)> {
        let novel: Vec<&String> = attrs.keys().filter(|k| !self.keys.contains(*k)).collect();
        if novel.is_empty() {
            return Ok(());
        }
        if self.keys.len() + novel.len() > MAX_ATTRIBUTE_KEY_CARDINALITY {
            return Err((
                RejectReason::AttributeKeyCardinalityExceeded,
                format!(
                    "this event introduces {} new attribute key(s) (e.g. `{}`) to a partition that \
                     already holds {} of a maximum {MAX_ATTRIBUTE_KEY_CARDINALITY}. Unbounded \
                     attribute KEY cardinality is almost always an instrumentation bug — a value \
                     (a session id, a user id) has been used as a key. Values may be unbounded; \
                     keys may not.",
                    novel.len(),
                    novel[0],
                    self.keys.len()
                ),
            ));
        }
        Ok(())
    }

    /// Admit the keys. Only ever called after `check` passed *and* the event has
    /// survived everything else.
    pub fn insert_all(&mut self, attrs: &Attributes) {
        for k in attrs.keys() {
            self.keys.insert(k.clone());
        }
    }
}

/// A token-bucket rate limiter over a sliding window, per tenant.
///
/// Rejects, never queues. An unbounded queue is not a quota — it is a slower way
/// to run out of memory, and it converts a rate problem into an availability
/// problem.
#[derive(Clone, Debug, Default)]
struct TenantUsage {
    window_start_ms: i64,
    events: u64,
    bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct QuotaEnforcer {
    per_tenant: BTreeMap<String, TenantUsage>,
    /// Overrides. A tenant with no entry gets `Quota::default()`.
    limits: BTreeMap<String, Quota>,
}

const WINDOW_MS: i64 = 1_000;

impl QuotaEnforcer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_limit(&mut self, tenant: &str, q: Quota) {
        self.limits.insert(tenant.to_string(), q);
    }

    pub fn limit_for(&self, tenant: &str) -> Quota {
        self.limits.get(tenant).copied().unwrap_or_default()
    }

    /// Charge one event against its tenant's quota.
    pub fn charge(&mut self, e: &Event, now_ms: i64) -> Result<(), (RejectReason, String)> {
        let q = self.limit_for(&e.tenant_id);
        let size = e.byte_size() as u64;

        let u = self
            .per_tenant
            .entry(e.tenant_id.clone())
            .or_insert(TenantUsage {
                window_start_ms: now_ms,
                events: 0,
                bytes: 0,
            });

        if now_ms - u.window_start_ms >= WINDOW_MS {
            u.window_start_ms = now_ms;
            u.events = 0;
            u.bytes = 0;
        }

        if u.events + 1 > q.events_per_sec {
            return Err((
                RejectReason::QuotaExceeded,
                format!(
                    "tenant `{}` is over its {} events/sec quota ({} already this second)",
                    e.tenant_id, q.events_per_sec, u.events
                ),
            ));
        }
        if u.bytes + size > q.bytes_per_sec {
            return Err((
                RejectReason::QuotaExceeded,
                format!(
                    "tenant `{}` is over its {} bytes/sec quota ({} already this second, this \
                     event is {size})",
                    e.tenant_id, q.bytes_per_sec, u.bytes
                ),
            ));
        }

        u.events += 1;
        u.bytes += size;
        Ok(())
    }
}

/// Interleave a batch round-robin across tenants.
///
/// **Starvation is the separate failure, and the more insidious one.** A tenant
/// that is comfortably *within* quota can still monopolise a batch simply by being
/// loud: ten thousand of their events arrive, and the one event from the quiet
/// tenant sits behind all of them.
///
/// So a batch is not processed in arrival order. It is processed by taking one
/// event from each active tenant in turn. A tenant with one event per second is
/// admitted at the same position whether or not another tenant is pushing ten
/// thousand.
///
/// The test that matters is not "the big tenant was throttled". It is **"the small
/// tenant's latency did not change when the big tenant arrived"** — which is the
/// thing the small tenant actually notices.
pub fn interleave_by_tenant(events: Vec<Event>) -> Vec<Event> {
    let mut by_tenant: BTreeMap<String, Vec<Event>> = BTreeMap::new();
    for e in events {
        by_tenant.entry(e.tenant_id.clone()).or_default().push(e);
    }

    let mut queues: Vec<std::vec::IntoIter<Event>> =
        by_tenant.into_values().map(|v| v.into_iter()).collect();

    let mut out = Vec::new();
    let mut active = true;
    while active {
        active = false;
        for q in queues.iter_mut() {
            if let Some(e) = q.next() {
                out.push(e);
                active = true;
            }
        }
    }
    out
}

/// What admission decided about a batch.
pub struct Admitted {
    pub accepted: Vec<Event>,
    pub rejected: Vec<DeadLetter>,
    /// Replays: the same key, the same content. Acknowledged, not stored again.
    pub duplicates_suppressed: usize,
}

/// The admission gauntlet, in the order the contract specifies.
///
/// Order matters and is not arbitrary:
///
/// 1. **Fairness first** — interleave, so the checks below are applied to a batch
///    that is already fair. Enforcing quotas on an unfair batch just rejects the
///    quiet tenant's event *later*.
/// 2. **Schema and caps** — cheap, local, and they reject the events most likely
///    to be malformed.
/// 3. **Skew** — an event outside the accepted window is refused before it can
///    resurrect a closed partition or make one immortal.
/// 4. **Cardinality** — the dictionary check, which is the one that protects the
///    *shape* of the data rather than the size of one event.
/// 5. **Quota** — last of the rejections, because it is the only one that depends
///    on how much we have already accepted.
///
/// Idempotency is *not* here: it needs durable state, so it lives in
/// `Ingestor` where the index is. Everything here is a pure function of the batch
/// plus the partition's dictionary and the tenant's rate.
pub fn admit(
    events: Vec<Event>,
    dict: &mut KeyDictionary,
    quotas: &mut QuotaEnforcer,
    now_ms: i64,
) -> Admitted {
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();

    for mut e in interleave_by_tenant(events) {
        // The admission boundary is where observed_time is *set*. A producer does
        // not get to tell us when we received something.
        e.observed_time = now_ms;

        let reject = |e: Event, r: RejectReason, d: String| DeadLetter {
            reason: r.to_string(),
            detail: d,
            stage: "admission".to_string(),
            event: e,
        };

        if let Err((r, d)) = e.validate() {
            rejected.push(reject(e, r, d));
            continue;
        }
        if let Err((r, d)) = e.check_skew(now_ms) {
            rejected.push(reject(e, r, d));
            continue;
        }
        if let Err((r, d)) = dict.check(&e.attributes) {
            rejected.push(reject(e, r, d));
            continue;
        }
        if let Err((r, d)) = quotas.charge(&e, now_ms) {
            rejected.push(reject(e, r, d));
            continue;
        }

        // Only now, having survived everything, may the event widen the partition's
        // key dictionary. A rejected event must never leave a trace in it.
        dict.insert_all(&e.attributes);
        accepted.push(e);
    }

    Admitted {
        accepted,
        rejected,
        duplicates_suppressed: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_types::attributes::AttrValue;

    fn ev(id: &str, tenant: &str) -> Event {
        Event {
            event_id: id.into(),
            tenant_id: tenant.into(),
            event_time: 1_000_000,
            observed_time: 0,
            event_name: "llm.call".into(),
            cost: 0.01,
            error: false,
            body: "the tool call timed out".into(),
            trace_id: String::new(),
            span_id: String::new(),
            attributes: Attributes::new(),
            idempotency_key: None,
        }
    }

    #[test]
    fn a_full_key_dictionary_refuses_a_new_key_and_keeps_the_old_ones() {
        let mut d = KeyDictionary::new();
        for i in 0..MAX_ATTRIBUTE_KEY_CARDINALITY {
            let mut a = Attributes::new();
            a.insert(format!("k{i}"), AttrValue::Int(1));
            d.check(&a).unwrap();
            d.insert_all(&a);
        }
        assert_eq!(d.len(), MAX_ATTRIBUTE_KEY_CARDINALITY);

        // One more novel key: refused, by name.
        let mut novel = Attributes::new();
        novel.insert("user_id_9f3c".into(), AttrValue::Int(1));
        let (r, detail) = d.check(&novel).unwrap_err();
        assert_eq!(r, RejectReason::AttributeKeyCardinalityExceeded);
        assert!(detail.contains("instrumentation bug"), "{detail}");

        // A key already in the dictionary is still fine — the cap bounds novelty,
        // not usage.
        let mut known = Attributes::new();
        known.insert("k7".into(), AttrValue::Int(2));
        d.check(&known).unwrap();
    }

    #[test]
    fn a_rejected_event_does_not_widen_the_dictionary() {
        // The subtle one. If a refused event's keys leaked into the dictionary, a
        // producer could exhaust a partition's cardinality budget using events that
        // were never even stored.
        let mut dict = KeyDictionary::new();
        let mut quotas = QuotaEnforcer::new();

        let mut e = ev("e1", "t1");
        e.body = String::new(); // will be rejected: empty body
        e.attributes.insert("novel_key".into(), AttrValue::Int(1));

        let r = admit(vec![e], &mut dict, &mut quotas, 1_000_000);
        assert_eq!(r.accepted.len(), 0);
        assert_eq!(r.rejected.len(), 1);
        assert!(
            !dict.contains("novel_key"),
            "a refused event widened the key dictionary"
        );
        assert!(dict.is_empty());
    }

    #[test]
    fn an_over_quota_tenant_is_rejected_not_queued() {
        let mut dict = KeyDictionary::new();
        let mut quotas = QuotaEnforcer::new();
        quotas.set_limit(
            "loud",
            Quota {
                events_per_sec: 3,
                ..Default::default()
            },
        );

        let events: Vec<Event> = (0..10).map(|i| ev(&format!("e{i}"), "loud")).collect();
        let r = admit(events, &mut dict, &mut quotas, 1_000_000);

        assert_eq!(r.accepted.len(), 3);
        assert_eq!(r.rejected.len(), 7);
        assert!(r
            .rejected
            .iter()
            .all(|d| d.reason == RejectReason::QuotaExceeded.to_string()));
    }

    #[test]
    fn a_loud_tenant_cannot_starve_a_quiet_one() {
        // The gate: "one tenant cannot exceed quota OR STARVE OTHERS."
        //
        // The quiet tenant sends one event, arriving *last*. Without fairness it
        // would be admitted 10,000th. With it, it is admitted within the first
        // handful — and, crucially, at the same position whether the loud tenant
        // sent 10 events or 10,000.
        let mut events: Vec<Event> = (0..10_000)
            .map(|i| ev(&format!("loud{i}"), "loud"))
            .collect();
        events.push(ev("quiet1", "quiet"));

        let mut dict = KeyDictionary::new();
        let mut quotas = QuotaEnforcer::new();
        let r = admit(events, &mut dict, &mut quotas, 1_000_000);

        let pos = r
            .accepted
            .iter()
            .position(|e| e.tenant_id == "quiet")
            .expect("the quiet tenant's event was not admitted at all");
        assert!(
            pos < 4,
            "the quiet tenant's only event was admitted at position {pos}; a loud tenant starved it"
        );
    }

    #[test]
    fn the_quiet_tenants_position_does_not_depend_on_how_loud_the_loud_one_is() {
        // The real test of fairness. Not "the big tenant was throttled" — that is
        // the quota's job — but "the small tenant did not notice the big one".
        let position_with = |loud_events: usize| -> usize {
            let mut events: Vec<Event> = (0..loud_events)
                .map(|i| ev(&format!("loud{i}"), "loud"))
                .collect();
            events.push(ev("quiet1", "quiet"));
            let mut dict = KeyDictionary::new();
            let mut quotas = QuotaEnforcer::new();
            let r = admit(events, &mut dict, &mut quotas, 1_000_000);
            r.accepted
                .iter()
                .position(|e| e.tenant_id == "quiet")
                .unwrap()
        };

        assert_eq!(
            position_with(10),
            position_with(10_000),
            "the quiet tenant's latency changed when the loud tenant got louder"
        );
    }

    #[test]
    fn observed_time_is_set_by_us_not_by_the_producer() {
        // A producer does not get to tell us when we received something. If they
        // could, they could hide their own lateness, and the skew check would be
        // theirs to defeat rather than ours to enforce.
        let mut e = ev("e1", "t1");
        e.observed_time = 42; // a lie

        let mut dict = KeyDictionary::new();
        let mut quotas = QuotaEnforcer::new();
        let r = admit(vec![e], &mut dict, &mut quotas, 1_000_000);
        assert_eq!(r.accepted[0].observed_time, 1_000_000);
    }

    #[test]
    fn a_clock_skewed_event_is_refused_at_the_boundary() {
        let mut e = ev("e1", "t1");
        e.event_time = 1_000_000 + 10 * 60 * 60 * 1000; // ten hours ahead

        let mut dict = KeyDictionary::new();
        let mut quotas = QuotaEnforcer::new();
        let r = admit(vec![e], &mut dict, &mut quotas, 1_000_000);
        assert_eq!(r.accepted.len(), 0);
        assert_eq!(
            r.rejected[0].reason,
            RejectReason::EventTimeInFuture.to_string()
        );
    }
}
