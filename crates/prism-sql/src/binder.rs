//! The binder: AST → a plan the engine can run, with tenant policy injected beneath it.
//!
//! > *"mandatory tenant policy injected by the authorization layer (not removable by SQL)"*
//! > — PRISM.md, Part III §11
//!
//! **The session's tenant is not a SQL-level predicate.** It is not something the user's
//! `WHERE` clause can see, reference, alias around, or reason about. The binder produces:
//!
//! ```text
//!   (whatever the user wrote)  AND  tenant_id = <session tenant>
//! ```
//!
//! The user's expression is a **subtree**. Nothing inside a subtree can widen the
//! conjunction it is nested inside — not an `OR`, not a `NOT`, not a rewritten comparison,
//! not an alias. That is not a check that has to be got right in fifty places; it is a
//! shape, and the shape is the security property.
//!
//! Two further defences, both of them boring on purpose:
//!
//! * **Aliases are not visible in `WHERE`.** `SELECT tenant_id AS t ... WHERE t = 'other'`
//!   fails to bind, because `t` is not a column. This is also just standard SQL — aliases
//!   are not in scope in `WHERE` — but here it is load-bearing.
//! * **The tenant predicate is carried in `Query.tenant`, the same field the direct API
//!   uses**, and it is *also* what drives partition pruning. So a query that somehow
//!   escaped the row filter would still be reading only its own tenant's parts.

use crate::ast::*;
use prism_types::error::{PrismError, Result};
use prism_types::query::{DEFAULT_CANDIDATES, DEFAULT_NPROBE, DEFAULT_RERANK};
use serde::{Deserialize, Serialize};

/// The columns a query may name. An unknown one is an error, not an empty result.
pub const COLUMNS: &[&str] = &[
    "event_id",
    "tenant_id",
    "event_time",
    "observed_time",
    "event_name",
    "cost",
    "error",
    "body",
    "trace_id",
    "span_id",
    "score",
];

/// Columns that may be aggregated numerically.
const NUMERIC: &[&str] = &["cost", "event_time", "observed_time", "score"];

/// Who is asking. Comes from the authorization layer, never from the SQL.
#[derive(Clone, Debug)]
pub struct Session {
    pub tenant: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// **Injected, not parsed.** Always `Some`.
    pub tenant: String,
    /// The user's row filter — a subtree, ANDed *under* the tenant policy.
    pub filter: Option<Expr>,
    pub semantic: Option<String>,
    pub projections: Vec<Item>,
    pub group_by: Vec<String>,
    pub limit: usize,
    pub nprobe: usize,
    pub candidates: usize,
    pub rerank: usize,
}

impl Plan {
    pub fn is_aggregate(&self) -> bool {
        !self.group_by.is_empty() || self.projections.iter().any(|i| matches!(i, Item::Agg(_)))
    }
}

pub fn bind(stmt: Select, session: &Session) -> Result<Plan> {
    if session.tenant.is_empty() {
        return Err(PrismError::Invariant(
            "no session tenant; a query with no tenant policy is a cross-tenant read, and this \
             engine does not have one"
                .into(),
        ));
    }

    // Validate the user's filter against the schema. Note what this does NOT do: it does
    // not look for tenant_id, or special-case it, or try to detect an attempt to escape.
    // There is nothing to detect. `tenant_id` is an ordinary column the user may constrain;
    // the policy lives one level above, in a place the expression cannot reach.
    if let Some(f) = &stmt.filter {
        check_expr(f)?;
    }

    for i in &stmt.items {
        match i {
            Item::Column(c) => check_column(c)?,
            Item::Agg(a) => check_agg(a)?,
            Item::Star | Item::Attribute(_) => {}
        }
    }
    for g in &stmt.group_by {
        check_column(g)?;
        if g == "score" || g == "semantic_cluster" {
            return Err(PrismError::Invalid(
                "cannot GROUP BY score or semantic_cluster in SQL yet. Semantic grouping — GROUP \
                 BY meaning — is built and gated at the engine level in S9 (Engine::semantic_cluster, \
                 with NOVELTY/SEMANTIC_DIFF alongside it); its determinism, ordering, bounding and \
                 exemplar rules are the query contract §15–§18. The SQL *keyword* surface for it is \
                 the next increment, deferred exactly as S8 deferred the Flight wire transport — the \
                 semantics ship first, the grammar follows."
                .into(),
            ));
        }
    }

    // An aggregate query must group by every non-aggregate column it projects, or the answer
    // is ill-defined and the engine would have to pick a row arbitrarily.
    if stmt.group_by.is_empty()
        && stmt.items.iter().any(|i| matches!(i, Item::Agg(_)))
        && stmt
            .items
            .iter()
            .any(|i| matches!(i, Item::Column(_) | Item::Star))
    {
        return Err(PrismError::Invalid(
            "a column is projected alongside an aggregate without a GROUP BY; which row's value \
             would that be?"
                .into(),
        ));
    }

    let limit = stmt.limit.unwrap_or(10);

    Ok(Plan {
        // THE INJECTION. Not derived from the statement, not overridable by it.
        tenant: session.tenant.clone(),
        filter: stmt.filter,
        semantic: stmt.semantic.map(|s| s.text),
        projections: stmt.items,
        group_by: stmt.group_by,
        limit,
        nprobe: stmt.controls.nprobe.unwrap_or(DEFAULT_NPROBE),
        candidates: stmt.controls.candidates.unwrap_or(DEFAULT_CANDIDATES),
        rerank: stmt.controls.rerank.unwrap_or(DEFAULT_RERANK),
    })
}

fn check_column(c: &str) -> Result<()> {
    if COLUMNS.contains(&c) {
        return Ok(());
    }
    Err(PrismError::Invalid(format!(
        "unknown column `{c}`. An alias is not a column and is not in scope in WHERE; the \
         columns are: {}",
        COLUMNS.join(", ")
    )))
}

fn check_agg(a: &Agg) -> Result<()> {
    let (name, col) = match a {
        Agg::CountStar => return Ok(()),
        Agg::Count(c) => ("count", c),
        Agg::Sum(c) => ("sum", c),
        Agg::Avg(c) => ("avg", c),
        Agg::Min(c) => ("min", c),
        Agg::Max(c) => ("max", c),
    };
    check_column(col)?;
    if matches!(a, Agg::Sum(_) | Agg::Avg(_)) && !NUMERIC.contains(&col.as_str()) {
        return Err(PrismError::Invalid(format!(
            "{name}({col}) is not defined: `{col}` is not numeric"
        )));
    }
    Ok(())
}

fn check_expr(e: &Expr) -> Result<()> {
    match e {
        Expr::Column(c) => check_column(c),
        Expr::Attribute(_) | Expr::Literal(_) => Ok(()),
        Expr::Not(a) => check_expr(a),
        Expr::And(a, b) | Expr::Or(a, b) => {
            check_expr(a)?;
            check_expr(b)
        }
        Expr::Cmp(a, _, b) => {
            check_expr(a)?;
            check_expr(b)
        }
        Expr::In(a, _) => check_expr(a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn plan(sql: &str) -> Result<Plan> {
        bind(
            parse(sql)?,
            &Session {
                tenant: "mine".into(),
            },
        )
    }

    #[test]
    fn the_tenant_is_injected_and_is_not_in_the_statement() {
        let p = plan("SELECT * FROM events").unwrap();
        assert_eq!(p.tenant, "mine");
        assert!(p.filter.is_none());
    }

    #[test]
    fn a_user_may_narrow_their_own_visibility_but_the_policy_still_applies() {
        // `tenant_id = 'other'` is a perfectly legal row predicate. It is ANDed UNDER the
        // policy, so it narrows to the empty set rather than widening to another tenant.
        let p = plan("SELECT * FROM events WHERE tenant_id = 'other'").unwrap();
        assert_eq!(
            p.tenant, "mine",
            "the policy tenant was overridden by the statement"
        );
        assert!(p.filter.is_some());
    }

    #[test]
    fn an_or_cannot_widen_the_conjunction_it_is_nested_inside() {
        // The classic escape attempt. It binds fine -- and it is harmless, because the whole
        // expression is a SUBTREE of `... AND tenant_id = 'mine'`. A subtree cannot widen the
        // conjunction it sits in. The shape IS the security property.
        let p = plan("SELECT * FROM events WHERE tenant_id = 'other' OR 1 = 1").unwrap();
        assert_eq!(p.tenant, "mine");
    }

    #[test]
    fn an_alias_is_not_a_column_and_is_not_in_scope_in_where() {
        let e = plan("SELECT tenant_id AS t FROM events WHERE t = 'other'")
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown column `t`"), "{e}");
        assert!(e.contains("not in scope in WHERE"), "{e}");
    }

    #[test]
    fn an_unknown_column_is_an_error_not_an_empty_result() {
        // Returning nothing for a typo is how a client concludes their data is missing.
        assert!(plan("SELECT * FROM events WHERE tenant = 'other'").is_err());
        assert!(plan("SELECT * FROM events WHERE TENANT_ID_ = 'x'").is_err());
    }

    #[test]
    fn an_empty_session_tenant_is_refused() {
        let e = bind(
            parse("SELECT * FROM events").unwrap(),
            &Session {
                tenant: String::new(),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("cross-tenant"), "{e}");
    }

    #[test]
    fn controls_default_to_their_receipted_values() {
        let p = plan("SELECT * FROM events").unwrap();
        assert_eq!(p.nprobe, DEFAULT_NPROBE);
        assert_eq!(p.candidates, DEFAULT_CANDIDATES);
        assert_eq!(p.rerank, DEFAULT_RERANK);
    }

    #[test]
    fn a_projection_beside_an_aggregate_without_group_by_is_refused() {
        assert!(plan("SELECT event_name, count(*) FROM events").is_err());
        plan("SELECT event_name, count(*) FROM events GROUP BY event_name").unwrap();
    }

    #[test]
    fn sum_of_a_non_numeric_column_is_refused() {
        assert!(plan("SELECT sum(event_name) FROM events").is_err());
        plan("SELECT sum(cost) FROM events").unwrap();
    }

    #[test]
    fn grouping_by_score_is_refused_with_the_reason() {
        let e = plan("SELECT count(*) FROM events GROUP BY score")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("GROUP BY meaning") && e.contains("engine level"),
            "{e}"
        );
    }
}
