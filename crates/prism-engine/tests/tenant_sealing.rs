//! Tenant identity at rest is sealed, and sealing changes no answer (S14 DATA-01,
//! [D-096](../../../../docs/DECISIONS.md), [encryption contract §6a](../../../../docs/ENCRYPTION-CONTRACT.md)).
//!
//! The DATA-01 remainder D-095 left behind was **metadata**: the part manifest's tenant list and
//! `TenantStats`, and the catalog mirror's `PartEntry` tenants. Raw disk or bucket access disclosed
//! which tenants existed and which shared a bucket. These gates hold the close, and hold it from
//! both sides — nothing legible that should not be, and **not one answer changed** by making it
//! illegible.

use prism_engine::keys::{KeyProvider, SoftwareKeystore};
use prism_engine::sharded::Cluster;
use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::Query;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static N: AtomicU64 = AtomicU64::new(0);
const TS: i64 = 1_760_000_000_000;

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-seal-{tag}-{}-{n}", std::process::id()));
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

fn keys() -> Arc<dyn KeyProvider> {
    Arc::new(SoftwareKeystore::new("key-v1", [11u8; 32]))
}

/// The corpus, with **long, distinctive tenant names**.
///
/// The generator's default names are `t0`..`t4`, and a two-character string turns up inside
/// ciphertext by chance often enough that scanning for it proves nothing — the first run of the
/// scan gate below "found" 21 leaks in sealed column files for exactly that reason. A leak scan is
/// only as good as the improbability of its needle.
fn events() -> Vec<prism_types::Event> {
    prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 600, 5)
        .into_iter()
        .map(|mut e| {
            e.tenant_id = format!("tenant-northwind-{}-inc", e.tenant_id);
            e
        })
        .collect()
}

/// The same shape for the second era of the mixed-store gate.
fn events2() -> Vec<prism_types::Event> {
    prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 600, 9)
        .into_iter()
        .map(|mut e| {
            e.tenant_id = format!("tenant-northwind-{}-inc", e.tenant_id);
            e
        })
        .collect()
}

/// Every distinct tenant name in the corpus — what must NOT appear in any sealed byte.
fn tenant_names() -> Vec<String> {
    events()
        .iter()
        .map(|e| e.tenant_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn hits(engine: &Engine, tenant: &str) -> Vec<String> {
    engine
        .search(&Query {
            text: "the tool call timed out retrying".into(),
            k: 15,
            tenant: Some(tenant.into()),
            rerank: 40,
            ..Default::default()
        })
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.event.event_id)
        .collect()
}

/// Every byte a store keeps on local disk, by path — what a disk image would show.
fn all_store_bytes(root: &PathBuf) -> Vec<(String, Vec<u8>)> {
    fn walk(base: &PathBuf, dir: &PathBuf, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else if let Ok(bytes) = std::fs::read(&p) {
                out.push((
                    p.strip_prefix(base).unwrap().to_string_lossy().into_owned(),
                    bytes,
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// **Gate 1 — a freshly-written v4 store names no tenant, anywhere on disk.**
///
/// Armed twice: the fixture must hold **several distinct tenant names**, and the parts must actually
/// be v4 with the token feature bit — otherwise a store that quietly kept writing v3 would pass by
/// having nothing to find.
#[test]
fn a_sealed_store_writes_no_tenant_name_to_disk() {
    let root = tmp("sealed");
    let engine = Engine::init(&root, config()).unwrap().with_keys(keys());
    engine.ingest(events(), TS).unwrap();

    // ARMED (a): distinct names to look for.
    let names = tenant_names();
    assert!(
        names.len() >= 2,
        "not armed: the fixture must hold several distinct tenants, got {names:?}"
    );

    // ARMED (b): the parts really are v4 with tokens, not v3 with nothing to seal.
    let ids = engine.snapshot().unwrap().part_ids();
    assert!(!ids.is_empty());
    for id in &ids {
        let m = &engine.open_part(id).unwrap().manifest;
        assert_eq!(m.format_version, 4, "part {id} is not v4");
        assert_ne!(
            m.feature_flags & prism_part::format::FEATURE_TENANT_TOKENS,
            0,
            "part {id} does not declare tenant tokens"
        );
    }

    let mut leaks = Vec::new();
    for (path, bytes) in all_store_bytes(&root) {
        let text = String::from_utf8_lossy(&bytes);
        for name in &names {
            if text.contains(name.as_str()) {
                leaks.push(format!("`{path}` contains tenant name `{name}`"));
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "a sealed store still names its tenants on disk — {} leak(s):\n  - {}",
        leaks.len(),
        leaks.join("\n  - ")
    );
}

/// **Gate 2 — sealing changes no answer, at 1, 2 and 4 shards.**
///
/// Encryption is a property of stored bytes, never of an answer; tokenization is the same claim one
/// layer up, for metadata. A plaintext store and a sealed store over the identical corpus must agree
/// byte-for-byte, per tenant, at every shard count — which also proves routing and pruning still
/// work when the thing they compare is opaque.
#[test]
fn sealing_changes_no_answer_at_any_shard_count() {
    let plain_root = tmp("inv-plain");
    let plain = Engine::init(&plain_root, config()).unwrap();
    plain.ingest(events(), TS).unwrap();

    let sealed_root = tmp("inv-sealed");
    let sealed = Engine::init(&sealed_root, config())
        .unwrap()
        .with_keys(keys());
    sealed.ingest(events(), TS).unwrap();

    let mut failures = Vec::new();
    for t in tenant_names() {
        let expect = hits(&plain, &t);
        assert!(!expect.is_empty(), "tenant {t} must answer on the control");
        if hits(&sealed, &t) != expect {
            failures.push(format!("single engine, tenant {t}"));
        }
    }

    for n in [1usize, 2, 4] {
        let c_plain = Cluster::init(&tmp(&format!("inv-cp{n}")), n, config()).unwrap();
        c_plain.ingest(events(), TS).unwrap();
        let c_sealed = Cluster::init(&tmp(&format!("inv-cs{n}")), n, config())
            .unwrap()
            .with_keys(keys());
        c_sealed.ingest(events(), TS).unwrap();

        for t in tenant_names() {
            let q = Query {
                text: "the tool call timed out retrying".into(),
                k: 15,
                tenant: Some(t.clone()),
                rerank: 40,
                ..Default::default()
            };
            let a: Vec<String> = c_plain
                .search(&q)
                .unwrap()
                .hits
                .into_iter()
                .map(|h| h.event.event_id)
                .collect();
            let b: Vec<String> = c_sealed
                .search(&q)
                .unwrap()
                .hits
                .into_iter()
                .map(|h| h.event.event_id)
                .collect();
            if a != b {
                failures.push(format!("{n} shards, tenant {t}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "sealing changed an answer in {} case(s): {}",
        failures.len(),
        failures.join(", ")
    );
}

/// **Gate 3 — a store holding both generations of handle answers correctly, merge migrates it, and
/// rollback is one catalog write.**
///
/// The migration state is not a transient to be hurried through: an operator may run mixed for as
/// long as they like. v3 parts hold names, v4 parts hold tokens, and one query must see both — which
/// is what the name-or-token match rule buys.
#[test]
fn a_mixed_v3_and_v4_store_answers_migrates_and_rolls_back() {
    let root = tmp("mixed");

    // Era 1: no key service — v3 parts, tenant NAMES.
    let plain = Engine::init(&root, config()).unwrap();
    plain.ingest(events(), TS).unwrap();
    let v3_parts: Vec<String> = plain.snapshot().unwrap().part_ids();
    let probe = tenant_names()[1].clone();
    let before = hits(&plain, &probe);
    assert!(!before.is_empty());
    let snapshot_before_v4 = plain.snapshot().unwrap().snapshot_id;
    drop(plain);

    // Era 2: keys attached — new parts are v4 with TOKENS, old ones untouched.
    let mixed = Engine::open(&root).unwrap().with_keys(keys());
    mixed.ingest(events2(), TS + 1).unwrap();

    let all: Vec<String> = mixed.snapshot().unwrap().part_ids();
    let new_parts: Vec<&String> = all.iter().filter(|p| !v3_parts.contains(p)).collect();
    assert!(!new_parts.is_empty(), "era 2 must add parts");
    // ARMED: the store really is mixed — and the axis is the FEATURE BIT, not the version number.
    // A plaintext store writes v4 as well; what it does not write is tokens. That is the whole point
    // of a feature bit inside a version, and asserting on the version here would have been asserting
    // on the wrong thing.
    let tokens_of = |id: &String| {
        mixed.open_part(id).unwrap().manifest.feature_flags
            & prism_part::format::FEATURE_TENANT_TOKENS
            != 0
    };
    for id in &v3_parts {
        assert!(!tokens_of(id), "part {id} should still hold tenant NAMES");
    }
    for id in new_parts.iter().copied() {
        assert!(tokens_of(id), "part {id} should hold tenant TOKENS");
    }

    // One query, both generations of handle.
    assert!(
        !hits(&mixed, &probe).is_empty(),
        "a mixed store must still answer — the name-or-token rule is what makes this work"
    );

    // MERGE MIGRATES FORWARD. `is_legacy` is now true for v3, which is exactly what drives the
    // rewrite, so migration is a consequence of the writer stamping v4 rather than a special path.
    let report = mixed.merge(prism_engine::engine::now_ms()).unwrap();
    assert!(
        report.parts_migrated > 0,
        "merge reported no migration; v3 parts were not rewritten forward"
    );
    for id in mixed.snapshot().unwrap().part_ids() {
        assert!(
            tokens_of(&id),
            "part {id} survived the merge still naming its tenants"
        );
    }
    assert!(
        !hits(&mixed, &probe).is_empty(),
        "the migrated store must still answer"
    );

    // ROLLBACK IS ONE CATALOG WRITE — not a restore, not a rewrite. The pre-v4 snapshot is still
    // nameable, and pointing at it brings the v3 parts back.
    mixed
        .rollback(&snapshot_before_v4, prism_engine::engine::now_ms())
        .expect("rollback to the pre-v4 snapshot must be a catalog write");
    assert_eq!(
        hits(&mixed, &probe),
        before,
        "after rollback the store must answer exactly as it did before v4 parts existed"
    );
    for id in mixed.snapshot().unwrap().part_ids() {
        assert!(
            !tokens_of(&id),
            "rollback should have restored the pre-token view"
        );
    }
}
