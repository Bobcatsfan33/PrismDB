//! **The S10 lease gate: invariant 6 holds by construction, and a crashed reader's lease expires**
//! ([merge contract §5](../../../docs/MERGE-CONTRACT.md), [query contract §2](../../../docs/QUERY-CONTRACT.md)).
//!
//! GC grace is *derived* from the reader-lease TTL — one constant — so the reclaim horizon is
//! always the lease plus its grace, and the two can never drift into `grace < lease`. This gate
//! proves the two halves that matters:
//! - a reader **within** its lease is never orphaned: even with `retain = 1`, a snapshot younger
//!   than the horizon is kept, so the reader's cursor still resolves;
//! - a reader **past** its lease (or crashed, which is the same thing to the server, since a lease
//!   is time-bounded not connection-held) has its snapshot reclaimed, and its stale cursor gets the
//!   explicit expired-snapshot error, never a wrong answer.

use prism_engine::Engine;
use prism_part::catalog::GC_HORIZON_MS;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_sql::{compile, Session};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-lease-{}-{}-{}", tag, std::process::id(), n));
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

fn sess(t: &str) -> Session {
    Session {
        tenant: t.to_string(),
    }
}

#[test]
fn a_reader_within_its_lease_survives_gc_and_a_crashed_one_expires() {
    let root = tmp("lease");
    let engine = Engine::init(&root, config()).unwrap();

    // The pinned snapshot is created at T0. A reader pins it by starting a paginated query.
    let t0 = 1_000_000i64;
    engine
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 1500, 5),
            t0,
        )
        .unwrap();
    let plan = compile(
        "SELECT event_id FROM events WHERE embedding ≈≈ 'the tool call timed out' LIMIT 5",
        &sess("t1"),
    )
    .unwrap();
    let page1 = engine.run_sql(&plan, None).unwrap();
    let cursor = page1.next_cursor.expect("there should be a second page");

    // Churn the catalog so the pinned snapshot is old by count (retain = 1 would drop it).
    for i in 0..4 {
        engine
            .ingest(
                prism_engine::corpus::generate(prism_engine::corpus::Kind::Uniform, 50, 100 + i),
                t0 + 1 + i as i64,
            )
            .unwrap();
    }

    // WITHIN the lease horizon: even at retain = 1, the young snapshot is time-protected, so the
    // reader's cursor still resolves. This is invariant 6, by construction.
    let removed = engine
        .catalog()
        .gc_at(1, t0 + GC_HORIZON_MS - 1, false)
        .unwrap();
    assert!(
        engine.run_sql(&plan, Some(&cursor)).is_ok(),
        "GC reclaimed a snapshot a reader was still within its lease on — invariant 6 violated. \
         removed snapshots: {:?}",
        removed.removed_snapshots
    );

    // PAST the horizon: the crashed/expired reader's snapshot is reclaimed, and its stale cursor
    // gets the explicit expired-snapshot error.
    engine
        .catalog()
        .gc_at(1, t0 + GC_HORIZON_MS + 10, false)
        .unwrap();
    let err = engine
        .run_sql(&plan, Some(&cursor))
        .expect_err("a cursor into a reclaimed snapshot must fail, not silently move");
    let msg = err.to_string();
    assert!(
        msg.contains("reclaimed") && msg.contains("re-run the query"),
        "{msg}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
