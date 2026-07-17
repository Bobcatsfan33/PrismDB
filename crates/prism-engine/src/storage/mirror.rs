//! The catalog mirror (S11 boundary b, [D-069](../../../../docs/DECISIONS.md)) — the object store is
//! a **mirror, not a master**.
//!
//! Local `CURRENT` is the single-writer authority. After the local rename commits, the snapshot is
//! CAS-written to a versioned remote key `catalog/SNAPSHOT-<id>`; the mirror is **at-or-behind the
//! local truth by construction, never ahead**. A CAS conflict is **split-brain** — another writer
//! exists, the single-writer contract is violated — and halts loudly: *detection, not tolerance*
//! (tolerating two writers is S12's deliberate choice). A local catalog lost to disk failure is
//! restored from the **highest verified mirror snapshot**, and WAL replay ([D-068](../../../../docs/DECISIONS.md))
//! closes the gap between the mirror and the truth. A crash between the rename and the mirror write
//! is healed by re-mirroring on the next write — safe *precisely because* the remote never leads.

use crate::engine::Engine;
use crate::storage::object::{cas_publish, CasOutcome};
use prism_part::catalog::Snapshot;
use prism_part::part::PartReader;
use prism_types::error::{PrismError, Result};

/// The remote key of a catalog mirror snapshot — the scheme [remote-orphan reconciliation](super::cold)
/// already protects.
pub(crate) fn mirror_key(id: &str) -> String {
    format!("catalog/SNAPSHOT-{id}")
}

impl Engine {
    /// CAS-write a snapshot to the catalog mirror ([D-069](../../../../docs/DECISIONS.md)). Idempotent
    /// — an identical snapshot already at the key is our own landed write (the mirror never leads, so
    /// re-mirroring is always safe). A **different** snapshot at the same key is split-brain: another
    /// writer published concurrently, and we halt with the named condition rather than tolerate
    /// divergence. The empty snapshot has nothing to mirror.
    pub fn mirror_snapshot(&self, snap: &Snapshot) -> Result<()> {
        if snap.snapshot_id == Snapshot::empty().snapshot_id {
            return Ok(());
        }
        let key = mirror_key(&snap.snapshot_id);
        let bytes = serde_json::to_vec_pretty(snap)?;
        match cas_publish(self.cold.backend().as_ref(), &key, &bytes)? {
            CasOutcome::Created | CasOutcome::AlreadyOurs => Ok(()),
            CasOutcome::Conflict => Err(PrismError::Invariant(format!(
                "split-brain detected: the catalog mirror already holds a DIFFERENT snapshot at \
                 `{key}` — another writer has published concurrently, violating the single-writer \
                 contract (D-069). Halting rather than tolerating divergence."
            ))),
        }
    }

    /// Bring the mirror up to the local `CURRENT` if it is behind — the convergence that heals a
    /// crash between the `CURRENT` rename and the mirror write. Idempotent and safe because the remote
    /// never leads: re-writing the current snapshot can only add what a crash dropped.
    pub fn remirror_current(&self) -> Result<()> {
        let snap = self.snapshot()?;
        self.mirror_snapshot(&snap)
    }

    /// Restore the local catalog from the **highest verified mirror snapshot** — the disaster path
    /// when local `CURRENT` (and its snapshot files) are lost to disk failure ([D-069](../../../../docs/DECISIONS.md)).
    /// Lists the mirror, takes the highest seq, verifies it (deserializes, its id matches, and every
    /// part it names still opens — invariant 2), then writes the snapshot file and points `CURRENT`
    /// at it. Returns the restored id, or `None` if the mirror is empty. WAL replay
    /// ([D-068](../../../../docs/DECISIONS.md)) is the caller's next step, closing any gap between the
    /// mirror and the acked-but-unpublished truth.
    pub fn recover_catalog_from_mirror(&self) -> Result<Option<String>> {
        let backend = self.cold.backend();
        let mut ids: Vec<String> = backend
            .list("catalog/")?
            .iter()
            .filter_map(|k| k.strip_prefix("catalog/SNAPSHOT-").map(str::to_string))
            .collect();
        ids.sort();
        let Some(highest) = ids.pop() else {
            return Ok(None);
        };

        let key = mirror_key(&highest);
        let bytes = backend.get(&key)?;
        let snap: Snapshot = serde_json::from_slice(&bytes)?;
        if snap.snapshot_id != highest {
            return Err(PrismError::Corrupt(format!(
                "catalog mirror at `{key}` declares id `{}`, not `{highest}`",
                snap.snapshot_id
            )));
        }
        // Invariant 2: refuse to restore a snapshot that names a part that is not readable locally —
        // the hot tier is local, so a snapshot the mirror holds is only recoverable if its parts
        // survived. A missing part is a named refusal, never a silent half-restore.
        for p in &snap.part_ids() {
            PartReader::open(&self.store.part_dir(p)).map_err(|e| {
                PrismError::Invariant(format!(
                    "cannot restore mirror snapshot `{highest}`: it names part `{p}`, which is not \
                     readable locally: {e}"
                ))
            })?;
        }

        // Write the snapshot file byte-identical to the mirror, then swap CURRENT to it.
        prism_part::io::ensure_dir(&self.store.snapshots_dir())?;
        let path = self.store.snapshots_dir().join(format!("{highest}.json"));
        prism_part::io::write_atomic(&path, &bytes)?;
        prism_part::io::write_atomic(&self.store.current_path(), highest.as_bytes())?;
        Ok(Some(highest))
    }
}
