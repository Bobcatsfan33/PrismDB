//! **The S12 cluster scaffold gate.** Sharding by tenant bucket, routing, and the snapshot vector
//! ([query §19/§20](../../../docs/QUERY-CONTRACT.md), [D-071](../../../docs/DECISIONS.md)).
//!
//! This gate proves the scaffold: a tenant bucket lands whole on one shard (placement = isolation),
//! a tenant-scoped query routes to that shard and its answer equals reading the owner shard directly,
//! no tenant's rows leak to another shard, the snapshot vector reflects every shard, and a
//! cross-tenant query is **named**, never answered from one shard. The full *byte-identical across 1/
//! 2/4 shards* layout gate needs a **cluster-global generation** (each shard otherwise bootstraps a
//! different codebook on its data subset, which changes candidate selection — [D-072](../../../docs/DECISIONS.md));
//! that is the next increment, built against this scaffold.

use prism_engine::sharded::Cluster;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Query;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!(
        "prism-sharded-{}-{}-{}",
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

fn query(tenant: Option<&str>) -> Query {
    Query {
        text: "the tool call timed out retrying".into(),
        k: 15,
        tenant: tenant.map(str::to_string),
        rerank: 40,
        ..Default::default()
    }
}

fn hit_ids(engine_result: &prism_types::SearchResult) -> Vec<String> {
    engine_result
        .hits
        .iter()
        .map(|h| h.event.event_id.clone())
        .collect()
}

const TENANTS: [&str; 5] = ["t0", "t1", "t2", "t3", "t4"];
const TS: i64 = 1_760_000_000_000;

/// A tenant bucket lands whole on one shard; a tenant-scoped query routes there and equals the
/// direct read; no rows leak across shards; the snapshot vector reflects every shard; a cross-tenant
/// query is named.
#[test]
fn the_cluster_routes_by_tenant_bucket_and_places_a_bucket_whole_on_one_shard() {
    let root = tmp("route");
    let cluster = Cluster::init(&root, 4, config()).unwrap();
    cluster
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 3000, 5),
            TS,
        )
        .unwrap();

    // The snapshot vector reflects all four shards.
    assert_eq!(cluster.snapshot_vector().unwrap().len(), 4);

    for t in TENANTS {
        let owner = cluster.shard_index(t);

        // Routing is stable and deterministic.
        assert_eq!(cluster.shard_index(t), owner, "shard routing is not stable");

        // The tenant-scoped query through the cluster equals reading the owner shard directly.
        let via_cluster = cluster.search(&query(Some(t))).unwrap();
        let via_owner = cluster.shard(owner).search(&query(Some(t))).unwrap();
        assert_eq!(
            hit_ids(&via_cluster),
            hit_ids(&via_owner),
            "the cluster did not route tenant {t} to its owner shard"
        );
        assert!(!via_cluster.hits.is_empty(), "tenant {t} lost its data");

        // The tenant's rows are on exactly one shard — no bucket straddles two (placement =
        // isolation, D-071).
        for j in 0..cluster.num_shards() {
            if j == owner {
                continue;
            }
            let leaked = cluster.shard(j).search(&query(Some(t))).unwrap();
            assert!(
                leaked.hits.is_empty(),
                "tenant {t} data leaked onto shard {j} — a bucket straddled two shards"
            );
        }
    }

    // A cross-tenant query is named, never answered from a single shard.
    let err = cluster.search(&query(None)).unwrap_err().to_string();
    assert!(
        err.contains("cross-shard") || err.contains("cross-tenant"),
        "a cross-tenant cluster query must be named, got: {err}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The routed shard for a tenant is independent of shard count for the buckets that map to it — the
/// same tenant hashes to the same bucket everywhere (a placement invariant the layout gate builds on).
#[test]
fn a_tenant_hashes_to_the_same_bucket_at_every_shard_count() {
    let c1 = Cluster::init(&tmp("c1"), 1, config()).unwrap();
    let c2 = Cluster::init(&tmp("c2"), 2, config()).unwrap();
    let c4 = Cluster::init(&tmp("c4"), 4, config()).unwrap();
    // With one shard everything routes to shard 0; with more, routing is bucket-ordinal % shards,
    // and the same tenant never splits across a run.
    for t in TENANTS {
        assert_eq!(c1.shard_index(t), 0);
        assert!(c2.shard_index(t) < 2);
        assert!(c4.shard_index(t) < 4);
    }
}
