//! The catalog: the only thing that makes data visible.
//!
//! A part on disk is not data. A part *named by the snapshot that `CURRENT`
//! points at* is data. That indirection is what buys us:
//!
//!   * crash recovery for free — a torn write leaves an orphan, not a hybrid
//!   * publication as a single atomic rename (invariant 3)
//!   * readers pinning a snapshot for a query's lifetime (invariant 4)
//!   * rollback as a catalog write, never a data rewrite
//!   * GC as a separate operation that runs *outside* publication (invariant 5)
//!
//! The last one is the one people get wrong. If GC ran inside the commit, a
//! reader holding an older snapshot could have the objects underneath it
//! deleted mid-query. So reclamation is explicit, retains history, and only
//! ever removes what no retained snapshot names.

use crate::faults;
use crate::generation::Generation;
use crate::io;
use crate::part::PartReader;
use crate::store::Store;
use prism_types::error::{PrismError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub snapshot_id: String,
    pub parent: Option<String>,
    /// Every part visible in this snapshot.
    pub parts: Vec<String>,
    /// The next part sequence number to hand out. Lives in the catalog so part
    /// naming is a property of the committed state, not of a writer's memory.
    pub next_seq: u64,
    /// The generation new writes are encoded under. Parts under older
    /// generations remain readable; each brings its own ADC table.
    pub active_generation: Option<String>,
    pub created_at_ms: i64,
}

impl Snapshot {
    pub fn empty() -> Self {
        Snapshot {
            snapshot_id: "s00000000".to_string(),
            parent: None,
            parts: Vec::new(),
            next_seq: 1,
            active_generation: None,
            created_at_ms: 0,
        }
    }
}

pub struct Catalog<'a> {
    store: &'a Store,
}

impl<'a> Catalog<'a> {
    pub fn new(store: &'a Store) -> Self {
        Catalog { store }
    }

    /// The live snapshot. An empty catalog reads as the empty snapshot rather
    /// than an error: a fresh store is a valid store with no rows.
    pub fn current(&self) -> Result<Snapshot> {
        let cur = self.store.current_path();
        if !cur.exists() {
            return Ok(Snapshot::empty());
        }
        let id = String::from_utf8(io::read_file(&cur)?)
            .map_err(|e| PrismError::Corrupt(format!("CURRENT is not utf-8: {e}")))?
            .trim()
            .to_string();
        self.load_snapshot(&id)
    }

    pub fn load_snapshot(&self, id: &str) -> Result<Snapshot> {
        let path = self.store.snapshots_dir().join(format!("{id}.json"));
        if !path.exists() {
            return Err(PrismError::Corrupt(format!(
                "CURRENT names snapshot `{id}`, which does not exist"
            )));
        }
        let snap: Snapshot = serde_json::from_slice(&io::read_file(&path)?)?;
        if snap.snapshot_id != id {
            return Err(PrismError::Corrupt(format!(
                "snapshot file `{id}` declares id `{}`",
                snap.snapshot_id
            )));
        }
        Ok(snap)
    }

    pub fn list_snapshots(&self) -> Result<Vec<String>> {
        let mut ids: Vec<String> = Vec::new();
        for e in fs::read_dir(self.store.snapshots_dir())? {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(id) = name.strip_suffix(".json") {
                ids.push(id.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }

    fn next_snapshot_id(&self, parent: &Snapshot) -> Result<String> {
        let n: u64 = parent
            .snapshot_id
            .trim_start_matches('s')
            .parse()
            .map_err(|_| {
                PrismError::Corrupt(format!("malformed snapshot id `{}`", parent.snapshot_id))
            })?;
        Ok(format!("s{:08}", n + 1))
    }

    /// Publish a new snapshot. This is the commit point, and the only one.
    ///
    /// Every part named must already be durable and openable — we check, rather
    /// than trust the caller, because invariant 2 says a snapshot references
    /// only durable, checksum-valid parts, and a snapshot that names a missing
    /// part is a store that will not open.
    pub fn commit(
        &self,
        parent: &Snapshot,
        parts: Vec<String>,
        next_seq: u64,
        active_generation: Option<String>,
        now_ms: i64,
    ) -> Result<Snapshot> {
        for p in &parts {
            let dir = self.store.part_dir(p);
            PartReader::open(&dir).map_err(|e| {
                PrismError::Invariant(format!(
                    "refusing to commit a snapshot naming part `{p}`, which is not readable: {e}"
                ))
            })?;
        }

        let snap = Snapshot {
            snapshot_id: self.next_snapshot_id(parent)?,
            parent: Some(parent.snapshot_id.clone()),
            parts,
            next_seq,
            active_generation,
            created_at_ms: now_ms,
        };

        let path = self
            .store
            .snapshots_dir()
            .join(format!("{}.json", snap.snapshot_id));
        io::write_atomic(&path, &serde_json::to_vec_pretty(&snap)?)?;
        faults::maybe_kill("snapshot.after_write_before_current");

        // The instant this rename lands, the new data is live. Before it, the
        // old snapshot is live. There is no third state.
        io::write_atomic(&self.store.current_path(), snap.snapshot_id.as_bytes())?;
        faults::maybe_kill("current.after_rename");

        Ok(snap)
    }

    // --- generations ---

    pub fn put_generation(&self, g: &Generation) -> Result<()> {
        g.verify_content_address()?;
        let path = self.store.generation_path(&g.generation_id);
        if path.exists() {
            // Content-addressed: it is already exactly this. Writing it again
            // would be a no-op at best and a mutation at worst.
            return Ok(());
        }
        io::write_atomic(&path, &serde_json::to_vec_pretty(g)?)?;
        Ok(())
    }

    pub fn get_generation(&self, id: &str) -> Result<Generation> {
        let path = self.store.generation_path(id);
        if !path.exists() {
            return Err(PrismError::NotFound(format!("generation `{id}`")));
        }
        let g: Generation = serde_json::from_slice(&io::read_file(&path)?)?;
        g.verify_content_address()?;
        Ok(g)
    }

    // --- garbage collection: explicit, and never on the publish path ---

    /// Remove parts and snapshots that no retained snapshot references.
    ///
    /// `retain_snapshots` is how many recent snapshots stay reachable. It is the
    /// S0 stand-in for reader leases (invariant 6): with no lease service yet,
    /// keeping the last N snapshots is what guarantees that a reader which
    /// pinned a snapshot still has its parts underneath it. Setting it to 1
    /// makes rollback impossible, which is why the default is not 1.
    pub fn gc(&self, retain_snapshots: usize, dry_run: bool) -> Result<GcReport> {
        let retain = retain_snapshots.max(1);
        let current = self.current()?;
        let all_snaps = self.list_snapshots()?;

        // Keep the newest `retain` snapshots, and always the live one.
        let keep_snaps: BTreeSet<String> = all_snaps
            .iter()
            .rev()
            .take(retain)
            .cloned()
            .chain(std::iter::once(current.snapshot_id.clone()))
            .collect();

        // Anything a retained snapshot names is live and must not be touched.
        let mut referenced: BTreeSet<String> = BTreeSet::new();
        for id in &keep_snaps {
            if let Ok(s) = self.load_snapshot(id) {
                referenced.extend(s.parts);
            }
        }

        let mut removed_parts = Vec::new();
        let mut removed_snapshots = Vec::new();
        let mut first = true;

        for entry in fs::read_dir(self.store.parts_dir())? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            // A `.tmp` directory is a part that never got published: pure orphan.
            let is_orphan_tmp = name.ends_with(".tmp");
            if referenced.contains(&name) && !is_orphan_tmp {
                continue;
            }
            removed_parts.push(name.clone());
            if !dry_run {
                fs::remove_dir_all(entry.path())?;
                if first {
                    faults::maybe_kill("gc.after_first_unlink");
                    first = false;
                }
            }
        }

        for id in &all_snaps {
            if keep_snaps.contains(id) {
                continue;
            }
            removed_snapshots.push(id.clone());
            if !dry_run {
                fs::remove_file(self.store.snapshots_dir().join(format!("{id}.json")))?;
            }
        }

        removed_parts.sort();
        removed_snapshots.sort();
        Ok(GcReport {
            dry_run,
            retained_snapshots: keep_snaps.into_iter().collect(),
            removed_parts,
            removed_snapshots,
        })
    }

    /// Full integrity audit: every part in the live snapshot opens, every
    /// checksum matches, every generation hashes to its own id.
    pub fn verify(&self) -> Result<VerifyReport> {
        let snap = self.current()?;
        let mut parts_ok = 0usize;
        let mut generations: BTreeSet<String> = BTreeSet::new();

        for p in &snap.parts {
            let r = PartReader::open(&self.store.part_dir(p))?;
            r.verify()?;
            generations.insert(r.manifest.generation_id.clone());
            parts_ok += 1;
        }
        for g in &generations {
            self.get_generation(g)?;
        }

        Ok(VerifyReport {
            snapshot_id: snap.snapshot_id,
            parts_verified: parts_ok,
            generations_verified: generations.into_iter().collect(),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GcReport {
    pub dry_run: bool,
    pub retained_snapshots: Vec<String>,
    pub removed_parts: Vec<String>,
    pub removed_snapshots: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerifyReport {
    pub snapshot_id: String,
    pub parts_verified: usize,
    pub generations_verified: Vec<String>,
}
