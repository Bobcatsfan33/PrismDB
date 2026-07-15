//! The Flight SQL door (S8) — a third door, the same door.
//!
//! Arrow Flight SQL is the third way a query reaches the engine, after the direct API and the SQL
//! text door ([query contract](../../../docs/QUERY-CONTRACT.md) §5). Directive 6's rules are §5's,
//! one door further: the tenant conjunction is injected **below** it, its counters are
//! **byte-identical** to the other doors on every query, and its decode obeys S1's bounded-
//! allocation discipline — every length capped, every violation a named error.
//!
//! **What ships and what does not (honest, like S7's GPU).** Real Arrow Flight SQL is a gRPC
//! service speaking Arrow IPC, which needs the `arrow` / `tonic` / `prost` ecosystem — a
//! dependency tree the charter's serde-only rule ([D-002](../../../docs/DECISIONS.md)) excludes,
//! and a network server the roadmap defers to S14. So this module is the door's **server-side
//! query path** — decode-bound, bind-below-tenant, same `Plan`, same executor — and *not* the
//! Arrow IPC / gRPC transport. That transport is S14's network layer, and taking on the arrow
//! dependency is a decision for the sprint that needs it. Building the wire format now, untested
//! against a real Flight client, would be exactly the faked completeness the project refuses.
//!
//! What *is* proven here is the property that matters for correctness: a Flight SQL query is the
//! **same door** as the other two — identical answer, identical counters, tenant injected below,
//! and a decode that cannot be made to allocate on a length a stranger sent.

use crate::engine::Engine;
use crate::sql::SqlResult;
use prism_sql::{compile, Session};
use prism_types::error::{PrismError, Result};

/// Caps on a decoded Flight SQL request. The wire is bytes from a stranger, so every length is
/// bounded before anything is allocated on it — S1's discipline, on the new decode path.
pub mod limits {
    /// Max bytes in one Flight SQL command message. A `CommandStatementQuery` is a query string
    /// and a little framing; megabytes of it is an attack, not a query.
    pub const MAX_MESSAGE_BYTES: usize = 256 * 1024;
    /// Max length of the query string it carries. Mirrors the SQL door's statement cap.
    pub const MAX_QUERY_BYTES: usize = 64 * 1024;
    /// Max number of parameters (bind values) a request may carry.
    pub const MAX_PARAMS: usize = 256;
    /// Max bytes in one parameter value.
    pub const MAX_PARAM_BYTES: usize = 4 * 1024;
}

/// A decoded Flight SQL statement request. The in-tree stand-in for `CommandStatementQuery`: the
/// fields a Flight SQL client actually sends, decoded under bounded-allocation discipline.
#[derive(Clone, Debug, PartialEq)]
pub struct FlightSqlRequest {
    pub query: String,
    pub params: Vec<String>,
}

impl FlightSqlRequest {
    /// Decode a request from a length-prefixed message, refusing anything oversized with a **named**
    /// error before it allocates.
    ///
    /// Framing (little-endian, deliberately trivial — this is the decode *discipline*, not the
    /// Arrow wire format): `u32 query_len | query bytes | u16 param_count | (u32 len | bytes)*`.
    /// The point is not the bytes; it is that **no length is trusted**: each is checked against the
    /// message actually present, and against its cap, before a byte is reserved.
    pub fn decode(msg: &[u8]) -> Result<FlightSqlRequest> {
        if msg.len() > limits::MAX_MESSAGE_BYTES {
            return Err(PrismError::Invalid(format!(
                "Flight SQL message is {} bytes, over the {} cap",
                msg.len(),
                limits::MAX_MESSAGE_BYTES
            )));
        }
        let mut c = Reader { buf: msg, pos: 0 };

        let query_len = c.u32("query length")? as usize;
        if query_len > limits::MAX_QUERY_BYTES {
            return Err(PrismError::Invalid(format!(
                "Flight SQL query is {query_len} bytes, over the {} cap",
                limits::MAX_QUERY_BYTES
            )));
        }
        let query = c.string(query_len, "query")?;

        let param_count = c.u16("parameter count")? as usize;
        if param_count > limits::MAX_PARAMS {
            return Err(PrismError::Invalid(format!(
                "Flight SQL request declares {param_count} parameters, over the {} cap",
                limits::MAX_PARAMS
            )));
        }
        // Reserve on the DECLARED count only after it passed its cap -- never on the raw number.
        let mut params = Vec::with_capacity(param_count);
        for i in 0..param_count {
            let len = c.u32("parameter length")? as usize;
            if len > limits::MAX_PARAM_BYTES {
                return Err(PrismError::Invalid(format!(
                    "Flight SQL parameter {i} is {len} bytes, over the {} cap",
                    limits::MAX_PARAM_BYTES
                )));
            }
            params.push(c.string(len, "parameter")?);
        }
        Ok(FlightSqlRequest { query, params })
    }

    /// Encode, for tests and a future client. The inverse of `decode`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.query.len() as u32).to_le_bytes());
        out.extend_from_slice(self.query.as_bytes());
        out.extend_from_slice(&(self.params.len() as u16).to_le_bytes());
        for p in &self.params {
            out.extend_from_slice(&(p.len() as u32).to_le_bytes());
            out.extend_from_slice(p.as_bytes());
        }
        out
    }
}

/// A bytes reader that never trusts a length: every read is checked against the bytes present.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize, what: &str) -> Result<&[u8]> {
        let end = self.pos.checked_add(n).ok_or_else(|| {
            PrismError::Invalid(format!("Flight SQL {what}: length overflows the message"))
        })?;
        if end > self.buf.len() {
            return Err(PrismError::Invalid(format!(
                "Flight SQL {what}: needs {n} bytes at offset {}, but the message has only {}",
                self.pos,
                self.buf.len() - self.pos.min(self.buf.len())
            )));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u32(&mut self, what: &str) -> Result<u32> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u16(&mut self, what: &str) -> Result<u16> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn string(&mut self, n: usize, what: &str) -> Result<String> {
        let b = self.take(n, what)?;
        String::from_utf8(b.to_vec())
            .map_err(|_| PrismError::Invalid(format!("Flight SQL {what} is not valid UTF-8")))
    }
}

impl Engine {
    /// Answer a Flight SQL request. **The same door**: the message is decoded under bounds, the
    /// query compiled with the session tenant injected **below** the statement, and run through the
    /// same executor the other two doors use. The counters are byte-identical to the direct API and
    /// the SQL text door on every query — the "same door" property, now three-way.
    pub fn run_flight_sql(&self, msg: &[u8], tenant: &str) -> Result<SqlResult> {
        let req = FlightSqlRequest::decode(msg)?;
        // Parameters are not yet part of the semantic grammar; a request that sends them is
        // refused rather than silently ignored (an ignored bind value is a wrong answer waiting).
        if !req.params.is_empty() {
            return Err(PrismError::Invalid(
                "Flight SQL bind parameters are not yet supported; inline the values".into(),
            ));
        }
        // Tenant is injected BELOW the statement, exactly as the SQL door does -- the query cannot
        // name, alias, or escape it (query contract §6).
        let session = Session {
            tenant: tenant.to_string(),
        };
        let plan = compile(&req.query, &session)?;
        self.run_sql(&plan, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips() {
        let r = FlightSqlRequest {
            query: "SELECT event_id FROM events".into(),
            params: vec!["a".into(), "bb".into()],
        };
        assert_eq!(FlightSqlRequest::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn every_length_is_bounded_and_named() {
        // A query length larger than the cap is refused, named.
        let mut msg = Vec::new();
        msg.extend_from_slice(&(u32::MAX).to_le_bytes()); // absurd query length
        let e = FlightSqlRequest::decode(&msg).unwrap_err().to_string();
        assert!(e.contains("query is") && e.contains("cap"), "{e}");

        // A truncated message (claims more bytes than present) is refused, named.
        let mut msg = Vec::new();
        msg.extend_from_slice(&(100u32).to_le_bytes()); // says 100 query bytes
        msg.extend_from_slice(b"short"); // has 5
        let e = FlightSqlRequest::decode(&msg).unwrap_err().to_string();
        assert!(e.contains("query") && e.contains("message has only"), "{e}");

        // A param count over the cap is refused before it reserves.
        let mut msg = Vec::new();
        msg.extend_from_slice(&(2u32).to_le_bytes());
        msg.extend_from_slice(b"ab");
        msg.extend_from_slice(&(u16::MAX).to_le_bytes()); // absurd param count
        let e = FlightSqlRequest::decode(&msg).unwrap_err().to_string();
        assert!(e.contains("parameters") && e.contains("cap"), "{e}");
    }

    #[test]
    fn an_oversized_message_is_refused_whole() {
        let big = vec![0u8; limits::MAX_MESSAGE_BYTES + 1];
        assert!(FlightSqlRequest::decode(&big).is_err());
    }

    #[test]
    fn garbage_bytes_decode_or_refuse_but_never_panic() {
        // The wire is a stranger's bytes. Every decode either succeeds or returns a named error --
        // never a panic, never an allocation on an untrusted length. S1's discipline, on the new
        // decode path (directive 6).
        let mut x = 0x1234_5678u32;
        for len in 0..600usize {
            let mut msg = vec![0u8; len];
            for b in msg.iter_mut() {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *b = (x >> 16) as u8;
            }
            // Must not panic; the result is decode-or-error, and we only care that it returned.
            let _ = FlightSqlRequest::decode(&msg);
        }
        // Every truncation of a valid message also decodes-or-refuses.
        let good = FlightSqlRequest {
            query: "SELECT event_id FROM events WHERE a = 'b'".into(),
            params: vec!["p".into()],
        }
        .encode();
        for cut in 0..=good.len() {
            let _ = FlightSqlRequest::decode(&good[..cut]);
        }
    }
}
