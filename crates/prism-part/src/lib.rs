//! Storage: immutable parts, content-addressed generations, atomic catalog.
//!
//! The five rules this crate exists to enforce (Part II §7):
//!
//! 1. A published part is never mutated. Not compacted in place, not appended
//!    to, not fixed up. Change means a new part.
//! 2. `CURRENT` only ever names a snapshot whose parts are fully durable.
//! 3. Publication is one atomic rename.
//! 4. GC is a separate operation and never runs inside the publish path.
//! 5. Codebooks and models are content-addressed; a part pins the generation it
//!    was written under, and its bytes mean nothing without it.

pub mod catalog;
pub mod faults;
pub mod generation;
pub mod io;
pub mod part;
pub mod store;

pub use catalog::{Catalog, Snapshot};
pub use generation::Generation;
pub use part::{CentroidRange, ColumnMeta, PartManifest, PartReader, PartRows, PartWriter};
pub use store::{Store, StoreConfig, FORMAT_VERSION};
