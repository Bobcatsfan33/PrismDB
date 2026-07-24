use crate::model::{HashModelPlane, ModelPlane};
use prism_part::catalog::{Catalog, Snapshot};
use prism_part::part::PartReader;
use prism_part::store::{Store, StoreConfig};
use prism_types::error::Result;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Distinguishes ownership acquisitions of two engines opened in the same process/millisecond.
static WRITER_NONCE: AtomicU64 = AtomicU64::new(0);

pub struct Engine {
    pub store: Store,
    pub plane: Arc<dyn ModelPlane>,
    /// The cold tier's object store, with a content-verified cache in front (S11). Every exact
    /// rerank-vector fetch goes through this, so the cold tier can be local (the default backend
    /// points at the store's own `parts/` directory — behaviour-preserving) or remote (an
    /// `S3ObjectStore`). A cache state is a physical layout, and a physical layout may not change an
    /// answer ([storage contract §3](../../../docs/STORAGE-CONTRACT.md)).
    pub cold: Arc<crate::storage::CachedObjectStore>,
    /// The write-ownership epoch this engine holds ([D-076](../../../docs/DECISIONS.md)), or `0` if it
    /// has not acquired ownership — a reader, or a writer before its first owned publish. Interior-
    /// mutable so `&self` write methods can acquire lazily and the commit path can fence on it.
    owner_epoch: AtomicU64,
    /// A process-unique id tagging this engine's ownership acquisitions.
    writer_id: String,
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

    fn writer_id() -> String {
        format!(
            "{}-{}",
            std::process::id(),
            WRITER_NONCE.fetch_add(1, Ordering::Relaxed)
        )
    }

    pub fn init(root: &Path, config: StoreConfig) -> Result<Engine> {
        let store = Store::init(root, config)?;
        let cold = Self::default_cold(&store);
        Ok(Engine {
            store,
            plane: Arc::new(HashModelPlane::new()),
            cold,
            owner_epoch: AtomicU64::new(0),
            writer_id: Self::writer_id(),
        })
    }

    pub fn open(root: &Path) -> Result<Engine> {
        let store = Store::open(root)?;
        let cold = Self::default_cold(&store);
        Ok(Engine {
            store,
            plane: Arc::new(HashModelPlane::new()),
            cold,
            owner_epoch: AtomicU64::new(0),
            writer_id: Self::writer_id(),
        })
    }

    /// **Acquire write ownership** for this engine ([D-076](../../../docs/DECISIONS.md)) — the next
    /// monotonic epoch in the object store. Idempotent within a process: once acquired it returns the
    /// held epoch. A writer calls this before it publishes, so the commit path can fence a stale
    /// writer a restart has overtaken. Returns the epoch now held.
    pub fn acquire_ownership(&self) -> Result<u64> {
        let held = self.owner_epoch.load(Ordering::SeqCst);
        if held != 0 {
            return Ok(held);
        }
        let e = crate::storage::ownership::acquire(self.cold.backend().as_ref(), &self.writer_id)?;
        self.owner_epoch.store(e, Ordering::SeqCst);
        Ok(e)
    }

    /// **Fence the write path** ([D-076](../../../docs/DECISIONS.md)): if this engine acquired
    /// ownership and a higher epoch has since taken the shard, refuse by name. A **no-op** for an
    /// engine that never acquired ownership (epoch `0`) — so the single-writer path is unchanged — and
    /// for the current owner. Called immediately before a catalog commit.
    pub fn assert_write_owner(&self) -> Result<()> {
        let held = self.owner_epoch.load(Ordering::SeqCst);
        if held == 0 {
            return Ok(());
        }
        crate::storage::ownership::assert_owner(self.cold.backend().as_ref(), held)
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
