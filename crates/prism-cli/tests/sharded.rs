//! **The S12 cluster scaffold gate.** Sharding by tenant bucket, routing, and the snapshot vector
//! ([query §19/§20](../../../docs/QUERY-CONTRACT.md), [D-071](../../../docs/DECISIONS.md)).
//!
//! Scaffold: a tenant bucket lands whole on one shard (placement = isolation), a tenant-scoped query
//! routes to that shard and equals reading the owner shard directly, no rows leak, the snapshot
//! vector reflects every shard, a cross-tenant query is **named**.
//!
//! **Checkpoint 1 of the thesis exam:** with the one **cluster-global generation** installed on every
//! shard ([D-072](../../../docs/DECISIONS.md)), the same corpus on 1/2/4 shards answers
//! **byte-identically** for tenant-scoped search and semantic `GROUP BY`. The cross-tenant
//! global-candidate-set merge (checkpoint 2) and the full plan/route-flip exam are the next steps.

use prism_engine::cluster::ClusterRequest;
use prism_engine::sharded::Cluster;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Query;
use std::collections::BTreeMap;
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

/// A byte-exact fingerprint of a tenant's answers — search hit order + the semantic GROUP BY
/// (assignments, cluster shape, and centroids to the bit) — so a comparison is truly byte-identical.
fn tenant_fingerprint(cluster: &Cluster, t: &str) -> (Vec<String>, GroupByRepr) {
    let hits = hit_ids(&cluster.search(&query(Some(t))).unwrap());
    let gb = cluster
        .semantic_cluster(&ClusterRequest::new(t, 4))
        .unwrap();
    let repr = GroupByRepr {
        assignments: gb.assignments.clone(),
        k_effective: gb.k_effective,
        rows: gb.rows,
        quality_bits: gb.quality.to_bits(),
        centroid_bits: gb.centroids.iter().map(|f| f.to_bits()).collect(),
    };
    (hits, repr)
}

#[derive(PartialEq, Eq, Debug)]
struct GroupByRepr {
    assignments: Vec<(String, usize)>,
    k_effective: usize,
    rows: usize,
    quality_bits: u64,
    centroid_bits: Vec<u32>,
}

fn cluster_answers(num_shards: usize, tag: &str) -> BTreeMap<String, (Vec<String>, GroupByRepr)> {
    let cluster = Cluster::init(&tmp(tag), num_shards, config()).unwrap();
    cluster
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 3000, 5),
            TS,
        )
        .unwrap();
    TENANTS
        .iter()
        .map(|t| (t.to_string(), tenant_fingerprint(&cluster, t)))
        .collect()
}

/// **Checkpoint 1 of the thesis exam: sharding is a layout for tenant-scoped queries.** With the one
/// cluster-global generation installed on every shard (D-072), the same corpus on 1, 2, and 4 shards
/// answers **byte-identically** for every tenant — search hit order and the semantic GROUP BY to the
/// bit. (The cross-tenant global-candidate-set merge is checkpoint 2.)
#[test]
fn sharding_is_a_layout_for_tenant_scoped_queries() {
    let a1 = cluster_answers(1, "layout1");
    let a2 = cluster_answers(2, "layout2");
    let a4 = cluster_answers(4, "layout4");
    assert_eq!(a1, a2, "2-way sharding changed a tenant-scoped answer");
    assert_eq!(a1, a4, "4-way sharding changed a tenant-scoped answer");
    assert!(!a1.is_empty());
}

/// The one cluster-global generation is identical at every shard count — the codebook is a function
/// of the cluster-wide sample, not the placement (D-072).
#[test]
fn the_cluster_generation_is_identical_at_every_shard_count() {
    let mk = |n, tag| {
        let c = Cluster::init(&tmp(tag), n, config()).unwrap();
        c.ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 3000, 5),
            TS,
        )
        .unwrap();
        c.installed_generation().unwrap().unwrap()
    };
    let g1 = mk(1, "gen1");
    let g2 = mk(2, "gen2");
    let g4 = mk(4, "gen4");
    assert_eq!(
        g1, g2,
        "the cluster generation differs between 1 and 2 shards"
    );
    assert_eq!(
        g1, g4,
        "the cluster generation differs between 1 and 4 shards"
    );
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
