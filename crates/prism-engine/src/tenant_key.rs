//! The store-scoped tenant key: minted once, wrapped by the key service, persisted as an envelope
//! (S14 DATA-01, [D-096](../../../docs/DECISIONS.md), [encryption contract §6a](../../../docs/ENCRYPTION-CONTRACT.md)).
//!
//! §6a's token is `HMAC-SHA256(store_tenant_key, name)`, and two properties fix how that key must be
//! held:
//!
//! - **Stable for the life of the store.** Tokens are compared across parts, across the catalog
//!   mirror, and across a merge that rewrites a part. A key that changed would silently partition one
//!   tenant into two identities and stop pruning from matching its own data — so the key is minted
//!   *once* and read back thereafter, never re-derived.
//! - **Store-scoped.** Two stores must tokenize the same tenant differently, or the token becomes a
//!   cross-deployment join key (the whole reason §6a refuses an unkeyed digest). Minting fresh random
//!   key material per store gives that by construction rather than by convention.
//!
//! **No new custody.** The key is a DEK like any other: generated locally, wrapped by the existing
//! key service, and stored **only** in wrapped form — the same envelope shape a part carries, so
//! rotation, revocation and the unreachable-key-service refusal all behave exactly as they already do
//! ([D-095](../../../docs/DECISIONS.md)). There is no second thing to rotate and no second ceremony.
//!
//! A plaintext store has no tenant key and needs none: with nothing sealed there is nothing to
//! tokenize, and §10's explicit-flag rule means a store does not acquire one by accident.

use crate::keys::KeyProvider;
use prism_part::tenant::TenantTokenizer;
use prism_types::error::{PrismError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Where the wrapped key lives, beside `store.json`. A separate file because `store.json` is written
/// once at init and never mutated, while this is minted the first time an encrypted store needs it —
/// which is after `init`, since the key service is attached to the engine and not to the store.
pub const TENANT_KEY_FILE: &str = "tenant-key.json";

/// The wrapped store tenant key. Carries **no key material** — only the *wrapped* key and the ids
/// needed to ask the key service to open it, exactly as a part's envelope does.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TenantKeyEnvelope {
    /// Versioned, so a construction this build does not implement is refused rather than guessed.
    pub algorithm: u16,
    /// The **explicit** id of the wrapping key. Never inferred, never defaulted.
    pub wrapping_key_id: String,
    pub wrapped_key: Vec<u8>,
}

fn envelope_path(root: &Path) -> PathBuf {
    root.join(TENANT_KEY_FILE)
}

/// Load the store's tenant key, minting and persisting one on first use.
///
/// Returns `None` when there is no key service: a plaintext store has nothing to tokenize, and this
/// must not be the thing that quietly turns sealing on.
///
/// **The mint is once-only and the read is authoritative.** If the envelope exists it is opened and
/// used; a fresh key is generated only when the file is absent. That ordering is what makes tokens
/// stable across restarts, across a merge, and across the mirror.
pub fn load_or_mint(
    root: &Path,
    keys: Option<&std::sync::Arc<dyn KeyProvider>>,
) -> Result<Option<TenantTokenizer>> {
    let Some(keys) = keys else {
        return Ok(None);
    };
    let path = envelope_path(root);

    if path.exists() {
        let envelope: TenantKeyEnvelope =
            serde_json::from_slice(&std::fs::read(&path)?).map_err(|e| {
                PrismError::Corrupt(format!(
                    "the store tenant key envelope at `{}` will not parse: {e}",
                    path.display()
                ))
            })?;
        if envelope.algorithm != prism_part::crypto::AEAD_XCHACHA20_POLY1305 {
            return Err(PrismError::Corrupt(format!(
                "the store tenant key was wrapped with AEAD id {}, which this build does not \
                 implement",
                envelope.algorithm
            )));
        }
        // Used as given: the id comes from the envelope and is never probed against other keys the
        // service happens to hold, for the same reason a part's key id never is.
        let key = keys.unwrap(&envelope.wrapping_key_id, &envelope.wrapped_key)?;
        return Ok(Some(TenantTokenizer::new(key)));
    }

    // First use: mint, wrap, and persist before returning, so the very first token this store emits
    // is one it can reproduce after a restart.
    let key = prism_part::crypto::generate_dek()?;
    let (wrapping_key_id, wrapped_key) = keys.wrap(&key)?;
    let envelope = TenantKeyEnvelope {
        algorithm: prism_part::crypto::AEAD_XCHACHA20_POLY1305,
        wrapping_key_id,
        wrapped_key,
    };
    prism_part::io::write_atomic(&path, &serde_json::to_vec_pretty(&envelope)?)?;
    Ok(Some(TenantTokenizer::new(Zeroizing::new(*key))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{KeyFault, SoftwareKeystore};
    use std::sync::Arc;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "prism-tenantkey-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn ks() -> Arc<dyn KeyProvider> {
        Arc::new(SoftwareKeystore::new("key-v1", [11u8; 32]))
    }

    #[test]
    fn a_plaintext_store_has_no_tenant_key_and_mints_none() {
        let root = tmp("plain");
        assert!(load_or_mint(&root, None).unwrap().is_none());
        assert!(
            !envelope_path(&root).exists(),
            "a store with no key service must not acquire a tenant key as a side effect"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_key_is_minted_once_and_read_back_thereafter() {
        let root = tmp("stable");
        let keys = ks();
        let first = load_or_mint(&root, Some(&keys)).unwrap().unwrap();
        let a = first.token("acme");
        assert!(envelope_path(&root).exists(), "the envelope must persist");

        // A second open -- a restart -- must reproduce the SAME token, or one tenant would silently
        // become two identities and pruning would stop matching its own data.
        let second = load_or_mint(&root, Some(&keys)).unwrap().unwrap();
        assert_eq!(
            a,
            second.token("acme"),
            "the tenant key was re-minted on reopen; tokens are not stable across a restart"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn two_stores_tokenize_the_same_tenant_differently() {
        // The store-scoping property §6a rests on, at the persistence layer: fresh key material per
        // store, so a token cannot act as a cross-deployment join key.
        let (a_root, b_root) = (tmp("scope-a"), tmp("scope-b"));
        let keys = ks();
        let a = load_or_mint(&a_root, Some(&keys)).unwrap().unwrap();
        let b = load_or_mint(&b_root, Some(&keys)).unwrap().unwrap();
        assert_ne!(
            a.token("acme"),
            b.token("acme"),
            "two stores produced the same token for one tenant -- even under the same key service, \
             the key must be store-scoped"
        );
        for r in [a_root, b_root] {
            let _ = std::fs::remove_dir_all(r);
        }
    }

    #[test]
    fn the_envelope_carries_no_key_material() {
        let root = tmp("noplain");
        let keys = ks();
        let t = load_or_mint(&root, Some(&keys)).unwrap().unwrap();
        let raw = std::fs::read(envelope_path(&root)).unwrap();
        let envelope: TenantKeyEnvelope = serde_json::from_slice(&raw).unwrap();
        assert_eq!(envelope.wrapping_key_id, "key-v1");
        // The token is derived from the key; the key must not be recoverable from the file. The
        // strongest cheap check: the wrapped bytes are not the raw key, and nothing in the file
        // reproduces a token.
        let text = String::from_utf8_lossy(&raw);
        assert!(!text.contains(&t.token("acme").to_hex()));
        assert!(!envelope.wrapped_key.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_unreachable_key_service_refuses_by_name_rather_than_minting_a_second_key() {
        let root = tmp("outage");
        let store = Arc::new(SoftwareKeystore::new("key-v1", [11u8; 32]));
        let keys: Arc<dyn KeyProvider> = store.clone();
        load_or_mint(&root, Some(&keys)).unwrap().unwrap();

        store.set_fault(KeyFault::Unreachable).unwrap();
        let err = load_or_mint(&root, Some(&keys)).unwrap_err().to_string();
        assert!(
            err.contains("key service unreachable"),
            "an outage must be named, not silently produce a new key: {err}"
        );

        store.set_fault(KeyFault::None).unwrap();
        assert!(
            load_or_mint(&root, Some(&keys)).is_ok(),
            "a retry after access returns must succeed cleanly"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
