//! Kill points.
//!
//! Every durability boundary in the write path is named here, and the process
//! can be made to die at any of them via `PRISM_FAULT=<point>`. The fault suite
//! (`testing/faults/`) drives each one and asserts the store still opens to the
//! old snapshot or the new one, never a hybrid of the two.
//!
//! We `abort()` rather than `exit()` on purpose: no destructors, no flushes, no
//! chance to tidy up. That is what a crash is.

/// Every boundary the fault harness is required to cover.
pub const KILL_POINTS: &[&str] = &[
    // A part's column files exist but nothing has been flushed.
    "part.after_write_before_fsync",
    // Column files are durable; the part directory has not been renamed in.
    "part.after_fsync_before_rename",
    // The part directory is durable and visible; no snapshot names it yet.
    "part.after_rename_before_snapshot",
    // The snapshot file is durable; CURRENT still points at the old one.
    "snapshot.after_write_before_current",
    // CURRENT has been swapped; the process dies before returning.
    "current.after_rename",
    // GC has unlinked something.
    "gc.after_first_unlink",
    // A merge wrote its output part but has not committed the new snapshot.
    "merge.after_part_before_commit",
    // --- S2: the admission path ---
    //
    // The durable admission log is appended but not yet fsynced. Nothing has been
    // acked, so nothing may be lost.
    "wal.after_append_before_fsync",
    // **The crash that matters most.** The batch is acked (it is in the WAL), the
    // GPU time has been spent, and the events exist nowhere durable but the log.
    // Recovery must replay them -- exactly once, with their semantic columns.
    "ingest.after_embed_before_part",
    // Published and visible, but the source offset has not been advanced. The
    // source will re-deliver; idempotency must recognise every one as a replay.
    // Offsets may lag reality. They must never lead it.
    "ingest.after_publish_before_offset_commit",
];

/// Abort if the process was asked to die here.
#[inline]
pub fn maybe_kill(point: &str) {
    // Read the env var every time rather than caching it: the fault harness
    // sets it per-process, and a cached value would make the kill point
    // untestable from within a single test binary.
    if let Ok(want) = std::env::var("PRISM_FAULT") {
        if want == point {
            eprintln!("prism: injected fault at kill point `{point}`");
            std::process::abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_points_are_unique_and_nonempty() {
        let mut seen = std::collections::HashSet::new();
        for p in KILL_POINTS {
            assert!(!p.is_empty());
            assert!(seen.insert(*p), "duplicate kill point {p}");
        }
    }

    #[test]
    fn no_fault_set_is_a_no_op() {
        // The test process has no PRISM_FAULT, so this must simply return.
        maybe_kill("part.after_rename_before_snapshot");
    }
}
