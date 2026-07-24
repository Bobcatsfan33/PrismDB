//! **Single-node write ownership by monotonic epoch** (S12, [D-076](../../../../docs/DECISIONS.md)).
//!
//! One node owns a shard's write path at a time. Ownership is a monotonic **epoch** recorded in the
//! object store as create-only `catalog/OWNER-<epoch>` objects (the `put_if_absent` /
//! `If-None-Match: *` CAS primitive — the only one the backend guarantees). A writer **acquires** the
//! next epoch above the current highest on start; a fresh process after a crash simply acquires a
//! higher one. The catalog commit then **fences on the write path**: it refuses, by name, to publish
//! under an epoch that a later acquisition has superseded — so a paused/zombie writer that a restart
//! overtook cannot tear the catalog or duplicate parts by completing an in-flight publish.
//!
//! This is deliberately **not** cross-node failover: the hot tier and admission log are local, so a
//! different node cannot honour [D-068](../../../../docs/DECISIONS.md)'s ack contract for
//! acked-but-unpublished data. Ownership here fences a *stale local writer*; taking a shard over on a
//! new node is the named HA increment (remote-durable admission log + transport), not this one.

use crate::storage::object::{cas_publish, CasOutcome, ObjectStore};
use prism_types::error::{PrismError, Result};

/// The key prefix under which ownership epochs live — sibling to the `catalog/SNAPSHOT-` mirror,
/// and, like it, ignored by remote-orphan reconciliation (which never sweeps the unrecognised).
const OWNER_PREFIX: &str = "catalog/OWNER-";

/// Zero-padded so lexicographic key order is numeric epoch order — 20 digits holds any `u64`.
fn owner_key(epoch: u64) -> String {
    format!("{OWNER_PREFIX}{epoch:020}")
}

/// The highest ownership epoch currently recorded, or `0` if none has ever been acquired. `list` is a
/// directory lister (its argument is a directory prefix, as reconcile uses it), so we enumerate the
/// `catalog/` directory and filter the `OWNER-` keys ourselves — the owners are files directly under
/// it, siblings of the `SNAPSHOT-` mirror keys.
pub fn highest_epoch(store: &dyn ObjectStore) -> Result<u64> {
    let mut max = 0u64;
    for key in store.list("catalog/")? {
        if let Some(rest) = key.strip_prefix(OWNER_PREFIX) {
            if let Ok(e) = rest.parse::<u64>() {
                max = max.max(e);
            }
        }
    }
    Ok(max)
}

/// **Acquire write ownership**: create the next epoch above the current highest, tagged with this
/// writer's unique id. On a lost race for that epoch (another writer created it first,
/// [`CasOutcome::Conflict`]) we retry above the new highest; [`cas_publish`]'s read-back
/// ([D-067](../../../../docs/DECISIONS.md)) distinguishes our own landed create from a rival's, so an
/// ambiguous backend failure never double-counts. Returns the epoch this writer now holds.
pub fn acquire(store: &dyn ObjectStore, writer_id: &str) -> Result<u64> {
    loop {
        let next = highest_epoch(store)?.saturating_add(1);
        match cas_publish(store, &owner_key(next), writer_id.as_bytes())? {
            CasOutcome::Created | CasOutcome::AlreadyOurs => return Ok(next),
            CasOutcome::Conflict => continue,
        }
    }
}

/// **Fence the write path** ([D-076](../../../../docs/DECISIONS.md)): refuse, by name, if a higher
/// epoch than `my_epoch` has since acquired the shard. Called immediately before a catalog commit, so
/// a writer that was paused mid-publication while a restart took over publishes **nothing** — no torn
/// catalog, no duplicate parts. A no-op for the common case (this writer is still the highest).
pub fn assert_owner(store: &dyn ObjectStore, my_epoch: u64) -> Result<()> {
    let highest = highest_epoch(store)?;
    if highest > my_epoch {
        return Err(PrismError::Invariant(format!(
            "write fenced: this writer holds ownership epoch {my_epoch}, but epoch {highest} has \
             since acquired the shard ([D-076](docs/DECISIONS.md)). Refusing to publish behind the \
             current owner — no torn catalog, no duplicate parts. Re-acquire ownership to write."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::object::LocalObjectStore;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("prism-own-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn acquisition_is_monotonic_and_fences_the_superseded() {
        let store = LocalObjectStore::new(tmp("mono"));

        assert_eq!(highest_epoch(&store).unwrap(), 0, "no owner yet");
        let e1 = acquire(&store, "writer-1").unwrap();
        let e2 = acquire(&store, "writer-2").unwrap();
        assert!(
            e2 > e1,
            "a second acquisition takes a strictly higher epoch"
        );

        // The highest owner (writer 2) may publish; the superseded writer 1 is fenced by name.
        assert!(assert_owner(&store, e2).is_ok());
        let err = assert_owner(&store, e1).expect_err("epoch 1 was superseded and must be fenced");
        assert!(
            err.to_string().contains("write fenced") && err.to_string().contains("D-076"),
            "the fence must be named: {err}"
        );
    }
}
