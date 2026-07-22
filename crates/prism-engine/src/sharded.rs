//! The distributed cluster (S12) — a set of shards, each an independent [`Engine`], sharded by
//! **tenant bucket** ([D-071](../../../docs/DECISIONS.md)).
//!
//! A tenant bucket never straddles two shards ([S4](../../../docs/PRISM.md) isolation becomes the
//! placement boundary), so a tenant's data is whole on exactly one shard, and a tenant-scoped query
//! routes to that one shard. **Sharding is a layout** ([query §20](../../../docs/QUERY-CONTRACT.md)):
//! the same corpus on 1, 2, or 4 shards answers byte-identically, because which shard a tenant lives
//! on — and how many shards exist — is erased by routing (for a tenant-scoped query) and by the merge
//! (for a cross-tenant one, the filed next increment).
//!
//! **Increment scope.** This lands the cluster scaffold — tenant-bucket sharding, routing, and the
//! global **snapshot vector** ([query §19](../../../docs/QUERY-CONTRACT.md)) — and gates that
//! tenant-scoped queries are a layout. The cross-shard merge (the two-round global-candidate-set
//! search and the coordinated canonical-shard-order `GROUP BY`, [query §20](../../../docs/QUERY-CONTRACT.md))
//! is the next increment, built against the now-locked contract.

use crate::cluster::{ClusterRequest, SemanticClusterResult};
use crate::engine::Engine;
use crate::search::Scored;
use crate::storage::object::{LocalObjectStore, ObjectStore};
use prism_part::generation::Generation;
use prism_part::partition::{Bucket, PartitionScheme};
use prism_part::store::StoreConfig;
use prism_types::error::{PrismError, Result};
use prism_types::{Counters, Event, Query, SearchResult};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

/// A cluster of shards. Each shard is a whole [`Engine`] over its own store; the cluster routes by
/// tenant bucket and never lets a tenant bucket straddle two shards. The **generation store** holds
/// the one cluster-global codebook, content-addressed, that every shard installs and serves
/// ([D-072](../../../docs/DECISIONS.md)) — shards never train their own.
pub struct Cluster {
    shards: Vec<Engine>,
    scheme: PartitionScheme,
    gen_store: Arc<dyn ObjectStore>,
}

/// The generation store key for a content-addressed codebook.
fn gen_key(id: &str) -> String {
    format!("generations/{id}")
}

/// A stable ordinal for a bucket, disjoint across `Shared`/`Dedicated`, so a bucket maps to exactly
/// one shard and two tenants in the same bucket always land together.
fn bucket_ordinal(scheme: &PartitionScheme, b: &Bucket) -> u64 {
    match b {
        Bucket::Shared(n) => *n as u64,
        Bucket::Dedicated(i) => scheme.buckets as u64 + *i as u64,
    }
}

impl Cluster {
    /// Create a cluster of `num_shards` shards under `root` (each shard a store `shard-<i>`), all
    /// sharing one partition scheme so a tenant hashes to the same bucket on every shard.
    pub fn init(root: &Path, num_shards: usize, config: StoreConfig) -> Result<Cluster> {
        if num_shards == 0 {
            return Err(PrismError::Invalid(
                "a cluster needs at least one shard".into(),
            ));
        }
        let scheme = config.partitions.clone();
        let mut shards = Vec::with_capacity(num_shards);
        for i in 0..num_shards {
            shards.push(Engine::init(
                &root.join(format!("shard-{i}")),
                config.clone(),
            )?);
        }
        let gen_store = Arc::new(LocalObjectStore::new(root.join("cluster-generations")));
        Ok(Cluster {
            shards,
            scheme,
            gen_store,
        })
    }

    pub fn num_shards(&self) -> usize {
        self.shards.len()
    }

    /// A shard by index — for inspection and the routing gate (a tenant's answer through the cluster
    /// equals its answer read directly off the owner shard).
    pub fn shard(&self, i: usize) -> &Engine {
        &self.shards[i]
    }

    /// The shard that owns a tenant's bucket. A function of the bucket, not the tenant, so a whole
    /// bucket lives on one shard.
    pub fn shard_index(&self, tenant: &str) -> usize {
        let bucket = self.scheme.bucket_of(tenant);
        (bucket_ordinal(&self.scheme, &bucket) % self.shards.len() as u64) as usize
    }

    /// The global **snapshot vector** ([query §19](../../../docs/QUERY-CONTRACT.md)): each shard's
    /// live catalog seq. A distributed query pins this at planning; a tenant-scoped query needs only
    /// its owner shard's element, but the vector is the cluster's one consistent instant.
    pub fn snapshot_vector(&self) -> Result<Vec<String>> {
        self.shards
            .iter()
            .map(|e| Ok(e.snapshot()?.snapshot_id))
            .collect()
    }

    /// The generation every shard serves (they serve one, uniformly), or `None` before the first
    /// ingest has installed it.
    pub fn installed_generation(&self) -> Result<Option<String>> {
        Ok(self.shards[0].snapshot()?.active_generation)
    }

    /// Publish a trained codebook to the cluster's generation store, content-addressed and
    /// idempotent — the store is a codebook's natural home ([D-071](../../../docs/DECISIONS.md)).
    fn publish_generation(&self, g: &Generation) -> Result<()> {
        g.verify_content_address()?;
        let key = gen_key(&g.generation_id);
        if self.gen_store.head(&key)?.is_none() {
            self.gen_store.put(&key, &serde_json::to_vec(g)?)?;
        }
        Ok(())
    }

    /// Install a published generation on **every** shard: fetch-by-hash, verify the bytes hash to the
    /// id asked for (the capability check — the store cannot hand a shard the wrong codebook), then
    /// activate. Returns only once every shard serves it: the **order invariant**
    /// ([D-071](../../../docs/DECISIONS.md)) — no shard writes a part pinned to a generation, or
    /// serves a query against it, before every assigned shard has installed and verified it.
    fn install_generation_everywhere(&self, id: &str, now_ms: i64) -> Result<()> {
        let bytes = self.gen_store.get(&gen_key(id))?;
        let g: Generation = serde_json::from_slice(&bytes)?;
        if g.generation_id != id {
            return Err(PrismError::Corrupt(format!(
                "the generation store returned `{}` for key `{id}` — not the codebook asked for",
                g.generation_id
            )));
        }
        for shard in &self.shards {
            shard.install_generation(&g, now_ms)?;
        }
        Ok(())
    }

    /// Ingest a batch, routing each event to the shard that owns its tenant bucket. **The first
    /// ingest trains the one cluster-global generation over a cluster-wide sample** (every event, not
    /// one shard's slice — a per-shard codebook is the [D-072](../../../docs/DECISIONS.md) mistake),
    /// publishes it, and installs it on every shard *before* any part is written. Thereafter every
    /// shard codes under the same codebook, so the same corpus lands byte-identically on 1, 2, or 4
    /// shards.
    pub fn ingest(&self, events: Vec<Event>, now_ms: i64) -> Result<()> {
        if self.installed_generation()?.is_none() {
            // Train cluster-wide, seeded on the empty snapshot id so the codebook is identical at any
            // shard count. `train_generation` does not commit — the install path does.
            let seed_snapshot = prism_part::catalog::Snapshot::empty().snapshot_id;
            let (trained, _dead) =
                self.shards[0].train_generation(&seed_snapshot, events.clone())?;
            if let Some(t) = trained {
                self.publish_generation(&t.generation)?;
                self.install_generation_everywhere(&t.generation.generation_id, now_ms)?;
            }
            // If nothing embeds, no generation is installed and each shard's ingest finishes empty.
        }

        let mut by_shard: Vec<Vec<Event>> = vec![Vec::new(); self.shards.len()];
        for e in events {
            let s = self.shard_index(&e.tenant_id);
            by_shard[s].push(e);
        }
        for (i, batch) in by_shard.into_iter().enumerate() {
            if !batch.is_empty() {
                self.shards[i].ingest(batch, now_ms)?;
            }
        }
        Ok(())
    }

    /// A tenant-scoped search, routed to the owner shard. Which shard that is, and how many shards
    /// the cluster has, are invisible to the answer ([query §20](../../../docs/QUERY-CONTRACT.md)).
    /// A cross-tenant query (`tenant = None`) needs the global-candidate-set merge — the next
    /// increment — and is named, never silently answered from one shard.
    pub fn search(&self, q: &Query) -> Result<SearchResult> {
        match q.tenant.as_deref() {
            // A tenant-scoped query lives on one shard: route to it, unchanged.
            Some(t) => self.shards[self.shard_index(t)].search(q),
            // A cross-tenant query fans out: the two-round global-candidate-set merge (query §20).
            None => self.search_cross_shard(q),
        }
    }

    /// **The two-round cross-shard search** ([query §20](../../../docs/QUERY-CONTRACT.md),
    /// [D-073](../../../docs/DECISIONS.md)). Round 1: every shard returns its bounded candidates by PQ
    /// distance. The coordinator merges to the **global** candidate set (PQ distance, C-4 `event_id`
    /// tie) and bounds it once — to the rerank width and the **single global fetch budget**. Round 2:
    /// each owning shard exact-scores exactly its subset, so total exact fetches stay within that one
    /// budget. The coordinator then runs the **shared** `finalize` (the same code single-store search
    /// runs) over the merged scores, materializing the survivors back on their shards.
    fn search_cross_shard(&self, q: &Query) -> Result<SearchResult> {
        // Pin the snapshot vector AT PLANNING (query §19): one snapshot per shard, captured once and
        // read from for BOTH rounds — a publication landing mid-query cannot change the answer. A
        // cursor paginating this query carries exactly this vector.
        let vector = self.snapshot_vector_pinned()?;
        self.search_cross_shard_at(&vector, q)
    }

    /// The snapshots the coordinator pins for a query: one per shard, captured at planning.
    fn snapshot_vector_pinned(&self) -> Result<Vec<prism_part::catalog::Snapshot>> {
        self.shards
            .iter()
            .map(|s| s.snapshot())
            .collect::<Result<Vec<_>>>()
    }

    /// Pin the snapshot vector — one snapshot per shard, captured now. A paginated query captures
    /// this once and carries it in its cursor, so every page reads the same corpus ([query §19](../../../docs/QUERY-CONTRACT.md)).
    pub fn pin_vector(&self) -> Result<Vec<prism_part::catalog::Snapshot>> {
        self.snapshot_vector_pinned()
    }

    /// Answer a query against an explicitly **pinned snapshot vector** — the door a cursor resumes
    /// through. A tenant-scoped query reads its owner shard's pinned snapshot; a cross-tenant query
    /// runs the two-round merge against the whole vector. Nothing published after the vector was
    /// pinned is visible ([query §19](../../../docs/QUERY-CONTRACT.md)).
    pub fn search_at_vector(
        &self,
        vector: &[prism_part::catalog::Snapshot],
        q: &Query,
    ) -> Result<SearchResult> {
        match q.tenant.as_deref() {
            Some(t) => {
                let owner = self.shard_index(t);
                self.shards[owner].search_at(&vector[owner], q)
            }
            None => self.search_cross_shard_at(vector, q),
        }
    }

    /// Execute the two-round merge against an **already-pinned** snapshot vector ([query §19](../../../docs/QUERY-CONTRACT.md)).
    /// Round 1 scans each shard's pinned snapshot; round 2 rescores the immutable parts those
    /// snapshots named. Nothing published after `vector` was captured can change the answer — which is
    /// what makes a mid-query (or mid-pagination) publication invisible.
    fn search_cross_shard_at(
        &self,
        vector: &[prism_part::catalog::Snapshot],
        q: &Query,
    ) -> Result<SearchResult> {
        let dim = self.shards[0].store.config.dim;
        let snaps = vector;
        let snapshot_id = snaps
            .iter()
            .map(|s| s.snapshot_id.as_str())
            .collect::<Vec<_>>()
            .join("+");
        let tombstones: BTreeSet<String> = snaps
            .iter()
            .flat_map(|s| s.tombstones.iter().cloned())
            .collect();

        // --- round 1: candidates from every shard, merged to the global set ---
        // (dist, event_id, shard, part_id, row)
        let mut global: Vec<(f32, String, usize, String, usize)> = Vec::new();
        for (si, shard) in self.shards.iter().enumerate() {
            for cand in shard.search_candidates(&snaps[si], q)? {
                global.push((cand.dist, cand.event_id, si, cand.part_id, cand.row));
            }
        }
        // Merge by PQ distance, ties on event_id (C-4 across the wire).
        global.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));

        // Bound ONCE, globally: rerank width, then the declared byte budget — for the query, not per
        // shard × N. Exhaustion is the same named degradation single-store reports (storage §6).
        global.truncate(q.rerank);
        let mut fetch_budget_exhausted = false;
        if let Some(budget) = q.fetch_budget_bytes {
            let max_vectors = budget / (dim * 4).max(1);
            if global.len() > max_vectors {
                global.truncate(max_vectors);
                fetch_budget_exhausted = true;
            }
        }

        // --- round 2: each owning shard exact-scores its subset of the global set ---
        let mut by_shard: BTreeMap<usize, Vec<(String, usize)>> = BTreeMap::new();
        for (_, _, si, pid, row) in &global {
            by_shard.entry(*si).or_default().push((pid.clone(), *row));
        }
        let mut scored: Vec<Scored> = Vec::new();
        // handle[gidx] = (shard, part_id, row) — how the coordinator routes materialization back.
        let mut handle: Vec<(usize, String, usize)> = Vec::new();
        let mut exact_bytes_fetched = 0usize;
        let mut object_requests = 0usize;
        for (si, sel) in &by_shard {
            for s in self.shards[*si].search_rerank_selected(q, sel)? {
                let gidx = handle.len();
                handle.push((*si, s.part_id.clone(), s.row));
                exact_bytes_fetched += s.vector.len() * 4;
                scored.push(Scored {
                    score: s.score,
                    part: gidx,
                    row: s.row,
                    vector: s.vector,
                    event_id: s.event_id,
                });
            }
            object_requests += 1;
        }

        // --- finalize: the SHARED implementation, with a materializer that routes to the shards ---
        let materialize =
            |needed: &BTreeSet<(usize, usize)>| -> Result<BTreeMap<(usize, usize), (Event, u32)>> {
                // Group survivors by shard, keeping each survivor's global handle to map results back.
                let mut by_shard_mat: BTreeMap<usize, Vec<(usize, String, usize)>> =
                    BTreeMap::new();
                for (gidx, row) in needed {
                    let (si, pid, _) = &handle[*gidx];
                    by_shard_mat
                        .entry(*si)
                        .or_default()
                        .push((*gidx, pid.clone(), *row));
                }
                let mut out: BTreeMap<(usize, usize), (Event, u32)> = BTreeMap::new();
                for (si, reqs) in &by_shard_mat {
                    let sel: Vec<(String, usize)> = reqs
                        .iter()
                        .map(|(_, pid, row)| (pid.clone(), *row))
                        .collect();
                    let mats = self.shards[*si].search_materialize(&sel)?;
                    for ((gidx, _, row), (ev, cen)) in reqs.iter().zip(mats) {
                        out.insert((*gidx, *row), (ev, cen));
                    }
                }
                Ok(out)
            };

        let gen_ids: BTreeSet<String> = self.installed_generation()?.into_iter().collect();
        let plan_choice = crate::plan::PlanChoice {
            strategy: crate::plan::Strategy::Interleaved,
            reason: "cluster coordinator (query §20)".into(),
            estimated_selectivity: f64::NAN,
        };
        let c = Counters {
            rerank_width: global.len(),
            fetch_budget_exhausted,
            exact_bytes_fetched,
            object_requests,
            ..Default::default()
        };

        self.shards[0].finalize(
            &tombstones,
            &snapshot_id,
            q,
            scored,
            &gen_ids,
            &plan_choice,
            c,
            materialize,
            || 0,
        )
    }

    /// A tenant-scoped semantic `GROUP BY`, routed to the owner shard. Cross-tenant clustering needs
    /// the coordinated canonical-shard-order partial merge (the next increment).
    pub fn semantic_cluster(&self, req: &ClusterRequest) -> Result<SemanticClusterResult> {
        self.shards[self.shard_index(&req.tenant)].semantic_cluster(req)
    }
}
