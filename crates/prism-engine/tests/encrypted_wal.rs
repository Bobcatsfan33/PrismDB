//! The admission log of an encrypted store holds no plaintext event (S14, [D-095](../../../../docs/DECISIONS.md),
//! [encryption contract §5](../../../../docs/ENCRYPTION-CONTRACT.md)).
//!
//! The WAL is the one place an admitted event is **durable and not yet a part**. Between the ack and
//! the catalog commit the event exists nowhere else — so an encrypted store with a plaintext WAL
//! has a window in which its rows sit in the clear on node-local disk and, in replicated mode, in
//! the shared authoritative bucket. The window is bounded by publication latency in the happy case
//! and unbounded by a crash, which is exactly the case a WAL exists for.
//!
//! These gates hold both edges: nothing legible that should not be, and nothing *lost* to sealing —
//! an acknowledged record that cannot be opened must be a named refusal, never a silent skip.

use prism_engine::keys::{KeyFault, KeyProvider, SoftwareKeystore};
use prism_engine::storage::object::{LocalObjectStore, ObjectStore};
use prism_engine::wal::{RemoteWal, Wal, WalCrypto, WalRecord};
use prism_types::event::Event;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-encwal-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// A distinctive body, so "is this event legible on disk?" is a substring question.
const SECRET_BODY: &str = "the-quick-brown-fox-jumped-over-the-lazy-dog-42";

fn record(record_id: u64) -> WalRecord {
    let event = Event {
        event_id: "evt-secret-1".into(),
        tenant_id: "tenant-zulu".into(),
        event_time: 1_760_000_000_000,
        observed_time: 1_760_000_000_001,
        event_name: "order.placed".into(),
        cost: 0.25,
        error: false,
        body: SECRET_BODY.into(),
        trace_id: String::new(),
        span_id: String::new(),
        attributes: Default::default(),
        idempotency_key: None,
    };
    WalRecord {
        record_id,
        events: vec![event],
        source: Some("kafka://orders".into()),
        source_offset: Some(77),
        created_at_ms: 1_760_000_000_000,
    }
}

fn keystore() -> Arc<SoftwareKeystore> {
    Arc::new(SoftwareKeystore::new("key-v1", [11u8; 32]))
}

fn crypto(ks: &Arc<SoftwareKeystore>) -> Arc<WalCrypto> {
    Arc::new(WalCrypto::new(Arc::clone(ks) as Arc<dyn KeyProvider>).unwrap())
}

#[test]
fn a_sealed_admission_log_holds_no_plaintext_event_on_disk() {
    let dir = tmp("local-sealed");
    let ks = keystore();
    let wal = Wal::open(&dir).unwrap().with_crypto(crypto(&ks));
    wal.append_record(&record(0)).unwrap();

    let raw = std::fs::read(dir.join("admission.wal")).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    for legible in [SECRET_BODY, "tenant-zulu", "evt-secret-1", "kafka://orders"] {
        assert!(
            !text.contains(legible),
            "the admission log holds `{legible}` in the clear: an acked, unpublished event is \
             readable from node-local disk\n{text}"
        );
    }

    // ...and it round-trips, so the sealing is encryption and not destruction.
    assert_eq!(wal.read_all().unwrap(), vec![record(0)]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_unencrypted_admission_log_is_byte_identical_to_what_it_always_wrote() {
    // The rollback edge. A store with no key service must produce exactly the frames it produced
    // before encryption existed -- no envelope, no version field, no new bytes.
    let dir = tmp("local-plain");
    let wal = Wal::open(&dir).unwrap();
    wal.append_record(&record(0)).unwrap();

    let raw = std::fs::read(dir.join("admission.wal")).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    assert!(
        text.contains(SECRET_BODY),
        "an unencrypted log is plaintext, and this test is worthless if that changes"
    );
    assert!(
        !text.contains("sealed_wal"),
        "an unencrypted store must not grow an encryption envelope: {text}"
    );
    assert_eq!(wal.read_all().unwrap(), vec![record(0)]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_sealed_record_is_refused_by_name_without_the_key_service_and_never_skipped() {
    // The failure that matters: an acknowledged record we cannot open must NOT read back as an
    // empty log. Silently dropping an acked record is the one thing a WAL exists to prevent, and
    // "no outstanding records" is precisely what recovery would believe.
    let dir = tmp("keyless");
    let ks = keystore();
    Wal::open(&dir)
        .unwrap()
        .with_crypto(crypto(&ks))
        .append_record(&record(0))
        .unwrap();

    let blind = Wal::open(&dir).unwrap();
    let err = match blind.read_all() {
        Ok(records) => panic!(
            "a keyless read returned {} record(s) instead of refusing; an unopenable acked \
             record must never look like an empty log",
            records.len()
        ),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("no key service configured"),
        "a keyless read must refuse by name: {err}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_key_service_outage_over_the_admission_log_is_named_and_retries_cleanly() {
    let dir = tmp("outage");
    let ks = keystore();
    let wal = Wal::open(&dir).unwrap().with_crypto(crypto(&ks));
    wal.append_record(&record(0)).unwrap();

    // A fresh log instance, so nothing is masked by a resident DEK.
    let ks2 = keystore();
    let reader = Wal::open(&dir).unwrap().with_crypto(crypto(&ks2));
    ks2.set_fault(KeyFault::Unreachable).unwrap();
    let err = reader.read_all().unwrap_err().to_string();
    assert!(
        err.contains("key service unreachable"),
        "an outage must be named, not mistaken for a torn log: {err}"
    );

    ks2.set_fault(KeyFault::None).unwrap();
    assert_eq!(
        reader.read_all().unwrap(),
        vec![record(0)],
        "once the key service returns, the same log must read exactly as before"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn compaction_does_not_rewrite_surviving_sealed_records_as_plaintext() {
    // Compaction rewrites the whole log. A maintenance operation that quietly undoes encryption
    // would leave the store looking encrypted and reading plaintext off disk.
    let dir = tmp("compact");
    let ks = keystore();
    let wal = Wal::open(&dir).unwrap().with_crypto(crypto(&ks));
    for id in 0..3 {
        wal.append_record(&record(id)).unwrap();
    }

    let dropped = wal.compact_through(Some(0)).unwrap();
    assert_eq!(dropped, 1, "record 0 is at the floor and must be dropped");

    let raw = std::fs::read(dir.join("admission.wal")).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    assert!(
        !text.contains(SECRET_BODY),
        "compaction rewrote surviving records as plaintext:\n{text}"
    );
    assert_eq!(
        wal.read_all().unwrap(),
        vec![record(1), record(2)],
        "the survivors must still be readable, and still be themselves"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_sealed_record_cannot_be_replayed_under_another_record_id() {
    // The AAD binds a payload to its own id. Without it, an operator with bucket write access could
    // promote an old batch to a new id and have recovery publish it a second time.
    let store_dir = tmp("remote-swap");
    let backend: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(&store_dir));
    let ks = keystore();
    let remote = RemoteWal::new(Arc::clone(&backend), 0).with_crypto(crypto(&ks));
    remote.append(&record(1)).unwrap();

    let key = backend.list("wal/").unwrap().into_iter().next().unwrap();
    let mut sealed: serde_json::Value =
        serde_json::from_slice(&backend.get(&key).unwrap()).unwrap();

    // The complete attack, not half of it: re-file the object under a new id AND relabel the
    // envelope's plaintext id to match, so the existing key-id-vs-body-id check is satisfied and
    // the ONLY thing standing between this and a second publication is the AAD.
    sealed["record_id"] = serde_json::json!(2);
    let moved = key.replace("00000000000000000001", "00000000000000000002");
    assert_ne!(moved, key, "the rename must actually change the id");
    backend
        .put(&moved, &serde_json::to_vec(&sealed).unwrap())
        .unwrap();
    backend.delete(&key).unwrap();

    let err = remote.read_all().unwrap_err().to_string();
    assert!(
        err.contains("failed authenticated decryption"),
        "a record promoted to another id must fail the tag, not decrypt: {err}"
    );
    let _ = std::fs::remove_dir_all(store_dir);
}

#[test]
fn the_remote_admission_log_holds_no_plaintext_event_in_the_bucket() {
    // The remote log is the one the contract names outright: its objects live in the shared
    // authoritative bucket, readable with no node-local disk at all.
    let store_dir = tmp("remote-sealed");
    let backend: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(&store_dir));
    let ks = keystore();
    let remote = RemoteWal::new(Arc::clone(&backend), 0).with_crypto(crypto(&ks));
    remote.append(&record(1)).unwrap();

    for key in backend.list("wal/").unwrap() {
        let text = String::from_utf8_lossy(&backend.get(&key).unwrap()).into_owned();
        assert!(
            !text.contains(SECRET_BODY),
            "the remote admission log object `{key}` holds a plaintext event body:\n{text}"
        );
    }
    assert_eq!(remote.read_all().unwrap(), vec![record(1)]);
    let _ = std::fs::remove_dir_all(store_dir);
}
