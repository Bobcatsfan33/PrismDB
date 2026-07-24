//! Object storage, the local cache, and the two-tier cost model (S11).
//!
//! The cold tier (exact rerank vectors, bodies) lives on object storage; a local cache sits in
//! front of it; and every query carries the two-tier bill. See [storage contract](../../../docs/STORAGE-CONTRACT.md).
//! This module starts with the **cost model** — the thing that makes the bill a number — and grows
//! to hold the `ObjectStore` trait, its backends, and the cache.

/// The estimated cost of one cold-tier object request, in micro-units.
///
/// **Policy** ([C-3](../../../docs/DECISIONS.md)), **backend-conditional** (storage contract §5): an
/// object request has a fixed per-call cost independent of its size — the round trip, the auth, the
/// request charge. This is the local-object-store figure (a syscall's worth); an S3-over-WAN request
/// is orders of magnitude more, and a receipt measured against one backend is not the number for the
/// other. Named here so `EXPLAIN`'s per-query cost is a consistent unit, re-derived per backend.
pub const OBJECT_REQUEST_COST_MICROS: u64 = 100;

/// The estimated cost of retrieving one cold-tier byte, in **pico**-units.
///
/// **Policy** ([C-3](../../../docs/DECISIONS.md)), **backend-conditional** (storage contract §5): the
/// per-byte egress/transfer cost. Local object storage is near-free per byte; S3-over-WAN egress is
/// the dominant term of a real bill. The unit is pico so a megabyte is a meaningful integer and the
/// two backends' figures stay distinguishable.
pub const RETRIEVED_BYTE_COST_PICOS: u64 = 1;

/// A query's estimated cold-tier cost, in micro-units: `requests × request-cost + bytes ×
/// byte-cost`. The two-tier economics as a single number on every `EXPLAIN`.
pub fn estimated_cost_micros(object_requests: usize, retrieved_bytes: usize) -> u64 {
    (object_requests as u64)
        .saturating_mul(OBJECT_REQUEST_COST_MICROS)
        .saturating_add(
            (retrieved_bytes as u64)
                .saturating_mul(RETRIEVED_BYTE_COST_PICOS)
                .saturating_div(1_000_000),
        )
}

/// The default local cache byte quota.
///
/// **Policy** ([C-3](../../../docs/DECISIONS.md)): the NVMe block cache is a bounded, disposable
/// optimization — it is sized, not unbounded, so it cannot itself become the thing that fills the
/// disk. A full cache evicts under pressure and, at the hard limit, reports the S10 `OutOfSpace`
/// backpressure rather than growing without bound. 256 MiB is a working default; the real size is a
/// deployment knob. Backend-conditional numbers ride on it (storage contract §5).
pub const CACHE_QUOTA_BYTES: usize = 256 * 1024 * 1024;

/// How many times a transient cold-tier fetch (a 5xx, a dropped connection) is retried before the
/// query fails with the named remote condition.
///
/// **Policy** ([C-3](../../../docs/DECISIONS.md)), backend-conditional: a remote read hiccups; a
/// small bounded retry turns a transient blip into the correct answer, and the *bound* turns a
/// dead remote into a **named** failure rather than an unbounded hang (storage contract §4). Three
/// is enough to ride out a blip and few enough that a genuinely-down remote is named quickly. A
/// truncated/corrupt read is *not* retried — it is a named byte error at once.
pub const COLD_FETCH_MAX_RETRIES: usize = 3;

/// The object size at or above which an upload uses **multipart** instead of a single `PUT`.
///
/// **Policy** ([C-3](../../../docs/DECISIONS.md)), backend-conditional: a single `PUT` is fine up to
/// the backend's limit, but a large object wants multipart so a mid-upload crash leaves resumable
/// server-side parts that GC's list-and-abort sweep reclaims, rather than re-sent whole. This is both
/// the threshold and the part size (16 MiB > S3's 5 MiB minimum); a cold-tier part's `rerank.vec`
/// crosses it at scale. The multipart client path (initiate/part/complete/abort) and the GC sweep are
/// implemented (S11 boundary d); this constant is where they engage.
pub const MULTIPART_THRESHOLD_BYTES: usize = 16 * 1024 * 1024;

pub mod cold;
pub mod mirror;
pub mod object;
pub mod ownership;
pub mod s3;
pub mod sigv4;
pub use object::{
    BlockCache, CacheStats, CachedObjectStore, FaultConfig, FaultStore, LocalObjectStore,
    ObjectStore,
};
