//! **Bounded, idempotent hedging** for cross-shard queries (S12 §21, [D-079](../../../docs/DECISIONS.md)).
//!
//! A slow shard can be **hedged** — its fragment re-issued — so the query's tail latency is not held
//! hostage by one straggler. This is *free of correctness risk* for exactly one reason: a fragment
//! executes against the **pinned snapshot vector** ([query §19](../../../docs/QUERY-CONTRACT.md)), so
//! the same fragment computed twice is **byte-identical**, and deduplicating the winner from the loser
//! is trivial — pick either. Hedging therefore changes latency and never the answer.
//!
//! Two of these constants are **load-bearing now** (the fan-out cap and the in-flight cap bound the
//! synchronous coordinator's re-execution and its blast radius); two describe the **timing** a real
//! asynchronous transport will use (`HEDGE_DELAY_MS`, `HEDGE_DEDUP_WINDOW_MS`) and are inert in the
//! synchronous coordinator, which has no latency to race — the honest-wall pattern (the idempotence,
//! dedup, and blast-radius *semantics* ship and are gated; the timing lands with the transport).

/// How long a fragment may run before it is hedged to a second issue. **Policy** — the tail-latency
/// threshold: long enough that hedging is rare (a fragment that beats it is never hedged, so hedging
/// adds no load in the common case), short enough to cut a real straggler. Inert in the synchronous
/// coordinator (there is no latency to wait on); the asynchronous transport will honour it.
pub const HEDGE_DELAY_MS: i64 = 50;

/// The most hedges a single fragment may spawn. **Policy** — one hedge cuts the tail without turning
/// every slow fragment into a fan-out storm; more issues have sharply diminishing returns and multiply
/// load exactly when the cluster is already struggling.
pub const HEDGE_FANOUT: usize = 1;

/// How long the coordinator holds a fragment's slot open to absorb a duplicate (hedged) response
/// before discarding it. **Policy** — inert here (the pinned vector makes duplicates byte-identical, so
/// dedup is by identity and needs no window); the asynchronous transport uses it to bound how long a
/// late duplicate is still recognised as a duplicate rather than treated as a new fragment.
pub const HEDGE_DEDUP_WINDOW_MS: i64 = 200;

/// The **blast-radius cap**: the most fragments — originals plus hedges — the coordinator will have in
/// flight for one query at once. **Policy** — a slow cluster must not hedge itself into collapse, so a
/// hedge is issued only while the total stays under this bound; past it, the query waits on the
/// originals rather than amplifying load during a degradation. Comfortably above a healthy query's
/// fan-out (one fragment per shard), small enough to cap the amplification.
pub const MAX_INFLIGHT_FRAGMENTS: usize = 32;
