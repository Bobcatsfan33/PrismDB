//! The OTel GenAI mapping (S2) — **pinned to a semantic-convention version**.
//!
//! > *"Pin the OTel GenAI semantic-convention version in the schema mapping and
//! > record it like a generation; the conventions are still moving."*
//!
//! And they are. `gen_ai.completion` became `gen_ai.output.messages`. The
//! `gen_ai.usage.*` shape changed. A mapping that silently follows "whatever the
//! latest convention says" would silently change what a stored column *means* —
//! which is precisely the failure that immutable generations exist to prevent, and
//! it would be no less fatal for happening in the ingest path rather than in a
//! codebook.
//!
//! So the mapping is versioned. [`SEMCONV_VERSION`] is recorded in the store and
//! travels with the data, and a payload mapped under one convention version is
//! never reinterpreted under another. Changing the mapping means a **new mapping
//! version**, and old data keeps its old meaning.
//!
//! ## What gets embedded — a product decision, made deliberately
//!
//! The contract says to decide this on purpose rather than by default, so:
//!
//! **The embedded body is the prompt and the completion content, in that order,
//! and nothing else.** Not the tool JSON. Not the request parameters. Not the token
//! counts.
//!
//! Those are scalars and attributes — things you *filter* by, not things you search
//! *by meaning*. Embedding them dilutes the vector with syntax, and makes "find
//! traces that resemble this failure" return traces that happen to share a
//! temperature setting. The whole value of a semantic index is that it indexes
//! semantics.

use prism_types::attributes::{AttrValue, Attributes};
use prism_types::error::{PrismError, Result};
use prism_types::event::Event;
use serde::{Deserialize, Serialize};

/// The OTel semantic-convention version this mapping implements.
///
/// **Recorded like a generation.** It is written into the store at init, carried in
/// the mapping's provenance, and a change to it is a change to what stored columns
/// mean — not a patch.
pub const SEMCONV_VERSION: &str = "1.27.0";

/// The version of *our* mapping of that convention. Bumped when we change how we
/// map, even if the convention itself has not moved.
pub const MAPPING_VERSION: u32 = 1;

/// Provenance for the mapping, stored alongside the data.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MappingProvenance {
    pub semconv_version: String,
    pub mapping_version: u32,
    /// Stated in the data, not just in a doc: what we chose to embed.
    pub embedded_content: String,
}

impl Default for MappingProvenance {
    fn default() -> Self {
        MappingProvenance {
            semconv_version: SEMCONV_VERSION.to_string(),
            mapping_version: MAPPING_VERSION,
            embedded_content: "gen_ai.prompt + gen_ai.completion content, in that order; \
                               tool JSON, request parameters and token counts are attributes, \
                               not embedded text"
                .to_string(),
        }
    }
}

// --- the OTLP/JSON wire shape (the subset we map) ----------------------------

#[derive(Debug, Deserialize)]
pub struct OtlpPayload {
    #[serde(default, rename = "resourceSpans")]
    pub resource_spans: Vec<ResourceSpans>,
}

#[derive(Debug, Deserialize)]
pub struct ResourceSpans {
    #[serde(default)]
    pub resource: Option<Resource>,
    #[serde(default, rename = "scopeSpans")]
    pub scope_spans: Vec<ScopeSpans>,
}

#[derive(Debug, Deserialize)]
pub struct Resource {
    #[serde(default)]
    pub attributes: Vec<KeyValue>,
}

#[derive(Debug, Deserialize)]
pub struct ScopeSpans {
    #[serde(default)]
    pub spans: Vec<Span>,
}

#[derive(Debug, Deserialize)]
pub struct Span {
    #[serde(default, rename = "traceId")]
    pub trace_id: String,
    #[serde(default, rename = "spanId")]
    pub span_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "startTimeUnixNano")]
    pub start_time_unix_nano: Option<serde_json::Value>,
    #[serde(default)]
    pub attributes: Vec<KeyValue>,
    #[serde(default)]
    pub status: Option<Status>,
}

#[derive(Debug, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub code: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct KeyValue {
    pub key: String,
    pub value: AnyValue,
}

#[derive(Debug, Deserialize)]
pub struct AnyValue {
    #[serde(default, rename = "stringValue")]
    pub string_value: Option<String>,
    #[serde(default, rename = "intValue")]
    pub int_value: Option<serde_json::Value>,
    #[serde(default, rename = "doubleValue")]
    pub double_value: Option<f64>,
    #[serde(default, rename = "boolValue")]
    pub bool_value: Option<bool>,
}

impl AnyValue {
    fn to_attr(&self) -> Option<AttrValue> {
        if let Some(s) = &self.string_value {
            return Some(AttrValue::Str(s.clone()));
        }
        if let Some(v) = &self.int_value {
            // OTLP/JSON encodes int64 as a *string*, per protobuf JSON mapping. A
            // mapper that assumed a JSON number here would silently drop every
            // token count in the payload.
            let i = match v {
                serde_json::Value::String(s) => s.parse::<i64>().ok(),
                serde_json::Value::Number(n) => n.as_i64(),
                _ => None,
            };
            return i.map(AttrValue::Int);
        }
        if let Some(d) = self.double_value {
            return Some(AttrValue::Double(d));
        }
        if let Some(b) = self.bool_value {
            return Some(AttrValue::Bool(b));
        }
        None
    }
}

fn nanos_to_millis(v: &Option<serde_json::Value>) -> Option<i64> {
    let raw = match v.as_ref()? {
        serde_json::Value::String(s) => s.parse::<i128>().ok()?,
        serde_json::Value::Number(n) => n.as_i64()? as i128,
        _ => return None,
    };
    Some((raw / 1_000_000) as i64)
}

// --- the GenAI semantic conventions we map (pinned at SEMCONV_VERSION) --------

/// The keys whose *content* is what a human would call "the conversation".
/// These become the embedded body and are **not** duplicated into attributes:
/// a 40 KB prompt in an attribute map would blow the attribute budget on every
/// event, for no benefit, since it is already the thing we indexed.
const PROMPT_KEYS: &[&str] = &["gen_ai.prompt", "gen_ai.input.messages"];
const COMPLETION_KEYS: &[&str] = &["gen_ai.completion", "gen_ai.output.messages"];

/// Keys mapped to first-class scalar columns rather than attributes.
const COST_KEY: &str = "gen_ai.usage.cost";

fn is_content_key(k: &str) -> bool {
    PROMPT_KEYS.contains(&k) || COMPLETION_KEYS.contains(&k)
}

/// Map one OTLP payload into events.
///
/// A span that carries no GenAI content produces **no event** — it is a span about
/// something else, and inventing an empty body for it would create a row whose
/// embedding is meaningless and which will never legitimately match anything.
pub fn map_payload(
    payload: &OtlpPayload,
    tenant_fallback: &str,
    now_ms: i64,
) -> Result<Vec<Event>> {
    let mut out = Vec::new();

    for rs in &payload.resource_spans {
        // Tenant comes from the resource, which is where a collector puts it.
        let mut tenant = tenant_fallback.to_string();
        if let Some(r) = &rs.resource {
            for kv in &r.attributes {
                if kv.key == "tenant.id" || kv.key == "service.namespace" {
                    if let Some(AttrValue::Str(s)) = kv.value.to_attr() {
                        tenant = s;
                    }
                }
            }
        }

        for ss in &rs.scope_spans {
            for span in &ss.spans {
                let mut prompt: Option<String> = None;
                let mut completion: Option<String> = None;
                let mut attributes = Attributes::new();
                let mut cost = 0.0f64;

                for kv in &span.attributes {
                    let Some(v) = kv.value.to_attr() else {
                        continue;
                    };

                    if PROMPT_KEYS.contains(&kv.key.as_str()) {
                        prompt = Some(v.as_display());
                        continue;
                    }
                    if COMPLETION_KEYS.contains(&kv.key.as_str()) {
                        completion = Some(v.as_display());
                        continue;
                    }
                    if kv.key == COST_KEY {
                        cost = match &v {
                            AttrValue::Double(d) => *d,
                            AttrValue::Int(i) => *i as f64,
                            _ => 0.0,
                        };
                        continue;
                    }
                    if !is_content_key(&kv.key) {
                        attributes.insert(kv.key.clone(), v);
                    }
                }

                // No conversation content: not a GenAI event. Skip it rather than
                // store a row with a meaningless vector.
                if prompt.is_none() && completion.is_none() {
                    continue;
                }

                // **The product decision, made deliberately.** Prompt then
                // completion, and nothing else.
                let body = [prompt.unwrap_or_default(), completion.unwrap_or_default()]
                    .join("\n")
                    .trim()
                    .to_string();

                let event_time = nanos_to_millis(&span.start_time_unix_nano).unwrap_or(now_ms);

                // OTel status code 2 == ERROR.
                let error = matches!(
                    span.status.as_ref().and_then(|s| s.code.as_ref()),
                    Some(serde_json::Value::Number(n)) if n.as_i64() == Some(2)
                ) || matches!(
                    span.status.as_ref().and_then(|s| s.code.as_ref()),
                    Some(serde_json::Value::String(s)) if s == "STATUS_CODE_ERROR"
                );

                let event_id = if span.span_id.is_empty() {
                    return Err(PrismError::Invalid(
                        "OTLP span has no spanId; there is nothing to identify it by".into(),
                    ));
                } else {
                    span.span_id.clone()
                };

                out.push(Event {
                    event_id,
                    tenant_id: tenant.clone(),
                    event_time,
                    observed_time: now_ms,
                    event_name: if span.name.is_empty() {
                        "gen_ai.call".to_string()
                    } else {
                        span.name.clone()
                    },
                    cost,
                    error,
                    body,
                    trace_id: span.trace_id.clone(),
                    span_id: span.span_id.clone(),
                    attributes,
                    // The span id *is* the idempotency key: OTLP collectors retry,
                    // and a retried span is the same span.
                    idempotency_key: Some(span.span_id.clone()),
                });
            }
        }
    }
    Ok(out)
}

pub fn parse(json: &str, tenant_fallback: &str, now_ms: i64) -> Result<Vec<Event>> {
    let payload: OtlpPayload = serde_json::from_str(json)
        .map_err(|e| PrismError::Invalid(format!("OTLP payload will not parse: {e}")))?;
    map_payload(&payload, tenant_fallback, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic OTLP/JSON GenAI span, in the shape a collector actually emits —
    /// int64 as a *string*, nanosecond timestamps, resource attributes.
    const PAYLOAD: &str = r#"
    {
      "resourceSpans": [{
        "resource": { "attributes": [
          {"key": "tenant.id", "value": {"stringValue": "acme"}}
        ]},
        "scopeSpans": [{
          "spans": [{
            "traceId": "5b8aa5a2d2c872e8321cf37308d69df2",
            "spanId": "051581bf3cb55c13",
            "name": "chat anthropic",
            "startTimeUnixNano": "1760000000000000000",
            "status": {"code": 2},
            "attributes": [
              {"key": "gen_ai.system", "value": {"stringValue": "anthropic"}},
              {"key": "gen_ai.prompt", "value": {"stringValue": "why did the tool call time out"}},
              {"key": "gen_ai.completion", "value": {"stringValue": "the upstream rate limited us"}},
              {"key": "gen_ai.usage.input_tokens", "value": {"intValue": "1200"}},
              {"key": "gen_ai.usage.cost", "value": {"doubleValue": 0.042}},
              {"key": "gen_ai.request.temperature", "value": {"doubleValue": 0.7}},
              {"key": "stream", "value": {"boolValue": true}}
            ]
          }]
        }]
      }]
    }"#;

    #[test]
    fn it_maps_a_real_genai_span() {
        let evs = parse(PAYLOAD, "fallback", 1_760_000_100_000).unwrap();
        assert_eq!(evs.len(), 1);
        let e = &evs[0];

        assert_eq!(e.tenant_id, "acme", "tenant comes from the resource");
        assert_eq!(e.event_id, "051581bf3cb55c13");
        assert_eq!(e.trace_id, "5b8aa5a2d2c872e8321cf37308d69df2");
        assert_eq!(e.span_id, "051581bf3cb55c13");
        assert_eq!(e.event_name, "chat anthropic");
        assert!(e.error, "OTel status code 2 is ERROR");

        // Nanoseconds -> milliseconds, and event_time is the span's start, not our
        // arrival: a span flushed a minute late still belongs to when it happened.
        assert_eq!(e.event_time, 1_760_000_000_000);
        assert_eq!(e.observed_time, 1_760_000_100_000);
        assert!(e.observed_time > e.event_time);
    }

    #[test]
    fn the_embedded_body_is_the_conversation_and_only_the_conversation() {
        // The product decision, asserted. If someone later "helpfully" folds the
        // tool JSON or the parameters into the body, this fails -- and it should,
        // because it would quietly change what every stored vector means.
        let e = &parse(PAYLOAD, "x", 1).unwrap()[0];
        assert_eq!(
            e.body,
            "why did the tool call time out\nthe upstream rate limited us"
        );
        assert!(
            !e.body.contains("0.7"),
            "a request parameter leaked into the embedded text"
        );
        assert!(
            !e.body.contains("1200"),
            "a token count leaked into the embedded text"
        );
        assert!(
            !e.body.contains("anthropic"),
            "a system name leaked into the embedded text"
        );
    }

    #[test]
    fn scalars_and_attributes_land_where_they_belong() {
        let e = &parse(PAYLOAD, "x", 1).unwrap()[0];

        // cost is a first-class column, not an attribute.
        assert!((e.cost - 0.042).abs() < 1e-9);
        assert!(!e.attributes.contains_key("gen_ai.usage.cost"));

        // Typed attributes stay typed. An int64 arriving as a JSON *string* -- which
        // is what the protobuf JSON mapping actually does -- must not be dropped or
        // stringified.
        assert_eq!(
            e.attributes.get("gen_ai.usage.input_tokens"),
            Some(&AttrValue::Int(1200))
        );
        assert_eq!(
            e.attributes.get("gen_ai.request.temperature"),
            Some(&AttrValue::Double(0.7))
        );
        assert_eq!(e.attributes.get("stream"), Some(&AttrValue::Bool(true)));
        assert_eq!(
            e.attributes.get("gen_ai.system"),
            Some(&AttrValue::Str("anthropic".into()))
        );

        // And the conversation is NOT duplicated into the attribute map: a 40 KB
        // prompt there would blow the attribute budget on every event, to store a
        // second copy of the thing we already indexed.
        assert!(!e.attributes.contains_key("gen_ai.prompt"));
        assert!(!e.attributes.contains_key("gen_ai.completion"));
    }

    #[test]
    fn the_span_id_is_the_idempotency_key() {
        // Collectors retry. A retried span is the same span, and must be recognised
        // as a replay rather than stored twice.
        let e = &parse(PAYLOAD, "x", 1).unwrap()[0];
        assert_eq!(e.idempotency_key.as_deref(), Some("051581bf3cb55c13"));
        assert_eq!(e.dedup_key(), ("acme", "051581bf3cb55c13"));
    }

    #[test]
    fn a_span_with_no_conversation_content_produces_no_event() {
        // A database span, a queue span, an HTTP span. Not GenAI. Inventing an empty
        // body for it would create a row whose vector means nothing and which can
        // never legitimately match anything.
        let payload = r#"{"resourceSpans":[{"scopeSpans":[{"spans":[{
            "spanId":"aaaa","name":"SELECT users",
            "attributes":[{"key":"db.system","value":{"stringValue":"postgres"}}]
        }]}]}]}"#;
        assert!(parse(payload, "t", 1).unwrap().is_empty());
    }

    #[test]
    fn the_newer_convention_spelling_maps_to_the_same_place() {
        // gen_ai.completion -> gen_ai.output.messages was a real rename. Both spellings
        // must land in the body, or a convention bump silently empties the index.
        let payload = r#"{"resourceSpans":[{"scopeSpans":[{"spans":[{
            "spanId":"bbbb",
            "attributes":[
              {"key":"gen_ai.input.messages","value":{"stringValue":"hello"}},
              {"key":"gen_ai.output.messages","value":{"stringValue":"world"}}
            ]
        }]}]}]}"#;
        let e = &parse(payload, "t", 1).unwrap()[0];
        assert_eq!(e.body, "hello\nworld");
    }

    #[test]
    fn the_mapping_declares_its_convention_version() {
        let p = MappingProvenance::default();
        assert_eq!(p.semconv_version, SEMCONV_VERSION);
        assert_eq!(p.mapping_version, MAPPING_VERSION);
        assert!(p.embedded_content.contains("gen_ai.prompt"));
    }

    #[test]
    fn a_span_without_a_span_id_is_refused_not_invented() {
        let payload = r#"{"resourceSpans":[{"scopeSpans":[{"spans":[{"attributes":[
            {"key":"gen_ai.prompt","value":{"stringValue":"hi"}}]}]}]}]}"#;
        assert!(parse(payload, "t", 1).is_err());
    }
}
