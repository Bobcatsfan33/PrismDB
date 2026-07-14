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
}

impl Engine {
    pub fn init(root: &Path, config: StoreConfig) -> Result<Engine> {
        let store = Store::init(root, config)?;
        Ok(Engine {
            store,
            plane: Arc::new(HashModelPlane::new()),
        })
    }

    pub fn open(root: &Path) -> Result<Engine> {
        Ok(Engine {
            store: Store::open(root)?,
            plane: Arc::new(HashModelPlane::new()),
        })
    }

    pub fn with_plane(mut self, plane: Arc<dyn ModelPlane>) -> Self {
        self.plane = plane;
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

    /// Open the manifests of every part in a snapshot. Manifests only — no
    /// column file is touched, which is what makes pruning free.
    pub fn open_parts(&self, snap: &Snapshot) -> Result<Vec<PartReader>> {
        snap.parts
            .iter()
            .map(|p| PartReader::open(&self.store.part_dir(p)))
            .collect()
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
