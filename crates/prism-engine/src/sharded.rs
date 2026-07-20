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
use prism_part::partition::{Bucket, PartitionScheme};
use prism_part::store::StoreConfig;
use prism_types::error::{PrismError, Result};
use prism_types::{Event, Query, SearchResult};
use std::path::Path;

/// A cluster of shards. Each shard is a whole [`Engine`] over its own store; the cluster routes by
/// tenant bucket and never lets a tenant bucket straddle two shards.
pub struct Cluster {
    shards: Vec<Engine>,
    scheme: PartitionScheme,
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
        Ok(Cluster { shards, scheme })
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

    /// Ingest a batch, routing each event to the shard that owns its tenant bucket. One event's
    /// placement is a pure function of its tenant, so the same corpus lands identically on the same
    /// shard regardless of shard count (modulo which shard index that is).
    pub fn ingest(&self, events: Vec<Event>, now_ms: i64) -> Result<()> {
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
            Some(t) => self.shards[self.shard_index(t)].search(q),
            None => Err(PrismError::Invalid(
                "a cross-tenant (tenant = None) cluster search needs the cross-shard \
                 global-candidate-set merge (query contract §20), which is the next S12 increment; \
                 refusing to answer it from a single shard rather than return a short answer"
                    .into(),
            )),
        }
    }

    /// A tenant-scoped semantic `GROUP BY`, routed to the owner shard. Cross-tenant clustering needs
    /// the coordinated canonical-shard-order partial merge (the next increment).
    pub fn semantic_cluster(&self, req: &ClusterRequest) -> Result<SemanticClusterResult> {
        self.shards[self.shard_index(&req.tenant)].semantic_cluster(req)
    }
}
