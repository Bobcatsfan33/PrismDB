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
use crate::generation::{Generation, GenerationState};
use crate::io;
use crate::part::PartReader;
use crate::partition::{PartRef, PartitionKey};
use crate::store::Store;
use prism_types::error::{PrismError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

/// The maximum time a reader may hold a snapshot pinned across a paginated query's lifetime
/// (invariant 4 / invariant 6).
///
/// **Policy** ([C-3](../../../docs/DECISIONS.md)): a query that paginates for longer than this has
/// its lease *expire*, and its next page returns the explicit expired-snapshot error rather than a
/// stale answer — an hour is generous for an interactive paginating reader and short enough that a
/// crashed reader does not pin storage for long. This is the **one** lifecycle-timing constant; the
/// grace is derived from it ([`GC_GRACE_MS`]), never tuned beside it, so the two cannot drift into
/// `grace < lease` and orphan a live reader (merge contract §5).
pub const LEASE_TTL_MS: i64 = 60 * 60 * 1000;

/// GC grace — **derived** from the lease, not an independent constant. Extra slack beyond the lease
/// before a snapshot may be reclaimed, so the horizon is comfortably past the last moment any
/// reader could still be within its lease. One lease-length of grace is the boring choice.
pub const GC_GRACE_MS: i64 = LEASE_TTL_MS;

/// A snapshot may be reclaimed only once it is older than this — lease plus its derived grace.
pub const GC_HORIZON_MS: i64 = LEASE_TTL_MS + GC_GRACE_MS;

/// A part as the catalog knows it.
///
/// **Untagged**, so a snapshot written before S4 -- whose `parts` were bare strings -- still
/// deserializes. A legacy entry carries no partition metadata, so it cannot be pruned in the
/// catalog and falls back to opening its manifest. That is exactly the pre-S4 behaviour, and
/// it is why old snapshots keep working.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum PartEntry {
    /// S4 and later: enough metadata to prune this part **without opening it**.
    Located(PartRef),
    /// Pre-S4: a bare part id.
    Legacy(String),
}

impl PartEntry {
    pub fn part_id(&self) -> &str {
        match self {
            PartEntry::Located(r) => &r.part_id,
            PartEntry::Legacy(id) => id,
        }
    }

    pub fn located(&self) -> Option<&PartRef> {
        match self {
            PartEntry::Located(r) => Some(r),
            PartEntry::Legacy(_) => None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            PartEntry::Located(r) => r.validate(),
            PartEntry::Legacy(_) => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub snapshot_id: String,
    pub parent: Option<String>,
    /// Every part visible in this snapshot, **with the metadata needed to prune it before a
    /// single one of its bytes is read** (S4).
    ///
    /// This is where "cross-tenant reads are physically impossible" stops being a slogan.
    /// Until S4, pruning opened every part's manifest -- so a tenant-A query already touched
    /// tenant B's bytes, and one corrupt part broke every query in the store. Now a part
    /// outside a query's partitions is never opened, never checksummed, never read.
    pub parts: Vec<PartEntry>,
    /// The next part sequence number to hand out. Lives in the catalog so part
    /// naming is a property of the committed state, not of a writer's memory.
    pub next_seq: u64,
    /// The generation new writes are encoded under. Parts under older
    /// generations remain readable; each brings its own ADC table.
    pub active_generation: Option<String>,
    /// Where every known generation is in its lifecycle (S5).
    ///
    /// In the snapshot, not in the generation record: the record is content-addressed and
    /// immutable, and a lifecycle *state* is a fact about the store at an instant. So every
    /// transition — create, canary, promote, retire — is one atomic catalog commit, and
    /// **rollback restores the states along with the parts**, because they are the same object.
    #[serde(default)]
    pub generations: BTreeMap<String, GenerationState>,
    /// Declared cross-space score bridges (S5, §6 of the generation contract). Empty is the
    /// normal state, and it means a cross-space query is refused.
    #[serde(default)]
    pub bridges: Vec<Bridge>,
    /// Drift baselines, and — the point — the ones that are **degraded**.
    #[serde(default)]
    pub baselines: Vec<BaselineRef>,
    pub created_at_ms: i64,
}

/// A declared bridge between two embedding spaces (generation contract §6).
///
/// The default is that there is no bridge and a cross-space query is **refused**. A bridge is
/// somebody explicitly saying "these two may be answered together, this way", and it carries a
/// receipt because an unvalidated bridge is a guess with a schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bridge {
    pub from_space: String,
    pub to_space: String,
    pub policy: BridgePolicy,
    /// Why this bridge is believed to be sound. Free text pointing at a receipt — reviewable,
    /// which is the most a document can promise.
    pub validation: String,
    pub declared_at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgePolicy {
    /// Merge **ranks**, never scores.
    ///
    /// Each space ranks its own rows, in its own units, against its own query embedding, and
    /// the ranks — which are unitless — are fused. This *obeys* invariant 9 rather than working
    /// around it. A policy that averaged scores across spaces would be forbidden by the
    /// generation contract even if somebody implemented it.
    RankFusion,
}

impl Bridge {
    /// Does this bridge join these two spaces? Bridges are symmetric: a declaration that two
    /// spaces may be fused is not a statement about which one you asked from.
    pub fn joins(&self, a: &str, b: &str) -> bool {
        (self.from_space == a && self.to_space == b) || (self.from_space == b && self.to_space == a)
    }
}

/// A drift baseline, and its state (generation contract §7).
///
/// A baseline is a statement about a distribution **in one embedding space**. When the space
/// changes underneath it, the baseline is not stale — it is *meaningless*, and invariant 9
/// forbids comparing across it. So a baseline is pinned to its generation, and a migration is
/// not complete until every one of them has been rebuilt in the new space.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BaselineRef {
    pub baseline_id: String,
    pub tenant: String,
    pub generation_id: String,
    pub state: BaselineState,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BaselineState {
    Ready,
    /// The baseline could not be rebuilt in this generation's space — most often because the
    /// raw bodies it would have to re-embed have expired under retention.
    ///
    /// **This state exists so that an alarm is never silently absent.** A drift alarm that
    /// quietly stops firing is worse than one that was never configured, because a configured
    /// alarm is *trusted*. Degraded says the alarm is not running, names the reason, and keeps
    /// saying it on every evaluation.
    Degraded {
        reason: String,
    },
}

impl Snapshot {
    pub fn part_ids(&self) -> Vec<String> {
        self.parts.iter().map(|p| p.part_id().to_string()).collect()
    }

    /// Parts a query for `tenant` over `[from, to]` could possibly need.
    ///
    /// **The pruning happens here, in the catalog, before any part is opened.** A legacy entry
    /// carries no partition metadata and so cannot be excluded -- we do not know where it is,
    /// and guessing would risk a false negative, which loses a row.
    pub fn candidate_parts(
        &self,
        tenant: Option<&str>,
        from: Option<i64>,
        to: Option<i64>,
    ) -> Vec<String> {
        self.parts
            .iter()
            .filter(|e| match (e.located(), tenant) {
                (Some(r), Some(t)) => r.may_match(t, from, to),
                // No tenant policy, or no catalog metadata: we cannot rule it out.
                _ => true,
            })
            .map(|e| e.part_id().to_string())
            .collect()
    }

    /// Every partition this snapshot holds.
    pub fn partitions(&self) -> std::collections::BTreeSet<PartitionKey> {
        self.parts
            .iter()
            .filter_map(|e| e.located().map(|r| r.partition.clone()))
            .collect()
    }

    pub fn empty() -> Self {
        Snapshot {
            snapshot_id: "s00000000".to_string(),
            parent: None,
            parts: Vec::new(),
            next_seq: 1,
            active_generation: None,
            generations: BTreeMap::new(),
            bridges: Vec::new(),
            baselines: Vec::new(),
            created_at_ms: 0,
        }
    }

    /// The generations this snapshot's parts are actually encoded under.
    ///
    /// Not the same thing as `generations`, which is the *lifecycle* map. This is the set a
    /// reader must be able to resolve, and it is what makes `retire` safe: retiring a generation
    /// that a retained snapshot still names would make that snapshot unreadable, and a rollback
    /// target that cannot be read is not a rollback target.
    pub fn generations_in_use(&self) -> BTreeSet<String> {
        self.parts
            .iter()
            .filter_map(|e| e.located().map(|r| r.partition.generation.clone()))
            .collect()
    }

    pub fn state_of(&self, gen: &str) -> Option<GenerationState> {
        self.generations.get(gen).copied()
    }

    /// The bridge joining two spaces, if one has been declared.
    pub fn bridge(&self, a: &str, b: &str) -> Option<&Bridge> {
        self.bridges.iter().find(|br| br.joins(a, b))
    }

    pub fn baseline_for(&self, tenant: &str, generation: &str) -> Option<&BaselineRef> {
        self.baselines
            .iter()
            .find(|b| b.tenant == tenant && b.generation_id == generation)
    }
}

/// Everything a lifecycle transition may change about a snapshot other than its parts.
///
/// Passed as one value so that a transition is one commit: there is no window in which the
/// generation map says one thing and the parts say another.
#[derive(Clone, Debug, Default)]
pub struct SnapshotMeta {
    pub generations: BTreeMap<String, GenerationState>,
    pub bridges: Vec<Bridge>,
    pub baselines: Vec<BaselineRef>,
}

impl SnapshotMeta {
    pub fn of(snap: &Snapshot) -> Self {
        SnapshotMeta {
            generations: snap.generations.clone(),
            bridges: snap.bridges.clone(),
            baselines: snap.baselines.clone(),
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
        parts: Vec<PartEntry>,
        next_seq: u64,
        active_generation: Option<String>,
        now_ms: i64,
    ) -> Result<Snapshot> {
        self.commit_meta(
            parent,
            parts,
            next_seq,
            active_generation,
            SnapshotMeta::of(parent),
            now_ms,
        )
    }

    /// Commit, and set the lifecycle metadata explicitly. Every S5 transition goes through here.
    pub fn commit_meta(
        &self,
        parent: &Snapshot,
        parts: Vec<PartEntry>,
        next_seq: u64,
        active_generation: Option<String>,
        meta: SnapshotMeta,
        now_ms: i64,
    ) -> Result<Snapshot> {
        for p in &parts {
            p.validate()?;
            let dir = self.store.part_dir(p.part_id());
            PartReader::open(&dir).map_err(|e| {
                PrismError::Invariant(format!(
                    "refusing to commit a snapshot naming part `{}`, which is not readable: {e}",
                    p.part_id()
                ))
            })?;
        }

        let snap = Snapshot {
            snapshot_id: self.next_snapshot_id(parent)?,
            parent: Some(parent.snapshot_id.clone()),
            parts,
            next_seq,
            active_generation,
            generations: meta.generations,
            bridges: meta.bridges,
            baselines: meta.baselines,
            created_at_ms: now_ms,
        };

        let path = self
            .store
            .snapshots_dir()
            .join(format!("{}.json", snap.snapshot_id));
        // Out of space writing the snapshot file: CURRENT still names the old snapshot, so the
        // store is unchanged — a clean refusal, not a hybrid (merge contract §3).
        faults::guard_space("catalog.snapshot")?;
        io::write_atomic(&path, &serde_json::to_vec_pretty(&snap)?)?;
        faults::maybe_kill("snapshot.after_write_before_current");

        // The instant this rename lands, the new data is live. Before it, the
        // old snapshot is live. There is no third state. Out of space swapping CURRENT leaves the
        // old CURRENT intact (the rename never happens) — again old-or-new, never hybrid.
        faults::guard_space("catalog.current")?;
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

    // --- drift baselines (S5) ---

    /// Baselines are content-addressed and immutable, exactly like generations, and for exactly
    /// the same reason: a baseline is the definition of "normal" that an alarm's numbers are
    /// measured against, so editing one in place would silently change the meaning of every
    /// novelty score ever computed from it.
    pub fn put_baseline(&self, b: &crate::baseline::Baseline) -> Result<()> {
        let path = self.store.baseline_path(&b.baseline_id);
        if path.exists() {
            return Ok(());
        }
        io::ensure_dir(&self.store.baselines_dir())?;
        io::write_atomic(&path, &serde_json::to_vec_pretty(b)?)?;
        Ok(())
    }

    pub fn get_baseline(&self, id: &str) -> Result<crate::baseline::Baseline> {
        let path = self.store.baseline_path(id);
        if !path.exists() {
            return Err(PrismError::NotFound(format!("baseline `{id}`")));
        }
        let b: crate::baseline::Baseline = serde_json::from_slice(&io::read_file(&path)?)?;
        b.verify_content_address()?;
        Ok(b)
    }

    // --- garbage collection: explicit, and never on the publish path ---

    /// Remove parts and snapshots that no retained snapshot references — count-based only.
    ///
    /// `retain_snapshots` keeps the newest N snapshots so a rollback target survives. Prefer
    /// [`gc_at`] on the live path: it *also* honours the reader-lease horizon, so a reader within
    /// its lease can never have its snapshot reclaimed (invariant 6, by construction). This method
    /// is `gc_at` with `now = i64::MAX`, i.e. the time horizon disabled.
    pub fn gc(&self, retain_snapshots: usize, dry_run: bool) -> Result<GcReport> {
        self.gc_at(retain_snapshots, i64::MAX, dry_run)
    }

    /// Remove parts and snapshots that neither the retain-count floor nor the **reader-lease
    /// horizon** protects (merge contract §5, invariant 6).
    ///
    /// A snapshot is kept if it is one of the newest `retain_snapshots`, or the live one, **or**
    /// younger than [`GC_HORIZON_MS`] — the lease plus its derived grace. That last clause is what
    /// makes invariant 6 hold *by construction*: a reader that pinned a snapshot has at most
    /// [`LEASE_TTL_MS`] to use it, and GC will not reclaim anything younger than the lease-plus-
    /// grace horizon, so a reader within its lease always finds its parts. A reader that holds a
    /// cursor past its lease is *expired*, and its stale cursor gets the explicit expired-snapshot
    /// error ([query contract §2](../../../docs/QUERY-CONTRACT.md)), never a wrong answer.
    pub fn gc_at(&self, retain_snapshots: usize, now_ms: i64, dry_run: bool) -> Result<GcReport> {
        let retain = retain_snapshots.max(1);
        let current = self.current()?;
        let all_snaps = self.list_snapshots()?;

        // Keep the newest `retain` snapshots, always the live one, and — the lease clause — any
        // snapshot younger than the lease-plus-grace horizon, so a live reader is never orphaned.
        let horizon_floor = now_ms.saturating_sub(GC_HORIZON_MS);
        let mut keep_snaps: BTreeSet<String> = all_snaps
            .iter()
            .rev()
            .take(retain)
            .cloned()
            .chain(std::iter::once(current.snapshot_id.clone()))
            .collect();
        for id in &all_snaps {
            if keep_snaps.contains(id) {
                continue;
            }
            if let Ok(s) = self.load_snapshot(id) {
                if s.created_at_ms > horizon_floor {
                    keep_snaps.insert(id.clone());
                }
            }
        }

        // Anything a retained snapshot names is live and must not be touched.
        let mut referenced: BTreeSet<String> = BTreeSet::new();
        for id in &keep_snaps {
            if let Ok(s) = self.load_snapshot(id) {
                referenced.extend(s.part_ids());
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

        for p in &snap.part_ids() {
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
