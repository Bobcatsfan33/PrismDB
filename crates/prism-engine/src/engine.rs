use crate::model::{HashModelPlane, ModelPlane};
use prism_part::catalog::{Catalog, Snapshot};
use prism_part::part::PartReader;
use prism_part::store::{Store, StoreConfig};
use prism_types::error::Result;
use std::path::Path;
use std::sync::Arc;

pub struct Engine {
    pub store: Store,
    pub plane: Arc<dyn ModelPlane>,
    /// The cold tier's object store, with a content-verified cache in front (S11). Every exact
    /// rerank-vector fetch goes through this, so the cold tier can be local (the default backend
    /// points at the store's own `parts/` directory — behaviour-preserving) or remote (an
    /// `S3ObjectStore`). A cache state is a physical layout, and a physical layout may not change an
    /// answer ([storage contract §3](../../../docs/STORAGE-CONTRACT.md)).
    pub cold: Arc<crate::storage::CachedObjectStore>,
}

impl Engine {
    /// The default cold-tier store: a cache over a local backend rooted at the store, so a cold
    /// read fetches `parts/<id>/rerank.vec` from the local disk exactly as the mmap path did.
    fn default_cold(store: &Store) -> Arc<crate::storage::CachedObjectStore> {
        let backend = crate::storage::object::LocalObjectStore::new(store.root.clone());
        Arc::new(crate::storage::CachedObjectStore::new(
            Arc::new(backend),
            crate::storage::CACHE_QUOTA_BYTES,
        ))
    }

    pub fn init(root: &Path, config: StoreConfig) -> Result<Engine> {
        let store = Store::init(root, config)?;
        let cold = Self::default_cold(&store);
        Ok(Engine {
            store,
            plane: Arc::new(HashModelPlane::new()),
            cold,
        })
    }

    pub fn open(root: &Path) -> Result<Engine> {
        let store = Store::open(root)?;
        let cold = Self::default_cold(&store);
        Ok(Engine {
            store,
            plane: Arc::new(HashModelPlane::new()),
            cold,
        })
    }

    pub fn with_plane(mut self, plane: Arc<dyn ModelPlane>) -> Self {
        self.plane = plane;
        self
    }

    /// Override the cold-tier object store (a fresh cache, a fault-injecting backend, or an
    /// `S3ObjectStore` for the remote gate). Used by the answer-invariance gate to force cache
    /// states and inject remote faults.
    pub fn with_cold(mut self, cold: Arc<crate::storage::CachedObjectStore>) -> Self {
        self.cold = cold;
        self
    }

    pub fn catalog(&self) -> Catalog<'_> {
        Catalog::new(&self.store)
    }

    /// Pin the live snapshot. A query holds this for its whole lifetime
    /// (invariant 4): parts cannot be pulled out from under it, because nothing
    /// mutates and GC only reclaims what no retained snapshot names.
    pub fn snapshot(&self) -> Result<Snapshot> {
        self.catalog().current()
    }

    /// Open the manifests of every part in a snapshot.
    ///
    /// Used by merge, verify, and the exact oracle — the operations that legitimately need the
    /// whole store. **The query path does not use this.** It uses [`Engine::open_candidates`],
    /// which opens only the parts the catalog says a query could possibly need, and that
    /// distinction is the S4 gate: a tenant-A query must never touch a byte of another tenant's
    /// partition.
    pub fn open_parts(&self, snap: &Snapshot) -> Result<Vec<PartReader>> {
        snap.part_ids()
            .iter()
            .map(|p| PartReader::open(&self.store.part_dir(p)))
            .collect()
    }

    /// Open only the parts a query could possibly need — **pruned in the catalog, before a
    /// single part byte is read.**
    ///
    /// This is where "cross-tenant reads are physically impossible" stops being a slogan and
    /// becomes an I/O property. A part outside the query's partitions is never opened, never
    /// checksummed, never read. Fill another tenant's partitions with unreadable garbage and a
    /// tenant-A query still answers correctly, because it never looked.
    ///
    /// Returns `(readers, parts_pruned)` — the count is a *measured* fact for the counters, not
    /// an estimate.
    pub fn open_candidates(
        &self,
        snap: &Snapshot,
        tenant: Option<&str>,
        from: Option<i64>,
        to: Option<i64>,
    ) -> Result<(Vec<PartReader>, usize)> {
        let ids = snap.candidate_parts(tenant, from, to);
        let pruned = snap.parts.len() - ids.len();
        let readers = ids
            .iter()
            .map(|p| PartReader::open(&self.store.part_dir(p)))
            .collect::<Result<Vec<_>>>()?;
        Ok((readers, pruned))
    }
}

/// Wall-clock milliseconds. Passed explicitly into anything that records a
/// timestamp so that tests and fixtures are not at the mercy of the clock.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
