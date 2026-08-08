//! Published-part backup and replacement-node hydration (S14, [D-094](../../../../docs/DECISIONS.md)).
//!
//! Three mechanisms already protected three different things, and the gap between them was the
//! recovery story. The replicated WAL ([D-068](../../../../docs/DECISIONS.md)) protects events that
//! were *acknowledged but not yet published*; the catalog mirror ([D-069](../../../../docs/DECISIONS.md))
//! protects the *snapshot*; the cold tier puts each part's `rerank.vec` on the object store. Nothing
//! protected the **hot tier of an already-published part** — the PQ codes, scalar and text columns,
//! bodies, and the manifest — which lived only on node-local disk. `recover_catalog_from_mirror`
//! said so in its own refusal: it will not restore a snapshot naming a part that is not readable
//! locally.
//!
//! This module closes that gap in both directions:
//!
//! - **Backup** uploads every file of a part under the key prefix the cold tier already used
//!   (`parts/<part_id>/<file>`) and then, only once every file is durable and verified, writes
//!   `parts/<part_id>/BACKUP.json` — the receipt naming each file's length and SHA-256 alongside the
//!   part's generation and tenants. The receipt's *presence* is the atomic assertion that the part
//!   is completely backed up, exactly as the catalog commit is the atomic assertion that a part is
//!   published. Upload, verify, then reference — storage §2, one level up.
//! - **Hydration** resolves the whole restore set before installing a byte of it (snapshot → parts →
//!   receipts → generations), stages each part, verifies every file against its receipt, renames
//!   only fully verified parts into place, and writes `CURRENT` **last**. A crash leaves staging
//!   directories and an unchanged `CURRENT`: old-or-new, never a partially restored snapshot.

use crate::engine::Engine;
use prism_part::generation::Generation;
use prism_part::part::PartReader;
use prism_part::partition::{Bucket, PartitionScheme};
use prism_types::error::{PrismError, Result};
use prism_types::hash::{hex, sha256};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The per-part backup receipt, written **last** so its presence means "complete".
pub const BACKUP_MANIFEST_FILE: &str = "BACKUP.json";

/// The remote prefix under which generation artifacts are backed up.
const GENERATION_PREFIX: &str = "generations/";

/// One backed-up file of a part: what it is called, how long it is, and what it hashes to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackedFile {
    pub name: String,
    pub len: u64,
    /// Lowercase hex SHA-256 of the whole file.
    pub sha256: String,
}

/// A part's backup receipt — the set of files that together *are* the part, plus the compatibility
/// facts a restore must check before installing it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PartBackup {
    pub part_id: String,
    /// The generation this part's vectors were embedded under. A restore set that cannot produce
    /// this exact generation is refused — never mix generations ([D-072](../../../../docs/DECISIONS.md)).
    pub generation_id: String,
    /// The tenants whose rows this part holds — checked against the shard's own placement so a part
    /// that belongs to another shard is refused rather than opened.
    pub tenants: Vec<String>,
    pub files: Vec<BackedFile>,
}

impl PartBackup {
    /// Total backed-up bytes, for the recovery-time receipt.
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.len).sum()
    }
}

/// What one backup pass did.
#[derive(Clone, Debug, Default)]
pub struct BackupReport {
    pub parts: Vec<String>,
    pub generations: Vec<String>,
    pub bytes: u64,
    /// The snapshot whose live set was backed up.
    pub snapshot_id: String,
}

/// Which tenants this shard is allowed to hold — S4 isolation as placement
/// ([D-071](../../../../docs/DECISIONS.md)). A part whose tenants route elsewhere is a routing
/// fault, not a file to open.
#[derive(Clone, Debug)]
pub struct ShardPlacement {
    pub scheme: PartitionScheme,
    pub shard_id: usize,
    pub shard_count: usize,
}

impl ShardPlacement {
    /// The same routing the cluster uses: a function of the bucket, not the tenant, so a whole
    /// bucket lives on one shard.
    pub fn owns(&self, tenant: &str) -> bool {
        if self.shard_count == 0 {
            return false;
        }
        let bucket = self.scheme.bucket_of(tenant);
        let ordinal = match bucket {
            Bucket::Shared(n) => n as u64,
            Bucket::Dedicated(i) => self.scheme.buckets as u64 + i as u64,
        };
        (ordinal % self.shard_count as u64) as usize == self.shard_id
    }
}

/// A resolved, compatibility-checked restore set. Producing one installs nothing; it only proves the
/// restore *can* succeed, so an impossible restore fails before it has touched the local store.
#[derive(Clone, Debug)]
pub struct HydrationPlan {
    pub snapshot_id: String,
    pub snapshot_bytes: Vec<u8>,
    pub parts: Vec<PartBackup>,
    /// Every generation the set requires, resolved and content-verified.
    pub generations: Vec<Generation>,
}

impl HydrationPlan {
    pub fn total_bytes(&self) -> u64 {
        self.parts.iter().map(PartBackup::total_bytes).sum()
    }
}

/// What one hydration did — the recovery-time half of the drill receipt.
#[derive(Clone, Debug, Default)]
pub struct HydrationReport {
    pub snapshot_id: String,
    pub parts: Vec<String>,
    pub generations: Vec<String>,
    pub bytes: u64,
}

/// The remote key of one file of a part's backup.
fn part_key(part_id: &str, file: &str) -> String {
    format!("parts/{part_id}/{file}")
}

/// The remote key of a generation artifact.
fn generation_key(gen_id: &str) -> String {
    format!("{GENERATION_PREFIX}{gen_id}.json")
}

/// The files that constitute a part on disk — every regular file in its directory except the backup
/// receipt itself, which describes them and must never describe itself.
fn part_files(dir: &std::path::Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == BACKUP_MANIFEST_FILE {
            continue;
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

impl Engine {
    /// Back up one published part: every file, then the receipt.
    ///
    /// The receipt is written **last and only after each file has been verified present at its full
    /// length on the backend**, so a receipt that exists is a promise the whole part is there. A
    /// crash mid-upload leaves files with no receipt — indistinguishable from "not backed up", which
    /// is exactly the safe reading, and reclaimed later by remote-orphan reconciliation.
    pub fn backup_part(&self, part_id: &str) -> Result<PartBackup> {
        let dir = self.store.part_dir(part_id);
        let reader = PartReader::open(&dir)
            .map_err(|e| PrismError::Invariant(format!("cannot back up part `{part_id}`: {e}")))?;
        let backend = self.cold.backend();
        let encrypted = reader.is_encrypted();

        let mut files = Vec::new();
        for name in part_files(&dir)? {
            let bytes = std::fs::read(dir.join(&name))?;
            let len = bytes.len() as u64;
            let key = part_key(part_id, &name);

            // Idempotent by size, exactly as `publish_part_cold` is — and with exactly the same
            // restriction. A content address names **logical content, not stored bytes**
            // ([D-095](../../../../docs/DECISIONS.md)): two sealings of one logical part share an
            // address and a size while differing in every byte, so for an **encrypted** part
            // "same key, same length" does not mean "same object". Encrypted parts always upload;
            // otherwise a backup could silently retain a ciphertext no live DEK opens, and the
            // receipt's SHA-256 would then indict the restore rather than the backup.
            if encrypted || backend.head(&key)? != Some(len) {
                backend.put(&key, &bytes)?;
            }
            match backend.head(&key)? {
                Some(got) if got == len => {}
                Some(got) => {
                    return Err(PrismError::Invariant(format!(
                        "backup of part `{part_id}` did not verify: the backend holds {got} bytes \
                         at `{key}`, but the file is {len} bytes"
                    )))
                }
                None => {
                    return Err(PrismError::Invariant(format!(
                        "backup of part `{part_id}` did not verify: the backend has no object at \
                         `{key}` after the upload"
                    )))
                }
            }
            files.push(BackedFile {
                name,
                len,
                sha256: hex(&sha256(&bytes)),
            });
        }

        let receipt = PartBackup {
            part_id: part_id.to_string(),
            generation_id: reader.manifest.generation_id.clone(),
            tenants: reader.manifest.tenants.clone(),
            files,
        };

        // A crash here leaves a complete file set with no receipt: still "not backed up", still
        // old-or-new. The receipt is the reference, and it comes last.
        prism_part::faults::maybe_kill("backup.after_files_before_receipt");

        backend.put(
            &part_key(part_id, BACKUP_MANIFEST_FILE),
            &serde_json::to_vec_pretty(&receipt)?,
        )?;
        Ok(receipt)
    }

    /// Back up a generation artifact. Content-addressed already
    /// ([D-072](../../../../docs/DECISIONS.md)), so the id *is* the integrity check on restore.
    pub fn backup_generation(&self, gen_id: &str) -> Result<u64> {
        let g = self.catalog().get_generation(gen_id)?;
        let bytes = serde_json::to_vec_pretty(&g)?;
        self.cold.backend().put(&generation_key(gen_id), &bytes)?;
        Ok(bytes.len() as u64)
    }

    /// Back up everything the live snapshot needs to be restored elsewhere: every part it names,
    /// every generation those parts require, and the catalog mirror itself.
    ///
    /// This is the supported bootstrap/restore product workflow — the thing an operator schedules —
    /// rather than a runbook that copies directories and hopes the set is consistent.
    pub fn backup_published(&self) -> Result<BackupReport> {
        let snap = self.snapshot()?;
        let mut report = BackupReport {
            snapshot_id: snap.snapshot_id.clone(),
            ..Default::default()
        };

        let mut generations: BTreeSet<String> = BTreeSet::new();
        for id in snap.part_ids() {
            let receipt = self.backup_part(&id)?;
            report.bytes += receipt.total_bytes();
            generations.insert(receipt.generation_id.clone());
            report.parts.push(id);
        }
        if let Some(active) = &snap.active_generation {
            generations.insert(active.clone());
        }
        for gen_id in generations {
            report.bytes += self.backup_generation(&gen_id)?;
            report.generations.push(gen_id);
        }

        // The snapshot itself. Idempotent, and safe because the mirror never leads (D-069).
        self.mirror_snapshot(&snap)?;
        Ok(report)
    }

    /// Resolve the restore set and prove it is internally compatible — **before installing
    /// anything**.
    ///
    /// Every refusal here costs nothing, because nothing has been written. The checks are the ones a
    /// restore cannot recover from later: a missing receipt, a part whose generation the set cannot
    /// produce, a generation artifact that is not what it claims, or a part that belongs to another
    /// shard entirely.
    pub fn plan_hydration(&self, placement: Option<&ShardPlacement>) -> Result<HydrationPlan> {
        let backend = self.cold.backend();

        // The highest mirrored snapshot is the restore target, exactly as the mirror recovery path
        // chooses it.
        let mut ids: Vec<String> = backend
            .list("catalog/")?
            .iter()
            .filter_map(|k| k.strip_prefix("catalog/SNAPSHOT-").map(str::to_string))
            .collect();
        ids.sort();
        let Some(snapshot_id) = ids.pop() else {
            return Err(PrismError::NotFound(
                "cannot hydrate: the object store holds no catalog mirror snapshot to restore from"
                    .into(),
            ));
        };
        let snapshot_bytes = backend.get(&format!("catalog/SNAPSHOT-{snapshot_id}"))?;
        let snap: prism_part::catalog::Snapshot = serde_json::from_slice(&snapshot_bytes)?;
        if snap.snapshot_id != snapshot_id {
            return Err(PrismError::Corrupt(format!(
                "catalog mirror snapshot `{snapshot_id}` declares id `{}`",
                snap.snapshot_id
            )));
        }

        let mut parts = Vec::new();
        let mut wanted_generations: BTreeSet<String> = BTreeSet::new();
        for part_id in snap.part_ids() {
            let key = part_key(&part_id, BACKUP_MANIFEST_FILE);
            let bytes = backend.get(&key).map_err(|e| {
                PrismError::NotFound(format!(
                    "cannot hydrate snapshot `{snapshot_id}`: part `{part_id}` has no backup \
                     receipt at `{key}` — it was never completely backed up ({e})"
                ))
            })?;
            let receipt: PartBackup = serde_json::from_slice(&bytes).map_err(|e| {
                PrismError::Corrupt(format!(
                    "backup receipt for part `{part_id}` is not readable: {e}"
                ))
            })?;
            if receipt.part_id != part_id {
                return Err(PrismError::Corrupt(format!(
                    "backup receipt at `{key}` describes part `{}`, not `{part_id}`",
                    receipt.part_id
                )));
            }
            if let Some(p) = placement {
                if let Some(foreign) = receipt.tenants.iter().find(|t| !p.owns(t)) {
                    return Err(PrismError::Invariant(format!(
                        "refusing to hydrate part `{part_id}`: it holds tenant `{foreign}`, which \
                         does not route to shard {} of {} — a part that arrived at the wrong shard \
                         is a routing fault, not a part to restore",
                        p.shard_id, p.shard_count
                    )));
                }
            }
            wanted_generations.insert(receipt.generation_id.clone());
            parts.push(receipt);
        }
        if let Some(active) = &snap.active_generation {
            wanted_generations.insert(active.clone());
        }

        let mut generations = Vec::new();
        for gen_id in &wanted_generations {
            let key = generation_key(gen_id);
            let bytes = backend.get(&key).map_err(|e| {
                PrismError::NotFound(format!(
                    "cannot hydrate snapshot `{snapshot_id}`: it requires generation `{gen_id}`, \
                     which has no backup at `{key}` — restoring parts without the codebook that \
                     embedded them would mix generations ({e})"
                ))
            })?;
            let g: Generation = serde_json::from_slice(&bytes)?;
            if &g.generation_id != gen_id {
                return Err(PrismError::Corrupt(format!(
                    "generation backup at `{key}` declares id `{}`, not `{gen_id}`",
                    g.generation_id
                )));
            }
            // Content-addressed: re-derive the id from the bytes. A codebook that is not what it
            // claims cannot be installed, which is the whole point of D-072's addressing.
            g.verify_content_address().map_err(|e| {
                PrismError::Corrupt(format!(
                    "generation `{gen_id}` fails its content address — the backed-up codebook is \
                     not the codebook it claims to be: {e}"
                ))
            })?;
            generations.push(g);
        }

        Ok(HydrationPlan {
            snapshot_id,
            snapshot_bytes,
            parts,
            generations,
        })
    }

    /// Install a planned restore set: stage, verify, rename, and only then point `CURRENT` at the
    /// restored snapshot.
    ///
    /// **Refuses to overwrite a live database.** A store that already has a `CURRENT` naming a
    /// snapshot is a running node, and a restore that silently replaced it would be one typo away
    /// from destroying the thing it was meant to protect.
    pub fn hydrate(&self, plan: &HydrationPlan) -> Result<HydrationReport> {
        if self.store.current_path().exists() {
            let current = std::fs::read_to_string(self.store.current_path())?;
            let current = current.trim().to_string();
            if !current.is_empty() {
                return Err(PrismError::Invariant(format!(
                    "refusing to hydrate onto a live store: `CURRENT` already names snapshot \
                     `{current}`. Hydration bootstraps an empty replacement node; restoring over a \
                     database that is already serving is a separate, explicit operator action."
                )));
            }
        }

        let backend = self.cold.backend();
        let mut report = HydrationReport {
            snapshot_id: plan.snapshot_id.clone(),
            ..Default::default()
        };

        prism_part::io::ensure_dir(&self.store.parts_dir())?;
        for receipt in &plan.parts {
            let staging = self
                .store
                .parts_dir()
                .join(format!(".hydrate-{}", receipt.part_id));
            if staging.exists() {
                std::fs::remove_dir_all(&staging)?;
            }
            prism_part::io::ensure_dir(&staging)?;

            for f in &receipt.files {
                let key = part_key(&receipt.part_id, &f.name);
                let bytes = backend.get(&key).map_err(|e| {
                    PrismError::NotFound(format!(
                        "hydrating part `{}`: the backup receipt names `{}`, which the object \
                         store does not hold at `{key}` ({e})",
                        receipt.part_id, f.name
                    ))
                })?;
                if bytes.len() as u64 != f.len {
                    return Err(PrismError::Corrupt(format!(
                        "hydrating part `{}`: file `{}` is truncated — the receipt records {} \
                         bytes, the object store returned {}",
                        receipt.part_id,
                        f.name,
                        f.len,
                        bytes.len()
                    )));
                }
                let got = hex(&sha256(&bytes));
                if got != f.sha256 {
                    return Err(PrismError::Corrupt(format!(
                        "hydrating part `{}`: file `{}` is corrupt — the receipt records SHA-256 \
                         {}, the restored bytes hash to {got}",
                        receipt.part_id, f.name, f.sha256
                    )));
                }
                prism_part::io::write_atomic(&staging.join(&f.name), &bytes)?;
                report.bytes += f.len;
            }

            // A staged part must open before it is installed: the receipt proves the bytes are the
            // bytes that were backed up, and this proves they are a readable part (invariant 2).
            PartReader::open(&staging).map_err(|e| {
                PrismError::Corrupt(format!(
                    "hydrated part `{}` verified byte-for-byte but does not open as a part: {e}",
                    receipt.part_id
                ))
            })?;

            let final_dir = self.store.part_dir(&receipt.part_id);
            if final_dir.exists() {
                std::fs::remove_dir_all(&final_dir)?;
            }
            std::fs::rename(&staging, &final_dir)?;
            report.parts.push(receipt.part_id.clone());
        }

        // Generations before the catalog: a snapshot whose codebook is missing is unreadable, and
        // `CURRENT` must never name a snapshot that cannot be served.
        for g in &plan.generations {
            self.catalog().put_generation(g)?;
            report.generations.push(g.generation_id.clone());
        }

        // A crash here leaves every part installed and `CURRENT` untouched — the store is still
        // "not restored", which is the safe reading, and re-running hydration converges it.
        prism_part::faults::maybe_kill("hydrate.after_parts_before_current");

        prism_part::io::ensure_dir(&self.store.snapshots_dir())?;
        let path = self
            .store
            .snapshots_dir()
            .join(format!("{}.json", plan.snapshot_id));
        prism_part::io::write_atomic(&path, &plan.snapshot_bytes)?;
        prism_part::io::write_atomic(&self.store.current_path(), plan.snapshot_id.as_bytes())?;
        Ok(report)
    }

    /// Plan and install in one step — the replacement-node bootstrap.
    pub fn hydrate_from_backup(
        &self,
        placement: Option<&ShardPlacement>,
    ) -> Result<HydrationReport> {
        let plan = self.plan_hydration(placement)?;
        self.hydrate(&plan)
    }

    /// Every backed-up part the object store holds a complete receipt for, by id — what an operator
    /// asks before trusting a recovery window.
    pub fn backed_up_parts(&self) -> Result<BTreeMap<String, u64>> {
        let backend = self.cold.backend();
        let mut out = BTreeMap::new();
        for key in backend.list("parts/")? {
            let Some(rest) = key.strip_prefix("parts/") else {
                continue;
            };
            let Some((id, tail)) = rest.split_once('/') else {
                continue;
            };
            if tail != BACKUP_MANIFEST_FILE {
                continue;
            }
            let receipt: PartBackup = serde_json::from_slice(&backend.get(&key)?)?;
            out.insert(id.to_string(), receipt.total_bytes());
        }
        Ok(out)
    }
}
