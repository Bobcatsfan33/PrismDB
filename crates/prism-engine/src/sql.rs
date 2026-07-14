//! Executing a bound SQL plan — through the **same door** as the direct API.
//!
//! A semantic plan compiles to the `Query` the direct API already takes, and calls
//! `search_at`. It does not have its own scan, its own pruning, or its own idea of
//! ordering. The parity tests assert the *counters* match, not just the rows — because if
//! SQL ever grew its own executor, the counters would diverge before the results did.
//!
//! Pagination is snapshot-pinned keyset pagination on `(score DESC, event_id ASC)`. See
//! [`docs/QUERY-CONTRACT.md`](../../../docs/QUERY-CONTRACT.md).

use crate::engine::Engine;
use crate::rowsource::EventRow;
use prism_sql::ast::{Agg, Item};
use prism_sql::Plan;
use prism_types::error::{PrismError, Result};
use prism_types::hash::{content_id, crc32};
use prism_types::predicate::Value;
use prism_types::{Counters, Event, Query};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One output row: ordered (column, value) pairs, so a projection is reproducible.
pub type Row = Vec<(String, serde_json::Value)>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SqlResult {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    pub counters: Counters,
    pub snapshot_id: String,
    /// Present only when there is another page. Opaque: clients do not parse it.
    pub next_cursor: Option<String>,
}

// --- the cursor ----------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CursorBody {
    snapshot: String,
    /// The plan this cursor belongs to. Presenting a cursor to a *different* query is an
    /// error, not a surprising result.
    plan: String,
    last_score: f32,
    last_event_id: String,
}

/// Opaque, and checksummed so that a mangled cursor is refused rather than misread.
fn encode_cursor(b: &CursorBody) -> Result<String> {
    let json = serde_json::to_vec(b)?;
    let mut out = String::with_capacity(json.len() * 2 + 8);
    out.push_str(&format!("{:08x}", crc32(&json)));
    for byte in &json {
        out.push_str(&format!("{byte:02x}"));
    }
    Ok(out)
}

fn decode_cursor(s: &str) -> Result<CursorBody> {
    let bad = || PrismError::Invalid("cursor is malformed".to_string());
    if s.len() < 8 || s.len() % 2 != 0 {
        return Err(bad());
    }
    let want = u32::from_str_radix(&s[..8], 16).map_err(|_| bad())?;
    let mut bytes = Vec::with_capacity((s.len() - 8) / 2);
    let hex = &s.as_bytes()[8..];
    for pair in hex.chunks_exact(2) {
        let h = std::str::from_utf8(pair).map_err(|_| bad())?;
        bytes.push(u8::from_str_radix(h, 16).map_err(|_| bad())?);
    }
    if crc32(&bytes) != want {
        return Err(PrismError::Invalid(
            "cursor failed its checksum; it has been truncated or edited".into(),
        ));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// A cursor is bound to a plan, so it cannot be replayed against a different query.
fn plan_fingerprint(p: &Plan) -> String {
    let canon = serde_json::to_vec(p).unwrap_or_default();
    content_id(&canon)
}

// --- execution -----------------------------------------------------------------

impl Engine {
    /// Compile a plan to the `Query` the direct API takes.
    ///
    /// Public because the parity tests use it: they build a `Query` by hand, compile the
    /// equivalent SQL, and assert the two are **equal as values** — before ever running
    /// them. If the compiled query differs, SQL is a different door, and no amount of
    /// matching output would prove otherwise.
    pub fn plan_to_query(&self, plan: &Plan) -> Result<Query> {
        let text = plan
            .semantic
            .clone()
            .ok_or_else(|| PrismError::Invalid("not a semantic query".into()))?;
        Ok(Query {
            text,
            // The injected tenant, in the same field the direct API uses -- so it also
            // drives partition pruning, not just row filtering.
            tenant: Some(plan.tenant.clone()),
            time_from: None,
            time_to: None,
            k: plan.limit,
            nprobe: plan.nprobe,
            candidates: plan.candidates,
            rerank: plan.rerank,
            group_k: None,
            predicate: plan.filter.clone(),
            space: None,
        })
    }

    pub fn run_sql(&self, plan: &Plan, cursor: Option<&str>) -> Result<SqlResult> {
        if plan.is_aggregate() {
            if cursor.is_some() {
                return Err(PrismError::Invalid(
                    "aggregate queries are not paginated; a GROUP BY produces groups, and there \
                     is no defined total order over them yet (S9 owns semantic grouping)"
                        .into(),
                ));
            }
            return self.run_aggregate(plan);
        }
        self.run_rows(plan, cursor)
    }

    /// Resolve the snapshot a query runs against: the cursor's, if there is one.
    fn resolve_snapshot(
        &self,
        plan: &Plan,
        cursor: Option<&str>,
    ) -> Result<(prism_part::catalog::Snapshot, Option<CursorBody>)> {
        let Some(tok) = cursor else {
            return Ok((self.snapshot()?, None));
        };
        let body = decode_cursor(tok)?;

        if body.plan != plan_fingerprint(plan) {
            return Err(PrismError::Invalid(
                "this cursor belongs to a different query. A cursor binds the statement and its \
                 four controls; presenting it to another query is an error rather than a \
                 surprising result."
                    .into(),
            ));
        }

        // The snapshot the cursor pinned -- NOT `CURRENT`. Silently continuing against a
        // newer snapshot is how a client receives a page that overlaps the last one, or
        // skips rows that existed the whole time, and concludes we are lying to them.
        let snap = self.catalog().load_snapshot(&body.snapshot).map_err(|_| {
            PrismError::NotFound(format!(
                "cursor is bound to snapshot {}, which has been reclaimed; re-run the query to \
                 start from the current snapshot",
                body.snapshot
            ))
        })?;
        Ok((snap, Some(body)))
    }

    fn run_rows(&self, plan: &Plan, cursor: Option<&str>) -> Result<SqlResult> {
        let (snap, cur) = self.resolve_snapshot(plan, cursor)?;

        // Score every row of the result set, in the one total order.
        let (mut scored, counters) = self.result_set(plan, &snap)?;
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.event_id.cmp(&b.0.event_id)));

        // Keyset skip. The order is a total order and the snapshot is immutable, so "after
        // this position" is exactly, deterministically defined -- no duplicates, no gaps.
        //
        // Written out longhand rather than as a tuple comparison, because a tuple compares
        // its first element ASCENDING and this order is `score DESC, event_id ASC`. A tuple
        // compare here silently treats a row with an equal score and a SMALLER id as "after"
        // the cursor -- so pagination rewinds, repeats rows, and never terminates. It did.
        let start = match &cur {
            None => 0,
            Some(c) => scored
                .iter()
                .position(|(e, s)| match c.last_score.total_cmp(s) {
                    // score < last_score: strictly after, because score descends.
                    std::cmp::Ordering::Greater => true,
                    // equal score: after iff the id sorts later, because ids ascend.
                    std::cmp::Ordering::Equal => e.event_id.as_str() > c.last_event_id.as_str(),
                    // score > last_score: before the cursor.
                    std::cmp::Ordering::Less => false,
                })
                .unwrap_or(scored.len()),
        };

        let page: Vec<(Event, f32)> = scored
            .iter()
            .skip(start)
            .take(plan.limit)
            .cloned()
            .collect();

        let next_cursor = if start + page.len() < scored.len() && !page.is_empty() {
            let last = page.last().unwrap();
            Some(encode_cursor(&CursorBody {
                snapshot: snap.snapshot_id.clone(),
                plan: plan_fingerprint(plan),
                last_score: last.1,
                last_event_id: last.0.event_id.clone(),
            })?)
        } else {
            None
        };

        let columns = projection_names(plan);
        let rows = page
            .iter()
            .map(|(e, s)| project(plan, e, *s))
            .collect::<Result<Vec<_>>>()?;

        Ok(SqlResult {
            columns,
            rows,
            counters,
            snapshot_id: snap.snapshot_id,
            next_cursor,
        })
    }

    /// The rows a plan produces, scored, unordered.
    ///
    /// For a semantic plan this is the re-rank survivor set — which **is** the result set
    /// (query contract §4), and is why `DEFAULT_RERANK` has a pagination floor.
    fn result_set(
        &self,
        plan: &Plan,
        snap: &prism_part::catalog::Snapshot,
    ) -> Result<(Vec<(Event, f32)>, Counters)> {
        match &plan.semantic {
            Some(_) => {
                let mut q = self.plan_to_query(plan)?;
                // The result set is the whole rerank survivor set, not just the first page.
                q.k = plan.rerank;
                let res = self.search_at(snap, &q)?;
                Ok((
                    res.hits.into_iter().map(|h| (h.event, h.score)).collect(),
                    res.counters,
                ))
            }
            None => self.scalar_scan(plan, snap),
        }
    }

    /// The scalar path: no semantic predicate, so no centroid index, no PQ, no rerank.
    ///
    /// Every row has the same score, so the total order of §1 collapses to `event_id ASC` —
    /// which is still a total order, and that is what pagination needs.
    fn scalar_scan(
        &self,
        plan: &Plan,
        snap: &prism_part::catalog::Snapshot,
    ) -> Result<(Vec<(Event, f32)>, Counters)> {
        let (from, to) = match &plan.filter {
            Some(p) => prism_types::predicate::time_bounds(p),
            None => (None, None),
        };

        // Catalog pruning, before any part is opened (S4).
        let (readers, catalog_pruned) = self.open_candidates(snap, Some(&plan.tenant), from, to)?;
        let mut c = Counters {
            parts_total: snap.parts.len(),
            parts_pruned: catalog_pruned,
            ..Default::default()
        };

        let mut out = Vec::new();
        for r in &readers {
            // Per-tenant zone map: a shared bucket's part-level range describes the BUCKET.
            let keep = match r.manifest.s4()?.stats_for(&plan.tenant) {
                Some(st) => st.may_match(from, to),
                None => r.manifest.may_match(Some(&plan.tenant), from, to),
            };
            if !keep {
                c.parts_pruned += 1;
                continue;
            }
            c.parts_opened += 1;
            c.rows_eligible += r.manifest.row_count;

            let rows = r.read_all()?;
            c.physical_bytes_read += r.io_bytes();

            for e in rows.events {
                // The tenant policy, applied here too -- the same value that pruned parts
                // above. A row of another tenant cannot reach this loop, and if it somehow
                // did, it would not leave it.
                if e.tenant_id != plan.tenant {
                    continue;
                }
                if let Some(p) = &plan.filter {
                    let view = EventRow {
                        event: &e,
                        score: 0.0,
                    };
                    if !prism_types::predicate::eval(p, &view, 0)? {
                        continue;
                    }
                }
                c.rows_passing_filter += 1;
                out.push((e, 0.0f32));
            }
        }
        Ok((out, c))
    }

    /// Is this a pure `COUNT(*)` — a question about the *number* of rows, not their values?
    ///
    /// Then it must not materialize a single one. Decoding bodies, vectors and attribute maps
    /// to answer "how many?" is work nobody asked for, and it is also what made a promoted
    /// column read *more* bytes than the attribute map it replaced: the win promotion buys is on
    /// the *predicate* path, and a scan that materializes everything anyway erases it.
    fn is_pure_count(plan: &Plan) -> bool {
        plan.group_by.is_empty()
            && !plan.projections.is_empty()
            && plan
                .projections
                .iter()
                .all(|i| matches!(i, Item::Agg(Agg::CountStar)))
    }

    /// Count matching rows without materializing any of them.
    fn scalar_count(
        &self,
        plan: &Plan,
        snap: &prism_part::catalog::Snapshot,
    ) -> Result<(usize, Counters)> {
        let (from, to) = match &plan.filter {
            Some(p) => prism_types::predicate::time_bounds(p),
            None => (None, None),
        };
        let (readers, catalog_pruned) = self.open_candidates(snap, Some(&plan.tenant), from, to)?;
        let mut c = Counters {
            parts_total: snap.parts.len(),
            parts_pruned: catalog_pruned,
            ..Default::default()
        };

        let mut n = 0usize;
        for r in &readers {
            let keep = match r.manifest.s4()?.stats_for(&plan.tenant) {
                Some(st) => st.may_match(from, to),
                None => r.manifest.may_match(Some(&plan.tenant), from, to),
            };
            if !keep {
                c.parts_pruned += 1;
                continue;
            }
            c.parts_opened += 1;
            c.rows_eligible += r.manifest.row_count;

            // Only the columns the predicate names -- plus tenant_id, which the policy names.
            let mut pred = crate::rowsource::PartRows::new(r, plan.filter.as_ref())?;
            let tenants = r.read_scalars()?;
            let _ = &mut pred;

            for row in 0..r.manifest.row_count {
                if !tenants.tenant_is(row, &plan.tenant) {
                    continue;
                }
                if let Some(p) = &plan.filter {
                    if !prism_types::predicate::eval(p, &pred, row)? {
                        continue;
                    }
                }
                c.rows_passing_filter += 1;
                n += 1;
            }
            c.physical_bytes_read += r.io_bytes();
        }
        Ok((n, c))
    }

    fn run_aggregate(&self, plan: &Plan) -> Result<SqlResult> {
        let snap = self.snapshot()?;

        if plan.semantic.is_none() && Self::is_pure_count(plan) {
            let (n, counters) = self.scalar_count(plan, &snap)?;
            let row: Row = plan
                .projections
                .iter()
                .map(|_| ("count(*)".to_string(), serde_json::json!(n)))
                .collect();
            return Ok(SqlResult {
                columns: projection_names(plan),
                rows: vec![row],
                counters,
                snapshot_id: snap.snapshot_id,
                next_cursor: None,
            });
        }

        let (rows, counters) = self.result_set(plan, &snap)?;

        // Group. An empty GROUP BY is one group over everything.
        let mut groups: BTreeMap<Vec<String>, Vec<(Event, f32)>> = BTreeMap::new();
        let ungrouped = plan.group_by.is_empty();
        for (e, s) in rows {
            let key: Vec<String> = plan
                .group_by
                .iter()
                .map(|g| {
                    let v = EventRow {
                        event: &e,
                        score: s,
                    };
                    prism_types::predicate::RowSource::column(&v, g, 0).map(|x| display(&x))
                })
                .collect::<Result<Vec<_>>>()?;
            groups.entry(key).or_default().push((e, s));
        }

        // `SELECT count(*) FROM events WHERE <nothing matches>` is **one row saying zero**,
        // not zero rows. An ungrouped aggregate is a question about the set, and the empty
        // set has an answer. Returning nothing would make "no rows matched" indistinguishable
        // from "the query failed", which is exactly the ambiguity a caller cannot resolve.
        if ungrouped && groups.is_empty() {
            groups.insert(Vec::new(), Vec::new());
        }

        let columns = projection_names(plan);
        let mut out = Vec::new();
        for (key, members) in groups {
            let mut row: Row = Vec::new();
            let mut ki = 0usize;
            for item in &plan.projections {
                match item {
                    Item::Column(c) => {
                        let v = if plan.group_by.contains(c) {
                            let v = key[ki].clone();
                            ki += 1;
                            serde_json::Value::String(v)
                        } else {
                            serde_json::Value::Null
                        };
                        row.push((c.clone(), v));
                    }
                    Item::Agg(a) => row.push((agg_name(a), aggregate(a, &members)?)),
                    Item::Star => {
                        return Err(PrismError::Invalid(
                            "`*` is not meaningful beside an aggregate".into(),
                        ))
                    }
                    Item::Attribute(k) => {
                        row.push((format!("attributes[{k}]"), serde_json::Value::Null))
                    }
                }
            }
            out.push(row);
        }

        Ok(SqlResult {
            columns,
            rows: out,
            counters,
            snapshot_id: snap.snapshot_id,
            next_cursor: None,
        })
    }
}

fn display(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
    }
}

fn agg_name(a: &Agg) -> String {
    match a {
        Agg::CountStar => "count(*)".into(),
        Agg::Count(c) => format!("count({c})"),
        Agg::Sum(c) => format!("sum({c})"),
        Agg::Avg(c) => format!("avg({c})"),
        Agg::Min(c) => format!("min({c})"),
        Agg::Max(c) => format!("max({c})"),
    }
}

fn aggregate(a: &Agg, members: &[(Event, f32)]) -> Result<serde_json::Value> {
    use serde_json::json;
    let nums = |col: &str| -> Result<Vec<f64>> {
        members
            .iter()
            .map(|(e, s)| {
                let v = EventRow {
                    event: e,
                    score: *s,
                };
                let x = prism_types::predicate::RowSource::column(&v, col, 0)?;
                Ok(x.as_f64())
            })
            .collect::<Result<Vec<Option<f64>>>>()
            .map(|v| v.into_iter().flatten().collect())
    };

    Ok(match a {
        Agg::CountStar => json!(members.len()),
        // COUNT(col) counts rows where the column is not null -- which is what SQL means,
        // and what makes `count(observed_time)` on a v1 part honestly report zero.
        Agg::Count(c) => {
            let n = members
                .iter()
                .filter(|(e, s)| {
                    let v = EventRow {
                        event: e,
                        score: *s,
                    };
                    !matches!(
                        prism_types::predicate::RowSource::column(&v, c, 0),
                        Ok(Value::Null)
                    )
                })
                .count();
            json!(n)
        }
        Agg::Sum(c) => json!(nums(c)?.iter().sum::<f64>()),
        Agg::Avg(c) => {
            let v = nums(c)?;
            if v.is_empty() {
                serde_json::Value::Null
            } else {
                json!(v.iter().sum::<f64>() / v.len() as f64)
            }
        }
        Agg::Min(c) => nums(c)?
            .into_iter()
            .fold(None::<f64>, |acc, x| Some(acc.map_or(x, |a| a.min(x))))
            .map(|x| json!(x))
            .unwrap_or(serde_json::Value::Null),
        Agg::Max(c) => nums(c)?
            .into_iter()
            .fold(None::<f64>, |acc, x| Some(acc.map_or(x, |a| a.max(x))))
            .map(|x| json!(x))
            .unwrap_or(serde_json::Value::Null),
    })
}

fn projection_names(plan: &Plan) -> Vec<String> {
    let mut out = Vec::new();
    for i in &plan.projections {
        match i {
            Item::Star => out.extend(
                prism_sql::COLUMNS
                    .iter()
                    .filter(|c| **c != "score")
                    .map(|c| c.to_string()),
            ),
            Item::Column(c) => out.push(c.clone()),
            Item::Attribute(k) => out.push(format!("attributes[{k}]")),
            Item::Agg(a) => out.push(agg_name(a)),
        }
    }
    out
}

fn project(plan: &Plan, e: &Event, score: f32) -> Result<Row> {
    let view = EventRow { event: e, score };
    let mut row: Row = Vec::new();
    for i in &plan.projections {
        match i {
            Item::Star => {
                for c in prism_sql::COLUMNS.iter().filter(|c| **c != "score") {
                    let v = prism_types::predicate::RowSource::column(&view, c, 0)?;
                    row.push((c.to_string(), to_json(&v)));
                }
            }
            Item::Column(c) => {
                let v = prism_types::predicate::RowSource::column(&view, c, 0)?;
                row.push((c.clone(), to_json(&v)));
            }
            Item::Attribute(k) => {
                let v = prism_types::predicate::RowSource::attribute(&view, k, 0)?;
                row.push((format!("attributes[{k}]"), to_json(&v)));
            }
            Item::Agg(_) => {
                return Err(PrismError::Invalid(
                    "an aggregate appeared in a non-aggregate query".into(),
                ))
            }
        }
    }
    Ok(row)
}

fn to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Str(s) => serde_json::Value::String(s.clone()),
        Value::Int(i) => serde_json::json!(i),
        Value::Float(f) => serde_json::json!(f),
        Value::Bool(b) => serde_json::json!(b),
        Value::Null => serde_json::Value::Null,
    }
}
