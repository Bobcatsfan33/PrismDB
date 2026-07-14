//! Row predicates.
//!
//! These live in `prism-types`, not in `prism-sql`, and that placement is the whole
//! "same door" argument made structural: **the direct API can build exactly the predicate
//! SQL compiles to.** If the filter language lived inside the SQL crate, the direct path
//! would need its own, and two filter languages that are supposed to agree is precisely
//! the class of bug we are trying not to have.
//!
//! Evaluation is defined once, here, and both doors call it.

use crate::attributes::AttrValue;
use crate::error::{PrismError, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Literal {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmpOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Predicate {
    Column(String),
    Attribute(String),
    Literal(Literal),
    Cmp(Box<Predicate>, CmpOp, Box<Predicate>),
    And(Box<Predicate>, Box<Predicate>),
    Or(Box<Predicate>, Box<Predicate>),
    Not(Box<Predicate>),
    In(Box<Predicate>, Vec<Literal>),
}

/// One cell.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// An attribute the row does not have. **Absent, not zero.**
    Null,
}

impl Value {
    pub fn from_attr(a: &AttrValue) -> Value {
        match a {
            AttrValue::Str(s) => Value::Str(s.clone()),
            AttrValue::Int(i) => Value::Int(*i),
            AttrValue::Double(d) => Value::Float(*d),
            AttrValue::Bool(b) => Value::Bool(*b),
        }
    }

    pub fn from_literal(l: &Literal) -> Value {
        match l {
            Literal::Str(s) => Value::Str(s.clone()),
            Literal::Int(i) => Value::Int(*i),
            Literal::Float(f) => Value::Float(*f),
            Literal::Bool(b) => Value::Bool(*b),
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Total order for comparison. `None` when the two values are not comparable —
    /// comparing a string to a number is a question with no answer, and inventing one is
    /// how `WHERE cost > 'abc'` silently returns rows.
    fn cmp_to(&self, other: &Value) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Value::Null, _) | (_, Value::Null) => None,
            (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            _ => {
                let (a, b) = (self.as_f64()?, other.as_f64()?);
                a.partial_cmp(&b)
            }
        }
    }
}

/// Whatever can hand a predicate the value of a column for a row.
///
/// Implemented over a part by the engine, and over a plain `Event` by the reference
/// evaluator the tests use as an oracle — so the same predicate, evaluated two entirely
/// different ways, must agree.
pub trait RowSource {
    fn column(&self, name: &str, row: usize) -> Result<Value>;
    fn attribute(&self, key: &str, row: usize) -> Result<Value>;
}

/// Evaluate. Three-valued at heart, collapsed to a bool at the top: a comparison against a
/// `Null` (an absent attribute) is **not true**, and `NOT (absent = 'x')` is **also not
/// true** — because the row simply has nothing to say about it.
pub fn eval(p: &Predicate, src: &dyn RowSource, row: usize) -> Result<bool> {
    Ok(matches!(eval3(p, src, row)?, Some(true)))
}

fn eval3(p: &Predicate, src: &dyn RowSource, row: usize) -> Result<Option<bool>> {
    match p {
        Predicate::And(a, b) => {
            // false AND unknown = false. Short-circuit honestly.
            let (x, y) = (eval3(a, src, row)?, eval3(b, src, row)?);
            Ok(match (x, y) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            })
        }
        Predicate::Or(a, b) => {
            let (x, y) = (eval3(a, src, row)?, eval3(b, src, row)?);
            Ok(match (x, y) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            })
        }
        Predicate::Not(a) => Ok(eval3(a, src, row)?.map(|v| !v)),
        Predicate::Cmp(l, op, r) => {
            let (a, b) = (value(l, src, row)?, value(r, src, row)?);
            let Some(ord) = a.cmp_to(&b) else {
                return Ok(None);
            };
            use std::cmp::Ordering::*;
            Ok(Some(match op {
                CmpOp::Eq => ord == Equal,
                CmpOp::NotEq => ord != Equal,
                CmpOp::Lt => ord == Less,
                CmpOp::LtEq => ord != Greater,
                CmpOp::Gt => ord == Greater,
                CmpOp::GtEq => ord != Less,
            }))
        }
        Predicate::In(l, list) => {
            let a = value(l, src, row)?;
            if a == Value::Null {
                return Ok(None);
            }
            Ok(Some(list.iter().any(|x| {
                a.cmp_to(&Value::from_literal(x)) == Some(std::cmp::Ordering::Equal)
            })))
        }
        // A bare column or literal used as a boolean.
        other => {
            let v = value(other, src, row)?;
            Ok(match v {
                Value::Bool(b) => Some(b),
                Value::Null => None,
                _ => {
                    return Err(PrismError::Invalid(
                        "expression is not a boolean".to_string(),
                    ))
                }
            })
        }
    }
}

fn value(p: &Predicate, src: &dyn RowSource, row: usize) -> Result<Value> {
    match p {
        Predicate::Column(c) => src.column(c, row),
        Predicate::Attribute(k) => src.attribute(k, row),
        Predicate::Literal(l) => Ok(Value::from_literal(l)),
        _ => Err(PrismError::Invalid(
            "expected a column or a literal, found a boolean expression".into(),
        )),
    }
}

/// Pull the `event_time` bounds out of a predicate, for zone-map pruning.
///
/// **Conservative by construction.** It only looks at the top-level conjunction, and it
/// returns a range that is guaranteed to *contain* every matching row. Being too wide costs
/// a scan. Being too narrow loses a row — and pruning that can lose a row is not pruning,
/// it is sampling.
pub fn time_bounds(p: &Predicate) -> (Option<i64>, Option<i64>) {
    let (mut lo, mut hi) = (None, None);
    conjuncts(p, &mut |c| {
        if let Predicate::Cmp(l, op, r) = c {
            if let (Predicate::Column(name), Predicate::Literal(Literal::Int(v))) = (&**l, &**r) {
                if name == "event_time" {
                    match op {
                        CmpOp::Eq => {
                            lo = Some(lo.map_or(*v, |x: i64| x.max(*v)));
                            hi = Some(hi.map_or(*v, |x: i64| x.min(*v)));
                        }
                        CmpOp::Gt => lo = Some(lo.map_or(v + 1, |x: i64| x.max(v + 1))),
                        CmpOp::GtEq => lo = Some(lo.map_or(*v, |x: i64| x.max(*v))),
                        CmpOp::Lt => hi = Some(hi.map_or(v - 1, |x: i64| x.min(v - 1))),
                        CmpOp::LtEq => hi = Some(hi.map_or(*v, |x: i64| x.min(*v))),
                        CmpOp::NotEq => {}
                    }
                }
            }
        }
    });
    (lo, hi)
}

/// Walk only the top-level AND chain. An `OR` anywhere means we cannot narrow the range,
/// so we simply do not descend into one.
fn conjuncts(p: &Predicate, f: &mut impl FnMut(&Predicate)) {
    match p {
        Predicate::And(a, b) => {
            conjuncts(a, f);
            conjuncts(b, f);
        }
        other => f(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Row(BTreeMap<String, Value>, BTreeMap<String, Value>);

    impl RowSource for Row {
        fn column(&self, name: &str, _row: usize) -> Result<Value> {
            self.0
                .get(name)
                .cloned()
                .ok_or_else(|| PrismError::Invalid(format!("no column {name}")))
        }
        fn attribute(&self, key: &str, _row: usize) -> Result<Value> {
            Ok(self.1.get(key).cloned().unwrap_or(Value::Null))
        }
    }

    fn row() -> Row {
        let mut c = BTreeMap::new();
        c.insert("cost".into(), Value::Float(0.5));
        c.insert("event_name".into(), Value::Str("tool.retry".into()));
        c.insert("error".into(), Value::Bool(true));
        c.insert("event_time".into(), Value::Int(1000));
        let mut a = BTreeMap::new();
        a.insert("gen_ai.system".into(), Value::Str("anthropic".into()));
        Row(c, a)
    }

    fn col(c: &str) -> Box<Predicate> {
        Box::new(Predicate::Column(c.into()))
    }
    fn lit(l: Literal) -> Box<Predicate> {
        Box::new(Predicate::Literal(l))
    }

    #[test]
    fn comparisons_work_across_int_and_float() {
        let p = Predicate::Cmp(col("cost"), CmpOp::Gt, lit(Literal::Int(0)));
        assert!(eval(&p, &row(), 0).unwrap());
        let p = Predicate::Cmp(col("event_time"), CmpOp::GtEq, lit(Literal::Float(1000.0)));
        assert!(eval(&p, &row(), 0).unwrap());
    }

    #[test]
    fn an_absent_attribute_is_null_and_null_is_not_true() {
        // Neither `absent = 'x'` nor `NOT (absent = 'x')` is true. The row has nothing to say.
        let p = Predicate::Cmp(
            Box::new(Predicate::Attribute("nope".into())),
            CmpOp::Eq,
            lit(Literal::Str("x".into())),
        );
        assert!(!eval(&p, &row(), 0).unwrap());
        assert!(!eval(&Predicate::Not(Box::new(p)), &row(), 0).unwrap());
    }

    #[test]
    fn comparing_a_string_to_a_number_is_not_true_rather_than_arbitrary() {
        // Inventing an ordering here is how `WHERE cost > 'abc'` silently returns rows.
        let p = Predicate::Cmp(col("cost"), CmpOp::Gt, lit(Literal::Str("abc".into())));
        assert!(!eval(&p, &row(), 0).unwrap());
    }

    #[test]
    fn attributes_are_addressable_and_typed() {
        let p = Predicate::Cmp(
            Box::new(Predicate::Attribute("gen_ai.system".into())),
            CmpOp::Eq,
            lit(Literal::Str("anthropic".into())),
        );
        assert!(eval(&p, &row(), 0).unwrap());
    }

    #[test]
    fn in_lists_work() {
        let p = Predicate::In(
            col("event_name"),
            vec![
                Literal::Str("db.error".into()),
                Literal::Str("tool.retry".into()),
            ],
        );
        assert!(eval(&p, &row(), 0).unwrap());
    }

    #[test]
    fn time_bounds_are_extracted_conservatively() {
        let p = Predicate::And(
            Box::new(Predicate::Cmp(
                col("event_time"),
                CmpOp::GtEq,
                lit(Literal::Int(100)),
            )),
            Box::new(Predicate::Cmp(
                col("event_time"),
                CmpOp::Lt,
                lit(Literal::Int(200)),
            )),
        );
        assert_eq!(time_bounds(&p), (Some(100), Some(199)));
    }

    #[test]
    fn an_or_never_narrows_the_time_bounds() {
        // The one that would lose rows if we got it wrong. `t >= 100 OR cost > 5` matches
        // rows at t = 1, so narrowing to [100, ..) would drop them. Pruning that can lose a
        // row is not pruning, it is sampling.
        let p = Predicate::Or(
            Box::new(Predicate::Cmp(
                col("event_time"),
                CmpOp::GtEq,
                lit(Literal::Int(100)),
            )),
            Box::new(Predicate::Cmp(col("cost"), CmpOp::Gt, lit(Literal::Int(5)))),
        );
        assert_eq!(time_bounds(&p), (None, None));
    }
}
