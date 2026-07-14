//! Sources and offsets (S2) — where invariant 7 lives.
//!
//! > **Invariant 7:** source offsets advance **with-or-after** publication, never
//! > before.
//!
//! The `Source` trait has exactly Kafka's shape — poll, then commit an offset —
//! because the semantics are the interesting part and the broker is not. Wiring an
//! actual broker is a transport detail; getting *this* wrong loses data
//! permanently, and no amount of correct transport will get it back.
//!
//! **Offsets may lag reality. They must never lead it.**
//!
//! - Lagging costs a redundant poll: the source re-delivers events we already have,
//!   and the idempotency index recognises every one of them as a replay.
//! - Leading loses data: the source believes we have events we never published, and
//!   nothing will ever deliver them again.
//!
//! So the offset is committed *after* the catalog commit, and never on ack, and
//! never on WAL append. The `ingest.after_publish_before_offset_commit` kill point
//! exists to prove the lagging case is survivable.

use prism_part::io;
use prism_types::error::Result;
use prism_types::event::Event;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A batch polled from a source, with the offset it ends at.
pub struct Batch {
    pub events: Vec<Event>,
    /// The offset to commit *once these events are published*. Not before.
    pub next_offset: u64,
}

pub trait Source {
    fn name(&self) -> &str;

    /// The offset we have durably published up to.
    fn committed_offset(&self) -> Result<u64>;

    /// Read forward from the committed offset. Idempotent: polling twice without
    /// committing returns the same events, which is exactly what a replay is.
    fn poll(&self, max: usize) -> Result<Batch>;

    /// Advance the committed offset. **Only ever called after publication.**
    fn commit(&self, offset: u64) -> Result<()>;
}

/// A file-backed source with a durable committed offset.
///
/// The file is the log; the offset is a line number. It is deliberately dull, and
/// it is a faithful model of the thing that matters: an external cursor that we
/// must not advance before we have made the data durable and visible.
pub struct FileSource {
    name: String,
    data: PathBuf,
    state_dir: PathBuf,
}

#[derive(Serialize, Deserialize, Default)]
struct OffsetFile {
    offset: u64,
}

/// Where a source's committed offset lives. A plain file, because the offset is
/// the one piece of ingestion state that must survive everything else.
pub fn offset_path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(format!("{name}.offset"))
}

pub fn committed_offset(state_dir: &Path, name: &str) -> Result<u64> {
    let p = offset_path(state_dir, name);
    if !p.exists() {
        return Ok(0);
    }
    let o: OffsetFile = serde_json::from_slice(&std::fs::read(&p)?)?;
    Ok(o.offset)
}

/// Advance a source's committed offset. **Only ever after publication.**
///
/// Free-standing rather than a method, because *recovery* has to do this too — and
/// recovery does not have a live `Source` object, it has a WAL record that
/// remembers which source and which offset. A recovery path that could not advance
/// an offset would replay the same batch forever.
pub fn commit_offset(state_dir: &Path, name: &str, offset: u64) -> Result<()> {
    io::ensure_dir(state_dir)?;
    io::write_atomic(
        &offset_path(state_dir, name),
        &serde_json::to_vec(&OffsetFile { offset })?,
    )
}

impl FileSource {
    pub fn new(name: &str, data: &Path, state_dir: &Path) -> Result<Self> {
        io::ensure_dir(state_dir)?;
        Ok(FileSource {
            name: name.to_string(),
            data: data.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
        })
    }

    fn lines(&self) -> Result<Vec<String>> {
        let text = std::fs::read_to_string(&self.data)?;
        Ok(text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|s| s.to_string())
            .collect())
    }
}

impl Source for FileSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn committed_offset(&self) -> Result<u64> {
        committed_offset(&self.state_dir, &self.name)
    }

    fn poll(&self, max: usize) -> Result<Batch> {
        let lines = self.lines()?;
        let start = self.committed_offset()? as usize;
        let end = (start + max).min(lines.len());

        let mut events = Vec::new();
        for line in lines.iter().take(end).skip(start) {
            // A source that cannot parse a line has a schema problem, not a
            // delivery problem. Parse failures become dead letters upstream; here we
            // simply refuse to guess.
            events.push(serde_json::from_str::<Event>(line)?);
        }
        Ok(Batch {
            events,
            next_offset: end as u64,
        })
    }

    fn commit(&self, offset: u64) -> Result<()> {
        // Atomic. A source offset lost in a crash goes *backwards*, which is safe;
        // one that is half-written and reads as garbage is not.
        commit_offset(&self.state_dir, &self.name, offset)
    }
}

/// An in-memory source, for tests that need to control delivery precisely.
pub struct MemorySource {
    name: String,
    events: Vec<Event>,
    state_dir: PathBuf,
}

impl MemorySource {
    pub fn new(name: &str, events: Vec<Event>, state_dir: &Path) -> Result<Self> {
        io::ensure_dir(state_dir)?;
        Ok(MemorySource {
            name: name.to_string(),
            events,
            state_dir: state_dir.to_path_buf(),
        })
    }
}

impl Source for MemorySource {
    fn name(&self) -> &str {
        &self.name
    }

    fn committed_offset(&self) -> Result<u64> {
        committed_offset(&self.state_dir, &self.name)
    }

    fn poll(&self, max: usize) -> Result<Batch> {
        let start = self.committed_offset()? as usize;
        let end = (start + max).min(self.events.len());
        Ok(Batch {
            events: self.events[start..end].to_vec(),
            next_offset: end as u64,
        })
    }

    fn commit(&self, offset: u64) -> Result<()> {
        commit_offset(&self.state_dir, &self.name, offset)
    }
}
