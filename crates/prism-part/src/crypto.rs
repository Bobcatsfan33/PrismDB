//! The block cipher (S14, [D-095](../../../docs/DECISIONS.md), [encryption contract](../../../docs/ENCRYPTION-CONTRACT.md)).
//!
//! Encryption is applied **per framed block**, never per whole file, because three existing
//! contracts depend on blocks staying independently addressable: ranged fetches and the declared
//! byte fetch budget ([storage §6](../../../docs/STORAGE-CONTRACT.md)), per-block CRC-32 named-byte
//! errors ([S1](../../../docs/PROGRESS.md)), and block-level cache admission and verification
//! ([storage §4](../../../docs/STORAGE-CONTRACT.md)).
//!
//! **The AAD is what stops a block being moved.** Confidentiality alone would leave an attacker
//! free to swap block 7 of a column for block 3, or a block of tenant A's part for the same-shaped
//! block of tenant B's. The associated data binds the part id, the column name, the block index and
//! the key id, so any of those substitutions fails the tag instead of decrypting into
//! plausible-looking bytes.
//!
//! **The nonce is random, not derived, and not seeded from the engine PRNG.** Everything else in
//! PrismDB draws from the reproducible seeded generator ([`prism_types::rng`]) because determinism
//! is a contract — but a nonce drawn from a reproducible stream is a reused nonce the moment two
//! writers share a seed, and nonce reuse under a stream cipher destroys confidentiality *and*
//! authenticity at once. This is the one place the project deliberately uses the operating system's
//! CSPRNG. XChaCha20's 192-bit nonce makes random selection safe with no counter and no
//! coordination between writers or ownership epochs ([D-076](../../../docs/DECISIONS.md)).

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use prism_types::error::{PrismError, Result};
use zeroize::Zeroizing;

/// The AEAD this build implements. Recorded in the envelope so the choice is **versioned**, exactly
/// as the rerank encoding is ([D-003-resolved](../../../docs/DECISIONS.md)): an id this build does
/// not implement is refused at open time, never decoded into plausible numbers.
pub const AEAD_XCHACHA20_POLY1305: u16 = 1;

/// Nonce bytes stored ahead of each sealed block.
pub const NONCE_BYTES: usize = 24;
/// Poly1305 tag bytes appended by the AEAD.
pub const TAG_BYTES: usize = 16;
/// What sealing adds to a block: the stored nonce plus the tag.
pub const SEAL_OVERHEAD: usize = NONCE_BYTES + TAG_BYTES;

/// The label a **part's** block cipher carries into its associated data.
///
/// **This names the DEK, not the wrapping key, and the distinction is load-bearing.** The AAD's
/// "key" component exists so a block sealed under one key cannot be opened as a block sealed under
/// another. For a *data block* the key in question is the DEK — the wrapping key never touches the
/// block, only the 32 bytes that open it.
///
/// Binding the block AAD to the wrapping key id instead would make [rotation §9](../../../docs/ENCRYPTION-CONTRACT.md)
/// structurally impossible: rewrapping changes which key holds the DEK, so every block in the store
/// would fail its tag the moment its envelope was rewrapped — and "rewrapping never rewrites part
/// bytes" would be a promise the format could not keep. A DEK's identity is stable across every
/// rewrap, by construction, because a rewrap does not change the DEK.
///
/// Key *wrapping* is the opposite case and keeps using the wrapping key id ([`wrap_dek`]): there the
/// key in question really is the wrapping key, and re-labelling a wrapped DEK must fail the tag.
pub fn dek_label(dek_epoch: u64) -> String {
    format!("dek-epoch-{dek_epoch}")
}

/// Generate a fresh 256-bit DEK from the operating system's CSPRNG.
///
/// Not from [`prism_types::rng`]: a key drawn from the reproducible seeded generator would be
/// reproducible by anyone holding the seed, which is the opposite of a key.
pub fn generate_dek() -> Result<Zeroizing<[u8; 32]>> {
    use chacha20poly1305::aead::rand_core::RngCore;
    let mut key = Zeroizing::new([0u8; 32]);
    OsRng
        .try_fill_bytes(&mut *key)
        .map_err(|e| PrismError::Io(format!("could not draw a data encryption key: {e}")))?;
    Ok(key)
}

/// Wrap a DEK under a wrapping key, binding the wrapping key's id as associated data.
///
/// Key wrapping is itself an AEAD operation here rather than a bespoke construction: the wrapped
/// blob is `nonce ‖ ciphertext ‖ tag` exactly as a sealed block is, and the key id in the AAD means
/// a wrapped DEK cannot be re-labelled as having come from a different key without failing the tag.
pub fn wrap_dek(wrapping_key: &[u8; 32], key_id: &str, dek: &[u8; 32]) -> Result<Vec<u8>> {
    let cipher = BlockCipher::new(Zeroizing::new(*wrapping_key), key_id);
    cipher.seal(WRAP_DOMAIN, WRAP_COLUMN, 0, dek)
}

/// Unwrap a DEK. A tag failure here means the wrapping key is wrong, the blob was modified, or it
/// was wrapped under a different key id — all of which are refusals, never fallbacks.
pub fn unwrap_dek(
    wrapping_key: &[u8; 32],
    key_id: &str,
    wrapped: &[u8],
) -> Result<Zeroizing<[u8; 32]>> {
    let cipher = BlockCipher::new(Zeroizing::new(*wrapping_key), key_id);
    let plain = cipher.open(WRAP_DOMAIN, WRAP_COLUMN, 0, wrapped)?;
    if plain.len() != 32 {
        return Err(PrismError::Corrupt(format!(
            "unwrapped key material is {} bytes, expected 32",
            plain.len()
        )));
    }
    let mut dek = Zeroizing::new([0u8; 32]);
    dek.copy_from_slice(&plain);
    Ok(dek)
}

/// Domain separation for key wrapping, so a wrapped DEK can never be confused with a sealed data
/// block even if an attacker could choose part ids.
const WRAP_DOMAIN: &str = "\u{0}prism-key-wrap";
const WRAP_COLUMN: &str = "\u{0}dek";

/// The reserved column name under which a backup receipt's tenant list is sealed
/// ([encryption contract §6](../../../docs/ENCRYPTION-CONTRACT.md)).
///
/// NUL-prefixed for the same reason [`WRAP_COLUMN`] is: a real column is named after a field
/// (`body.data`, `attr.gen_ai.system`) and can never begin with a NUL, so a sealed tenant list can
/// never be substituted for a data block, nor a data block for it. The AAD already binds the part
/// id, so a receipt's tenant list is also non-transplantable between parts.
pub const RECEIPT_TENANTS_COLUMN: &str = "\u{0}backup-receipt-tenants";

/// A cipher over a data encryption key held in memory.
///
/// The plaintext key exists here and nowhere else — never in a manifest, a receipt, a log, a metric,
/// an error, or `EXPLAIN` output. Errors name a key by **id**, never by value.
///
/// **Scope of the zeroization claim, stated precisely.** The caller's copy of the key bytes is
/// zeroized by [`Zeroizing`] when [`BlockCipher::new`] returns, and the bounded DEK cache drops its
/// entries on eviction. The AEAD's *internal* key schedule is owned by the `chacha20poly1305`
/// crate, and this build does not claim to control when that is wiped. Bounding how many keys are
/// resident is what this codebase can guarantee; scrubbing another crate's private state is not.
pub struct BlockCipher {
    cipher: XChaCha20Poly1305,
    key_id: String,
    algorithm: u16,
}

impl std::fmt::Debug for BlockCipher {
    /// Deliberately opaque: a `Debug` that printed the key would put it in every error chain that
    /// ever formatted a struct containing one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockCipher")
            .field("key_id", &self.key_id)
            .field("algorithm", &self.algorithm)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// The canonical associated data for one block.
///
/// Length-prefixed so the encoding is unambiguous: without the prefixes, part `"ab"` column `"c"`
/// and part `"a"` column `"bc"` would produce identical AAD and become substitutable for each other.
fn aad(part_id: &str, column: &str, block_index: usize, key_id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(part_id.len() + column.len() + key_id.len() + 32);
    for field in [part_id.as_bytes(), column.as_bytes(), key_id.as_bytes()] {
        out.extend_from_slice(&(field.len() as u64).to_le_bytes());
        out.extend_from_slice(field);
    }
    out.extend_from_slice(&(block_index as u64).to_le_bytes());
    out
}

impl BlockCipher {
    /// Build a cipher over a 32-byte DEK. The key is moved into the AEAD state and the caller's
    /// copy is zeroized by [`Zeroizing`].
    pub fn new(key: Zeroizing<[u8; 32]>, key_id: impl Into<String>) -> Self {
        Self {
            cipher: XChaCha20Poly1305::new((&*key).into()),
            key_id: key_id.into(),
            algorithm: AEAD_XCHACHA20_POLY1305,
        }
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn algorithm(&self) -> u16 {
        self.algorithm
    }

    /// Seal one block: `nonce ‖ ciphertext ‖ tag`.
    pub fn seal(
        &self,
        part_id: &str,
        column: &str,
        block_index: usize,
        plaintext: &[u8],
    ) -> Result<Vec<u8>> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let sealed = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad(part_id, column, block_index, &self.key_id),
                },
            )
            .map_err(|_| {
                // The AEAD's error carries no detail by design; naming the block is ours to add.
                PrismError::Invariant(format!(
                    "sealing {column} block {block_index} of part `{part_id}` failed under key \
                     `{}`",
                    self.key_id
                ))
            })?;
        let mut out = Vec::with_capacity(NONCE_BYTES + sealed.len());
        out.extend_from_slice(nonce.as_slice());
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// Open one sealed block. A tag failure is a **named corruption**, distinct from a CRC failure
    /// and from a truncation, because the three mean different things to an operator: a wrong key,
    /// a flipped bit, and a short read.
    pub fn open(
        &self,
        part_id: &str,
        column: &str,
        block_index: usize,
        sealed: &[u8],
    ) -> Result<Vec<u8>> {
        if sealed.len() < SEAL_OVERHEAD {
            return Err(PrismError::Corrupt(format!(
                "{column} block {block_index} of part `{part_id}` is truncated: a sealed block \
                 needs at least {SEAL_OVERHEAD} bytes of nonce and tag, got {}",
                sealed.len()
            )));
        }
        let (nonce, body) = sealed.split_at(NONCE_BYTES);
        self.cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: body,
                    aad: &aad(part_id, column, block_index, &self.key_id),
                },
            )
            .map_err(|_| {
                PrismError::Corrupt(format!(
                    "{column} block {block_index} of part `{part_id}` failed authenticated \
                     decryption under key `{}`: the bytes were modified, or they were sealed for a \
                     different part, column, block position, or key",
                    self.key_id
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher(byte: u8, id: &str) -> BlockCipher {
        BlockCipher::new(Zeroizing::new([byte; 32]), id)
    }

    #[test]
    fn a_sealed_block_round_trips() {
        let c = cipher(1, "key-a");
        let sealed = c.seal("p1", "body.data", 3, b"hello block").unwrap();
        assert_ne!(&sealed[NONCE_BYTES..], b"hello block".as_slice());
        assert_eq!(
            c.open("p1", "body.data", 3, &sealed).unwrap(),
            b"hello block"
        );
    }

    #[test]
    fn sealing_twice_never_repeats_a_nonce() {
        let c = cipher(1, "key-a");
        let a = c.seal("p1", "body.data", 0, b"same").unwrap();
        let b = c.seal("p1", "body.data", 0, b"same").unwrap();
        assert_ne!(a[..NONCE_BYTES], b[..NONCE_BYTES], "nonces must not repeat");
        assert_ne!(a, b, "identical plaintext must not produce identical bytes");
    }

    #[test]
    fn a_block_cannot_be_moved_to_another_position_column_or_part() {
        let c = cipher(1, "key-a");
        let sealed = c.seal("p1", "body.data", 3, b"payload").unwrap();
        for (part, column, index, why) in [
            ("p1", "body.data", 4usize, "another block index"),
            ("p1", "event_id.data", 3, "another column"),
            ("p2", "body.data", 3, "another part"),
        ] {
            let err = c.open(part, column, index, &sealed).unwrap_err();
            assert!(
                matches!(err, PrismError::Corrupt(_)),
                "moving a block to {why} must be refused: {err}"
            );
        }
    }

    #[test]
    fn another_tenants_key_cannot_open_the_block() {
        let a = cipher(1, "tenant-a");
        let b = cipher(2, "tenant-b");
        let sealed = a.seal("p1", "body.data", 0, b"tenant a rows").unwrap();
        let err = b.open("p1", "body.data", 0, &sealed).unwrap_err();
        assert!(matches!(err, PrismError::Corrupt(_)), "{err}");
    }

    #[test]
    fn the_same_key_bytes_under_a_different_key_id_are_refused() {
        // The key id is in the AAD, so an envelope that lies about which key sealed a block fails
        // even when the bytes happen to match.
        let a = cipher(7, "key-v1");
        let b = cipher(7, "key-v2");
        let sealed = a.seal("p1", "body.data", 0, b"rows").unwrap();
        assert!(b.open("p1", "body.data", 0, &sealed).is_err());
    }

    #[test]
    fn a_flipped_ciphertext_or_nonce_byte_is_named_corruption() {
        let c = cipher(1, "key-a");
        let base = c.seal("p1", "body.data", 0, b"payload bytes").unwrap();
        for i in [0usize, NONCE_BYTES, base.len() - 1] {
            let mut bad = base.clone();
            bad[i] ^= 0xff;
            let err = c.open("p1", "body.data", 0, &bad).unwrap_err();
            assert!(matches!(err, PrismError::Corrupt(_)), "byte {i}: {err}");
        }
    }

    #[test]
    fn a_short_block_is_a_truncation_not_a_tag_failure() {
        let c = cipher(1, "key-a");
        let err = c.open("p1", "body.data", 0, &[0u8; 8]).unwrap_err();
        assert!(format!("{err}").contains("truncated"), "{err}");
    }

    #[test]
    fn debug_never_prints_key_material() {
        let c = cipher(0xAB, "key-a");
        let rendered = format!("{c:?}");
        assert!(rendered.contains("key-a"));
        assert!(rendered.contains("redacted"));
        assert!(
            !rendered.contains("171"),
            "key bytes must not appear: {rendered}"
        );
    }

    #[test]
    fn a_dek_round_trips_through_wrapping() {
        let master = [3u8; 32];
        let dek = generate_dek().unwrap();
        let wrapped = wrap_dek(&master, "key-v1", &dek).unwrap();
        assert_ne!(
            &wrapped[NONCE_BYTES..],
            &dek[..],
            "the DEK must not be stored in the clear"
        );
        let back = unwrap_dek(&master, "key-v1", &wrapped).unwrap();
        assert_eq!(&*back, &*dek);
    }

    #[test]
    fn two_generated_deks_differ() {
        assert_ne!(&*generate_dek().unwrap(), &*generate_dek().unwrap());
    }

    #[test]
    fn a_wrapped_dek_cannot_be_relabelled_as_another_keys() {
        let master = [3u8; 32];
        let dek = generate_dek().unwrap();
        let wrapped = wrap_dek(&master, "key-v1", &dek).unwrap();
        // Same wrapping key bytes, but claiming a different key id: the id is in the AAD.
        assert!(unwrap_dek(&master, "key-v2", &wrapped).is_err());
    }

    #[test]
    fn the_wrong_wrapping_key_cannot_unwrap() {
        let dek = generate_dek().unwrap();
        let wrapped = wrap_dek(&[3u8; 32], "key-v1", &dek).unwrap();
        assert!(unwrap_dek(&[4u8; 32], "key-v1", &wrapped).is_err());
    }

    #[test]
    fn a_sealed_data_block_cannot_be_unwrapped_as_a_dek() {
        // Domain separation: the wrap AAD uses a reserved part/column no data block can claim.
        let master = [3u8; 32];
        let c = BlockCipher::new(Zeroizing::new(master), "key-v1");
        let block = c.seal("p1", "body.data", 0, &[0u8; 32]).unwrap();
        assert!(unwrap_dek(&master, "key-v1", &block).is_err());
    }

    #[test]
    fn aad_is_unambiguous_across_field_boundaries() {
        // Without length prefixes these two would produce identical associated data.
        assert_ne!(aad("ab", "c", 0, "k"), aad("a", "bc", 0, "k"));
    }
}
