//! PrismDB's SQL surface: lexer, parser, binder.
//!
//! **This is a second door into the same engine, and it must be provably the SAME door.**
//!
//! It compiles to the `Query` the direct API already takes and calls the same executor. It
//! is a parser and a binder; it is not a second implementation of pruning, scanning,
//! ordering, or anything else. Every gate test runs each query through *both* doors and
//! asserts the results are byte-identical **and that the physical-execution counters match
//! too** — because if SQL ever grows its own scan, the counters diverge before the results
//! do, and we would rather find out from a counter than from a customer.
//!
//! Two doors into a database that disagree is a class of bug that takes years to find,
//! because each door is individually self-consistent.
//!
//! The contract is [`docs/QUERY-CONTRACT.md`](../../../docs/QUERY-CONTRACT.md).

pub mod ast;
pub mod binder;
pub mod lexer;
pub mod limits;
pub mod parser;

pub use ast::{Agg, CmpOp, Expr, Item, Literal, Select};
pub use binder::{bind, Plan, Session, COLUMNS};
pub use parser::parse;

/// Parse and bind in one step.
pub fn compile(sql: &str, session: &Session) -> prism_types::error::Result<Plan> {
    bind(parse(sql)?, session)
}
