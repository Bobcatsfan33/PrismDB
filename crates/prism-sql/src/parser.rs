//! Recursive-descent parser, with a depth counter.
//!
//! The depth counter is the important part. A recursive-descent parser without one is a
//! stack overflow waiting for `((((((...))))))` — and a stack overflow is a *process
//! death*, not an error. It cannot be caught, reported, or attributed to the query that
//! caused it. Every descent goes through [`Parser::deeper`].

use crate::ast::*;
use crate::lexer::{lex, Tok};
use crate::limits::*;
use prism_types::error::{PrismError, Result};

pub struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    depth: usize,
}

pub fn parse(sql: &str) -> Result<Select> {
    Parser::new(lex(sql)?).select()
}

impl Parser {
    fn new(toks: Vec<Tok>) -> Self {
        Parser {
            toks,
            pos: 0,
            depth: 0,
        }
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    /// Is the next token this keyword? Case-insensitive, and it does not consume.
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(PrismError::Invalid(format!(
                "expected `{kw}`, found {}",
                self.here()
            )))
        }
    }

    fn expect(&mut self, t: Tok) -> Result<()> {
        if self.peek() == Some(&t) {
            self.pos += 1;
            Ok(())
        } else {
            Err(PrismError::Invalid(format!(
                "expected {}, found {}",
                t.describe(),
                self.here()
            )))
        }
    }

    fn here(&self) -> String {
        match self.peek() {
            Some(t) => t.describe(),
            None => "end of statement".into(),
        }
    }

    /// Every recursive descent goes through here. There is no other way down.
    fn deeper<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            return Err(PrismError::Invalid(format!(
                "expression nests deeper than {MAX_EXPR_DEPTH} levels"
            )));
        }
        let r = f(self);
        self.depth -= 1;
        r
    }

    // --- statement ---

    fn select(&mut self) -> Result<Select> {
        self.expect_kw("SELECT")?;
        let items = self.items()?;

        self.expect_kw("FROM")?;
        match self.next() {
            Some(Tok::Ident(t)) if t.eq_ignore_ascii_case("events") => {}
            other => {
                return Err(PrismError::Invalid(format!(
                    "the only table is `events`, found {}",
                    other.map(|t| t.describe()).unwrap_or("nothing".into())
                )))
            }
        }

        let mut filter = None;
        let mut semantic = None;
        if self.eat_kw("WHERE") {
            let (f, s) = self.where_clause()?;
            filter = f;
            semantic = s;
        }

        let mut group_by = Vec::new();
        if self.eat_kw("GROUP") {
            self.expect_kw("BY")?;
            loop {
                match self.next() {
                    Some(Tok::Ident(c)) => {
                        if group_by.len() >= MAX_GROUP_KEYS {
                            return Err(PrismError::Invalid(format!(
                                "more than {MAX_GROUP_KEYS} GROUP BY keys"
                            )));
                        }
                        group_by.push(c);
                    }
                    other => {
                        return Err(PrismError::Invalid(format!(
                            "expected a column in GROUP BY, found {}",
                            other.map(|t| t.describe()).unwrap_or("nothing".into())
                        )))
                    }
                }
                if !self.eat(Tok::Comma) {
                    break;
                }
            }
        }

        // ORDER BY is accepted only in the one form the contract defines, and rejected
        // otherwise -- rather than silently ignored, which would let a caller believe they
        // had asked for an order they did not get.
        if self.eat_kw("ORDER") {
            self.expect_kw("BY")?;
            let ok = self.eat_kw("score")
                && self.eat_kw("DESC")
                && self.eat(Tok::Comma)
                && self.eat_kw("event_id")
                && self.eat_kw("ASC");
            if !ok {
                return Err(PrismError::Invalid(
                    "results have one total order, and it is not negotiable: \
                     `ORDER BY score DESC, event_id ASC`. See docs/QUERY-CONTRACT.md §1 — \
                     ties break on event_id so that a merge cannot change the order of an \
                     unchanged answer."
                        .into(),
                ));
            }
        }

        let mut limit = None;
        if self.eat_kw("LIMIT") {
            match self.next() {
                Some(Tok::Int(n)) if n >= 0 => limit = Some(n as usize),
                other => {
                    return Err(PrismError::Invalid(format!(
                        "LIMIT wants a non-negative integer, found {}",
                        other.map(|t| t.describe()).unwrap_or("nothing".into())
                    )))
                }
            }
        }

        if self.eat_kw("OFFSET") {
            return Err(PrismError::Invalid(
                "OFFSET is not supported and will not be. It gets slower the deeper you page, \
                 and against a moving dataset it duplicates and drops rows. Use a cursor: \
                 keyset pagination on (score DESC, event_id ASC) against a pinned snapshot \
                 costs the same at page 1000 as at page 1 and cannot do either. \
                 See docs/QUERY-CONTRACT.md §3."
                    .into(),
            ));
        }

        let controls = self.controls()?;

        if let Some(t) = self.peek() {
            return Err(PrismError::Invalid(format!(
                "unexpected {} after the end of the statement",
                t.describe()
            )));
        }

        Ok(Select {
            items,
            filter,
            semantic,
            group_by,
            limit,
            controls,
        })
    }

    fn eat(&mut self, t: Tok) -> bool {
        if self.peek() == Some(&t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// `WITH (nprobe = 8, candidates = 200, rerank = 50)`.
    fn controls(&mut self) -> Result<Controls> {
        let mut c = Controls::default();
        if !self.eat_kw("WITH") {
            return Ok(c);
        }
        self.expect(Tok::LParen)?;
        loop {
            let name = match self.next() {
                Some(Tok::Ident(s)) => s,
                other => {
                    return Err(PrismError::Invalid(format!(
                        "expected a control name, found {}",
                        other.map(|t| t.describe()).unwrap_or("nothing".into())
                    )))
                }
            };
            self.expect(Tok::Eq)?;
            let v = match self.next() {
                Some(Tok::Int(n)) if n > 0 => n as usize,
                _ => {
                    return Err(PrismError::Invalid(format!(
                        "control `{name}` wants a positive integer"
                    )))
                }
            };
            match name.to_ascii_lowercase().as_str() {
                "nprobe" => c.nprobe = Some(v),
                "candidates" => c.candidates = Some(v),
                "rerank" => c.rerank = Some(v),
                other => {
                    return Err(PrismError::Invalid(format!(
                        "unknown control `{other}`; the four controls are k (LIMIT), nprobe, \
                         candidates and rerank"
                    )))
                }
            }
            if !self.eat(Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        Ok(c)
    }

    // --- projections ---

    fn items(&mut self) -> Result<Vec<Item>> {
        let mut out = Vec::new();
        loop {
            if out.len() >= MAX_PROJECTIONS {
                return Err(PrismError::Invalid(format!(
                    "more than {MAX_PROJECTIONS} projected expressions"
                )));
            }
            out.push(self.item()?);
            if !self.eat(Tok::Comma) {
                break;
            }
        }
        Ok(out)
    }

    fn item(&mut self) -> Result<Item> {
        if self.eat(Tok::Star) {
            return Ok(Item::Star);
        }
        // aggregate?
        if let Some(Tok::Ident(name)) = self.peek().cloned() {
            let lower = name.to_ascii_lowercase();
            if matches!(lower.as_str(), "count" | "sum" | "avg" | "min" | "max")
                && self.toks.get(self.pos + 1) == Some(&Tok::LParen)
            {
                self.pos += 2;
                if lower == "count" && self.eat(Tok::Star) {
                    self.expect(Tok::RParen)?;
                    return Ok(Item::Agg(Agg::CountStar));
                }
                let col = match self.next() {
                    Some(Tok::Ident(c)) => c,
                    other => {
                        return Err(PrismError::Invalid(format!(
                            "{lower}() wants a column, found {}",
                            other.map(|t| t.describe()).unwrap_or("nothing".into())
                        )))
                    }
                };
                self.expect(Tok::RParen)?;
                let a = match lower.as_str() {
                    "count" => Agg::Count(col),
                    "sum" => Agg::Sum(col),
                    "avg" => Agg::Avg(col),
                    "min" => Agg::Min(col),
                    "max" => Agg::Max(col),
                    _ => unreachable!(),
                };
                return Ok(Item::Agg(a));
            }

            if lower == "attributes" && self.toks.get(self.pos + 1) == Some(&Tok::LBracket) {
                self.pos += 1;
                return Ok(Item::Attribute(self.attribute_key()?));
            }

            self.pos += 1;
            // An alias is accepted syntactically and then IGNORED for binding purposes: an
            // alias may not be referenced in WHERE. That is standard SQL, and here it is
            // also a security property -- see the binder.
            if self.eat_kw("AS") {
                match self.next() {
                    Some(Tok::Ident(_)) => {}
                    other => {
                        return Err(PrismError::Invalid(format!(
                            "expected an alias after AS, found {}",
                            other.map(|t| t.describe()).unwrap_or("nothing".into())
                        )))
                    }
                }
            }
            return Ok(Item::Column(name));
        }
        Err(PrismError::Invalid(format!(
            "expected a column, `*`, or an aggregate, found {}",
            self.here()
        )))
    }

    fn attribute_key(&mut self) -> Result<String> {
        self.expect(Tok::LBracket)?;
        let k = match self.next() {
            Some(Tok::Str(s)) => s,
            other => {
                return Err(PrismError::Invalid(format!(
                    "attributes[...] wants a string key, found {}",
                    other.map(|t| t.describe()).unwrap_or("nothing".into())
                )))
            }
        };
        self.expect(Tok::RBracket)?;
        Ok(k)
    }

    // --- WHERE ---

    /// Splits the semantic predicate out of the boolean expression.
    ///
    /// `embedding ≈≈ 'text'` is not a row predicate — it is a *plan*, and it may appear at
    /// most once, at the top level of the conjunction. Allowing it under an `OR` would mean
    /// a query whose meaning is "similar to this, or else anything at all", which is not a
    /// semantic query, it is a full scan with extra steps.
    fn where_clause(&mut self) -> Result<(Option<Expr>, Option<Semantic>)> {
        let mut semantic: Option<Semantic> = None;
        let mut conj: Vec<Expr> = Vec::new();

        loop {
            if self.is_semantic_ahead() {
                self.pos += 2; // embedding ≈≈
                let text = match self.next() {
                    Some(Tok::Str(s)) => s,
                    other => {
                        return Err(PrismError::Invalid(format!(
                            "`≈≈` wants a string on the right, found {}",
                            other.map(|t| t.describe()).unwrap_or("nothing".into())
                        )))
                    }
                };
                if semantic.is_some() {
                    return Err(PrismError::Invalid(
                        "two semantic predicates in one statement. Two different meanings is not \
                         one query, it is two."
                            .into(),
                    ));
                }
                if text.trim().is_empty() {
                    return Err(PrismError::Invalid(
                        "the semantic predicate's text is empty; it would produce a zero-norm \
                         query vector"
                            .into(),
                    ));
                }
                semantic = Some(Semantic { text });
            } else {
                conj.push(self.expr()?);
            }

            if !self.eat_kw("AND") {
                break;
            }
        }

        // A dangling OR at the top would have been consumed by expr(); if a semantic
        // predicate appears anywhere else, the lexer's Approx token survives into expr()
        // and fails there with a clear message.
        let filter = conj
            .into_iter()
            .reduce(|a, b| Expr::And(Box::new(a), Box::new(b)));
        Ok((filter, semantic))
    }

    fn is_semantic_ahead(&self) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("embedding"))
            && self.toks.get(self.pos + 1) == Some(&Tok::Approx)
    }

    fn expr(&mut self) -> Result<Expr> {
        self.deeper(|p| p.or_expr())
    }

    fn or_expr(&mut self) -> Result<Expr> {
        let mut lhs = self.deeper(|p| p.and_expr())?;
        while self.eat_kw("OR") {
            let rhs = self.deeper(|p| p.and_expr())?;
            lhs = Expr::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        // Note: the top-level AND is handled by where_clause, so this only sees ANDs nested
        // inside parentheses or under an OR.
        let mut lhs = self.deeper(|p| p.unary())?;
        while self.is_kw("AND") && !self.semantic_follows_and() {
            self.pos += 1;
            let rhs = self.deeper(|p| p.unary())?;
            lhs = Expr::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn semantic_follows_and(&self) -> bool {
        matches!(self.toks.get(self.pos + 1), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("embedding"))
            && self.toks.get(self.pos + 2) == Some(&Tok::Approx)
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat_kw("NOT") {
            return Ok(Expr::Not(Box::new(self.deeper(|p| p.unary())?)));
        }
        self.deeper(|p| p.cmp())
    }

    fn cmp(&mut self) -> Result<Expr> {
        let lhs = self.deeper(|p| p.primary())?;

        if self.eat_kw("IN") {
            self.expect(Tok::LParen)?;
            let mut list = Vec::new();
            loop {
                if list.len() >= MAX_IN_LIST {
                    return Err(PrismError::Invalid(format!(
                        "IN list has more than {MAX_IN_LIST} elements"
                    )));
                }
                list.push(self.literal()?);
                if !self.eat(Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RParen)?;
            return Ok(Expr::In(Box::new(lhs), list));
        }

        let op = match self.peek() {
            Some(Tok::Eq) => CmpOp::Eq,
            Some(Tok::NotEq) => CmpOp::NotEq,
            Some(Tok::Lt) => CmpOp::Lt,
            Some(Tok::LtEq) => CmpOp::LtEq,
            Some(Tok::Gt) => CmpOp::Gt,
            Some(Tok::GtEq) => CmpOp::GtEq,
            Some(Tok::Approx) => {
                return Err(PrismError::Invalid(
                    "`≈≈` may only appear at the top level of the WHERE conjunction, as \
                     `embedding ≈≈ 'text'`. Under an OR it would mean \"similar to this, or \
                     else anything at all\", which is not a semantic query."
                        .into(),
                ))
            }
            _ => return Ok(lhs),
        };
        self.pos += 1;
        let rhs = self.deeper(|p| p.primary())?;
        Ok(Expr::Cmp(Box::new(lhs), op, Box::new(rhs)))
    }

    fn primary(&mut self) -> Result<Expr> {
        if self.eat(Tok::LParen) {
            let e = self.deeper(|p| p.or_expr())?;
            self.expect(Tok::RParen)?;
            return Ok(e);
        }
        match self.peek().cloned() {
            Some(Tok::Ident(name)) => {
                let lower = name.to_ascii_lowercase();
                if lower == "true" {
                    self.pos += 1;
                    return Ok(Expr::Literal(Literal::Bool(true)));
                }
                if lower == "false" {
                    self.pos += 1;
                    return Ok(Expr::Literal(Literal::Bool(false)));
                }
                if lower == "attributes" && self.toks.get(self.pos + 1) == Some(&Tok::LBracket) {
                    self.pos += 1;
                    return Ok(Expr::Attribute(self.attribute_key()?));
                }
                self.pos += 1;
                Ok(Expr::Column(name))
            }
            Some(Tok::Str(_)) | Some(Tok::Int(_)) | Some(Tok::Float(_)) => {
                Ok(Expr::Literal(self.literal()?))
            }
            _ => Err(PrismError::Invalid(format!(
                "expected a column or a literal, found {}",
                self.here()
            ))),
        }
    }

    fn literal(&mut self) -> Result<Literal> {
        match self.next() {
            Some(Tok::Str(s)) => Ok(Literal::Str(s)),
            Some(Tok::Int(i)) => Ok(Literal::Int(i)),
            Some(Tok::Float(f)) => Ok(Literal::Float(f)),
            Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("true") => Ok(Literal::Bool(true)),
            Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("false") => Ok(Literal::Bool(false)),
            other => Err(PrismError::Invalid(format!(
                "expected a literal, found {}",
                other.map(|t| t.describe()).unwrap_or("nothing".into())
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_parses_a_hybrid_query() {
        let s = parse(
            "SELECT event_id, cost FROM events \
             WHERE embedding ≈≈ 'the tool call timed out' AND cost > 0.01 \
             LIMIT 5 WITH (nprobe = 8)",
        )
        .unwrap();
        assert_eq!(s.semantic.unwrap().text, "the tool call timed out");
        assert_eq!(s.limit, Some(5));
        assert_eq!(s.controls.nprobe, Some(8));
        assert!(s.filter.is_some());
    }

    #[test]
    fn deep_nesting_is_refused_and_does_not_blow_the_stack() {
        // Without a depth counter this is a process death, not an error -- and a process
        // death cannot be caught, reported, or attributed to the query that caused it.
        let sql = format!(
            "SELECT * FROM events WHERE {}cost > 1{}",
            "(".repeat(MAX_EXPR_DEPTH + 50),
            ")".repeat(MAX_EXPR_DEPTH + 50)
        );
        let e = parse(&sql).unwrap_err().to_string();
        assert!(e.contains("nests deeper"), "{e}");
    }

    #[test]
    fn an_oversized_in_list_is_refused_by_name() {
        let list: Vec<String> = (0..MAX_IN_LIST + 10).map(|i| i.to_string()).collect();
        let sql = format!("SELECT * FROM events WHERE cost IN ({})", list.join(", "));
        let e = parse(&sql).unwrap_err().to_string();
        assert!(e.contains("IN list"), "{e}");
    }

    #[test]
    fn offset_is_refused_with_the_reason() {
        let e = parse("SELECT * FROM events LIMIT 10 OFFSET 20")
            .unwrap_err()
            .to_string();
        assert!(e.contains("OFFSET is not supported"), "{e}");
        assert!(e.contains("cursor"), "{e}");
    }

    #[test]
    fn a_contradicting_order_by_is_refused_rather_than_ignored() {
        // Silently ignoring ORDER BY would let a caller believe they had asked for an order
        // they did not get.
        assert!(parse("SELECT * FROM events ORDER BY cost DESC").is_err());
        // The one true order parses.
        parse("SELECT * FROM events ORDER BY score DESC, event_id ASC").unwrap();
    }

    #[test]
    fn a_semantic_predicate_under_or_is_refused() {
        let e = parse("SELECT * FROM events WHERE cost > 1 OR embedding ≈≈ 'x'")
            .unwrap_err()
            .to_string();
        assert!(e.contains("top level"), "{e}");
    }

    #[test]
    fn two_semantic_predicates_are_refused() {
        assert!(parse("SELECT * FROM events WHERE embedding ≈≈ 'a' AND embedding ≈≈ 'b'").is_err());
    }

    #[test]
    fn only_the_events_table_exists() {
        assert!(parse("SELECT * FROM users").is_err());
    }

    #[test]
    fn attributes_are_addressable() {
        let s =
            parse("SELECT * FROM events WHERE attributes['gen_ai.system'] = 'anthropic'").unwrap();
        match s.filter.unwrap() {
            Expr::Cmp(l, CmpOp::Eq, _) => assert_eq!(*l, Expr::Attribute("gen_ai.system".into())),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn aggregates_and_group_by_parse() {
        let s = parse("SELECT event_name, count(*), avg(cost) FROM events GROUP BY event_name")
            .unwrap();
        assert_eq!(s.group_by, vec!["event_name"]);
        assert!(matches!(s.items[1], Item::Agg(Agg::CountStar)));
    }
}
