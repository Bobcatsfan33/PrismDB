//! Typed, bounded attributes (S2).
//!
//! OTel attribute values are typed, and flattening them all to strings would
//! throw away the thing that makes half of them useful: `gen_ai.usage.input_tokens`
//! is a number you want to `SUM`, not a string you want to `LIKE`.
//!
//! The type set is deliberately small and closed. Arrays and nested maps are
//! **flattened at the OTLP boundary** (`a.b[0]` becomes a key), because a nested
//! value in a columnar store is a value you cannot build a zone map over, and a
//! value you cannot skip is a value you always scan.

use crate::limits::{
    RejectReason, MAX_ATTRIBUTES_BYTES, MAX_ATTRIBUTE_KEYS, MAX_ATTRIBUTE_KEY_BYTES,
    MAX_ATTRIBUTE_VALUE_BYTES,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Double(f64),
    Bool(bool),
}

/// Wire type tags. Stored in the part, so they are part of the format: an id, not
/// a `#[repr]` accident.
pub const ATTR_TYPE_STR: u8 = 1;
pub const ATTR_TYPE_INT: u8 = 2;
pub const ATTR_TYPE_DOUBLE: u8 = 3;
pub const ATTR_TYPE_BOOL: u8 = 4;

impl AttrValue {
    pub fn type_tag(&self) -> u8 {
        match self {
            AttrValue::Str(_) => ATTR_TYPE_STR,
            AttrValue::Int(_) => ATTR_TYPE_INT,
            AttrValue::Double(_) => ATTR_TYPE_DOUBLE,
            AttrValue::Bool(_) => ATTR_TYPE_BOOL,
        }
    }

    /// Bytes this value contributes to the per-event budget.
    pub fn byte_size(&self) -> usize {
        match self {
            AttrValue::Str(s) => s.len(),
            AttrValue::Int(_) => 8,
            AttrValue::Double(_) => 8,
            AttrValue::Bool(_) => 1,
        }
    }

    pub fn as_display(&self) -> String {
        match self {
            AttrValue::Str(s) => s.clone(),
            AttrValue::Int(i) => i.to_string(),
            AttrValue::Double(d) => d.to_string(),
            AttrValue::Bool(b) => b.to_string(),
        }
    }
}

/// An event's attributes. Ordered, so encoding is deterministic and a part is
/// content-addressable.
pub type Attributes = BTreeMap<String, AttrValue>;

/// Check one event's attributes against the per-event caps.
///
/// The partition-wide key-cardinality cap is **not** checked here — it is not a
/// property of one event, it is a property of the dataset, and it lives in the
/// admission path where the dictionary is (see `prism_engine::admission`).
pub fn validate(attrs: &Attributes) -> Result<(), (RejectReason, String)> {
    if attrs.len() > MAX_ATTRIBUTE_KEYS {
        return Err((
            RejectReason::TooManyAttributeKeys,
            format!(
                "{} attribute keys, limit is {MAX_ATTRIBUTE_KEYS}",
                attrs.len()
            ),
        ));
    }

    let mut total = 0usize;
    for (k, v) in attrs {
        if k.is_empty() {
            return Err((
                RejectReason::AttributeKeyTooLong,
                "attribute key is empty".to_string(),
            ));
        }
        if k.len() > MAX_ATTRIBUTE_KEY_BYTES {
            return Err((
                RejectReason::AttributeKeyTooLong,
                format!(
                    "attribute key `{}…` is {} bytes, limit is {MAX_ATTRIBUTE_KEY_BYTES}",
                    &k[..k.len().min(32)],
                    k.len()
                ),
            ));
        }
        let vsz = v.byte_size();
        if vsz > MAX_ATTRIBUTE_VALUE_BYTES {
            return Err((
                RejectReason::AttributeValueTooLong,
                format!(
                    "attribute `{k}` has a {vsz}-byte value, limit is {MAX_ATTRIBUTE_VALUE_BYTES}"
                ),
            ));
        }
        if let AttrValue::Double(d) = v {
            if !d.is_finite() {
                return Err((
                    RejectReason::AttributeValueTooLong,
                    format!("attribute `{k}` is not a finite number: {d}"),
                ));
            }
        }
        total += k.len() + vsz;
    }

    if total > MAX_ATTRIBUTES_BYTES {
        return Err((
            RejectReason::AttributesTooLarge,
            format!("attributes total {total} bytes, limit is {MAX_ATTRIBUTES_BYTES}"),
        ));
    }
    Ok(())
}

/// Total attribute bytes, for quota accounting.
pub fn byte_size(attrs: &Attributes) -> usize {
    attrs.iter().map(|(k, v)| k.len() + v.byte_size()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(pairs: &[(&str, AttrValue)]) -> Attributes {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn a_normal_attribute_map_passes() {
        let a = attrs(&[
            ("gen_ai.system", AttrValue::Str("anthropic".into())),
            ("gen_ai.usage.input_tokens", AttrValue::Int(1200)),
            ("gen_ai.request.temperature", AttrValue::Double(0.7)),
            ("stream", AttrValue::Bool(true)),
        ]);
        validate(&a).unwrap();
    }

    #[test]
    fn too_many_keys_is_refused_by_name() {
        let mut a = Attributes::new();
        for i in 0..(MAX_ATTRIBUTE_KEYS + 1) {
            a.insert(format!("k{i}"), AttrValue::Int(i as i64));
        }
        let (r, _) = validate(&a).unwrap_err();
        assert_eq!(r, RejectReason::TooManyAttributeKeys);
    }

    #[test]
    fn an_oversized_key_or_value_is_refused_by_name() {
        let long_key = "k".repeat(MAX_ATTRIBUTE_KEY_BYTES + 1);
        let (r, _) = validate(&attrs(&[(&long_key, AttrValue::Int(1))])).unwrap_err();
        assert_eq!(r, RejectReason::AttributeKeyTooLong);

        let long_val = "v".repeat(MAX_ATTRIBUTE_VALUE_BYTES + 1);
        let (r, _) = validate(&attrs(&[("k", AttrValue::Str(long_val))])).unwrap_err();
        assert_eq!(r, RejectReason::AttributeValueTooLong);
    }

    #[test]
    fn the_total_budget_binds_even_when_every_single_value_is_legal() {
        // The point of a total cap: 32 keys, each with a 4 KiB value, is 32 legal
        // values adding up to an illegal event. Without the total, the per-value
        // cap would be trivially defeatable.
        let mut a = Attributes::new();
        for i in 0..32 {
            a.insert(
                format!("k{i}"),
                AttrValue::Str("v".repeat(MAX_ATTRIBUTE_VALUE_BYTES)),
            );
        }
        assert!(a.len() <= MAX_ATTRIBUTE_KEYS);
        let (r, _) = validate(&a).unwrap_err();
        assert_eq!(r, RejectReason::AttributesTooLarge);
    }

    #[test]
    fn a_nonfinite_double_never_reaches_a_zone_map() {
        let a = attrs(&[("x", AttrValue::Double(f64::NAN))]);
        assert!(validate(&a).is_err());
    }

    #[test]
    fn an_empty_key_is_refused() {
        let a = attrs(&[("", AttrValue::Int(1))]);
        assert!(validate(&a).is_err());
    }

    #[test]
    fn type_tags_are_stable_and_distinct() {
        let tags = [
            AttrValue::Str(String::new()).type_tag(),
            AttrValue::Int(0).type_tag(),
            AttrValue::Double(0.0).type_tag(),
            AttrValue::Bool(false).type_tag(),
        ];
        assert_eq!(tags, [1, 2, 3, 4]);
    }
}
