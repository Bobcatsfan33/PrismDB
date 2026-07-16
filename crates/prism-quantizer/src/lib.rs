//! The compression codec and the sparse index of meaning.
//!
//! Two structures, both trained offline and both immutable once published:
//!
//! * [`CoarseCodebook`] — the IVF centroids. Tiny, always resident. A query
//!   scores every centroid and probes only the nearest `nprobe`. This is the
//!   semantic twin of a sparse mark index: it is what lets us not look at
//!   almost everything.
//! * [`PqCodebook`] — product quantization. The vector is split into `m`
//!   sub-vectors; each sub-vector is replaced by the id of the nearest of 256
//!   learned codewords. A 64-d float32 vector (256 B) becomes 8 B. Distance is
//!   then `m` table lookups and adds ([`AdcTable`]).
//!
//! PQ distances are approximate *by construction*. They never reach the
//! surface: they decide who gets reranked, and exact vectors decide the answer.

pub mod kernel;
pub mod kmeans;
pub mod pq;

pub use kernel::{adc_scan, Isa};
pub use kmeans::{
    kmeans, kmeans_minibatch, kmeans_plusplus_init, nearest_centroid, CoarseCodebook,
};
pub use pq::{AdcTable, PqCodebook, PQ_KSUB};
