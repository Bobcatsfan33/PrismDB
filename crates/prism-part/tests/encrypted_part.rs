//! An encrypted part is ciphertext on disk, identical in answer, and refuses to be read blind
//! (S14, [D-095](../../../../docs/DECISIONS.md)).
//!
//! The properties that matter here are the ones a reviewer cannot check by reading the writer:
//! that the bytes on disk really are sealed, that reading them back through the key returns exactly
//! what a plaintext part would, that an encrypted part and its plaintext twin share a content
//! address, and that a reader without the key refuses rather than handing back nonce-and-tag as
//! though it were column data.

use prism_part::crypto::BlockCipher;
use prism_part::ext::S14Ext;
use prism_part::part::{PartEncryption, PartReader, PartSpec, PartWriter, RowIn};
use prism_part::store::{Store, StoreConfig, STORE_VERSION};
use prism_types::rng::Rng;
use prism_types::Event;
use std::sync::Arc;
use zeroize::Zeroizing;

const DIM: usize = 8;
const PQ_M: usize = 2;

fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("prism-encpart-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn config() -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: DIM,
        nlist: 2,
        pq_m: PQ_M,
        seed: 7,
        kmeans_restarts: 1,
        block_size: 1024,
        partitions: Default::default(),
        promote: Vec::new(),
    }
}

fn rows(n: usize) -> Vec<RowIn> {
    let mut rng = Rng::new(7);
    (0..n)
        .map(|i| {
            let mut v: Vec<f32> = (0..DIM).map(|_| rng.normal()).collect();
            prism_types::validate_and_normalize(&mut v).unwrap();
            RowIn {
                event: Event {
                    observed_time: 1_000 + i as i64,
                    trace_id: String::new(),
                    span_id: String::new(),
                    attributes: Default::default(),
                    idempotency_key: None,
                    event_id: format!("e{i:06}"),
                    tenant_id: "t1".into(),
                    event_time: 1_000 + i as i64,
                    event_name: "x".into(),
                    cost: 0.01,
                    error: false,
                    body: format!(
                        "a distinctive body number {i} that would be obvious in plaintext"
                    ),
                },
                centroid: (i % 2) as u32,
                code: (0..PQ_M).map(|j| ((i + j) % 251) as u8).collect(),
                vector: v,
            }
        })
        .collect()
}

fn encryption() -> PartEncryption {
    PartEncryption {
        cipher: Arc::new(BlockCipher::new(Zeroizing::new([9u8; 32]), "key-v1")),
        envelope: S14Ext {
            algorithm: prism_part::crypto::AEAD_XCHACHA20_POLY1305,
            wrapping_key_id: "key-v1".into(),
            dek_epoch: 1,
            wrapped_dek: vec![7u8; 48],
            bucket_ordinal: 3,
        },
    }
}

fn write(store: &Store, spec: &PartSpec) -> prism_part::part::PartManifest {
    PartWriter::write(
        &store.parts_dir(),
        1,
        "gen0",
        "hash-embedder",
        "1",
        DIM,
        PQ_M,
        1024,
        spec,
        rows(120),
        1_000,
    )
    .unwrap()
}

#[test]
fn an_encrypted_part_is_ciphertext_on_disk_and_reads_back_identically() {
    let plain_root = tmp("plain");
    let enc_root = tmp("enc");
    let plain_store = Store::init(&plain_root, config()).unwrap();
    let enc_store = Store::init(&enc_root, config()).unwrap();

    let enc = encryption();
    let plain_manifest = write(&plain_store, &PartSpec::default());
    let enc_manifest = write(
        &enc_store,
        &PartSpec {
            encryption: Some(enc.clone()),
            ..Default::default()
        },
    );

    // Content address is a function of the DATA, not of whether it was sealed.
    assert_eq!(
        plain_manifest.part_id, enc_manifest.part_id,
        "an encrypted part and its plaintext twin must share a content address"
    );

    // The bodies are on disk in the clear for the plaintext part, and nowhere for the encrypted one.
    let needle = b"a distinctive body number 7 that";
    let plain_body = std::fs::read(
        plain_store
            .part_dir(&plain_manifest.part_id)
            .join("body.dat"),
    )
    .unwrap();
    let enc_body =
        std::fs::read(enc_store.part_dir(&enc_manifest.part_id).join("body.dat")).unwrap();
    assert!(
        plain_body.windows(needle.len()).any(|w| w == needle),
        "the plaintext control must actually contain the body, or this test proves nothing"
    );
    assert!(
        !enc_body.windows(needle.len()).any(|w| w == needle),
        "an encrypted part must not contain its bodies in the clear"
    );

    // The feature bit and the required envelope both say so.
    let reader = PartReader::open(&enc_store.part_dir(&enc_manifest.part_id)).unwrap();
    assert!(reader.is_encrypted());
    let envelope = reader.encryption_envelope().expect("envelope present");
    assert_eq!(envelope.wrapping_key_id, "key-v1");
    assert_eq!(envelope.dek_epoch, 1);
    assert_eq!(envelope.bucket_ordinal, 3);
    assert_eq!(envelope.wrapped_dek, vec![7u8; 48]);

    // With the key, the answer is the plaintext part's answer.
    let plain_reader = PartReader::open(&plain_store.part_dir(&plain_manifest.part_id)).unwrap();
    let keyed = PartReader::open(&enc_store.part_dir(&enc_manifest.part_id))
        .unwrap()
        .with_cipher(Arc::clone(&enc.cipher));
    let want = plain_reader
        .read_vectors_for_rows(&[0, 5, 40, 119])
        .unwrap();
    let got = keyed.read_vectors_for_rows(&[0, 5, 40, 119]).unwrap();
    assert_eq!(got, want, "decryption must be answer-invariant");

    for r in [plain_root, enc_root] {
        let _ = std::fs::remove_dir_all(r);
    }
}

#[test]
fn an_encrypted_part_read_without_a_key_refuses_by_name() {
    let root = tmp("nokey");
    let store = Store::init(&root, config()).unwrap();
    let manifest = write(
        &store,
        &PartSpec {
            encryption: Some(encryption()),
            ..Default::default()
        },
    );

    // Opening succeeds: the manifest decodes and the block directory is readable. Integrity is
    // checkable without a key at all, because the CRC covers the STORED bytes.
    let reader = PartReader::open(&store.part_dir(&manifest.part_id)).unwrap();
    assert!(reader.is_encrypted());

    // Asking for data does not.
    let err = reader.read_vectors_for_rows(&[0]).unwrap_err().to_string();
    assert!(
        err.contains("encrypted") && err.contains("no key"),
        "a keyless read must refuse by name rather than return sealed bytes: {err}"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_wrong_key_is_refused_rather_than_returning_garbage() {
    let root = tmp("wrongkey");
    let store = Store::init(&root, config()).unwrap();
    let manifest = write(
        &store,
        &PartSpec {
            encryption: Some(encryption()),
            ..Default::default()
        },
    );

    let wrong = Arc::new(BlockCipher::new(Zeroizing::new([1u8; 32]), "key-v1"));
    let reader = PartReader::open(&store.part_dir(&manifest.part_id))
        .unwrap()
        .with_cipher(wrong);
    let err = reader.read_vectors_for_rows(&[0]).unwrap_err().to_string();
    assert!(
        err.contains("authenticated decryption"),
        "a wrong key must be a named authentication failure, never plausible-looking bytes: {err}"
    );
    let _ = std::fs::remove_dir_all(root);
}
