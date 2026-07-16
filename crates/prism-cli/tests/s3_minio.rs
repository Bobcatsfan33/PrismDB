//! **The S11 MinIO integration gate** — the hand-rolled S3 client against a **real S3 server**
//! ([storage contract §1](../../../docs/STORAGE-CONTRACT.md)), no mock anywhere in the path.
//!
//! Runs only when `PRISM_S3_ENDPOINT` is set (MinIO in CI); skips locally where there is no server.
//! This is the gate that flips S11 from 🟡 to ✅ once it is green against MinIO in CI: put/get/
//! ranged-get/head/delete/list round-trip, the CAS create race (D-066/D-067), and the capability
//! check — all against the real wire, verifying the SigV4 signing and the S3 semantics end-to-end.

use prism_engine::storage::object::{assert_cas_capability, cas_publish, CasOutcome, ObjectStore};
use prism_engine::storage::s3::{S3Config, S3ObjectStore};
use prism_engine::storage::sigv4::Credentials;

fn minio() -> Option<S3Config> {
    let endpoint = std::env::var("PRISM_S3_ENDPOINT").ok()?;
    Some(S3Config {
        endpoint,
        region: std::env::var("PRISM_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
        bucket: std::env::var("PRISM_S3_BUCKET").unwrap_or_else(|_| "prism".into()),
        credentials: Credentials {
            access_key: std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| "minioadmin".into()),
            secret_key: std::env::var("AWS_SECRET_ACCESS_KEY")
                .unwrap_or_else(|_| "minioadmin".into()),
        },
        fixed_amz_date: None,
    })
}

#[test]
fn the_hand_rolled_s3_client_round_trips_against_minio() {
    let Some(cfg) = minio() else {
        eprintln!("skipping MinIO integration: PRISM_S3_ENDPOINT is not set");
        return;
    };
    let store = S3ObjectStore::new(cfg);

    // The backend must provide conditional-create, or we refuse it.
    assert_cas_capability(&store).expect("MinIO must provide If-None-Match conditional create");

    // Put / get / ranged get / head.
    store.delete("it/k").ok();
    store.put("it/k", b"hello world").unwrap();
    assert_eq!(store.get("it/k").unwrap(), b"hello world");
    assert_eq!(store.get_range("it/k", 6, 5).unwrap(), b"world");
    assert_eq!(store.head("it/k").unwrap(), Some(11));

    // CAS publication: create wins, our-own succeeds, a different write conflicts (D-067).
    store.delete("it/CURRENT").ok();
    assert_eq!(
        cas_publish(&store, "it/CURRENT", b"snap-a").unwrap(),
        CasOutcome::Created
    );
    assert_eq!(
        cas_publish(&store, "it/CURRENT", b"snap-a").unwrap(),
        CasOutcome::AlreadyOurs
    );
    assert_eq!(
        cas_publish(&store, "it/CURRENT", b"snap-b").unwrap(),
        CasOutcome::Conflict
    );

    // List sees the keys we wrote.
    let keys = store.list("it/").unwrap();
    assert!(
        keys.iter().any(|k| k == "it/k"),
        "list did not return it/k: {keys:?}"
    );

    // Delete removes it; head then reports absence.
    store.delete("it/k").unwrap();
    assert!(store.head("it/k").unwrap().is_none());
    store.delete("it/CURRENT").ok();
}
