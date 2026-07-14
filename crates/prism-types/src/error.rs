use std::fmt;

/// Every failure mode PrismDB distinguishes.
///
/// `Corrupt` is deliberately separate from `Io`: the S1 gate requires that
/// damaged bytes are rejected with a *specific* error rather than surfacing as
/// a generic read failure, and the compat fixtures assert on that distinction.
#[derive(Debug)]
pub enum PrismError {
    /// The filesystem said no.
    Io(String),
    /// Stored bytes failed validation: checksum mismatch, truncation,
    /// impossible length, unknown format version.
    Corrupt(String),
    /// Data rejected at the admission boundary (Part III §10).
    Invalid(String),
    /// The requested object is not in the catalog.
    NotFound(String),
    /// A JSON manifest / snapshot / generation record would not parse.
    Decode(String),
    /// Refused because it would violate a consistency invariant (Part II §7).
    Invariant(String),
}

impl fmt::Display for PrismError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrismError::Io(m) => write!(f, "io error: {m}"),
            PrismError::Corrupt(m) => write!(f, "corrupt data: {m}"),
            PrismError::Invalid(m) => write!(f, "invalid input: {m}"),
            PrismError::NotFound(m) => write!(f, "not found: {m}"),
            PrismError::Decode(m) => write!(f, "decode error: {m}"),
            PrismError::Invariant(m) => write!(f, "invariant violation: {m}"),
        }
    }
}

impl std::error::Error for PrismError {}

impl From<std::io::Error> for PrismError {
    fn from(e: std::io::Error) -> Self {
        PrismError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for PrismError {
    fn from(e: serde_json::Error) -> Self {
        PrismError::Decode(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, PrismError>;
