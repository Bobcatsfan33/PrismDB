use crate::model::{HashModelPlane, ModelPlane};
use prism_part::catalog::{Catalog, Snapshot};
use prism_part::part::PartReader;
use prism_part::store::{Store, StoreConfig};
use prism_types::error::Result;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Distinguishes ownership acquisitions of two engines opened in the same process/millisecond.
static WRITER_NONCE: AtomicU64 = AtomicU64::new(0);

/// What one rewrap pass did ([encryption contract §9](../../docs/ENCRYPTION-CONTRACT.md)).
///
/// Names the backend, because a rotation exercised against a software keystore has proven the code
/// and not the custody, and a receipt that does not say which is one nobody can act on.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct RewrapReport {
    pub backend: String,
    pub active_key_id: String,
    /// Live encrypted parts considered.
    pub examined: usize,
    /// Parts whose envelope was re-encrypted under the active key.
    pub rewrapped: Vec<String>,
    /// Parts already wrapped under the active key — the resumability evidence.
    pub already_current: Vec<String>,
    /// Parts carrying no envelope at all. Left alone: a rewrap does not turn encryption on.
    pub plaintext: Vec<String>,
}

pub struct Engine {
    pub store: Store,
    pub plane: Arc<dyn ModelPlane>,
    /// The cold tier's object store, with a content-verified cache in front (S11). Every exact
    /// rerank-vector fetch goes through this, so the cold tier can be local (the default backend
    /// points at the store's own `parts/` directory — behaviour-preserving) or remote (an
    /// `S3ObjectStore`). A cache state is a physical layout, and a physical layout may not change an
    /// answer ([storage contract §3](../../../docs/STORAGE-CONTRACT.md)).
    pub cold: Arc<crate::storage::CachedObjectStore>,
    /// The write-ownership epoch this engine holds ([D-076](../../../docs/DECISIONS.md)), or `0` if it
    /// has not acquired ownership — a reader, or a writer before its first owned publish. Interior-
    /// mutable so `&self` write methods can acquire lazily and the commit path can fence on it.
    owner_epoch: AtomicU64,
    /// A process-unique id tagging this engine's ownership acquisitions.
    writer_id: String,
    /// The key service, when this store is encrypted ([D-095](../../../docs/DECISIONS.md)).
    ///
    /// `None` is a plaintext store — the default, and byte-identical to every earlier build. The
    /// flag is **explicit**: a store is encrypted because it was configured to be, never because a
    /// part happened to carry a feature bit.
    keys: Option<Arc<dyn crate::keys::KeyProvider>>,
    /// Opened DEKs, bounded and clearable.
    dek_cache: crate::keys::DekCache,
    /// The store's tenant tokenizer, resolved once and cached ([D-096](../../docs/DECISIONS.md)).
    /// `None` inside the outer `Option` means "resolved, and this store has none" — a plaintext
    /// store — so a miss is not re-resolved on every part write.
    tenant_tok: std::sync::Mutex<Option<Option<Arc<prism_part::tenant::TenantTokenizer>>>>,
}

/// How many tenant/bucket DEKs stay resident at once.
///
/// **Policy** ([C-3](../../../docs/DECISIONS.md)): an unbounded key cache is an unbounded amount of
/// key material in memory. A shard serves a bounded set of tenant buckets, so a small cache covers
/// the working set; exceeding it costs an unwrap, never a wrong answer.
pub const DEK_CACHE_ENTRIES: usize = 64;

impl Engine {
    /// The default cold-tier store: a cache over a local backend rooted at the store, so a cold
    /// read fetches `parts/<id>/rerank.vec` from the local disk exactly as the mmap path did.
    fn default_cold(store: &Store) -> Arc<crate::storage::CachedObjectStore> {
        let backend = crate::storage::object::LocalObjectStore::new(store.root.clone());
        Arc::new(crate::storage::CachedObjectStore::new(
            Arc::new(backend),
            crate::storage::CACHE_QUOTA_BYTES,
        ))
    }

    fn writer_id() -> String {
        format!(
            "{}-{}",
            std::process::id(),
            WRITER_NONCE.fetch_add(1, Ordering::Relaxed)
        )
    }

    pub fn init(root: &Path, config: StoreConfig) -> Result<Engine> {
        let store = Store::init(root, config)?;
        let cold = Self::default_cold(&store);
        Ok(Engine {
            store,
            plane: Arc::new(HashModelPlane::new()),
            cold,
            owner_epoch: AtomicU64::new(0),
            writer_id: Self::writer_id(),
            keys: None,
            dek_cache: crate::keys::DekCache::new(DEK_CACHE_ENTRIES),
            tenant_tok: std::sync::Mutex::new(None),
        })
    }

    pub fn open(root: &Path) -> Result<Engine> {
        let store = Store::open(root)?;
        let cold = Self::default_cold(&store);
        Ok(Engine {
            store,
            plane: Arc::new(HashModelPlane::new()),
            cold,
            owner_epoch: AtomicU64::new(0),
            writer_id: Self::writer_id(),
            keys: None,
            dek_cache: crate::keys::DekCache::new(DEK_CACHE_ENTRIES),
            tenant_tok: std::sync::Mutex::new(None),
        })
    }

    /// **Acquire write ownership** for this engine ([D-076](../../../docs/DECISIONS.md)) — the next
    /// monotonic epoch in the object store. Idempotent within a process: once acquired it returns the
    /// held epoch. A writer calls this before it publishes, so the commit path can fence a stale
    /// writer a restart has overtaken. Returns the epoch now held.
    pub fn acquire_ownership(&self) -> Result<u64> {
        let held = self.owner_epoch.load(Ordering::SeqCst);
        if held != 0 {
            return Ok(held);
        }
        let e = crate::storage::ownership::acquire(self.cold.backend().as_ref(), &self.writer_id)?;
        self.owner_epoch.store(e, Ordering::SeqCst);
        Ok(e)
    }

    /// **Fence the write path** ([D-076](../../../docs/DECISIONS.md)): if this engine acquired
    /// ownership and a higher epoch has since taken the shard, refuse by name. A **no-op** for an
    /// engine that never acquired ownership (epoch `0`) — so the single-writer path is unchanged — and
    /// for the current owner. Called immediately before a catalog commit.
    pub fn assert_write_owner(&self) -> Result<()> {
        let held = self.owner_epoch.load(Ordering::SeqCst);
        if held == 0 {
            return Ok(());
        }
        crate::storage::ownership::assert_owner(self.cold.backend().as_ref(), held)
    }

    /// The ownership epoch held by this process, or `0` before ownership is acquired.
    ///
    /// The replicated admission log uses this value as the high half of its record IDs, so a
    /// replacement writer's WAL records are ordered strictly after every record admitted by the
    /// writer it fenced.
    pub fn ownership_epoch(&self) -> u64 {
        self.owner_epoch.load(Ordering::SeqCst)
    }

    pub fn with_plane(mut self, plane: Arc<dyn ModelPlane>) -> Self {
        self.plane = plane;
        self
    }

    /// Turn on envelope encryption for this store by supplying the key service.
    ///
    /// Explicit by construction: there is no ambient default and no inference from stored bytes.
    pub fn with_keys(mut self, keys: Arc<dyn crate::keys::KeyProvider>) -> Self {
        self.keys = Some(keys);
        self
    }

    pub fn keys(&self) -> Option<&Arc<dyn crate::keys::KeyProvider>> {
        self.keys.as_ref()
    }

    /// Resolve the cipher that opens a part, from the envelope the part itself carries.
    ///
    /// The key id comes from the part, is used **as given**, and is never inferred: a part that
    /// names a key this deployment does not hold is a named refusal, not an invitation to try the
    /// keys we do have. A plaintext part needs nothing and returns `None`.
    pub fn cipher_for(
        &self,
        reader: &prism_part::part::PartReader,
    ) -> Result<Option<std::sync::Arc<prism_part::crypto::BlockCipher>>> {
        if !reader.is_encrypted() {
            return Ok(None);
        }
        let envelope = reader.encryption_envelope().ok_or_else(|| {
            prism_types::error::PrismError::Corrupt(format!(
                "part {} sets the encryption feature but carries no envelope",
                reader.manifest.part_id
            ))
        })?;
        if envelope.algorithm != prism_part::crypto::AEAD_XCHACHA20_POLY1305 {
            return Err(prism_types::error::PrismError::Corrupt(format!(
                "part {} was sealed with AEAD id {}, which this build does not implement",
                reader.manifest.part_id, envelope.algorithm
            )));
        }
        let keys = self.keys.as_ref().ok_or_else(|| {
            prism_types::error::PrismError::Policy(format!(
                "part {} is encrypted under key `{}` but this store has no key service configured",
                reader.manifest.part_id, envelope.wrapping_key_id
            ))
        })?;
        // Cached per **DEK**, not per wrapping key.
        //
        // A fresh DEK is minted for every part, so many parts share one wrapping key id and one
        // epoch while holding entirely different DEKs. Keying the cache on the wrapping key alone
        // therefore handed the first part's DEK to every subsequent part — which failed closed on
        // the AEAD tag rather than returning wrong rows, but made any query touching more than one
        // encrypted part fail outright. The wrapped DEK's digest is what actually identifies the
        // key, and it is derivable without unwrapping anything.
        let cache_key = format!(
            "{}#{}#{}",
            envelope.wrapping_key_id,
            envelope.dek_epoch,
            prism_types::hash::hex(&prism_types::hash::sha256(&envelope.wrapped_dek))
        );
        self.dek_cache
            .get_or_open(&cache_key, || {
                let dek = keys.unwrap(&envelope.wrapping_key_id, &envelope.wrapped_dek)?;
                // Labelled by the DEK, never by the wrapping key: the label goes into every block's
                // AAD, and a label that moved when the envelope was rewrapped would fail the tag on
                // every block in the store the moment a rotation ran.
                Ok(prism_part::crypto::BlockCipher::new(
                    dek,
                    prism_part::crypto::dek_label(envelope.dek_epoch),
                ))
            })
            .map(Some)
    }

    /// Open a part and attach its cipher if it is encrypted.
    pub fn open_part(&self, part_id: &str) -> Result<prism_part::part::PartReader> {
        let reader = PartReader::open(&self.store.part_dir(part_id))?;
        match self.cipher_for(&reader)? {
            Some(c) => Ok(reader.with_cipher(c)),
            None => Ok(reader),
        }
    }

    /// Mint the encryption for a new part in `bucket`, or `None` for a plaintext store.
    ///
    /// A fresh DEK per part keeps the blast radius of one compromised key to one part, and costs
    /// only a wrap. The envelope records the **explicit** wrapping key id the provider used and the
    /// **bucket ordinal** rather than tenant names, so raw disk access does not disclose which
    /// tenants exist while [D-094](../../../docs/DECISIONS.md)'s routing check keeps working —
    /// routing was always a function of the bucket.
    pub fn part_encryption_for(
        &self,
        bucket_ordinal: u64,
        dek_epoch: u64,
    ) -> Result<Option<prism_part::part::PartEncryption>> {
        let Some(keys) = self.keys.as_ref() else {
            return Ok(None);
        };
        let dek = prism_part::crypto::generate_dek()?;
        let (wrapping_key_id, wrapped_dek) = keys.wrap(&dek)?;
        Ok(Some(prism_part::part::PartEncryption {
            cipher: std::sync::Arc::new(prism_part::crypto::BlockCipher::new(
                dek,
                prism_part::crypto::dek_label(dek_epoch),
            )),
            envelope: prism_part::ext::S14Ext {
                algorithm: prism_part::crypto::AEAD_XCHACHA20_POLY1305,
                wrapping_key_id,
                dek_epoch,
                wrapped_dek,
                bucket_ordinal,
            },
        }))
    }

    /// The store's tenant tokenizer, minted-or-loaded once and cached
    /// ([D-096](../../docs/DECISIONS.md), [contract §6a](../../docs/ENCRYPTION-CONTRACT.md)).
    ///
    /// `None` for a plaintext store: with nothing sealed there is nothing to tokenize, and this is
    /// not the thing that turns sealing on.
    pub fn tenant_tokenizer(&self) -> Result<Option<Arc<prism_part::tenant::TenantTokenizer>>> {
        let mut slot = self.tenant_tok.lock().map_err(|_| {
            prism_types::error::PrismError::Invariant(
                "the tenant tokenizer lock was poisoned by a panic".into(),
            )
        })?;
        if let Some(resolved) = slot.as_ref() {
            return Ok(resolved.clone());
        }
        let resolved =
            crate::tenant_key::load_or_mint(&self.store.root, self.keys.as_ref())?.map(Arc::new);
        *slot = Some(resolved.clone());
        Ok(resolved)
    }

    /// The store's wrapped tenant-key envelope as bytes, for the backup set. `None` when the store
    /// has none — a plaintext store, which has nothing to tokenize.
    pub fn tenant_key_envelope_bytes(&self) -> Result<Option<Vec<u8>>> {
        let path = self.store.root.join(crate::tenant_key::TENANT_KEY_FILE);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(std::fs::read(&path)?))
    }

    /// Drop every resident DEK, so a revocation takes effect against this running process.
    pub fn clear_key_cache(&self) -> Result<()> {
        self.dek_cache.clear()
    }

    /// **Rewrap**: re-encrypt every live part's wrapped DEK under the key service's active key
    /// ([encryption contract §9](../../../docs/ENCRYPTION-CONTRACT.md)).
    ///
    /// The third step of expand → activate → rewrap → retire, and the only one that touches stored
    /// state. It touches **envelopes and never part bytes**: no column file is opened, no block is
    /// decrypted or re-encrypted, no nonce changes, and no content address moves. What changes is a
    /// 32-byte key's wrapping, which is the whole point — a rotation that had to rewrite part bytes
    /// would cost a full re-ingest, and a rotation that costs a re-ingest is one that never happens.
    ///
    /// **Idempotent and resumable.** A part already wrapped under the active key is skipped, so
    /// re-running after a crash resumes rather than redoes; and because *expand* left both keys
    /// accepted for unwrap before any rewrap ran, a half-rewrapped store is fully readable
    /// throughout.
    pub fn rewrap_to_active_key(&self) -> Result<RewrapReport> {
        let keys = self.keys.as_ref().ok_or_else(|| {
            prism_types::error::PrismError::Policy(
                "cannot rewrap: this store has no key service configured".into(),
            )
        })?;
        let active_key_id = keys.active_key_id()?;
        let mut report = RewrapReport {
            backend: keys.backend().to_string(),
            active_key_id: active_key_id.clone(),
            ..Default::default()
        };

        for part_id in self.snapshot()?.part_ids() {
            let dir = self.store.part_dir(&part_id);
            let reader = PartReader::open(&dir)?;
            if !reader.is_encrypted() {
                report.plaintext.push(part_id);
                continue;
            }
            let envelope = reader.encryption_envelope().ok_or_else(|| {
                prism_types::error::PrismError::Corrupt(format!(
                    "part {part_id} sets the encryption feature but carries no envelope"
                ))
            })?;
            report.examined += 1;
            if envelope.wrapping_key_id == active_key_id {
                report.already_current.push(part_id);
                continue;
            }

            // Unwrap under the id the part names -- as given, never probed -- and wrap under the
            // active key. The DEK itself is unchanged, so every block this part holds stays sealed
            // under exactly the key that sealed it.
            let dek = keys.unwrap(&envelope.wrapping_key_id, &envelope.wrapped_dek)?;
            let (wrapping_key_id, wrapped_dek) = keys.wrap(&dek)?;
            prism_part::part::rewrap_part_envelope(
                &dir,
                &prism_part::ext::S14Ext {
                    wrapping_key_id,
                    wrapped_dek,
                    ..envelope
                },
            )?;
            report.rewrapped.push(part_id);
        }

        // The cache is keyed on the *old* envelope's key id, so a resident entry would keep serving
        // reads that no longer reflect what is on disk. Correct either way, but a rewrap that left
        // stale key state resident is the kind of thing that only shows up after a retire.
        self.dek_cache.clear()?;
        Ok(report)
    }

    /// Every wrapping key some live envelope still needs, and how many envelopes need it.
    ///
    /// Counts **published parts and the local admission log**, because both hold wrapped DEKs and
    /// both are things a retire could make unreadable. The WAL is scanned without opening a single
    /// record: a sealed frame names its wrapping key in the clear precisely so this question is
    /// answerable without the key it is asking about.
    pub fn wrapping_keys_in_use(&self) -> Result<std::collections::BTreeMap<String, usize>> {
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        for part_id in self.snapshot()?.part_ids() {
            let reader = PartReader::open(&self.store.part_dir(&part_id))?;
            if let Some(envelope) = reader.encryption_envelope() {
                *counts.entry(envelope.wrapping_key_id).or_default() += 1;
            }
        }
        let wal_dir = self.store.root.join("wal");
        if crate::wal::exists(&wal_dir) {
            for key_id in crate::wal::Wal::open(&wal_dir)?.wrapping_key_ids()? {
                *counts.entry(key_id).or_default() += 1;
            }
        }
        Ok(counts)
    }

    /// Refuse to retire a wrapping key while any live envelope still needs it
    /// ([encryption contract §9](../../../docs/ENCRYPTION-CONTRACT.md)).
    ///
    /// The same shape as refusing to retire a generation a retained snapshot still names: the
    /// software will not let an operator make its own data unreadable by a single command, and the
    /// refusal names how many envelopes are in the way so the answer is "run the rewrap", not
    /// "try harder".
    pub fn assert_key_retirable(&self, key_id: &str) -> Result<()> {
        match self.wrapping_keys_in_use()?.get(key_id) {
            None | Some(0) => Ok(()),
            Some(n) => Err(prism_types::error::PrismError::Invalid(format!(
                "refusing to retire wrapping key `{key_id}`: {n} live envelope(s) still need it to \
                 unwrap. Rewrap them under the active key first — retiring now would make data \
                 this store is serving unreadable."
            ))),
        }
    }

    /// Override the cold-tier object store (a fresh cache, a fault-injecting backend, or an
    /// `S3ObjectStore` for the remote gate). Used by the answer-invariance gate to force cache
    /// states and inject remote faults.
    pub fn with_cold(mut self, cold: Arc<crate::storage::CachedObjectStore>) -> Self {
        self.cold = cold;
        self
    }

    pub fn catalog(&self) -> Catalog<'_> {
        Catalog::new(&self.store)
    }

    /// Pin the live snapshot. A query holds this for its whole lifetime
    /// (invariant 4): parts cannot be pulled out from under it, because nothing
    /// mutates and GC only reclaims what no retained snapshot names.
    pub fn snapshot(&self) -> Result<Snapshot> {
        self.catalog().current()
    }

    /// Open the manifests of every part in a snapshot.
    ///
    /// Used by merge, verify, and the exact oracle — the operations that legitimately need the
    /// whole store. **The query path does not use this.** It uses [`Engine::open_candidates`],
    /// which opens only the parts the catalog says a query could possibly need, and that
    /// distinction is the S4 gate: a tenant-A query must never touch a byte of another tenant's
    /// partition.
    pub fn open_parts(&self, snap: &Snapshot) -> Result<Vec<PartReader>> {
        snap.part_ids().iter().map(|p| self.open_part(p)).collect()
    }

    /// Open only the parts a query could possibly need — **pruned in the catalog, before a
    /// single part byte is read.**
    ///
    /// This is where "cross-tenant reads are physically impossible" stops being a slogan and
    /// becomes an I/O property. A part outside the query's partitions is never opened, never
    /// checksummed, never read. Fill another tenant's partitions with unreadable garbage and a
    /// tenant-A query still answers correctly, because it never looked.
    ///
    /// Returns `(readers, parts_pruned)` — the count is a *measured* fact for the counters, not
    /// an estimate.
    pub fn open_candidates(
        &self,
        snap: &Snapshot,
        tenant: Option<&str>,
        from: Option<i64>,
        to: Option<i64>,
    ) -> Result<(Vec<PartReader>, usize)> {
        let tok = self.tenant_tokenizer()?;
        let ids = snap.candidate_parts(tenant, from, to, tok.as_deref());
        let pruned = snap.parts.len() - ids.len();
        let readers = ids
            .iter()
            .map(|p| self.open_part(p))
            .collect::<Result<Vec<_>>>()?;
        Ok((readers, pruned))
    }
}

/// Wall-clock milliseconds. Passed explicitly into anything that records a
/// timestamp so that tests and fixtures are not at the mercy of the clock.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
