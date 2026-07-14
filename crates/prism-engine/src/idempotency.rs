//! The idempotency index (S2).
//!
//! Answers exactly one question: **have I already accepted this exact event?**
//!
//! Three outcomes, and the distinction between the second and the third is the
//! entire reason this file exists (see the ingestion contract, §2):
//!
//! | key | content hash | verdict |
//! |---|---|---|
//! | new | — | **new** — admit it |
//! | seen | **same** | **replay** — acknowledge, do not store again |
//! | seen | **different** | **conflict** — dead-letter it |
//!
//! A **replay** is a producer doing exactly the right thing: retrying after an ack
//! that got lost, or a source re-delivering after our crash. Punishing them for it
//! is how you teach producers to drop data on error.
//!
//! A **conflict** is something else entirely: an id that was supposed to identify
//! one event now identifies two. Last-write-wins here is the seductive option and
//! it is wrong — it silently rewrites history under a reused id, and it makes the
//! system's behaviour depend on message *arrival order*, which no producer
//! controls. We refuse, loudly, and keep both: the stored one, and the rejected one
//! in the dead-letter log where a human can compare them.

use prism_types::error::Result;
use prism_types::limits::{IDEMPOTENCY_MAX_ENTRIES, IDEMPOTENCY_WINDOW_MS};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    New,
    /// Same key, same content. Ack it; do not store it again.
    Replay,
    /// Same key, different content. Refuse.
    Conflict {
        stored_hash: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    content_hash: String,
    /// The `event_time` of the event, which is what the retention window is
    /// measured in. Not `observed_time`: a replay arrives later by definition, and
    /// a window measured in arrival time would expire an event *because* it was
    /// retried.
    event_time: i64,
}

/// A bounded, durable index of what we have already accepted.
///
/// **Bounded, and honest about it.** Beyond `IDEMPOTENCY_WINDOW_MS` a replayed key
/// is no longer recognised and will be admitted as a new event — becoming a
/// duplicate *row*, which merge then reconciles by last-write-wins on `event_time`
/// (D-012). Two mechanisms, one seam, and the seam is documented rather than
/// pretended away.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IdempotencyIndex {
    /// `tenant \u{1f} key` → entry.
    ///
    /// A single composite string, not a `(String, String)` tuple: JSON object keys
    /// must be strings, and a tuple key serializes to nothing at all. The separator
    /// is the ASCII unit separator, which cannot occur in a tenant id or an
    /// idempotency key, so the composite is unambiguous.
    entries: BTreeMap<String, Entry>,
}

/// The composite key. `\u{1f}` is the ASCII unit separator — it cannot appear in a
/// tenant id or an idempotency key, so `("a\u{1f}b", "c")` and `("a", "b\u{1f}c")`
/// cannot collide.
fn ck(tenant: &str, key: &str) -> String {
    format!("{tenant}\u{1f}{key}")
}

impl IdempotencyIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// What is this event, as far as we are concerned?
    pub fn check(&self, tenant: &str, key: &str, content_hash: &str) -> Verdict {
        match self.entries.get(&ck(tenant, key)) {
            None => Verdict::New,
            Some(e) if e.content_hash == content_hash => Verdict::Replay,
            Some(e) => Verdict::Conflict {
                stored_hash: e.content_hash.clone(),
            },
        }
    }

    /// Record an accepted event.
    ///
    /// Called **at publication**, never at admission. Invariant 7: an idempotency
    /// record advances with-or-after publication, never before. If we recorded a
    /// key and then crashed before the part was written, the retry would be
    /// suppressed as a "replay" of an event that does not exist anywhere — the
    /// event would be silently, permanently lost, and the index would be the thing
    /// that lost it.
    pub fn record(&mut self, tenant: &str, key: &str, content_hash: &str, event_time: i64) {
        self.entries.insert(
            ck(tenant, key),
            Entry {
                content_hash: content_hash.to_string(),
                event_time,
            },
        );
    }

    /// Drop entries outside the window, and hard-cap the total.
    ///
    /// The cap is the backstop for a store whose clock never advances: without it,
    /// a window that never rolls over is not a bound.
    pub fn prune(&mut self, now_ms: i64) -> usize {
        let cutoff = now_ms - IDEMPOTENCY_WINDOW_MS;
        let before = self.entries.len();
        self.entries.retain(|_, e| e.event_time >= cutoff);

        if self.entries.len() > IDEMPOTENCY_MAX_ENTRIES {
            // Evict the oldest by event_time. A BTreeMap is keyed by (tenant, key),
            // so this costs a sort — acceptable, because it only runs when the hard
            // cap is hit, which means something has already gone wrong upstream.
            let mut by_time: Vec<(String, i64)> = self
                .entries
                .iter()
                .map(|(k, v)| (k.clone(), v.event_time))
                .collect();
            by_time.sort_by_key(|(_, t)| *t);
            let excess = self.entries.len() - IDEMPOTENCY_MAX_ENTRIES;
            for (k, _) in by_time.into_iter().take(excess) {
                self.entries.remove(&k);
            }
        }
        before - self.entries.len()
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let bytes = std::fs::read(path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Persist. Written atomically, and — critically — **after** the catalog
    /// commit that made the events visible.
    pub fn save(&self, path: &Path) -> Result<()> {
        prism_part::io::write_atomic(path, &serde_json::to_vec(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_key_is_new() {
        let ix = IdempotencyIndex::new();
        assert_eq!(ix.check("t1", "e1", "h1"), Verdict::New);
    }

    #[test]
    fn the_same_key_with_the_same_content_is_a_replay() {
        let mut ix = IdempotencyIndex::new();
        ix.record("t1", "e1", "h1", 1_000);
        assert_eq!(ix.check("t1", "e1", "h1"), Verdict::Replay);
    }

    #[test]
    fn the_same_key_with_different_content_is_a_conflict_not_an_overwrite() {
        let mut ix = IdempotencyIndex::new();
        ix.record("t1", "e1", "h1", 1_000);
        match ix.check("t1", "e1", "h2") {
            Verdict::Conflict { stored_hash } => assert_eq!(stored_hash, "h1"),
            other => panic!("expected a conflict, got {other:?}"),
        }
        // And the stored entry is untouched: a conflict never rewrites history.
        assert_eq!(ix.check("t1", "e1", "h1"), Verdict::Replay);
    }

    #[test]
    fn keys_are_scoped_per_tenant() {
        // One tenant must never be able to suppress another tenant's event by
        // guessing an id.
        let mut ix = IdempotencyIndex::new();
        ix.record("t1", "e1", "h1", 1_000);
        assert_eq!(ix.check("t2", "e1", "hDIFFERENT"), Verdict::New);
    }

    #[test]
    fn the_window_expires_by_event_time_not_arrival_time() {
        // Subtle and load-bearing. A replay arrives *later by definition*. If the
        // window were measured in arrival time, an event could be expired from the
        // index precisely *because* it was retried.
        let mut ix = IdempotencyIndex::new();
        ix.record("t1", "old", "h", 1_000);
        ix.record("t1", "new", "h", 1_000 + IDEMPOTENCY_WINDOW_MS);

        let pruned = ix.prune(1_000 + IDEMPOTENCY_WINDOW_MS + 1);
        assert_eq!(pruned, 1);
        assert_eq!(ix.check("t1", "old", "h"), Verdict::New);
        assert_eq!(ix.check("t1", "new", "h"), Verdict::Replay);
    }

    #[test]
    fn the_index_is_hard_capped_even_if_the_window_never_rolls_over() {
        let mut ix = IdempotencyIndex::new();
        for i in 0..(IDEMPOTENCY_MAX_ENTRIES + 100) {
            ix.record("t", &format!("k{i}"), "h", 1_000 + i as i64);
        }
        ix.prune(1_000);
        assert!(ix.len() <= IDEMPOTENCY_MAX_ENTRIES);
        // The oldest went first.
        assert_eq!(ix.check("t", "k0", "h"), Verdict::New);
    }

    #[test]
    fn it_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("prism-idem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("idem.json");

        let mut ix = IdempotencyIndex::new();
        ix.record("t1", "e1", "h1", 5);
        ix.save(&p).unwrap();

        let back = IdempotencyIndex::load(&p).unwrap();
        assert_eq!(back.check("t1", "e1", "h1"), Verdict::Replay);
        assert_eq!(
            back.check("t1", "e1", "h2"),
            Verdict::Conflict {
                stored_hash: "h1".into()
            }
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_index_file_is_an_empty_index_not_an_error() {
        let ix = IdempotencyIndex::load(Path::new("/nonexistent/idem.json")).unwrap();
        assert!(ix.is_empty());
    }
}
