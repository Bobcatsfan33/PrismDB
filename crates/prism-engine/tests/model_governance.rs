//! S13 tenant-governance gate: a denied tenant is visible but never reaches
//! the durable ACK, model execution, or a part.

use prism_engine::model::HashModelPlane;
use prism_engine::model_policy::{
    GovernedModelPlane, ModelGrant, ModelPolicy, ModelPolicyEnforcer, TenantPolicy,
};
use prism_engine::{Engine, Ingestor};
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::{EmbeddingPurpose, Event};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const VERSION: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
static NEXT: AtomicU64 = AtomicU64::new(0);

fn root() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "prism-model-governance-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn denied_tenant_is_dead_lettered_before_the_wal_ack() {
    let root = root();
    let store = root.join("store");
    let audit = root.join("model-usage.jsonl");
    let engine = Engine::init(
        &store,
        StoreConfig {
            format_version: STORE_VERSION,
            dim: 8,
            nlist: 2,
            pq_m: 2,
            seed: 17,
            kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
            block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
            partitions: Default::default(),
            promote: Vec::new(),
        },
    )
    .unwrap();
    let policy = ModelPolicy {
        schema_version: 1,
        default_action: "deny".into(),
        tenants: vec![TenantPolicy {
            tenant_id: "allowed".into(),
            grants: vec![ModelGrant {
                model_id: "hash-embedder".into(),
                model_version: VERSION.into(),
                purposes: vec![EmbeddingPurpose::Ingest],
            }],
            max_inputs_per_minute: 10,
            max_input_bytes_per_minute: 4096,
        }],
    };
    let governed = GovernedModelPlane::new(
        Arc::new(HashModelPlane::at_version(VERSION)),
        Arc::new(ModelPolicyEnforcer::new(policy, &audit).unwrap()),
    );
    let mut ingestor = Ingestor::open(engine.with_plane(Arc::new(governed))).unwrap();
    let now = 1_760_000_000_000;
    let event = Event {
        event_id: "denied-1".into(),
        tenant_id: "blocked".into(),
        event_time: now,
        observed_time: 0,
        event_name: "llm.call".into(),
        cost: 0.01,
        error: false,
        body: "body that must not cross the model boundary".into(),
        trace_id: String::new(),
        span_id: String::new(),
        attributes: Default::default(),
        idempotency_key: None,
    };

    let report = ingestor.ingest(vec![event], None, None, now).unwrap();
    assert_eq!(report.published, 0);
    assert_eq!(report.dead_lettered, 1);
    assert_eq!(report.wal_record, None);
    assert_eq!(report.by_reason["model_policy_denied"], 1);
    assert!(ingestor.wal.read_all().unwrap().is_empty());
    assert!(ingestor.engine.snapshot().unwrap().parts.is_empty());

    let audit_body = std::fs::read_to_string(audit).unwrap();
    assert!(audit_body.contains("\"outcome\":\"denied\""));
    assert!(!audit_body.contains("body that must not cross"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn query_requires_its_own_exact_purpose_grant() {
    let root = root();
    let store = root.join("store");
    let audit = root.join("model-usage.jsonl");
    let engine = Engine::init(
        &store,
        StoreConfig {
            format_version: STORE_VERSION,
            dim: 8,
            nlist: 2,
            pq_m: 2,
            seed: 19,
            kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
            block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
            partitions: Default::default(),
            promote: Vec::new(),
        },
    )
    .unwrap();
    let policy = ModelPolicy {
        schema_version: 1,
        default_action: "deny".into(),
        tenants: vec![TenantPolicy {
            tenant_id: "acme".into(),
            grants: vec![ModelGrant {
                model_id: "hash-embedder".into(),
                model_version: VERSION.into(),
                purposes: vec![EmbeddingPurpose::Ingest],
            }],
            max_inputs_per_minute: 100,
            max_input_bytes_per_minute: 1024 * 1024,
        }],
    };
    let governed = GovernedModelPlane::new(
        Arc::new(HashModelPlane::at_version(VERSION)),
        Arc::new(ModelPolicyEnforcer::new(policy, &audit).unwrap()),
    );
    let engine = engine.with_plane(Arc::new(governed));
    let now = 1_760_000_000_000;
    let mut events = prism_engine::corpus::generate(prism_engine::corpus::Kind::Uniform, 32, 23);
    for event in &mut events {
        event.tenant_id = "acme".into();
        event.event_time = now;
    }
    assert_eq!(engine.ingest(events, now).unwrap().admitted, 32);

    let query = prism_types::Query {
        text: "a query whose text must not enter the usage ledger".into(),
        tenant: Some("acme".into()),
        ..Default::default()
    };
    let error = engine.search(&query).unwrap_err().to_string();
    assert!(error.contains("not authorized"));
    assert!(error.contains("purpose `query`"));
    let audit_body = std::fs::read_to_string(audit).unwrap();
    assert!(audit_body.contains("\"outcome\":\"denied\""));
    assert!(!audit_body.contains("whose text must not enter"));
    std::fs::remove_dir_all(root).ok();
}
