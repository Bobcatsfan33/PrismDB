//! **Bounded, idempotent hedging** for cross-shard queries (S12 §21, [D-079](../../../docs/DECISIONS.md)).
//!
//! A slow shard can be **hedged** — its fragment re-issued — so the query's tail latency is not held
//! hostage by one straggler. This is *free of correctness risk* for exactly one reason: a fragment
//! executes against the **pinned snapshot vector** ([query §19](../../../docs/QUERY-CONTRACT.md)), so
//! the same fragment computed twice is **byte-identical**, and deduplicating the winner from the loser
//! is trivial — pick either. Hedging therefore changes latency and never the answer.
//!
//! All four constants are **load-bearing**: the fan-out cap and the in-flight cap bound every
//! coordinator's re-execution and blast radius, and the two timing constants (`HEDGE_DELAY_MS`,
//! `HEDGE_DEDUP_WINDOW_MS`) drive the **remote** coordinator's real races ([D-098](../../../docs/DECISIONS.md),
//! the per-query hedged client in `shard_rpc`), where a fragment has genuine transport latency.
//! They remain inert only in the synchronous in-process coordinator, which has no latency to race
//! and exercises the same semantics through its seam.

/// How long a fragment may run before it is hedged to a second issue. **Policy** — the tail-latency
/// threshold: long enough that hedging is rare (a fragment that beats it is never hedged, so hedging
/// adds no load in the common case), short enough to cut a real straggler. The remote coordinator
/// honours it for real (a fragment slower than this races a hedge on a fresh connection); the
/// synchronous in-process coordinator has no latency to wait on and leaves it inert.
pub const HEDGE_DELAY_MS: i64 = 50;

/// The most hedges a single fragment may spawn. **Policy** — one hedge cuts the tail without turning
/// every slow fragment into a fan-out storm; more issues have sharply diminishing returns and multiply
/// load exactly when the cluster is already struggling.
pub const HEDGE_FANOUT: usize = 1;

/// How long the coordinator holds a fragment's slot open to absorb a duplicate (hedged) response
/// before discarding it. **Policy** — the remote coordinator honours it for real: a duplicate landing
/// inside the window is compared to the winner bit-for-bit and absorbed; one landing later is
/// discarded as late. The in-process coordinator's identity dedup needs no window and leaves it inert.
pub const HEDGE_DEDUP_WINDOW_MS: i64 = 200;

/// The **blast-radius cap**: the most fragments — originals plus hedges — the coordinator will have in
/// flight for one query at once. **Policy** — a slow cluster must not hedge itself into collapse, so a
/// hedge is issued only while the total stays under this bound; past it, the query waits on the
/// originals rather than amplifying load during a degradation. Comfortably above a healthy query's
/// fan-out (one fragment per shard), small enough to cap the amplification.
pub const MAX_INFLIGHT_FRAGMENTS: usize = 32;
