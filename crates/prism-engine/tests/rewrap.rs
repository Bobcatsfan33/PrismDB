//! Key rotation: expand → activate → rewrap → retire (S14, [D-095](../../../../docs/DECISIONS.md),
//! [encryption contract §9](../../../../docs/ENCRYPTION-CONTRACT.md)).
//!
//! The property that makes rotation affordable is that **rewrapping touches wrapped-DEK envelopes
//! and never part bytes**. If a rotation had to re-encrypt data, rotating a large store would cost
//! a full re-ingest — and a rotation that costs a re-ingest is one a deployment quietly never
//! performs, which is how a key ends up live for five years.
//!
//! So these gates hold the expensive claim to its literal meaning: the column files are compared
//! byte-for-byte across the rewrap, and the answer is compared to the answer from before it.

use prism_engine::keys::{KeyProvider, SoftwareKeystore};
use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Query;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-rewrap-{tag}-{}-{n}", std::process::id()));
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

fn answer(engine: &Engine) -> Vec<String> {
    engine
        .search(&Query {
            text: "the tool call timed out retrying".into(),
            k: 15,
            tenant: Some("t1".into()),
            rerank: 40,
            ..Default::default()
        })
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.event.event_id)
        .collect()
}

/// Every non-manifest file of every live part, by path, with its bytes. This is what "never part
/// bytes" has to mean if it means anything.
fn column_bytes(engine: &Engine) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    for part_id in engine.snapshot().unwrap().part_ids() {
        let dir = engine.store.part_dir(&part_id);
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "manifest.bin" || !entry.file_type().unwrap().is_file() {
                continue;
            }
            out.insert(
                format!("{part_id}/{name}"),
                std::fs::read(entry.path()).unwrap(),
            );
        }
    }
    out
}

fn envelope_keys(engine: &Engine) -> Vec<String> {
    engine
        .snapshot()
        .unwrap()
        .part_ids()
        .iter()
        .map(|id| {
            engine
                .open_part(id)
                .unwrap()
                .encryption_envelope()
                .unwrap()
                .wrapping_key_id
        })
        .collect()
}

/// A store on key-v1 with data published, plus its keystore.
fn rotating(tag: &str) -> (Engine, Arc<SoftwareKeystore>, PathBuf) {
    let root = tmp(tag);
    let keystore = Arc::new(SoftwareKeystore::new("key-v1", [11u8; 32]));
    let engine = Engine::init(&root, config())
        .unwrap()
        .with_keys(Arc::clone(&keystore) as Arc<dyn KeyProvider>);
    engine.ingest(events(), 1_760_000_000_000).unwrap();
    (engine, keystore, root)
}

#[test]
fn a_rewrap_changes_every_envelope_and_not_one_part_byte() {
    let (engine, ks, root) = rotating("bytes");
    let before_answer = answer(&engine);
    let before_bytes = column_bytes(&engine);
    assert!(!before_bytes.is_empty(), "the fixture must have columns");
    assert!(envelope_keys(&engine).iter().all(|k| k == "key-v1"));

    // Expand, then activate: both keys accepted for unwrap before anything is rewrapped.
    ks.expand("key-v2", [22u8; 32]).unwrap();
    ks.activate("key-v2").unwrap();

    let report = engine.rewrap_to_active_key().unwrap();
    assert_eq!(report.active_key_id, "key-v2");
    assert_eq!(
        report.backend, "software-keystore",
        "the receipt names the backend"
    );
    assert_eq!(report.rewrapped.len(), report.examined);
    assert!(report.already_current.is_empty());

    assert!(
        envelope_keys(&engine).iter().all(|k| k == "key-v2"),
        "every live envelope must now name the active key"
    );
    assert_eq!(
        column_bytes(&engine),
        before_bytes,
        "a rewrap must not rewrite a single part byte -- no block re-encrypted, no nonce changed"
    );
    assert_eq!(
        answer(&engine),
        before_answer,
        "and the answer is untouched, because the DEK under the seal never changed"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_rewrap_is_idempotent_so_re_running_it_resumes_rather_than_redoes() {
    let (engine, ks, root) = rotating("idempotent");
    ks.expand("key-v2", [22u8; 32]).unwrap();
    ks.activate("key-v2").unwrap();

    let first = engine.rewrap_to_active_key().unwrap();
    assert!(!first.rewrapped.is_empty());

    let after_first = column_bytes(&engine);
    let second = engine.rewrap_to_active_key().unwrap();
    assert!(
        second.rewrapped.is_empty(),
        "a second pass must rewrap nothing: {:?}",
        second.rewrapped
    );
    assert_eq!(
        second.already_current.len(),
        first.rewrapped.len(),
        "every part the first pass rewrapped must be recognised as current by the second"
    );
    assert_eq!(column_bytes(&engine), after_first);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_store_left_half_rewrapped_by_a_crash_still_reads_every_part() {
    // Resumability, stated as the property that makes it safe rather than as a code path: because
    // *expand* left both keys accepted for unwrap before any rewrap ran, a store holding some
    // envelopes on the old key and some on the new is fully readable throughout.
    let (engine, ks, root) = rotating("halfway");
    let before = answer(&engine);
    ks.expand("key-v2", [22u8; 32]).unwrap();
    ks.activate("key-v2").unwrap();

    // Rewrap exactly one part by hand -- the state a crash mid-loop leaves.
    let part_id = engine.snapshot().unwrap().part_ids()[0].clone();
    let reader = engine.open_part(&part_id).unwrap();
    let envelope = reader.encryption_envelope().unwrap();
    let dek = ks
        .unwrap(&envelope.wrapping_key_id, &envelope.wrapped_dek)
        .unwrap();
    let (wrapping_key_id, wrapped_dek) = ks.wrap(&dek).unwrap();
    prism_part::part::rewrap_part_envelope(
        &engine.store.part_dir(&part_id),
        &prism_part::ext::S14Ext {
            wrapping_key_id,
            wrapped_dek,
            ..envelope
        },
    )
    .unwrap();
    engine.clear_key_cache().unwrap();

    let keys = envelope_keys(&engine);
    assert!(
        keys.iter().any(|k| k == "key-v1") && keys.iter().any(|k| k == "key-v2"),
        "the fixture must actually be half-rewrapped: {keys:?}"
    );
    assert_eq!(
        answer(&engine),
        before,
        "a half-rewrapped store must answer exactly as it did before the rotation began"
    );

    // ...and finishing the job converges it.
    engine.rewrap_to_active_key().unwrap();
    assert!(envelope_keys(&engine).iter().all(|k| k == "key-v2"));
    assert_eq!(answer(&engine), before);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn retiring_a_key_live_envelopes_still_need_is_refused_and_says_how_many() {
    let (engine, ks, root) = rotating("retire");
    ks.expand("key-v2", [22u8; 32]).unwrap();
    ks.activate("key-v2").unwrap();

    let in_use = engine.wrapping_keys_in_use().unwrap();
    let needed = *in_use.get("key-v1").expect("key-v1 is still in use");
    assert!(needed > 0);

    let err = engine
        .assert_key_retirable("key-v1")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("live envelope(s) still need it"),
        "retiring a key the data needs must be refused by name: {err}"
    );
    assert!(
        err.contains(&needed.to_string()),
        "the refusal must say how many envelopes are in the way, so the answer is `run the \
         rewrap` rather than `try harder`: {err}"
    );

    // After the rewrap the key is genuinely free, and the guard says so.
    engine.rewrap_to_active_key().unwrap();
    engine
        .assert_key_retirable("key-v1")
        .expect("a fully rewrapped store must let the old key go");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn an_outstanding_admission_record_keeps_its_wrapping_key_from_being_retired() {
    // Parts are not the only thing holding a wrapped DEK. An acked, unpublished WAL record holds
    // one too, and retiring its key would destroy an event a producer was promised we kept.
    let (engine, ks, root) = rotating("wal-holds");
    engine.rewrap_to_active_key().unwrap();

    // A record sealed under key-v1, left outstanding in the log.
    let crypto = Arc::new(
        prism_engine::wal::WalCrypto::new(Arc::clone(&ks) as Arc<dyn KeyProvider>).unwrap(),
    );
    let wal = prism_engine::wal::Wal::open(&engine.store.root.join("wal"))
        .unwrap()
        .with_crypto(crypto);
    let next = wal
        .append(Vec::new(), None, None, 1_760_000_000_000)
        .unwrap();
    assert_eq!(next, 0);

    ks.expand("key-v2", [22u8; 32]).unwrap();
    ks.activate("key-v2").unwrap();
    // Every PART is now on key-v2 after a rewrap -- only the log still needs key-v1.
    engine.rewrap_to_active_key().unwrap();
    assert!(envelope_keys(&engine).iter().all(|k| k == "key-v2"));

    let err = engine
        .assert_key_retirable("key-v1")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("live envelope(s) still need it"),
        "an outstanding admission record must keep its wrapping key alive: {err}"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_rewrap_refuses_to_turn_encryption_on_for_a_plaintext_part() {
    // A store is encrypted because it was configured to be (contract section 10). A rewrap is not
    // where that decision gets made, and a part with no envelope is left exactly as it is.
    let root = tmp("plaintext");
    let engine = Engine::init(&root, config()).unwrap();
    engine.ingest(events(), 1_760_000_000_000).unwrap();
    let part_id = engine.snapshot().unwrap().part_ids()[0].clone();

    let err = prism_part::part::rewrap_part_envelope(
        &engine.store.part_dir(&part_id),
        &prism_part::ext::S14Ext {
            algorithm: prism_part::crypto::AEAD_XCHACHA20_POLY1305,
            wrapping_key_id: "key-v2".into(),
            dek_epoch: 1,
            wrapped_dek: vec![7u8; 64],
            bucket_ordinal: 0,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("a rewrap does not turn encryption on"),
        "rewrapping a plaintext part must be refused by name: {err}"
    );
    let _ = std::fs::remove_dir_all(root);
}
