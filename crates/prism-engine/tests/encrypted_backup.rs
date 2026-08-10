//! A backup receipt of an encrypted store discloses no tenant (S14, [D-095](../../../../docs/DECISIONS.md),
//! [encryption contract §6](../../../../docs/ENCRYPTION-CONTRACT.md)).
//!
//! The receipt is the one artifact of an encrypted store that is **designed to be read without a
//! key** — hydration verifies stored-byte integrity against it before it can unwrap anything. That
//! is exactly what makes it the easiest place to leave the DATA-01 metadata gap open: a receipt
//! naming its tenants tells anyone holding the bucket which tenants exist and which share it,
//! without decrypting a single row.
//!
//! So the tenant list is sealed and the **bucket ordinal** stays in the clear — and these gates
//! prove the trade landed on the right side of both edges: nothing legible that should not be, and
//! the wrong-shard routing refusal still firing with no key at all.

use prism_engine::keys::{KeyProvider, SoftwareKeystore};
use prism_engine::storage::object::{CachedObjectStore, LocalObjectStore, ObjectStore};
use prism_engine::storage::{PartBackup, ShardPlacement, BACKUP_MANIFEST_FILE, CACHE_QUOTA_BYTES};
use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-encbak-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn config() -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 64,
        nlist: 16,
        pq_m: 8,
        seed: 9,
        kmeans_restarts: 1,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        partitions: Default::default(),
        promote: Vec::new(),
    }
}

fn events() -> Vec<prism_types::Event> {
    prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 400, 4)
}

/// An encrypted store with its published parts backed up, plus the backend holding them.
struct Backed {
    engine: Engine,
    backend: Arc<dyn ObjectStore>,
    keystore: Arc<SoftwareKeystore>,
    roots: Vec<PathBuf>,
}

impl Drop for Backed {
    fn drop(&mut self) {
        for r in &self.roots {
            let _ = std::fs::remove_dir_all(r);
        }
    }
}

fn backed_up(tag: &str, encrypted: bool) -> Backed {
    let store_dir = tmp(&format!("{tag}-objects"));
    let root = tmp(&format!("{tag}-root"));
    let backend: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(&store_dir));
    let keystore = Arc::new(SoftwareKeystore::new("key-v1", [11u8; 32]));

    let mut engine =
        Engine::init(&root, config())
            .unwrap()
            .with_cold(Arc::new(CachedObjectStore::new(
                Arc::clone(&backend),
                CACHE_QUOTA_BYTES,
            )));
    if encrypted {
        engine = engine.with_keys(Arc::clone(&keystore) as Arc<dyn KeyProvider>);
    }
    engine.ingest(events(), 1_760_000_000_000).unwrap();
    engine.backup_published().unwrap();

    Backed {
        engine,
        backend,
        keystore,
        roots: vec![store_dir, root],
    }
}

/// The raw bytes of the first backup receipt the object store holds, and the part it describes.
fn first_receipt(b: &Backed) -> (String, Vec<u8>) {
    let key = b
        .backend
        .list("parts/")
        .unwrap()
        .into_iter()
        .find(|k| k.ends_with(BACKUP_MANIFEST_FILE))
        .expect("a backup receipt");
    let part_id = key
        .strip_prefix("parts/")
        .and_then(|r| r.split_once('/'))
        .map(|(id, _)| id.to_string())
        .unwrap();
    (part_id, b.backend.get(&key).unwrap())
}

#[test]
fn an_encrypted_stores_backup_receipt_names_no_tenant_in_plaintext() {
    let b = backed_up("sealed", true);
    let tenants: Vec<String> = b
        .engine
        .snapshot()
        .unwrap()
        .part_ids()
        .iter()
        .flat_map(|id| b.engine.open_part(id).unwrap().manifest.tenants.clone())
        .collect();
    assert!(!tenants.is_empty(), "the fixture must hold real tenants");

    let (part_id, bytes) = first_receipt(&b);
    let text = String::from_utf8_lossy(&bytes).into_owned();

    // The receipt is readable without a key -- that is its job -- and what it says must not
    // include a tenant name anywhere in its bytes, in any field, encoded or not.
    for tenant in &tenants {
        assert!(
            !text.contains(tenant.as_str()),
            "the backup receipt for part `{part_id}` names tenant `{tenant}` in plaintext; \
             anyone holding the bucket now knows which tenants exist without decrypting a row\n\
             {text}"
        );
    }

    // And it is sealed *positively*, not merely missing the field.
    let receipt: PartBackup = serde_json::from_slice(&bytes).unwrap();
    match receipt.tenants().unwrap() {
        prism_engine::storage::ReceiptTenants::Plain(t) => {
            panic!("an encrypted store's receipt must not carry a plaintext tenant list: {t:?}")
        }
        prism_engine::storage::ReceiptTenants::Sealed(s) => {
            assert_eq!(s.wrapping_key_id, "key-v1", "the key id must be explicit");
            assert!(!s.ciphertext.is_empty());
            assert!(!s.wrapped_dek.is_empty());
        }
    }
}

#[test]
fn a_plaintext_stores_receipt_still_names_its_tenants_and_grows_no_sealed_field() {
    // The rollback edge: an unencrypted store's receipt is exactly what it always was.
    let b = backed_up("plain", false);
    let (_, bytes) = first_receipt(&b);
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        json.get("sealed_tenants").is_none(),
        "an unencrypted store must not grow an encryption field: {json}"
    );
    let receipt: PartBackup = serde_json::from_slice(&bytes).unwrap();
    assert!(
        !receipt.open_tenants(None).unwrap().is_empty(),
        "a plaintext receipt still names its tenants, with no key service at all"
    );
}

#[test]
fn the_sealed_tenant_list_opens_to_exactly_the_tenants_the_part_holds() {
    let b = backed_up("roundtrip", true);
    let (part_id, bytes) = first_receipt(&b);
    let receipt: PartBackup = serde_json::from_slice(&bytes).unwrap();

    let reader = b.engine.open_part(&part_id).unwrap();
    let expected = reader.manifest.tenants.clone();
    let cipher = b.engine.cipher_for(&reader).unwrap().expect("a cipher");

    assert_eq!(
        receipt.open_tenants(Some(cipher.as_ref())).unwrap(),
        expected,
        "the sealed list must open to the part's own tenants, in order"
    );

    // Without the key service it is not readable -- and that is a refusal, not an empty list:
    // "no tenants" and "I cannot read the tenants" are different facts.
    let err = receipt.open_tenants(None).unwrap_err().to_string();
    assert!(
        err.contains("needs the key service"),
        "a keyless read of a sealed list must refuse by name: {err}"
    );
}

#[test]
fn a_sealed_receipt_still_refuses_a_part_that_routes_to_another_shard() {
    // The check hydration cannot lose to encryption. It runs before anything is unwrapped, on the
    // plaintext bucket ordinal -- which is what routing was always a function of.
    let b = backed_up("routing", true);
    let (_, bytes) = first_receipt(&b);
    let receipt: PartBackup = serde_json::from_slice(&bytes).unwrap();
    let ordinal = match receipt.tenants().unwrap() {
        prism_engine::storage::ReceiptTenants::Sealed(s) => s.bucket_ordinal,
        other => panic!("expected a sealed receipt, got {other:?}"),
    };

    // Deliberately the shard this part does NOT route to.
    let wrong = ShardPlacement {
        scheme: Default::default(),
        shard_id: ((ordinal + 1) % 2) as usize,
        shard_count: 2,
    };
    let err = b
        .engine
        .plan_hydration(Some(&wrong))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("does not route to shard"),
        "a foreign shard's part must be refused by name even when its tenants are sealed: {err}"
    );

    // ...and the placement that *does* own these parts plans cleanly, so the refusal is
    // discrimination on the ordinal and not a blanket rejection of every sealed receipt. A
    // single-shard placement owns every bucket by construction.
    let owning = ShardPlacement {
        scheme: Default::default(),
        shard_id: 0,
        shard_count: 1,
    };
    assert!(
        b.engine.plan_hydration(Some(&owning)).is_ok(),
        "the owning placement must still be able to plan the restore"
    );
    let _ = &b.keystore;
}

#[test]
fn a_receipt_that_seals_its_tenants_and_also_names_them_is_refused() {
    // Belt and braces for the leak this closes: if a future writer ever populates both forms, the
    // reader treats it as corruption rather than quietly preferring one.
    let b = backed_up("both", true);
    let (_, bytes) = first_receipt(&b);
    let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    json["tenants"] = serde_json::json!(["t1"]);
    let tampered: PartBackup = serde_json::from_value(json).unwrap();

    let err = tampered.tenants().unwrap_err().to_string();
    assert!(
        err.contains("has defeated the sealing"),
        "a receipt carrying both forms must be refused, not silently resolved: {err}"
    );
}
