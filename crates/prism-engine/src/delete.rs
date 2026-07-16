//! Deletes are tombstones (S10) — [merge contract §6](../../../docs/MERGE-CONTRACT.md).
//!
//! A user delete does not rewrite or erase a part in place — nothing in this engine does. It writes
//! a **tombstone**: a durable record, committed as one atomic catalog transaction, that a set of
//! `event_id`s is deleted as of a snapshot. From that instant the rows are **logically deleted** —
//! excluded from every query answer (the search path filters them) — while they may still be
//! physically present until a merge reconciles them away. Reconciliation is **idempotent**:
//! replaying the same tombstone drops the same rows and no others.

use crate::engine::Engine;
use prism_part::catalog::SnapshotMeta;
use prism_types::error::Result;

impl Engine {
    /// Delete a set of `event_id`s. Writes a tombstone (one atomic catalog commit), so the delete
    /// is crash-safe old-or-new like every other commit, and the rows vanish from queries at once.
    /// Returns the number of ids newly tombstoned (a re-delete of an already-deleted id is a no-op,
    /// which is what makes delete idempotent).
    pub fn delete(&self, event_ids: &[String], now_ms: i64) -> Result<usize> {
        let snap = self.snapshot()?;
        let mut tombstones = snap.tombstones.clone();
        let before = tombstones.len();
        for id in event_ids {
            if let Err(pos) = tombstones.binary_search(id) {
                tombstones.insert(pos, id.clone());
            }
        }
        let added = tombstones.len() - before;
        if added == 0 {
            return Ok(0); // nothing new; do not churn the catalog
        }

        let mut meta = SnapshotMeta::of(&snap);
        meta.tombstones = tombstones;
        self.catalog().commit_meta(
            &snap,
            snap.parts.clone(),
            snap.next_seq,
            snap.active_generation.clone(),
            meta,
            now_ms,
        )?;
        Ok(added)
    }
}
