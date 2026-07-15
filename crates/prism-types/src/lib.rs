//! Core types shared by every PrismDB crate.
//!
//! Contract: `docs/PRISM.md`. This crate owns the logical event model
//! (Part III §9), the boundary validation rules (Part III §10), the `Embedder`
//! trait, and the primitives that durability and content-addressing depend on
//! (CRC-32, SHA-256, a deterministic PRNG) so that none of them come from a
//! third party.

pub mod attributes;
pub mod embed;
pub mod error;
pub mod event;
pub mod half;
pub mod hash;
pub mod limits;
pub mod predicate;
pub mod query;
pub mod rng;
pub mod vector;

pub use attributes::{AttrValue, Attributes};
pub use embed::{Embedder, HashEmbedder, MAX_EMBED_INPUT_BYTES};
pub use error::{PrismError, Result};
pub use event::{DeadLetter, Event, MAX_BODY_BYTES};
pub use limits::{Quota, RejectReason};
pub use predicate::{CmpOp, Literal, Predicate, RowSource, Value};
pub use query::{ClusterSummary, Counters, Explain, Hit, Query, SearchResult};
pub use vector::validate_and_normalize;
