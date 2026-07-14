//! Evaluating a row predicate against a part, without materializing the part.
//!
//! The predicate runs once per *scanned row*, so it must not allocate per row and it must
//! not decode a column nobody asked about. Columns are loaded lazily, once per part, and
//! only if the predicate actually names them.

use prism_part::io;
use prism_part::part::PartReader;
use prism_types::attributes::Attributes;
use prism_types::error::{PrismError, Result};
use prism_types::predicate::{Predicate, RowSource, Value};
use std::cell::RefCell;
use std::collections::BTreeSet;

/// Which columns a predicate actually touches. A predicate that never mentions `body` must
/// not cost a `body` decode.
pub fn columns_used(p: &Predicate, out: &mut BTreeSet<String>, attrs: &mut bool) {
    match p {
        Predicate::Column(c) => {
            out.insert(c.clone());
        }
        Predicate::Attribute(_) => *attrs = true,
        Predicate::Literal(_) => {}
        Predicate::Not(a) => columns_used(a, out, attrs),
        Predicate::And(a, b) | Predicate::Or(a, b) => {
            columns_used(a, out, attrs);
            columns_used(b, out, attrs);
        }
        Predicate::Cmp(a, _, b) => {
            columns_used(a, out, attrs);
            columns_used(b, out, attrs);
        }
        Predicate::In(a, _) => columns_used(a, out, attrs),
    }
}

/// A string column's blob and its offset array.
type StringCol = (Vec<u8>, Vec<i64>);

/// A lazily-loaded view of one part's columns, for predicate evaluation.
pub struct PartRows<'a> {
    reader: &'a PartReader,
    n: usize,
    times: Option<Vec<i64>>,
    observed: Option<Vec<i64>>,
    costs: Option<Vec<f64>>,
    errors: Option<Vec<u8>>,
    strings: RefCell<std::collections::BTreeMap<String, StringCol>>,
    attrs: Option<StringCol>,
    /// Filled in per query, since `score` is not a stored column.
    pub score: RefCell<f32>,
}

impl<'a> PartRows<'a> {
    pub fn new(reader: &'a PartReader, pred: Option<&Predicate>) -> Result<Self> {
        let mut cols = BTreeSet::new();
        let mut wants_attrs = false;
        if let Some(p) = pred {
            columns_used(p, &mut cols, &mut wants_attrs);
        }
        let n = reader.manifest.row_count;

        let load_i64 = |name: &str| -> Result<Option<Vec<i64>>> {
            if !cols.contains(name) || !reader.manifest.has_column(name) {
                return Ok(None);
            }
            Ok(Some(io::decode_i64(&reader.read_column_checked(name)?)))
        };

        Ok(PartRows {
            n,
            times: load_i64("event_time")?,
            observed: load_i64("observed_time")?,
            costs: if cols.contains("cost") {
                Some(io::decode_f64(&reader.read_column_checked("cost")?))
            } else {
                None
            },
            errors: if cols.contains("error") {
                Some(reader.read_column_checked("error")?)
            } else {
                None
            },
            attrs: if wants_attrs && reader.manifest.has_attributes() {
                let d = reader.read_column_checked("attributes.data")?;
                let o = io::string_offsets(&reader.read_column_checked("attributes.offsets")?, n)?;
                Some((d, o))
            } else {
                None
            },
            strings: RefCell::new(std::collections::BTreeMap::new()),
            score: RefCell::new(0.0),
            reader,
        })
    }

    fn string_col(&self, base: &str, row: usize) -> Result<Value> {
        {
            let cache = self.strings.borrow();
            if let Some((d, o)) = cache.get(base) {
                return Ok(Value::Str(io::string_at(d, o, row, self.n)?.to_string()));
            }
        }
        let d = self.reader.read_column_checked(&format!("{base}.data"))?;
        let o = io::string_offsets(
            &self
                .reader
                .read_column_checked(&format!("{base}.offsets"))?,
            self.n,
        )?;
        let v = Value::Str(io::string_at(&d, &o, row, self.n)?.to_string());
        self.strings.borrow_mut().insert(base.to_string(), (d, o));
        Ok(v)
    }
}

impl RowSource for PartRows<'_> {
    fn column(&self, name: &str, row: usize) -> Result<Value> {
        match name {
            "event_time" => Ok(self
                .times
                .as_ref()
                .map(|v| Value::Int(v[row]))
                .unwrap_or(Value::Null)),
            "observed_time" => Ok(self
                .observed
                .as_ref()
                .map(|v| Value::Int(v[row]))
                // A v1/v2 part has no observed_time. It is absent, not zero -- and Null is
                // the only honest answer to "when did you receive this?" for data written
                // before we recorded it.
                .unwrap_or(Value::Null)),
            "cost" => Ok(self
                .costs
                .as_ref()
                .map(|v| Value::Float(v[row]))
                .unwrap_or(Value::Null)),
            "error" => Ok(self
                .errors
                .as_ref()
                .map(|v| Value::Bool(v[row] == 1))
                .unwrap_or(Value::Null)),
            "score" => Ok(Value::Float(*self.score.borrow() as f64)),
            "event_id" | "tenant_id" | "event_name" | "body" | "trace_id" | "span_id" => {
                self.string_col(name, row)
            }
            other => Err(PrismError::Invalid(format!("unknown column `{other}`"))),
        }
    }

    fn attribute(&self, key: &str, row: usize) -> Result<Value> {
        let Some((d, o)) = &self.attrs else {
            return Ok(Value::Null);
        };
        let a: Attributes =
            io::decode_attributes_at(d, o, row, self.n, &self.reader.manifest.attribute_keys)?;
        Ok(a.get(key).map(Value::from_attr).unwrap_or(Value::Null))
    }
}

/// A `RowSource` over a fully materialized `Event`. Used by the reference evaluator that
/// the parity tests treat as an oracle: the same predicate, evaluated a completely
/// different way, must agree.
pub struct EventRow<'a> {
    pub event: &'a prism_types::Event,
    pub score: f32,
}

impl RowSource for EventRow<'_> {
    fn column(&self, name: &str, _row: usize) -> Result<Value> {
        let e = self.event;
        Ok(match name {
            "event_id" => Value::Str(e.event_id.clone()),
            "tenant_id" => Value::Str(e.tenant_id.clone()),
            "event_time" => Value::Int(e.event_time),
            "observed_time" => Value::Int(e.observed_time),
            "event_name" => Value::Str(e.event_name.clone()),
            "cost" => Value::Float(e.cost),
            "error" => Value::Bool(e.error),
            "body" => Value::Str(e.body.clone()),
            "trace_id" => Value::Str(e.trace_id.clone()),
            "span_id" => Value::Str(e.span_id.clone()),
            "score" => Value::Float(self.score as f64),
            other => return Err(PrismError::Invalid(format!("unknown column `{other}`"))),
        })
    }

    fn attribute(&self, key: &str, _row: usize) -> Result<Value> {
        Ok(self
            .event
            .attributes
            .get(key)
            .map(Value::from_attr)
            .unwrap_or(Value::Null))
    }
}
