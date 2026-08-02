//! Key providers and the DEK cache (S14, [D-095](../../../docs/DECISIONS.md)).
//!
//! **One interface, one error taxonomy, two backends.** Production wraps under AWS KMS; staging
//! wraps under a software keystore. The staging backend exists so the drill exercises *the real
//! code* with only the custody claim differing — which is worth nothing unless it can also produce
//! the failures KMS produces. A local keystore has no throttling, no revoked-key state, no denied
//! principal and no network partition of its own, so those are **injectable** here: the same
//! branches run, and the same named errors come out, whichever backend is underneath.
//!
//! Every receipt records [`KeyProvider::backend`], because a gate that passed against a software
//! keystore has proven the code and not the custody, and a receipt that does not say which is a
//! receipt nobody can act on ([storage §5](../../../docs/STORAGE-CONTRACT.md)).

use prism_part::crypto::{unwrap_dek, wrap_dek, BlockCipher};
use prism_types::error::{PrismError, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use zeroize::Zeroizing;

/// What a backend calls itself in a receipt. Not an enum on purpose: a receipt records a string a
/// human reads, and a future backend should not require a match arm here to be nameable.
pub const BACKEND_SOFTWARE_KEYSTORE: &str = "software-keystore";
pub const BACKEND_AWS_KMS: &str = "aws-kms";

/// The failures a key service produces, named identically across backends.
///
/// These are the branches an encrypted store's behaviour actually depends on, so the staging
/// backend can produce every one of them on demand. `Unreachable` in particular is the one the
/// named hydration gate turns on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KeyFault {
    #[default]
    None,
    /// The key service cannot be reached at all — a region-wide outage, a severed network.
    Unreachable,
    /// The principal is not permitted to use the key.
    Denied,
    /// The service is up but shedding load.
    Throttled,
}

impl KeyFault {
    fn as_error(self, key_id: &str) -> Option<PrismError> {
        match self {
            KeyFault::None => None,
            // Io, not Invariant: an outage is an environment condition a retry can resolve, and the
            // hydration gate's "retries cleanly" half depends on it not being mistaken for damage.
            KeyFault::Unreachable => Some(PrismError::Io(format!(
                "key service unreachable: cannot reach the wrapping key `{key_id}`"
            ))),
            KeyFault::Denied => Some(PrismError::Policy(format!(
                "key service denied access to the wrapping key `{key_id}`"
            ))),
            KeyFault::Throttled => Some(PrismError::Io(format!(
                "key service throttled the request for the wrapping key `{key_id}`"
            ))),
        }
    }
}

/// Wrap and unwrap data encryption keys. Never sees plaintext data — only DEKs.
pub trait KeyProvider: Send + Sync {
    /// What produced this result, for the receipt.
    fn backend(&self) -> &'static str;
    /// The key new DEKs are wrapped under right now.
    fn active_key_id(&self) -> Result<String>;
    /// Wrap a DEK, returning the **explicit** id of the key that wrapped it.
    fn wrap(&self, dek: &[u8; 32]) -> Result<(String, Vec<u8>)>;
    /// Unwrap a DEK that was wrapped under `key_id`.
    ///
    /// The id comes from the part's envelope and is used **as given**: a provider must never fall
    /// back to trying other keys it happens to hold, because a successful unwrap under a key the
    /// part did not name is indistinguishable from a confusion attack.
    fn unwrap(&self, key_id: &str, wrapped: &[u8]) -> Result<Zeroizing<[u8; 32]>>;
}

/// The staging backend: wrapping keys held in memory, with the key service's failure modes
/// available on demand.
pub struct SoftwareKeystore {
    keys: Mutex<BTreeMap<String, Zeroizing<[u8; 32]>>>,
    active: Mutex<String>,
    /// Retired but still authorized to **unwrap** — the state that lets a backup taken before a
    /// rotation still restore ([D-095](../../../docs/DECISIONS.md) rotation).
    retired: Mutex<BTreeSet<String>>,
    /// Revoked: refused for every operation, by name.
    revoked: Mutex<BTreeSet<String>>,
    fault: Mutex<KeyFault>,
}

impl SoftwareKeystore {
    /// A keystore with one active wrapping key.
    pub fn new(key_id: impl Into<String>, key: [u8; 32]) -> Self {
        let key_id = key_id.into();
        let mut keys = BTreeMap::new();
        keys.insert(key_id.clone(), Zeroizing::new(key));
        Self {
            keys: Mutex::new(keys),
            active: Mutex::new(key_id),
            retired: Mutex::new(BTreeSet::new()),
            revoked: Mutex::new(BTreeSet::new()),
            fault: Mutex::new(KeyFault::None),
        }
    }

    /// **Expand**: add a key version without changing which one is active.
    pub fn expand(&self, key_id: impl Into<String>, key: [u8; 32]) -> Result<()> {
        self.keys
            .lock()
            .map_err(lock_poisoned)?
            .insert(key_id.into(), Zeroizing::new(key));
        Ok(())
    }

    /// **Activate**: new DEKs wrap under this key. The previous active key becomes retired — still
    /// authorized to unwrap, so nothing already encrypted becomes unreadable.
    pub fn activate(&self, key_id: &str) -> Result<()> {
        if !self
            .keys
            .lock()
            .map_err(lock_poisoned)?
            .contains_key(key_id)
        {
            return Err(PrismError::NotFound(format!(
                "cannot activate wrapping key `{key_id}`: the key service does not hold it"
            )));
        }
        let mut active = self.active.lock().map_err(lock_poisoned)?;
        if *active != key_id {
            self.retired
                .lock()
                .map_err(lock_poisoned)?
                .insert(active.clone());
            *active = key_id.to_string();
        }
        Ok(())
    }

    /// **Retire**: stop accepting a key for unwrap entirely.
    pub fn retire(&self, key_id: &str) -> Result<()> {
        if *self.active.lock().map_err(lock_poisoned)? == key_id {
            return Err(PrismError::Invalid(format!(
                "refusing to retire wrapping key `{key_id}`: it is the active key"
            )));
        }
        self.retired.lock().map_err(lock_poisoned)?.remove(key_id);
        self.keys.lock().map_err(lock_poisoned)?.remove(key_id);
        Ok(())
    }

    /// Revoke a key: every operation naming it is refused.
    pub fn revoke(&self, key_id: &str) -> Result<()> {
        self.revoked
            .lock()
            .map_err(lock_poisoned)?
            .insert(key_id.to_string());
        Ok(())
    }

    pub fn restore_revoked(&self, key_id: &str) -> Result<()> {
        self.revoked.lock().map_err(lock_poisoned)?.remove(key_id);
        Ok(())
    }

    /// Inject (or clear) the key service's failure mode.
    pub fn set_fault(&self, fault: KeyFault) -> Result<()> {
        *self.fault.lock().map_err(lock_poisoned)? = fault;
        Ok(())
    }

    fn check(&self, key_id: &str) -> Result<()> {
        if let Some(e) = (*self.fault.lock().map_err(lock_poisoned)?).as_error(key_id) {
            return Err(e);
        }
        if self.revoked.lock().map_err(lock_poisoned)?.contains(key_id) {
            return Err(PrismError::Policy(format!(
                "wrapping key `{key_id}` is revoked; data sealed under it cannot be opened"
            )));
        }
        Ok(())
    }
}

fn lock_poisoned<T>(_: T) -> PrismError {
    PrismError::Invariant("the key service lock was poisoned by a panic".into())
}

impl KeyProvider for SoftwareKeystore {
    fn backend(&self) -> &'static str {
        BACKEND_SOFTWARE_KEYSTORE
    }

    fn active_key_id(&self) -> Result<String> {
        Ok(self.active.lock().map_err(lock_poisoned)?.clone())
    }

    fn wrap(&self, dek: &[u8; 32]) -> Result<(String, Vec<u8>)> {
        let key_id = self.active_key_id()?;
        self.check(&key_id)?;
        let keys = self.keys.lock().map_err(lock_poisoned)?;
        let wrapping = keys.get(&key_id).ok_or_else(|| {
            PrismError::NotFound(format!(
                "the key service does not hold wrapping key `{key_id}`"
            ))
        })?;
        let wrapped = wrap_dek(wrapping, &key_id, dek)?;
        Ok((key_id, wrapped))
    }

    fn unwrap(&self, key_id: &str, wrapped: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
        self.check(key_id)?;
        let keys = self.keys.lock().map_err(lock_poisoned)?;
        // Used as given. No fallback to any other key we hold: an unwrap that succeeded under a key
        // the part did not name would be indistinguishable from a key-confusion attack.
        let wrapping = keys.get(key_id).ok_or_else(|| {
            PrismError::NotFound(format!(
                "the key service does not hold wrapping key `{key_id}`; it was retired, or this \
                 part was sealed by a deployment whose keys this one does not have"
            ))
        })?;
        unwrap_dek(wrapping, key_id, wrapped)
    }
}

/// A bounded cache of opened DEKs.
///
/// Bounded because an unbounded key cache is an unbounded amount of key material resident in
/// memory. Entries are dropped — and their key material zeroized — when evicted.
pub struct DekCache {
    entries: Mutex<VecDeque<(String, Arc<BlockCipher>)>>,
    capacity: usize,
}

impl DekCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
        }
    }

    /// The cipher for `cache_key`, opening it through `provider` on a miss.
    pub fn get_or_open(
        &self,
        cache_key: &str,
        open: impl FnOnce() -> Result<BlockCipher>,
    ) -> Result<Arc<BlockCipher>> {
        {
            let entries = self.entries.lock().map_err(lock_poisoned)?;
            if let Some((_, c)) = entries.iter().find(|(k, _)| k == cache_key) {
                return Ok(Arc::clone(c));
            }
        }
        let cipher = Arc::new(open()?);
        let mut entries = self.entries.lock().map_err(lock_poisoned)?;
        entries.push_back((cache_key.to_string(), Arc::clone(&cipher)));
        while entries.len() > self.capacity {
            entries.pop_front();
        }
        Ok(cipher)
    }

    /// Drop every cached key. Called when a key is revoked, so revocation takes effect against a
    /// running process rather than only against the next one.
    pub fn clear(&self) -> Result<()> {
        self.entries.lock().map_err(lock_poisoned)?.clear();
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.lock().map(|e| e.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_part::crypto::generate_dek;

    fn keystore() -> SoftwareKeystore {
        SoftwareKeystore::new("key-v1", [11u8; 32])
    }

    #[test]
    fn a_dek_wraps_and_unwraps_under_the_named_key() {
        let ks = keystore();
        let dek = generate_dek().unwrap();
        let (id, wrapped) = ks.wrap(&dek).unwrap();
        assert_eq!(id, "key-v1");
        assert_eq!(&*ks.unwrap(&id, &wrapped).unwrap(), &*dek);
        assert_eq!(ks.backend(), BACKEND_SOFTWARE_KEYSTORE);
    }

    #[test]
    fn an_unreachable_key_service_is_named_and_recovers_on_retry() {
        let ks = keystore();
        let dek = generate_dek().unwrap();
        let (id, wrapped) = ks.wrap(&dek).unwrap();

        ks.set_fault(KeyFault::Unreachable).unwrap();
        let err = ks.unwrap(&id, &wrapped).unwrap_err();
        assert!(
            format!("{err}").contains("key service unreachable"),
            "an outage must be named: {err}"
        );

        ks.set_fault(KeyFault::None).unwrap();
        assert_eq!(
            &*ks.unwrap(&id, &wrapped).unwrap(),
            &*dek,
            "a retry must succeed cleanly"
        );
    }

    #[test]
    fn a_revoked_key_is_refused_by_name() {
        let ks = keystore();
        let dek = generate_dek().unwrap();
        let (id, wrapped) = ks.wrap(&dek).unwrap();
        ks.revoke(&id).unwrap();
        let err = ks.unwrap(&id, &wrapped).unwrap_err();
        assert!(format!("{err}").contains("revoked"), "{err}");
        ks.restore_revoked(&id).unwrap();
        assert!(
            ks.unwrap(&id, &wrapped).is_ok(),
            "restoring access must be clean"
        );
    }

    #[test]
    fn an_unknown_key_id_is_refused_and_never_probed_against_other_keys() {
        let ks = keystore();
        let dek = generate_dek().unwrap();
        let (_, wrapped) = ks.wrap(&dek).unwrap();
        ks.expand("key-v2", [22u8; 32]).unwrap();
        let err = ks.unwrap("key-v9", &wrapped).unwrap_err();
        assert!(
            format!("{err}").contains("does not hold wrapping key"),
            "{err}"
        );
    }

    #[test]
    fn rotation_keeps_a_retired_key_authorized_to_unwrap() {
        let ks = keystore();
        let old_dek = generate_dek().unwrap();
        let (old_id, old_wrapped) = ks.wrap(&old_dek).unwrap();

        ks.expand("key-v2", [22u8; 32]).unwrap();
        ks.activate("key-v2").unwrap();

        // New DEKs wrap under the new key...
        let new_dek = generate_dek().unwrap();
        let (new_id, _) = ks.wrap(&new_dek).unwrap();
        assert_eq!(new_id, "key-v2");
        // ...and the retired key still opens what it sealed.
        assert_eq!(&*ks.unwrap(&old_id, &old_wrapped).unwrap(), &*old_dek);
    }

    #[test]
    fn a_fully_retired_key_can_no_longer_unwrap() {
        let ks = keystore();
        let dek = generate_dek().unwrap();
        let (old_id, wrapped) = ks.wrap(&dek).unwrap();
        ks.expand("key-v2", [22u8; 32]).unwrap();
        ks.activate("key-v2").unwrap();
        ks.retire(&old_id).unwrap();
        assert!(ks.unwrap(&old_id, &wrapped).is_err());
    }

    #[test]
    fn the_active_key_cannot_be_retired() {
        let ks = keystore();
        let err = ks.retire("key-v1").unwrap_err();
        assert!(format!("{err}").contains("active key"), "{err}");
    }

    #[test]
    fn activating_a_key_the_service_does_not_hold_is_refused() {
        assert!(keystore().activate("key-nope").is_err());
    }

    #[test]
    fn the_dek_cache_is_bounded_and_clearable() {
        let cache = DekCache::new(2);
        for i in 0..5 {
            cache
                .get_or_open(&format!("k{i}"), || {
                    Ok(BlockCipher::new(Zeroizing::new([i as u8; 32]), "key-v1"))
                })
                .unwrap();
        }
        assert_eq!(cache.len(), 2, "the cache must not grow without bound");
        cache.clear().unwrap();
        assert!(
            cache.is_empty(),
            "revocation must be able to drop resident key material"
        );
    }

    #[test]
    fn a_cache_hit_does_not_reopen_the_key() {
        let cache = DekCache::new(4);
        cache
            .get_or_open("a", || {
                Ok(BlockCipher::new(Zeroizing::new([1u8; 32]), "key-v1"))
            })
            .unwrap();
        let second = cache.get_or_open("a", || {
            panic!("a cache hit must not call the key service");
        });
        assert!(second.is_ok());
    }
}
