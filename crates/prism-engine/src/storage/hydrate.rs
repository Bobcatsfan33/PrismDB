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
use prism_part::partition::{bucket_ordinal, PartitionScheme};
use prism_types::error::{PrismError, Result};
use prism_types::hash::{hex, sha256};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The per-part backup receipt, written **last** so its presence means "complete".
pub const BACKUP_MANIFEST_FILE: &str = "BACKUP.json";

/// Where the store's wrapped tenant-key envelope lives in the object store (D-096). One object per
/// store, beside the catalog mirror rather than under a part's prefix, because it is store-scoped.
pub const TENANT_KEY_KEY: &str = "store/tenant-key.json";

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

/// The tenant list of an encrypted part's receipt: ciphertext, plus the plaintext facts the restore
/// path must read *before* it can unwrap anything ([encryption contract §6](../../../../docs/ENCRYPTION-CONTRACT.md)).
///
/// The DEK here is the **part's own**, carried in wrapped form. Reusing it rather than minting a
/// second key means a receipt introduces no key material that did not already exist, and that the
/// receipt is openable by exactly the principal who could already read the part — no more, no less.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SealedTenants {
    /// Which placement this part belongs to. **This is what routing is a function of**, so the
    /// wrong-shard refusal keeps working with no key at all — see [`ShardPlacement::owns_ordinal`].
    pub bucket_ordinal: u64,
    /// Versioned, so a cipher this build does not implement is refused rather than guessed.
    pub algorithm: u16,
    /// The **explicit** id of the wrapping key. Never inferred, never defaulted.
    pub wrapping_key_id: String,
    pub dek_epoch: u64,
    /// The part's DEK, wrapped. Useless without the key service.
    pub wrapped_dek: Vec<u8>,
    /// The tenant names, sealed under [`prism_part::crypto::RECEIPT_TENANTS_COLUMN`].
    pub ciphertext: Vec<u8>,
}

/// How a receipt names the tenants whose rows a part holds.
///
/// Two cases, not one field plus a flag: "encrypted, but the plaintext list is still populated" is
/// the exact bug this closes, and a borrowed view that cannot express it is a better guarantee than
/// a rule somebody has to remember.
#[derive(Clone, Copy, Debug)]
pub enum ReceiptTenants<'a> {
    /// An unencrypted store: the names ride in the clear, exactly as they always did.
    Plain(&'a [String]),
    /// An encrypted store: the names are ciphertext and only the ordinal is legible.
    Sealed(&'a SealedTenants),
}

/// A part's backup receipt — the set of files that together *are* the part, plus the compatibility
/// facts a restore must check before installing it.
///
/// **The tenant fields are private on purpose.** A receipt is the one artifact of an encrypted store
/// that is *designed* to be read without a key, which makes it the easiest place to leak the
/// DATA-01 metadata gap back open. Construction goes through [`PartBackup::plain`] or
/// [`PartBackup::sealed`], so there is no way to write a receipt that is both.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PartBackup {
    pub part_id: String,
    /// The generation this part's vectors were embedded under. A restore set that cannot produce
    /// this exact generation is refused — never mix generations ([D-072](../../../../docs/DECISIONS.md)).
    pub generation_id: String,
    /// The tenants whose rows this part holds, **in the clear** — populated only for an unencrypted
    /// store. Left empty when the names are sealed, and `default` on read so a receipt written
    /// before encryption existed still parses and still restores.
    #[serde(default)]
    tenants: Vec<String>,
    /// Present exactly when the store is encrypted. Skipped on write when absent, so a plaintext
    /// store's receipt is byte-identical to the one it wrote before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sealed_tenants: Option<SealedTenants>,
    pub files: Vec<BackedFile>,
}

impl PartBackup {
    /// The receipt of an unencrypted part.
    pub fn plain(
        part_id: impl Into<String>,
        generation_id: impl Into<String>,
        tenants: Vec<String>,
        files: Vec<BackedFile>,
    ) -> Self {
        Self {
            part_id: part_id.into(),
            generation_id: generation_id.into(),
            tenants,
            sealed_tenants: None,
            files,
        }
    }

    /// The receipt of an encrypted part: no tenant named in the clear.
    pub fn sealed(
        part_id: impl Into<String>,
        generation_id: impl Into<String>,
        sealed: SealedTenants,
        files: Vec<BackedFile>,
    ) -> Self {
        Self {
            part_id: part_id.into(),
            generation_id: generation_id.into(),
            tenants: Vec::new(),
            sealed_tenants: Some(sealed),
            files,
        }
    }

    /// How this receipt names its tenants.
    ///
    /// A receipt carrying both forms is corrupt, not a merge: it would mean the writer sealed the
    /// names and then also wrote them out, which is the leak itself.
    pub fn tenants(&self) -> Result<ReceiptTenants<'_>> {
        match &self.sealed_tenants {
            Some(_) if !self.tenants.is_empty() => Err(PrismError::Corrupt(format!(
                "backup receipt for part `{}` carries {} tenant name(s) in plaintext *and* a sealed \
                 tenant list; a receipt that seals its tenants and then also names them has \
                 defeated the sealing",
                self.part_id,
                self.tenants.len()
            ))),
            Some(s) => Ok(ReceiptTenants::Sealed(s)),
            None => Ok(ReceiptTenants::Plain(&self.tenants)),
        }
    }

    /// The tenant names this part holds, opening the sealed list when there is one.
    ///
    /// Needs the key for an encrypted part, and says so by failing rather than by returning the
    /// empty list — "no tenants" and "I cannot read the tenants" are different facts.
    pub fn open_tenants(
        &self,
        cipher: Option<&prism_part::crypto::BlockCipher>,
    ) -> Result<Vec<String>> {
        match self.tenants()? {
            ReceiptTenants::Plain(t) => Ok(t.to_vec()),
            ReceiptTenants::Sealed(s) => {
                let cipher = cipher.ok_or_else(|| {
                    PrismError::Policy(format!(
                        "the tenant list of part `{}`'s backup receipt is sealed under key `{}`; \
                         reading it needs the key service",
                        self.part_id, s.wrapping_key_id
                    ))
                })?;
                let plain = cipher.open(
                    &self.part_id,
                    prism_part::crypto::RECEIPT_TENANTS_COLUMN,
                    0,
                    &s.ciphertext,
                )?;
                serde_json::from_slice(&plain).map_err(|e| {
                    PrismError::Corrupt(format!(
                        "the sealed tenant list of part `{}` decrypted but will not parse: {e}",
                        self.part_id
                    ))
                })
            }
        }
    }

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
    /// Routing, on the bucket ordinal alone.
    ///
    /// **This is the whole reason the envelope stores an ordinal rather than tenant names.** The
    /// restore path must decide "does this part belong here?" *before* it has unwrapped anything —
    /// and routing was never a function of the tenant name, only of the bucket that name hashes to.
    /// So the wrong-shard refusal loses nothing by going keyless.
    pub fn owns_ordinal(&self, ordinal: u64) -> bool {
        if self.shard_count == 0 {
            return false;
        }
        (ordinal % self.shard_count as u64) as usize == self.shard_id
    }

    /// The same routing the cluster uses: a function of the bucket, not the tenant, so a whole
    /// bucket lives on one shard.
    pub fn owns(&self, tenant: &str) -> bool {
        self.owns_ordinal(bucket_ordinal(&self.scheme, &self.scheme.bucket_of(tenant)))
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

        // The tenant list is the DATA-01 gap in receipt form: a receipt is *designed* to be read
        // without a key, so an encrypted store that names its tenants here has disclosed which
        // tenants exist and which share a bucket to anyone holding the bucket — without decrypting
        // a row. Seal it, and keep exactly the ordinal the routing check needs (§6).
        let generation_id = reader.manifest.generation_id.clone();
        let sealing: Option<std::sync::Arc<prism_part::crypto::BlockCipher>> =
            self.cipher_for(&reader)?;
        let receipt = match sealing {
            None => PartBackup::plain(
                part_id,
                generation_id,
                reader.manifest.tenants.clone(),
                files,
            ),
            Some(cipher) => {
                let envelope = reader.encryption_envelope().ok_or_else(|| {
                    PrismError::Corrupt(format!(
                        "part `{part_id}` is encrypted but carries no envelope to back up"
                    ))
                })?;
                let plain = serde_json::to_vec(&reader.manifest.tenants)?;
                let ciphertext = cipher.seal(
                    part_id,
                    prism_part::crypto::RECEIPT_TENANTS_COLUMN,
                    0,
                    &plain,
                )?;
                PartBackup::sealed(
                    part_id,
                    generation_id,
                    SealedTenants {
                        bucket_ordinal: envelope.bucket_ordinal,
                        algorithm: envelope.algorithm,
                        wrapping_key_id: envelope.wrapping_key_id,
                        dek_epoch: envelope.dek_epoch,
                        wrapped_dek: envelope.wrapped_dek,
                        ciphertext,
                    },
                    files,
                )
            }
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

        // **The store's tenant-key envelope travels with the data it opens** (D-096).
        //
        // Found by the drill rather than by reasoning: without this a replacement node mints a
        // *fresh* tenant key, tokenizes every query differently from the parts it just restored, and
        // answers **empty** — a silently wrong answer that looks exactly like "no rows". The key is
        // stored only in wrapped form, so backing it up leaks nothing the bucket did not already
        // hold; what it buys is that a restored store tokenizes the way the store that wrote those
        // parts did.
        if let Some(bytes) = self.tenant_key_envelope_bytes()? {
            self.cold.backend().put(TENANT_KEY_KEY, &bytes)?;
            report.bytes += bytes.len() as u64;
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
            // The wrong-shard refusal, and it is **keyless in both forms**. A plaintext receipt
            // routes on its tenant names; a sealed one routes on the bucket ordinal, which is what
            // routing was always a function of. Encryption must not cost this check — a restore
            // that installed a foreign shard's parts because it could not read their tenant names
            // would be the DATA-01 fix buying a routing fault.
            if let Some(p) = placement {
                match receipt.tenants()? {
                    ReceiptTenants::Plain(tenants) => {
                        if let Some(foreign) = tenants.iter().find(|t| !p.owns(t)) {
                            return Err(PrismError::Invariant(format!(
                                "refusing to hydrate part `{part_id}`: it holds tenant \
                                 `{foreign}`, which does not route to shard {} of {} — a part that \
                                 arrived at the wrong shard is a routing fault, not a part to \
                                 restore",
                                p.shard_id, p.shard_count
                            )));
                        }
                    }
                    ReceiptTenants::Sealed(s) => {
                        if !p.owns_ordinal(s.bucket_ordinal) {
                            return Err(PrismError::Invariant(format!(
                                "refusing to hydrate part `{part_id}`: it belongs to bucket \
                                 ordinal {}, which does not route to shard {} of {} — a part that \
                                 arrived at the wrong shard is a routing fault, not a part to \
                                 restore",
                                s.bucket_ordinal, p.shard_id, p.shard_count
                            )));
                        }
                    }
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

        // The tenant key first: a part installed before its store can tokenize is a part no query
        // can match (D-096). Absent is fine — a plaintext store never wrote one.
        if let Ok(bytes) = backend.get(TENANT_KEY_KEY) {
            prism_part::io::write_atomic(
                &self.store.root.join(crate::tenant_key::TENANT_KEY_FILE),
                &bytes,
            )?;
        }

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
            let staged = PartReader::open(&staging).map_err(|e| {
                PrismError::Corrupt(format!(
                    "hydrated part `{}` verified byte-for-byte but does not open as a part: {e}",
                    receipt.part_id
                ))
            })?;

            // **Decrypt, verify, then rename** — decryption sits *inside* the D-094 staging
            // boundary ([encryption contract §8](../../../../docs/ENCRYPTION-CONTRACT.md)).
            //
            // What the receipt proved is *stored-byte integrity*: these are the bytes that were
            // backed up. It did not, and could not, prove they are the right data — that check
            // needs the key and happens when the AEAD tag is verified on read (§4a). Doing it here,
            // in staging, is what makes the failure clean: a key service that is unreachable,
            // denied, revoked, or simply does not hold this part's key refuses **before a single
            // directory has been renamed into place**, so there is no partially-decrypted state to
            // unwind and a retry after access returns starts from an untouched store.
            //
            // Deferring it to first query would install a store `CURRENT` names and nothing can
            // serve — the same failure the generation ordering below exists to prevent.
            if staged.is_encrypted() {
                let cipher = self.cipher_for(&staged)?.ok_or_else(|| {
                    PrismError::Corrupt(format!(
                        "hydrated part `{}` sets the encryption feature but carries no envelope",
                        receipt.part_id
                    ))
                })?;
                staged.with_cipher(cipher).verify().map_err(|e| {
                    PrismError::Corrupt(format!(
                        "hydrated part `{}` restored byte-for-byte but will not decrypt: {e}",
                        receipt.part_id
                    ))
                })?;
            }

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
