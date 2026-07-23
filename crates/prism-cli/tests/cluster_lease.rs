//! **S12 increment 3, item 1: the distributed reader lease, gated single-node-style** ([D-075](../../../docs/DECISIONS.md),
//! [query §19](../../../docs/QUERY-CONTRACT.md), [merge §5](../../../docs/MERGE-CONTRACT.md)).
//!
//! A cross-shard paginated query pins **one snapshot per shard** and carries them in its cursor. The
//! distributed lease is the **conjunction of per-shard leases**: each pinned snapshot is protected on
//! its own shard for `LEASE_TTL_MS` plus its derived grace, so a reader within its lease finds every
//! shard's parts, and a reader that crashes (which is the same thing to the server as one that
//! paginated too long — the lease is time-bounded, not connection-held) has its snapshots age out per
//! shard and reclaimed, its stale cursor then getting the explicit expired-snapshot error.
//!
//! This is the gate the architect asked for *first*, before the chaos harness: kill a reader
//! mid-pagination at 2 and 4 shards and prove both halves — within-lease survives GC, past-horizon
//! expires by name and GC proceeds. GC uses the monotonic lease clock, driven here by injected time.

use prism_engine::sharded::Cluster;
use prism_part::catalog::GC_HORIZON_MS;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Query;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!(
        "prism-cl-lease-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn config() -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 64,
        nlist: 16,
        pq_m: 8,
        seed: 9,
        kmeans_restarts: 1,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        partitions: Default::default(),
        promote: Vec::new(),
    }
}

const TS: i64 = 1_760_000_000_000;

/// A cross-tenant query paginated 5 at a time, so page 1 pins the vector and returns a live cursor.
fn page_query() -> Query {
    Query {
        text: "the tool call timed out retrying".into(),
        tenant: None,
        k: 5,
        rerank: 60,
        nprobe: 8,
        candidates: 200,
        ..Default::default()
    }
}

#[test]
fn a_reader_killed_mid_pagination_expires_per_shard_and_gc_proceeds() {
    for n in [2usize, 4] {
        let cluster = Cluster::init(&tmp(&format!("kill-{n}")), n, config()).unwrap();
        cluster
            .ingest(
                prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 3000, 5),
                TS,
            )
            .unwrap();

        // The reader starts paginating: page 1 pins the snapshot vector (one snapshot per shard, at
        // TS) and hands back a cursor for the next page.
        let (_page1, cursor) = cluster.search_page(&page_query(), None).unwrap();
        let cursor =
            cursor.expect("a 60-survivor result paged 5 at a time must have a second page");

        // ...then the reader goes away, while the cluster keeps ingesting — every churn batch
        // publishes new snapshots that supersede the pinned ones across the shards.
        for i in 0..4 {
            cluster
                .ingest(
                    prism_engine::corpus::generate(
                        prism_engine::corpus::Kind::Uniform,
                        60,
                        100 + i,
                    ),
                    TS + 1 + i as i64,
                )
                .unwrap();
        }

        // WITHIN the lease horizon: even at retain = 1, every shard's pinned snapshot is time-
        // protected, so the reader's cursor still resolves. This is invariant 6, by construction,
        // holding across the whole vector.
        cluster.gc_at(1, TS + GC_HORIZON_MS - 1, false).unwrap();
        assert!(
            cluster.search_page(&page_query(), Some(&cursor)).is_ok(),
            "{n}-shard: GC reclaimed a snapshot a reader was still within its lease on — the \
             distributed lease failed to protect the pinned vector (invariant 6)"
        );

        // PAST the horizon: the crashed reader's leases expire, GC reclaims the superseded snapshots
        // cluster-wide (it proceeds — a crashed reader must not pin storage forever), and the stale
        // cursor gets the explicit expired-snapshot error, never a short or wrong answer.
        let reports = cluster.gc_at(1, TS + GC_HORIZON_MS + 10, false).unwrap();
        assert!(
            reports.iter().any(|r| !r.removed_snapshots.is_empty()),
            "{n}-shard: GC reclaimed nothing past the horizon — a crashed reader would pin storage \
             forever"
        );
        let err = cluster
            .search_page(&page_query(), Some(&cursor))
            .expect_err("{n}-shard: a cursor into a reclaimed vector must fail, not silently move");
        let msg = err.to_string();
        assert!(
            msg.contains("expired") && msg.contains("Re-run the query"),
            "{n}-shard: the expired condition must be named: {msg}"
        );
    }
}

/// **The clock-skew property the chaos suite will stress, proved at the cluster level now** (D-075).
/// GC reasons about a **monotonic** lease clock, never the wall clock, so a node whose wall clock
/// skews ±30d neither expires a live reader's lease early nor keeps a crashed reader's forever. This
/// drives the lease clock by hand (as a chaos run's cross-process skew would) and contrasts it with
/// what a naive wall-clock horizon *would* do at the skewed reading — the bug the discipline avoids.
#[test]
fn leases_hold_when_the_wall_clock_skews_but_the_lease_clock_is_monotonic() {
    use prism_engine::clock::set_lease_now_override;

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    let thirty_days = 30 * DAY_MS;

    for n in [2usize, 4] {
        let cluster = Cluster::init(&tmp(&format!("skew-{n}")), n, config()).unwrap();
        // Snapshots are stamped `created_at_ms = TS` by the wall clock at ingest.
        cluster
            .ingest(
                prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 3000, 5),
                TS,
            )
            .unwrap();
        let (_page1, cursor) = cluster.search_page(&page_query(), None).unwrap();
        let cursor = cursor.expect("need a second page to hold a lease across");
        for i in 0..4 {
            cluster
                .ingest(
                    prism_engine::corpus::generate(
                        prism_engine::corpus::Kind::Uniform,
                        60,
                        200 + i,
                    ),
                    TS + 1 + i as i64,
                )
                .unwrap();
        }

        // The bug the discipline avoids, made visible with a dry run (it reclaims nothing for real):
        // a node whose WALL clock reads +30d would, under a naive wall-clock horizon, see the pinned
        // snapshot as ancient and reclaim it — cutting a live reader's lease short.
        let naive_fwd = cluster.gc_at(1, TS + thirty_days, true).unwrap();
        assert!(
            naive_fwd.iter().any(|r| !r.removed_snapshots.is_empty()),
            "{n}-shard sanity: a +30d wall-clock reading is exactly the skew the lease clock must \
             defend against (a naive horizon would reclaim the pinned snapshot)"
        );
        // ...and a node whose wall clock reads −30d would, naively, see it as newer than now and keep
        // it forever.
        let naive_bwd = cluster.gc_at(1, TS - thirty_days, true).unwrap();
        assert!(
            naive_bwd.iter().all(|r| r.removed_snapshots.is_empty()),
            "{n}-shard sanity: a −30d wall-clock reading would keep everything forever"
        );

        // The lease clock is MONOTONIC: only a little real time has actually elapsed, whatever the
        // wall clock reads. GC uses it, so nothing expires early — the reader keeps its lease.
        set_lease_now_override(Some(TS + 5_000));
        cluster.gc(1, false).unwrap();
        assert!(
            cluster.search_page(&page_query(), Some(&cursor)).is_ok(),
            "{n}-shard: a wall-clock skew expired a live reader's lease — the lease clock was not \
             monotonic"
        );

        // And nothing lives forever: advance the monotonic lease clock past the horizon and GC
        // reclaims, the stale cursor then getting the named expired error — the monotonic clock still
        // reaches the horizon no matter which way the wall clock drifted.
        set_lease_now_override(Some(TS + GC_HORIZON_MS + 10));
        let reports = cluster.gc(1, false).unwrap();
        assert!(
            reports.iter().any(|r| !r.removed_snapshots.is_empty()),
            "{n}-shard: past the monotonic horizon GC reclaimed nothing — a lease that lives forever"
        );
        let err = cluster
            .search_page(&page_query(), Some(&cursor))
            .expect_err("{n}-shard: past the monotonic horizon the cursor must be named-expired");
        assert!(err.to_string().contains("expired"), "{n}-shard: {err}");

        set_lease_now_override(None);
    }
}
