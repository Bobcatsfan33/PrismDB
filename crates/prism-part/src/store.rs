//! The on-disk store: directory layout and configuration.
//!
//! ```text
//! <root>/
//!   store.json                    config; written once at init, never mutated
//!   generations/<gen>.json        content-addressed, immutable
//!   parts/<part_id>/              immutable; manifest.json + column files
//!   catalog/CURRENT               one line: the id of the live snapshot
//!   catalog/snapshots/<id>.json   append-only history
//!   deadletter/*.jsonl            events that failed admission, visibly
//! ```

use crate::io;
use prism_types::error::{PrismError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The store-layout version recorded in `store.json`.
///
/// Distinct from the *part* format version, which every part carries in its own
/// manifest — a store can hold parts of several formats at once, which is
/// exactly what happens while a v1 store is being migrated forward by merges.
/// New stores are written at [`STORE_VERSION`]; older ones are still opened.
pub const STORE_VERSION: u32 = 2;

/// Store layouts this build can open.
pub const SUPPORTED_STORE_VERSIONS: &[u32] = &[1, 2];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StoreConfig {
    pub format_version: u32,
    pub dim: usize,
    /// Coarse centroid count. An *empirical* parameter, not a magic constant:
    /// its right value is an output of recall, skew and latency measurement on
    /// a real corpus (Part I §5.3).
    pub nlist: usize,
    /// PQ sub-quantizers; must divide `dim`. Bytes per coded row.
    pub pq_m: usize,
    /// Seed for every deterministic step, so a store is reproducible.
    pub seed: u64,
}

impl StoreConfig {
    pub fn validate(&self) -> Result<()> {
        if self.dim == 0 {
            return Err(PrismError::Invalid("dim must be positive".into()));
        }
        if self.pq_m == 0 || self.dim % self.pq_m != 0 {
            return Err(PrismError::Invalid(format!(
                "pq_m ({}) must be positive and divide dim ({})",
                self.pq_m, self.dim
            )));
        }
        if self.nlist == 0 {
            return Err(PrismError::Invalid("nlist must be positive".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Store {
    pub root: PathBuf,
    pub config: StoreConfig,
}

impl Store {
    pub fn init(root: &Path, config: StoreConfig) -> Result<Self> {
        config.validate()?;
        if root.join("store.json").exists() {
            return Err(PrismError::Invalid(format!(
                "{} is already a PrismDB store",
                root.display()
            )));
        }
        io::ensure_dir(root)?;
        io::ensure_dir(&root.join("generations"))?;
        io::ensure_dir(&root.join("parts"))?;
        io::ensure_dir(&root.join("catalog/snapshots"))?;
        io::ensure_dir(&root.join("deadletter"))?;

        io::write_atomic(
            &root.join("store.json"),
            &serde_json::to_vec_pretty(&config)?,
        )?;

        Ok(Store {
            root: root.to_path_buf(),
            config,
        })
    }

    pub fn open(root: &Path) -> Result<Self> {
        let path = root.join("store.json");
        if !path.exists() {
            return Err(PrismError::NotFound(format!(
                "{} is not a PrismDB store (no store.json)",
                root.display()
            )));
        }
        let config: StoreConfig = serde_json::from_slice(&io::read_file(&path)?)?;
        if !SUPPORTED_STORE_VERSIONS.contains(&config.format_version) {
            return Err(PrismError::Corrupt(format!(
                "store is layout version {}, this build reads {SUPPORTED_STORE_VERSIONS:?}",
                config.format_version
            )));
        }
        config.validate()?;
        Ok(Store {
            root: root.to_path_buf(),
            config,
        })
    }

    pub fn parts_dir(&self) -> PathBuf {
        self.root.join("parts")
    }
    pub fn part_dir(&self, part_id: &str) -> PathBuf {
        self.root.join("parts").join(part_id)
    }
    pub fn generations_dir(&self) -> PathBuf {
        self.root.join("generations")
    }
    pub fn generation_path(&self, gen_id: &str) -> PathBuf {
        self.root.join("generations").join(format!("{gen_id}.json"))
    }
    pub fn catalog_dir(&self) -> PathBuf {
        self.root.join("catalog")
    }
    pub fn snapshots_dir(&self) -> PathBuf {
        self.root.join("catalog/snapshots")
    }
    pub fn current_path(&self) -> PathBuf {
        self.root.join("catalog/CURRENT")
    }
    pub fn deadletter_path(&self) -> PathBuf {
        self.root.join("deadletter/deadletter.jsonl")
    }
}
