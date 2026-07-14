//! Bounds on untrusted SQL text (S3).
//!
//! **The parser is network-facing input.** The SQL string is now the same category of
//! thing as a part file: bytes from a stranger. S1's discipline applies in full — nothing
//! allocates on an untrusted length, nothing recurses without a depth counter, and every
//! bound is *named in its error*, because "syntax error" is not something an operator can
//! act on.
//!
//! These are **policy** constants (charter C-1): not measured optima, decisions about what
//! we are willing to parse. The rationale is [`docs/QUERY-CONTRACT.md`](../../../docs/QUERY-CONTRACT.md) §7.

/// Bytes in one statement.
pub const MAX_STATEMENT_BYTES: usize = 64 * 1024;

/// Tokens in one statement. Bounds the lexer independently of the byte cap, because a
/// 64 KiB statement of single-character tokens is a different attack from one long string.
pub const MAX_TOKENS: usize = 4_096;

/// Expression nesting depth.
///
/// The one that matters most. A recursive-descent parser without a depth counter is a
/// stack overflow waiting for `((((((...))))))`, and a stack overflow is a process death,
/// not an error — it cannot be caught, reported, or attributed.
pub const MAX_EXPR_DEPTH: usize = 32;

/// Elements in an `IN (...)` list.
pub const MAX_IN_LIST: usize = 1_024;

/// Projected expressions in a `SELECT`.
pub const MAX_PROJECTIONS: usize = 64;

/// Keys in a `GROUP BY`.
pub const MAX_GROUP_KEYS: usize = 16;
