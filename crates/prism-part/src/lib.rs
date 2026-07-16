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

pub mod baseline;
pub mod catalog;
pub mod disk;
pub mod ext;
pub mod faults;
pub mod format;
pub mod fsck;
pub mod generation;
pub mod io;
pub mod legacy_v1;
pub mod mmap;
pub mod part;
pub mod partition;
pub mod store;

pub use catalog::{Catalog, PartEntry, Snapshot};
pub use ext::{PromotedColumn, S4Ext, TenantStats};
pub use format::{RerankDescriptor, FORMAT_VERSION, LEGACY_FORMAT_VERSION};
pub use generation::Generation;
pub use part::{CentroidRange, ColumnMeta, PartManifest, PartReader, PartRows, PartWriter};
pub use partition::{Bucket, PartRef, PartitionKey, PartitionScheme};
pub use store::{Store, StoreConfig};
