//! The `ObjectStore` trait and its backends (S11) — [storage contract](../../../../docs/STORAGE-CONTRACT.md).
//!
//! One trait is the whole multi-cloud abstraction (storage contract §8): the engine asks for bytes
//! `[offset, len)` of an object, or puts one, or CASes one into existence. Behind it live the
//! backends — a real **local** object store (this sprint's gated backend), a **fault-injecting**
//! wrapper that makes it fail the way a remote fails, a **content-verified cache**, and — filed for
//! the next increment — a hand-rolled S3 client ([D-065](../../../../docs/DECISIONS.md)).
//!
//! Two disciplines are absolute here. A **truncated or corrupt read is a named-byte error**, never
//! a silent short read (the S1 rule, at the storage boundary). And the **cache trusts nothing**:
//! every served block is content-verified, a corrupt entry is evicted and refetched, and the truth
//! it caches is never in doubt because the cache is disposable.

use prism_types::error::{PrismError, Result};
use prism_types::hash::crc32;
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The one seam over storage. The engine asks for byte ranges of content-addressed objects; the
/// backend decides whether those bytes are on a local disk, in a cache, or across a network.
pub trait ObjectStore: Send + Sync {
    /// The whole object, or `NotFound`.
    fn get(&self, key: &str) -> Result<Vec<u8>>;
    /// Exactly `len` bytes at `offset`, or a **named** error — a short remote body is a truncation,
    /// named by its byte shortfall, never a silently-shorter `Vec` handed to a decoder.
    fn get_range(&self, key: &str, offset: u64, len: usize) -> Result<Vec<u8>>;
    /// Put (overwrite) an object durably.
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()>;
    /// **CAS create** ([D-066](../../../../docs/DECISIONS.md)): `Ok(true)` if this call created the
    /// object, `Ok(false)` if it already existed — the `If-None-Match: *` primitive that makes
    /// catalog publication a resolved race, never a lost write.
    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<bool>;
    /// The object's length if it exists — the durability check publication runs before it references
    /// a part (storage contract §2).
    fn head(&self, key: &str) -> Result<Option<u64>>;
    fn delete(&self, key: &str) -> Result<()>;
    /// Every key under `prefix` — GC lists the remote to reconcile orphans against the referenced
    /// set (storage contract §2).
    fn list(&self, prefix: &str) -> Result<Vec<String>>;
}

// --- the local backend -----------------------------------------------------------------------

/// A real object store backed by a local directory. Content-addressed keys map to files under
/// `root`. Not a mock of S3 — a genuine (local) object store, exercised through the fault wrapper
/// and the cache exactly as a remote would be.
pub struct LocalObjectStore {
    root: PathBuf,
}

impl LocalObjectStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        LocalObjectStore { root: root.into() }
    }
    fn path(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

impl ObjectStore for LocalObjectStore {
    fn get(&self, key: &str) -> Result<Vec<u8>> {
        let p = self.path(key);
        std::fs::read(&p).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PrismError::NotFound(format!("object `{key}` does not exist"))
            } else {
                e.into()
            }
        })
    }

    fn get_range(&self, key: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let p = self.path(key);
        let f = std::fs::File::open(&p).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PrismError::NotFound(format!("object `{key}` does not exist"))
            } else {
                e.into()
            }
        })?;
        let object_len = f.metadata()?.len();
        // A range past the end is a truncated object, named by its byte shortfall (storage §1).
        let end = offset.checked_add(len as u64).ok_or_else(|| {
            PrismError::Corrupt(format!(
                "object `{key}` range overflows: offset {offset} + {len}"
            ))
        })?;
        if end > object_len {
            return Err(PrismError::Corrupt(format!(
                "object `{key}` is truncated: range needs {len} bytes at offset {offset} (through \
                 {end}), but the object is only {object_len} bytes"
            )));
        }
        let mut buf = vec![0u8; len];
        f.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    }

    fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let p = self.path(key);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        prism_part::io::write_atomic(&p, bytes)
    }

    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<bool> {
        let p = self.path(key);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // `create_new` is the filesystem's `If-None-Match: *`: an atomic create that fails if the
        // object already exists (D-066). The loser of a publication race lands here.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            Ok(mut f) => {
                f.write_all(bytes)?;
                f.sync_all()?;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn head(&self, key: &str) -> Result<Option<u64>> {
        match std::fs::metadata(self.path(key)) {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        match std::fs::remove_file(self.path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let base = self.path(prefix);
        let mut out = Vec::new();
        list_rec(&self.root, &base, &mut out)?;
        out.sort();
        Ok(out)
    }
}

fn list_rec(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            list_rec(root, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().to_string());
        }
    }
    Ok(())
}

// --- the fault-injecting wrapper -------------------------------------------------------------

/// What the fault wrapper injects — the failures a real remote produces (storage contract §1).
#[derive(Clone, Default)]
pub struct FaultConfig {
    /// Make every call fail with the named `remote unavailable` condition.
    pub unavailable: bool,
    /// Make every read return the named-byte **truncation** error, as if the body arrived short.
    pub truncate_reads: bool,
    /// Fail the next call of any kind with a 5xx-equivalent error, then clear.
    pub fail_next: bool,
}

/// Wraps any `ObjectStore` and injects remote-style faults into it — the real backend, faulted,
/// never a hand-mocked S3 (storage contract §1). In-process, like the S10 ENOSPC guard.
pub struct FaultStore {
    inner: Box<dyn ObjectStore>,
    cfg: Mutex<FaultConfig>,
}

impl FaultStore {
    pub fn new(inner: impl ObjectStore + 'static) -> Self {
        FaultStore {
            inner: Box::new(inner),
            cfg: Mutex::new(FaultConfig::default()),
        }
    }
    pub fn set(&self, cfg: FaultConfig) {
        *self.cfg.lock().expect("fault cfg") = cfg;
    }
    fn gate(&self) -> Result<()> {
        let mut cfg = self.cfg.lock().expect("fault cfg");
        if cfg.unavailable {
            return Err(PrismError::Io(
                "remote unavailable: injected connection failure".into(),
            ));
        }
        if cfg.fail_next {
            cfg.fail_next = false;
            return Err(PrismError::Io("remote error: injected 5xx".into()));
        }
        Ok(())
    }
}

impl ObjectStore for FaultStore {
    fn get(&self, key: &str) -> Result<Vec<u8>> {
        self.gate()?;
        self.inner.get(key)
    }
    fn get_range(&self, key: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.gate()?;
        if self.cfg.lock().expect("fault cfg").truncate_reads {
            return Err(PrismError::Corrupt(format!(
                "object `{key}` is truncated: injected short body — needed {len} bytes at offset \
                 {offset}, the remote returned fewer"
            )));
        }
        self.inner.get_range(key, offset, len)
    }
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        self.gate()?;
        self.inner.put(key, bytes)
    }
    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<bool> {
        self.gate()?;
        self.inner.put_if_absent(key, bytes)
    }
    fn head(&self, key: &str) -> Result<Option<u64>> {
        self.gate()?;
        self.inner.head(key)
    }
    fn delete(&self, key: &str) -> Result<()> {
        self.gate()?;
        self.inner.delete(key)
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.gate()?;
        self.inner.list(prefix)
    }
}

// --- the content-verified cache --------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: usize,
    pub misses: usize,
    /// Entries served-attempts that failed their content hash and were evicted+repaired.
    pub corrupt_repaired: usize,
    pub evictions: usize,
    pub bytes_resident: usize,
}

type CacheKey = (String, u64, usize);

struct Entry {
    bytes: Vec<u8>,
    crc: u32,
}

/// A bounded, byte-quota, LRU block cache that **trusts nothing**: every entry carries the CRC-32 of
/// its bytes, verified on every read, and a mismatch evicts-and-refetches rather than serves
/// (storage contract §4). It caches the cold tier's blocks; the truth is on the remote.
pub struct BlockCache {
    quota: usize,
    inner: Mutex<CacheInner>,
}

struct CacheInner {
    map: HashMap<CacheKey, Entry>,
    lru: VecDeque<CacheKey>,
    bytes: usize,
    stats: CacheStats,
}

impl BlockCache {
    pub fn new(quota_bytes: usize) -> Self {
        BlockCache {
            quota: quota_bytes,
            inner: Mutex::new(CacheInner {
                map: HashMap::new(),
                lru: VecDeque::new(),
                bytes: 0,
                stats: CacheStats::default(),
            }),
        }
    }

    pub fn stats(&self) -> CacheStats {
        let g = self.inner.lock().expect("cache");
        let mut s = g.stats.clone();
        s.bytes_resident = g.bytes;
        s
    }

    /// Serve a cached block **only if it verifies**. A corrupt entry is evicted (and counted as a
    /// repair, because the caller will refetch), and `None` is returned so the truth is refetched.
    pub fn get(&self, key: &str, offset: u64, len: usize) -> Option<Vec<u8>> {
        let mut g = self.inner.lock().expect("cache");
        let k: CacheKey = (key.to_string(), offset, len);
        enum Verdict {
            Hit(Vec<u8>),
            Corrupt,
            Miss,
        }
        let verdict = match g.map.get(&k) {
            Some(e) if crc32(&e.bytes) == e.crc => Verdict::Hit(e.bytes.clone()),
            Some(_) => Verdict::Corrupt,
            None => Verdict::Miss,
        };
        match verdict {
            Verdict::Hit(bytes) => {
                g.stats.hits += 1;
                touch(&mut g.lru, &k);
                Some(bytes)
            }
            Verdict::Corrupt => {
                // Content hash mismatch: a corrupt cache block. Evict, log, and let the caller
                // refetch from the remote — the cache is disposable, the truth is not.
                eprintln!("prism: cache block `{key}`@{offset}+{len} failed its content hash; evicting and refetching");
                remove(&mut g, &k);
                g.stats.corrupt_repaired += 1;
                None
            }
            Verdict::Miss => {
                g.stats.misses += 1;
                None
            }
        }
    }

    /// Admit a verified block, evicting LRU entries to stay within the quota. A block larger than
    /// the whole quota is simply not cached (it still serves from the fetch); it never OOMs.
    pub fn insert(&self, key: &str, offset: u64, len: usize, bytes: Vec<u8>) {
        let mut g = self.inner.lock().expect("cache");
        let sz = bytes.len();
        if sz > self.quota {
            return;
        }
        let k: CacheKey = (key.to_string(), offset, len);
        if g.map.contains_key(&k) {
            return;
        }
        while g.bytes + sz > self.quota {
            let Some(victim) = g.lru.pop_front() else {
                break;
            };
            if let Some(e) = g.map.remove(&victim) {
                g.bytes -= e.bytes.len();
                g.stats.evictions += 1;
            }
        }
        let crc = crc32(&bytes);
        g.bytes += sz;
        g.map.insert(k.clone(), Entry { bytes, crc });
        g.lru.push_back(k);
    }

    /// **Test hook:** flip a byte of a cached entry to simulate on-disk cache corruption.
    #[doc(hidden)]
    pub fn corrupt_entry(&self, key: &str, offset: u64, len: usize) -> bool {
        let mut g = self.inner.lock().expect("cache");
        let k: CacheKey = (key.to_string(), offset, len);
        if let Some(e) = g.map.get_mut(&k) {
            if let Some(b) = e.bytes.first_mut() {
                *b ^= 0xFF;
                return true;
            }
        }
        false
    }
}

fn touch(lru: &mut VecDeque<CacheKey>, k: &CacheKey) {
    if let Some(pos) = lru.iter().position(|x| x == k) {
        lru.remove(pos);
    }
    lru.push_back(k.clone());
}

fn remove(g: &mut CacheInner, k: &CacheKey) {
    if let Some(e) = g.map.remove(k) {
        g.bytes -= e.bytes.len();
    }
    if let Some(pos) = g.lru.iter().position(|x| x == k) {
        g.lru.remove(pos);
    }
}

/// A cache in front of a (possibly remote) object store. A read is served from the cache if it is
/// present **and verifies**, else fetched from the remote, verified, and admitted. When the remote
/// is unreachable, cached blocks still serve and an uncached block fails with the **named** remote
/// condition — never a silent partial answer (storage contract §4, the S12 slow-shard rule early).
pub struct CachedObjectStore {
    remote: std::sync::Arc<dyn ObjectStore>,
    cache: BlockCache,
}

impl CachedObjectStore {
    pub fn new(remote: std::sync::Arc<dyn ObjectStore>, quota_bytes: usize) -> Self {
        CachedObjectStore {
            remote,
            cache: BlockCache::new(quota_bytes),
        }
    }
    pub fn cache(&self) -> &BlockCache {
        &self.cache
    }

    /// A cold-tier ranged read, cache-first. The workhorse the query path calls.
    pub fn get_range_cached(&self, key: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        if let Some(bytes) = self.cache.get(key, offset, len) {
            return Ok(bytes); // verified hit
        }
        // Miss (or a corrupt entry the cache just evicted): fetch the truth. A remote failure here
        // is propagated with its name intact — never masked as a partial answer.
        let bytes = self.remote.get_range(key, offset, len)?;
        self.cache.insert(key, offset, len, bytes.clone());
        Ok(bytes)
    }
}
