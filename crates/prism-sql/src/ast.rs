//! The AST. Deliberately small: PrismDB executes one shape of statement.

use serde::{Deserialize, Serialize};

// The filter language is NOT defined here. It lives in `prism-types::predicate`, so the
// direct API can build exactly what SQL compiles to -- see the crate docs. If the two doors
// had separate filter languages, "the same door" would be a slogan rather than a type.
pub use prism_types::predicate::{CmpOp, Literal, Predicate as Expr};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Agg {
    CountStar,
    Count(String),
    Sum(String),
    Avg(String),
    Min(String),
    Max(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Item {
    Star,
    Column(String),
    Attribute(String),
    Agg(Agg),
}

/// `embedding ≈≈ 'text'` — the semantic predicate. At most one per statement: two
/// different meanings in one query is not a query, it is two.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Semantic {
    pub text: String,
}

/// The four controls, settable per statement. They are separate for a reason
/// (PRISM.md Part III §11) and the SQL surface does not collapse them into one
/// "quality" knob.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Controls {
    pub nprobe: Option<usize>,
    pub candidates: Option<usize>,
    pub rerank: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Select {
    pub items: Vec<Item>,
    pub filter: Option<Expr>,
    pub semantic: Option<Semantic>,
    pub group_by: Vec<String>,
    pub limit: Option<usize>,
    pub controls: Controls,
}
