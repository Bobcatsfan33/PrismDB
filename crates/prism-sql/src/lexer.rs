//! The lexer. Bounded, and it never trusts a length.

use crate::limits::{MAX_STATEMENT_BYTES, MAX_TOKENS};
use prism_types::error::{PrismError, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),

    // punctuation
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Star,
    Dot,

    // operators
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    /// The semantic-similarity operator. `≈≈`, with the ASCII alias `~~` for the sake of
    /// anyone who has to type it into a shell.
    Approx,
}

impl Tok {
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident(s) => format!("identifier `{s}`"),
            Tok::Str(_) => "string literal".into(),
            Tok::Int(i) => format!("number {i}"),
            Tok::Float(f) => format!("number {f}"),
            Tok::LParen => "`(`".into(),
            Tok::RParen => "`)`".into(),
            Tok::LBracket => "`[`".into(),
            Tok::RBracket => "`]`".into(),
            Tok::Comma => "`,`".into(),
            Tok::Star => "`*`".into(),
            Tok::Dot => "`.`".into(),
            Tok::Eq => "`=`".into(),
            Tok::NotEq => "`<>`".into(),
            Tok::Lt => "`<`".into(),
            Tok::LtEq => "`<=`".into(),
            Tok::Gt => "`>`".into(),
            Tok::GtEq => "`>=`".into(),
            Tok::Approx => "`≈≈`".into(),
        }
    }
}

pub fn lex(sql: &str) -> Result<Vec<Tok>> {
    if sql.len() > MAX_STATEMENT_BYTES {
        return Err(PrismError::Invalid(format!(
            "statement is {} bytes, over the {MAX_STATEMENT_BYTES}-byte limit",
            sql.len()
        )));
    }

    let cs: Vec<char> = sql.chars().collect();
    let mut i = 0usize;
    let mut out: Vec<Tok> = Vec::new();

    let push = |out: &mut Vec<Tok>, t: Tok| -> Result<()> {
        if out.len() >= MAX_TOKENS {
            return Err(PrismError::Invalid(format!(
                "statement has more than {MAX_TOKENS} tokens"
            )));
        }
        out.push(t);
        Ok(())
    };

    while i < cs.len() {
        let c = cs[i];

        // whitespace
        if c.is_whitespace() {
            i += 1;
            continue;
        }

        // -- line comment
        if c == '-' && i + 1 < cs.len() && cs[i + 1] == '-' {
            while i < cs.len() && cs[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // /* block comment */ — must terminate. An unterminated comment is not a way to
        // hide the rest of a statement from the parser; it is an error.
        if c == '/' && i + 1 < cs.len() && cs[i + 1] == '*' {
            let start = i;
            i += 2;
            let mut closed = false;
            while i + 1 < cs.len() {
                if cs[i] == '*' && cs[i + 1] == '/' {
                    i += 2;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return Err(PrismError::Invalid(format!(
                    "unterminated block comment starting at character {start}"
                )));
            }
            continue;
        }

        // 'string literal', with '' as an escaped quote
        if c == '\'' {
            i += 1;
            let mut s = String::new();
            let mut closed = false;
            while i < cs.len() {
                if cs[i] == '\'' {
                    if i + 1 < cs.len() && cs[i + 1] == '\'' {
                        s.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    closed = true;
                    break;
                }
                s.push(cs[i]);
                i += 1;
            }
            if !closed {
                return Err(PrismError::Invalid("unterminated string literal".into()));
            }
            push(&mut out, Tok::Str(s))?;
            continue;
        }

        // number
        if c.is_ascii_digit() || (c == '-' && i + 1 < cs.len() && cs[i + 1].is_ascii_digit()) {
            let start = i;
            if c == '-' {
                i += 1;
            }
            let mut float = false;
            while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
                if cs[i] == '.' {
                    // A second dot is not part of the number.
                    if float {
                        break;
                    }
                    float = true;
                }
                i += 1;
            }
            let text: String = cs[start..i].iter().collect();
            if float {
                let f: f64 = text
                    .parse()
                    .map_err(|_| PrismError::Invalid(format!("`{text}` is not a valid number")))?;
                if !f.is_finite() {
                    return Err(PrismError::Invalid(format!("`{text}` is not finite")));
                }
                push(&mut out, Tok::Float(f))?;
            } else {
                let n: i64 = text.parse().map_err(|_| {
                    PrismError::Invalid(format!("`{text}` does not fit in a 64-bit integer"))
                })?;
                push(&mut out, Tok::Int(n))?;
            }
            continue;
        }

        // identifier / keyword
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_' || cs[i] == '.') {
                i += 1;
            }
            let text: String = cs[start..i].iter().collect();
            push(&mut out, Tok::Ident(text))?;
            continue;
        }

        // the similarity operator, in both spellings
        if c == '≈' {
            if i + 1 < cs.len() && cs[i + 1] == '≈' {
                push(&mut out, Tok::Approx)?;
                i += 2;
                continue;
            }
            return Err(PrismError::Invalid(
                "`≈` is not an operator; the similarity operator is `≈≈` (or `~~`)".into(),
            ));
        }
        if c == '~' && i + 1 < cs.len() && cs[i + 1] == '~' {
            push(&mut out, Tok::Approx)?;
            i += 2;
            continue;
        }

        // multi-char operators
        if c == '<' && i + 1 < cs.len() && cs[i + 1] == '=' {
            push(&mut out, Tok::LtEq)?;
            i += 2;
            continue;
        }
        if c == '<' && i + 1 < cs.len() && cs[i + 1] == '>' {
            push(&mut out, Tok::NotEq)?;
            i += 2;
            continue;
        }
        if c == '!' && i + 1 < cs.len() && cs[i + 1] == '=' {
            push(&mut out, Tok::NotEq)?;
            i += 2;
            continue;
        }
        if c == '>' && i + 1 < cs.len() && cs[i + 1] == '=' {
            push(&mut out, Tok::GtEq)?;
            i += 2;
            continue;
        }

        let t = match c {
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            '[' => Tok::LBracket,
            ']' => Tok::RBracket,
            ',' => Tok::Comma,
            '*' => Tok::Star,
            '=' => Tok::Eq,
            '<' => Tok::Lt,
            '>' => Tok::Gt,
            ';' => {
                // A trailing semicolon is fine. A second statement is not: we execute one
                // statement, and quietly ignoring the rest of the string is how a
                // stacked-query injection becomes invisible.
                i += 1;
                while i < cs.len() && cs[i].is_whitespace() {
                    i += 1;
                }
                if i < cs.len() {
                    return Err(PrismError::Invalid(
                        "more than one statement; PrismDB executes exactly one".into(),
                    ));
                }
                break;
            }
            other => {
                return Err(PrismError::Invalid(format!(
                    "unexpected character `{other}`"
                )))
            }
        };
        push(&mut out, t)?;
        i += 1;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_lexes_a_hybrid_query() {
        let t = lex("SELECT event_id FROM events WHERE embedding ≈≈ 'a failure' AND cost > 0.5")
            .unwrap();
        assert!(t.contains(&Tok::Approx));
        assert!(t.contains(&Tok::Str("a failure".into())));
        assert!(t.contains(&Tok::Float(0.5)));
    }

    #[test]
    fn the_ascii_alias_lexes_the_same() {
        let a = lex("x ≈≈ 'q'").unwrap();
        let b = lex("x ~~ 'q'").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn an_oversized_statement_is_refused_by_name() {
        let sql = "SELECT ".to_string() + &"a,".repeat(MAX_STATEMENT_BYTES);
        let e = lex(&sql).unwrap_err().to_string();
        assert!(e.contains("over the"), "{e}");
    }

    #[test]
    fn too_many_tokens_is_refused_by_name() {
        let sql = "(".repeat(MAX_TOKENS + 10);
        let e = lex(&sql).unwrap_err().to_string();
        assert!(e.contains("tokens"), "{e}");
    }

    #[test]
    fn a_second_statement_is_refused_rather_than_ignored() {
        // Quietly parsing the first statement and dropping the rest is how a stacked-query
        // injection becomes invisible.
        let e = lex("SELECT 1; DROP TABLE events").unwrap_err().to_string();
        assert!(e.contains("exactly one"), "{e}");
        // A trailing semicolon on its own is fine.
        lex("SELECT event_id FROM events;").unwrap();
    }

    #[test]
    fn an_unterminated_string_or_comment_is_an_error() {
        assert!(lex("SELECT 'abc").is_err());
        assert!(lex("SELECT /* abc").is_err());
    }

    #[test]
    fn comments_do_not_smuggle_tokens_through() {
        let t = lex("SELECT event_id -- AND tenant_id = 'other'\nFROM events").unwrap();
        assert!(!t.iter().any(|x| matches!(x, Tok::Str(s) if s == "other")));
    }

    #[test]
    fn quotes_escape_by_doubling() {
        let t = lex("'it''s'").unwrap();
        assert_eq!(t, vec![Tok::Str("it's".into())]);
    }
}
