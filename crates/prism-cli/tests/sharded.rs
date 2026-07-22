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
use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::{Query, SearchResult};
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

    // A cross-tenant query now fans out and answers (the two-round merge, checkpoint 2), never a
    // single shard's short answer.
    let cross = cluster.search(&query(None)).unwrap();
    assert!(
        !cross.hits.is_empty(),
        "a cross-tenant cluster query must fan out and answer"
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

/// Hits as (event_id, exact-score bits) — a byte-exact fingerprint of a search answer.
fn hit_fp(r: &SearchResult) -> Vec<(String, u32)> {
    r.hits
        .iter()
        .map(|h| (h.event.event_id.clone(), h.score.to_bits()))
        .collect()
}

fn cross_tenant_query(group_k: Option<usize>) -> Query {
    Query {
        text: "the tool call timed out retrying".into(),
        k: 20,
        tenant: None,
        rerank: 60,
        nprobe: 8,
        candidates: 200,
        group_k,
        ..Default::default()
    }
}

fn group_repr(r: &SearchResult) -> Vec<(String, usize)> {
    r.clusters
        .as_ref()
        .unwrap()
        .iter()
        .map(|c| (c.exemplar.event_id.clone(), c.count))
        .collect()
}

/// **Checkpoint 2 of the thesis exam: the two-round cross-shard merge is a layout.** A cross-tenant
/// query fans out to every shard, and answers **byte-identically** on a single engine (the ground
/// truth) and on the cluster at 1, 2, and 4 shards — the coordinator reconstructs the global
/// candidate set, reranks within one global budget, and merges with the C-4 `event_id` tie, so the
/// placement of tenants across shards is invisible to the answer. The battery covers plain search,
/// **budget-starved** (trap 2 — the named degradation must be identical at every shard count),
/// **threshold**, small-rerank, forced **plan** flips, and cross-tenant semantic **GROUP BY**.
#[test]
fn cross_tenant_search_and_group_by_are_a_layout_1_2_4_way() {
    let corpus = || prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 3000, 5);

    // Ground truth: a single engine over the whole corpus (same generation the cluster trains).
    let single = Engine::init(&tmp("xt-single"), config()).unwrap();
    single.ingest(corpus(), TS).unwrap();
    let clusters: Vec<(usize, Cluster)> = [1usize, 2, 4]
        .iter()
        .map(|&n| {
            let c = Cluster::init(&tmp(&format!("xt-{n}")), n, config()).unwrap();
            c.ingest(corpus(), TS).unwrap();
            (n, c)
        })
        .collect();

    // The search battery — every shape that stresses the merge.
    let variants: Vec<(&str, Query)> = vec![
        ("plain", cross_tenant_query(None)),
        ("budget-starved", {
            let mut q = cross_tenant_query(None);
            q.fetch_budget_bytes = Some(8 * 64 * 4); // room for ~8 of many exact vectors
            q
        }),
        ("threshold", {
            let mut q = cross_tenant_query(None);
            q.threshold = Some(0.3);
            q
        }),
        ("small-rerank", {
            let mut q = cross_tenant_query(None);
            q.rerank = 12;
            q.k = 8;
            q
        }),
        ("plan-interleaved", {
            let mut q = cross_tenant_query(None);
            q.plan = Some("interleaved".into());
            q
        }),
        ("plan-scalar-first", {
            let mut q = cross_tenant_query(None);
            q.plan = Some("scalar-first".into());
            q
        }),
        ("plan-semantic-first", {
            let mut q = cross_tenant_query(None);
            q.plan = Some("semantic-first".into());
            q
        }),
        ("route-cpu", {
            let mut q = cross_tenant_query(None);
            q.force_route = Some("cpu".into());
            q
        }),
        ("route-gpu-reference", {
            let mut q = cross_tenant_query(None);
            q.force_route = Some("gpu-reference".into());
            q
        }),
    ];

    for (tag, q) in &variants {
        let g = single.search(q).unwrap();
        assert!(!g.hits.is_empty(), "ground-truth `{tag}` is empty");
        for (n, c) in &clusters {
            let cr = c.search(q).unwrap();
            assert_eq!(
                hit_fp(&cr),
                hit_fp(&g),
                "`{tag}` cross-tenant SEARCH diverged from ground truth at {n} shards"
            );
            // Trap 2: the budget degradation is the SAME outcome class at every shard count.
            assert_eq!(
                cr.counters.fetch_budget_exhausted, g.counters.fetch_budget_exhausted,
                "`{tag}` budget-exhaustion flag diverged at {n} shards"
            );
        }
    }
    // At least one variant must actually exercise the degradation, or trap 2 proves nothing.
    let starved = single
        .search(
            &variants
                .iter()
                .find(|(t, _)| *t == "budget-starved")
                .unwrap()
                .1,
        )
        .unwrap();
    assert!(
        starved.counters.fetch_budget_exhausted,
        "the budget-starved variant did not actually exhaust the budget"
    );

    // Cross-tenant semantic GROUP BY: exemplars + per-cluster counts identical at every shard count.
    let g_group = group_repr(&single.search(&cross_tenant_query(Some(4))).unwrap());
    for (n, c) in &clusters {
        let cg = group_repr(&c.search(&cross_tenant_query(Some(4))).unwrap());
        assert_eq!(
            cg, g_group,
            "cross-tenant GROUP BY diverged from ground truth at {n} shards"
        );
    }
}

/// Events whose body is exactly the query text — so they dominate the top-k the moment they land,
/// which makes "did a publication become visible?" a sharp, deterministic question. Built by
/// overriding real corpus events (valid attributes, spread across tenants).
fn matching_events(n: usize, tag: &str) -> Vec<prism_types::Event> {
    prism_engine::corpus::generate(prism_engine::corpus::Kind::Uniform, n, 99)
        .into_iter()
        .enumerate()
        .map(|(i, mut e)| {
            e.event_id = format!("{tag}-{i:04}");
            e.body = "the tool call timed out retrying".into();
            e
        })
        .collect()
}

/// **Item 3 — the snapshot vector is pinned at planning (QUERY-CONTRACT §19).** A cross-tenant query
/// captures the vector once and runs both rounds against it, so a publication landing after the pin
/// is **invisible** to a query resumed against that vector — while a live query sees it. This is the
/// contract's declared consistency made real, at 1, 2, and 4 shards; the cursor merely carries the
/// vector this proves the engine already honours.
#[test]
fn a_pinned_snapshot_vector_hides_later_publications() {
    for n in [1usize, 2, 4] {
        let cluster = Cluster::init(&tmp(&format!("pin-{n}")), n, config()).unwrap();
        cluster
            .ingest(
                prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 3000, 5),
                TS,
            )
            .unwrap();

        // Pin the vector, and record the answer against it.
        let vector = cluster.pin_vector().unwrap();
        let q = cross_tenant_query(None);
        let pinned_before = hit_fp(&cluster.search_at_vector(&vector, &q).unwrap());
        assert!(!pinned_before.is_empty());

        // Publish more — events that would dominate the top-k the instant they are visible.
        cluster
            .ingest(matching_events(50, "late"), TS + 10_000)
            .unwrap();

        // The pinned query is unchanged: the publication is invisible to it.
        let pinned_after = hit_fp(&cluster.search_at_vector(&vector, &q).unwrap());
        assert_eq!(
            pinned_after, pinned_before,
            "{n}-shard: a publication after the pin changed a pinned query's answer (§19 violated)"
        );

        // A live query DOES see it — otherwise the pin proves nothing.
        let live = cluster.search(&q).unwrap();
        assert!(
            live.hits.iter().any(|h| h.event.event_id.starts_with("late-")),
            "{n}-shard: a live query did not see the publication — the test cannot distinguish pinned"
        );
        assert_ne!(
            hit_fp(&live),
            pinned_before,
            "{n}-shard: the live answer must differ from the pinned one after a publication"
        );
    }
}

/// **Item 3, gate (b): pagination against the pinned vector, with a mid-pagination publication.** A
/// paginated cross-tenant query pins the vector on page 1 and carries it in the cursor, so the pages
/// **tile the pinned answer with no duplicate and no gap** even as a publication lands between pages —
/// which is invisible to the cursor. Holds at 1, 2, and 4 shards.
#[test]
fn cross_tenant_pagination_tiles_the_pinned_answer_across_a_publication() {
    for n in [1usize, 2, 4] {
        let cluster = Cluster::init(&tmp(&format!("page-{n}")), n, config()).unwrap();
        cluster
            .ingest(
                prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 3000, 5),
                TS,
            )
            .unwrap();

        // The reference: the whole ordered result against the vector page 1 will pin (current now).
        let vector = cluster.pin_vector().unwrap();
        let mut full_q = cross_tenant_query(None);
        full_q.k = full_q.rerank;
        let full: Vec<String> = hit_ids(&cluster.search_at_vector(&vector, &full_q).unwrap());
        assert!(full.len() > 10, "need enough survivors to paginate");

        // Paginate five at a time, publishing after the first page.
        let mut page_q = cross_tenant_query(None);
        page_q.k = 5;
        let mut collected: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut published = false;
        loop {
            let (page, next) = cluster.search_page(&page_q, cursor.as_deref()).unwrap();
            collected.extend(hit_ids(&page));
            if !published {
                cluster
                    .ingest(matching_events(50, "mid"), TS + 10_000)
                    .unwrap();
                published = true;
            }
            match next {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }

        // The pages tiled exactly the pinned answer — no duplicate, no gap — and the mid-pagination
        // publication is invisible (no `mid-` event leaked in).
        assert_eq!(
            collected, full,
            "{n}-shard: pagination did not tile the pinned answer across a publication"
        );
        assert!(
            !collected.iter().any(|id| id.starts_with("mid-")),
            "{n}-shard: a mid-pagination publication leaked into the cursor"
        );
    }
}

/// **Item 3, gate (c): an expired pinned vector is a named condition, never a short answer.** When
/// the parts a pinned vector names have been reclaimed (here, simulated by removing a part the way
/// GC past the lease horizon would), a query resumed against that vector fails with the **named**
/// expired-snapshot error — the S3 contract, at the cluster.
#[test]
fn a_pinned_vector_whose_parts_were_reclaimed_is_expired_by_name() {
    let cluster = Cluster::init(&tmp("expired"), 2, config()).unwrap();
    cluster
        .ingest(
            prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 3000, 5),
            TS,
        )
        .unwrap();
    let vector = cluster.pin_vector().unwrap();
    let q = cross_tenant_query(None);
    assert!(!cluster
        .search_at_vector(&vector, &q)
        .unwrap()
        .hits
        .is_empty());

    // Reclaim a part the vector names, as GC past the lease horizon would.
    let part = vector
        .iter()
        .enumerate()
        .find_map(|(si, s)| s.part_ids().into_iter().next().map(|p| (si, p)))
        .unwrap();
    std::fs::remove_dir_all(cluster.shard(part.0).store.part_dir(&part.1)).unwrap();

    let err = cluster
        .search_at_vector(&vector, &q)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("expired") && err.contains(&part.1),
        "an expired pinned vector must be named, got: {err}"
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
