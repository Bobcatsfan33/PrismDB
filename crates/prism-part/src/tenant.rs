//! Tenant identity at rest: a keyed, store-scoped equality token (S14 DATA-01,
//! [D-096](../../../docs/DECISIONS.md), [encryption contract §6a](../../../docs/ENCRYPTION-CONTRACT.md)).
//!
//! **Routing and pruning need tenant *equality*, not tenant *identity*.** The catalog asks "is this
//! part's tenant the one I am querying for?" and "do these rows belong to the same tenant?" — both
//! answered by comparing opaque handles. Nothing in the read path recovers a name, so nothing stored
//! needs to contain one.
//!
//! **Why this is keyed, and why an unkeyed hash is refused.** Tenant names are **low-entropy**:
//! short, human-chosen, drawn from a small realistic space (`acme`, `t1`, a customer's own name),
//! and usually *already known* to whoever holds the disk image. Their question is not "who exists in
//! the world" but "**which of them are on this box, and which share a bucket**" — and a bare
//! `SHA-256(tenant)` answers exactly that by **dictionary lookup**: precompute the digest of every
//! plausible name and the map inverts in full. An unkeyed digest is also **identical in every
//! deployment**, so it correlates the same tenant across stores, backups and customers — a global
//! join key wearing the costume of a protection.
//!
//! The keyed construction defeats both. Without the key a token is uninvertible and unforgeable, and
//! because the key is **store-scoped** the same tenant tokenizes *differently* in different stores —
//! which [`tests::the_same_tenant_tokenizes_differently_under_different_store_keys`] asserts rather
//! than leaving as a claim.
//!
//! **What a token is not.** It authorizes nothing. Tenant authorization stays exact-certificate
//! policy at the service boundary, above the engine. A token is an equality handle and no more.
//!
//! **The residual leak, named.** Token *equality* is visible without the key: an observer can still
//! see that some tenant occupies N parts and shares a bucket with some other tenant. Sealing
//! identity is not sealing the existence of distinct tenants, and this module does not pretend
//! otherwise.

use prism_types::error::{PrismError, Result};
use prism_types::hash::hmac_sha256;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// Bytes of HMAC output kept. 128 bits: collision-resistant far beyond any realistic tenant
/// cardinality (a store with 10^6 tenants sits at ~10^-27 collision probability), while keeping the
/// manifest and mirror entries small. Truncating a MAC is sound — the security of a truncated HMAC
/// is bounded by the truncation length, and 128 bits is well above the preimage bar that matters
/// here.
pub const TENANT_TOKEN_BYTES: usize = 16;

/// The domain separator mixed into every tenant token, so a `store_tenant_key` used for tenant
/// tokenization can never produce a value that collides with some other keyed construction built on
/// the same key material.
const TENANT_TOKEN_DOMAIN: &[u8] = b"prism-tenant-token-v1\x00";

/// An opaque, store-scoped handle for a tenant name. Comparable, storable, and not reversible
/// without the store's tenant key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TenantToken(pub [u8; TENANT_TOKEN_BYTES]);

impl TenantToken {
    pub fn as_bytes(&self) -> &[u8; TENANT_TOKEN_BYTES] {
        &self.0
    }

    /// Lowercase hex — how a token appears in a manifest dump or an error. Deliberately the *only*
    /// rendering: there is no `Display` that could accidentally print a name, because there is no
    /// name here to print.
    pub fn to_hex(&self) -> String {
        prism_types::hash::hex(&self.0)
    }

    pub fn from_hex(text: &str) -> Result<Self> {
        if text.len() != TENANT_TOKEN_BYTES * 2 {
            return Err(PrismError::Corrupt(format!(
                "a tenant token is {} hex characters; got {}",
                TENANT_TOKEN_BYTES * 2,
                text.len()
            )));
        }
        let mut out = [0u8; TENANT_TOKEN_BYTES];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                .map_err(|_| PrismError::Corrupt("a tenant token is not valid hex".into()))?;
        }
        Ok(TenantToken(out))
    }
}

impl std::fmt::Debug for TenantToken {
    /// Shows the token and never anything else — there is no plaintext here, and a `Debug` that
    /// implied one would be a lie in every error chain that formatted it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TenantToken({})", self.to_hex())
    }
}

/// Turns tenant names into tokens under one store's key.
///
/// Holds key material, so it is never `Debug`-printable and its bytes are zeroized on drop.
pub struct TenantTokenizer {
    key: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for TenantTokenizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantTokenizer")
            .field("key", &"<redacted>")
            .finish()
    }
}

impl TenantTokenizer {
    /// Build a tokenizer over a store-scoped key. The key comes from the **existing** hierarchy
    /// (§1) — no new custody, no new ceremony, nothing extra to rotate.
    pub fn new(key: Zeroizing<[u8; 32]>) -> Self {
        Self { key }
    }

    /// `HMAC-SHA256(store_tenant_key, domain ‖ tenant)`, truncated to [`TENANT_TOKEN_BYTES`].
    ///
    /// Deterministic **within a store** — the same name always yields the same token, which is what
    /// makes equality comparison work across parts and across the mirror — and unrelated **between**
    /// stores, which is what stops a token being a cross-deployment join key.
    pub fn token(&self, tenant: &str) -> TenantToken {
        let mut msg = Vec::with_capacity(TENANT_TOKEN_DOMAIN.len() + tenant.len());
        msg.extend_from_slice(TENANT_TOKEN_DOMAIN);
        msg.extend_from_slice(tenant.as_bytes());
        let full = hmac_sha256(&*self.key, &msg);
        let mut out = [0u8; TENANT_TOKEN_BYTES];
        out.copy_from_slice(&full[..TENANT_TOKEN_BYTES]);
        TenantToken(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tk(byte: u8) -> TenantTokenizer {
        TenantTokenizer::new(Zeroizing::new([byte; 32]))
    }

    #[test]
    fn a_token_is_deterministic_within_a_store() {
        let t = tk(7);
        assert_eq!(t.token("acme"), t.token("acme"));
        // ...which is the whole basis of equality-without-identity: two parts holding the same
        // tenant produce the same handle, so pruning and routing still work.
        assert_eq!(t.token("acme").to_hex(), t.token("acme").to_hex());
    }

    #[test]
    fn different_tenants_get_different_tokens() {
        let t = tk(7);
        assert_ne!(t.token("acme"), t.token("acme2"));
        assert_ne!(t.token("t1"), t.token("t2"));
    }

    /// **The store-scoping gate — the half that makes the keying worth having.**
    ///
    /// An unkeyed digest is identical in every deployment, so it correlates the same tenant across
    /// stores, backups and customers. This asserts the keyed construction does not: two stores with
    /// different keys must produce unrelated tokens for the *same* name.
    #[test]
    fn the_same_tenant_tokenizes_differently_under_different_store_keys() {
        let a = tk(1);
        let b = tk(2);
        for tenant in ["acme", "t1", "a-very-well-known-customer", ""] {
            assert_ne!(
                a.token(tenant),
                b.token(tenant),
                "tenant `{tenant}` tokenized identically under two different store keys — the token \
                 is acting as a cross-deployment join key, which is exactly what keying it was for"
            );
        }
    }

    /// **The forbidden construction, stated as a test.** A token must not be derivable from the name
    /// alone; if it ever equalled a bare digest of the name, every low-entropy tenant name would
    /// invert by dictionary lookup.
    #[test]
    fn a_token_is_not_an_unkeyed_hash_of_the_name() {
        let t = tk(7);
        for tenant in ["acme", "t1"] {
            let unkeyed = prism_types::hash::sha256(tenant.as_bytes());
            assert_ne!(
                t.token(tenant).as_bytes()[..],
                unkeyed[..TENANT_TOKEN_BYTES],
                "the token equals SHA-256(name) truncated — it is dictionary-invertible"
            );
            // ...and not a bare digest of the domain-prefixed name either.
            let mut msg = TENANT_TOKEN_DOMAIN.to_vec();
            msg.extend_from_slice(tenant.as_bytes());
            let unkeyed_domained = prism_types::hash::sha256(&msg);
            assert_ne!(
                t.token(tenant).as_bytes()[..],
                unkeyed_domained[..TENANT_TOKEN_BYTES],
                "the token is an unkeyed digest with a domain prefix — still dictionary-invertible"
            );
        }
    }

    #[test]
    fn a_token_round_trips_through_hex_and_refuses_malformed_input() {
        let t = tk(3).token("acme");
        assert_eq!(TenantToken::from_hex(&t.to_hex()).unwrap(), t);
        assert!(TenantToken::from_hex("abcd").is_err());
        assert!(TenantToken::from_hex(&"z".repeat(TENANT_TOKEN_BYTES * 2)).is_err());
    }

    #[test]
    fn neither_the_tokenizer_nor_a_token_can_print_key_material_or_a_name() {
        let t = tk(0xAB);
        let rendered = format!("{t:?}");
        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains("171"), "key bytes leaked: {rendered}");
        let token = t.token("acme");
        assert!(!format!("{token:?}").contains("acme"));
    }
}

/// **The mixed-version match rule** ([D-096](../../../docs/DECISIONS.md)).
///
/// A store can hold v3 parts (handles are **names**) and v4 parts (handles are **tokens**) at the
/// same time — during a migration, and for as long as an operator chooses not to merge. Both must
/// answer correctly from one query, so a handle matches if it equals **either** the tenant's name
/// **or** its token under this store's key.
///
/// It is written once, here, rather than at each of the comparison sites, because a site that
/// remembered only one half would not fail loudly: matching names alone silently returns **empty**
/// for every v4 part, and matching tokens alone silently returns empty for every v3 part. An empty
/// answer is indistinguishable from a legitimate "no rows", which is the failure mode this whole
/// increment is trying not to introduce.
///
/// Accepting both is safe rather than merely convenient: a token is 128 bits of keyed MAC rendered
/// as hex, so a *name* colliding with some other tenant's token would require an operator to have
/// literally named a tenant after that hex string — and even then the token is keyed, so it is not
/// something an attacker can arrange from outside.
pub fn handle_matches(
    handles: &[String],
    tenant: &str,
    tokenizer: Option<&TenantTokenizer>,
) -> bool {
    if handles.iter().any(|h| h == tenant) {
        return true;
    }
    match tokenizer {
        Some(t) => {
            let token = t.token(tenant).to_hex();
            handles.contains(&token)
        }
        None => false,
    }
}

#[cfg(test)]
mod match_tests {
    use super::*;

    fn tk(byte: u8) -> TenantTokenizer {
        TenantTokenizer::new(Zeroizing::new([byte; 32]))
    }

    #[test]
    fn a_v3_handle_matches_by_name_with_or_without_a_key() {
        let names = vec!["acme".to_string(), "t1".to_string()];
        assert!(handle_matches(&names, "acme", None));
        assert!(handle_matches(&names, "acme", Some(&tk(1))));
        assert!(!handle_matches(&names, "nobody", Some(&tk(1))));
    }

    #[test]
    fn a_v4_handle_matches_by_token_only_with_the_key() {
        let t = tk(1);
        let tokens = vec![t.token("acme").to_hex()];
        assert!(handle_matches(&tokens, "acme", Some(&t)));
        // Without the key there is nothing to compare against -- and the honest outcome is "no
        // match", which the caller turns into a named keyless refusal rather than a silent empty.
        assert!(!handle_matches(&tokens, "acme", None));
        // A different store's key must not match this store's tokens.
        assert!(!handle_matches(&tokens, "acme", Some(&tk(2))));
    }

    #[test]
    fn a_mixed_store_matches_both_generations_of_handle() {
        // The migration state: some parts still name their tenants, some have been rewritten.
        let t = tk(1);
        let v3 = vec!["acme".to_string()];
        let v4 = vec![t.token("acme").to_hex()];
        assert!(handle_matches(&v3, "acme", Some(&t)));
        assert!(handle_matches(&v4, "acme", Some(&t)));
        assert!(!handle_matches(&v3, "other", Some(&t)));
        assert!(!handle_matches(&v4, "other", Some(&t)));
    }
}
