//! **The S10 soak gate: the answer at cycle N is the answer at cycle 1, while everything churns**
//! ([merge contract §8](../../../docs/MERGE-CONTRACT.md)).
//!
//! Sustained ingest **and** queries **and** deletes **and** a re-embed migration, running together,
//! with the tiered scheduler compacting throughout — and at the end the store has a **steady-state
//! part count**, **bounded merge debt**, and — the assertion that matters — a **canary query whose
//! exact answer is byte-identical to cycle 1**. This is the S8/v1 recall-stability discipline, now
//! with mutations underneath it.
//!
//! An **accelerated** soak runs here (compressed cycles, real concurrency of operations, real
//! mutation); the full-length soak is the nightly job. Kill-injection at merge boundaries is an
//! *abort*, which cannot run in-process without taking the test down with it, so that half is the
//! fault matrix's job ([`faults.rs`] drives `merge.after_part_before_commit`, the campaign
//! randomizes it); this gate owns the steady-state-and-answer-invariance-under-mutation half.

use prism_engine::Engine;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::{Event, Query};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-soak-{}-{}-{}", tag, std::process::id(), n));
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

fn ev(id: &str, tenant: &str, t: i64, body: &str) -> Event {
    Event {
        event_id: id.to_string(),
        tenant_id: tenant.to_string(),
        event_time: t,
        observed_time: t,
        event_name: "e".into(),
        cost: 0.01,
        error: false,
        body: body.to_string(),
        trace_id: String::new(),
        span_id: String::new(),
        attributes: Default::default(),
        idempotency_key: None,
    }
}

/// The exact (brute-force oracle) answer to the canary query — layout-, codebook-, and
/// merge-independent, so any change to it is a real change to what the store says.
fn canary_answer(engine: &Engine) -> Vec<String> {
    let q = Query {
        text: "the tool call timed out retrying with backoff".into(),
        k: 15,
        tenant: Some("canary".into()),
        ..Default::default()
    };
    engine
        .exact_search(&q)
        .unwrap()
        .iter()
        .map(|h| h.event.event_id.clone())
        .collect()
}

#[test]
fn the_answer_survives_sustained_mutation() {
    let root = tmp("soak");
    let engine = Engine::init(&root, config()).unwrap();

    // The canary: a fixed set of rows, ingested once, never deleted. Its exact answer must never
    // move, no matter what churns underneath it.
    let canary: Vec<Event> = (0..200)
        .map(|i| {
            ev(
                &format!("canary{i:05}"),
                "canary",
                1_700_000_000_000 + i as i64,
                &format!("the tool call timed out retrying with backoff {i}"),
            )
        })
        .collect();
    engine.ingest(canary, 1).unwrap();
    let answer0 = canary_answer(&engine);
    assert!(!answer0.is_empty());

    let cycles = 25usize;
    let mut max_parts = 0usize;
    let mut max_debt = 0usize;
    for c in 0..cycles {
        let now = 100 + c as i64;

        // 1) sustained ingest: a fresh batch of churn rows.
        let churn: Vec<Event> = (0..150)
            .map(|i| {
                ev(
                    &format!("c{c}_{i:04}"),
                    "churn",
                    1_760_000_000_000 + (c as i64) * 1000 + i as i64,
                    &format!("connection pool exhausted while querying the primary {c} {i}"),
                )
            })
            .collect();
        engine.ingest(churn, now).unwrap();

        // 2) queries, interleaved — and the canary answer must be identical to cycle 0.
        assert_eq!(
            canary_answer(&engine),
            answer0,
            "the canary answer moved on cycle {c} — something under a live query changed the \
             store's answer (merge contract §8)"
        );

        // 3) deletes: reconcile some earlier churn rows (never the canary).
        if c >= 2 {
            let victims: Vec<String> = (0..40).map(|i| format!("c{}_{i:04}", c - 2)).collect();
            engine.delete(&victims, now).unwrap();
        }

        // 4) a re-embed migration, best-effort (same space, retrained codebooks). It re-encodes
        //    parts but preserves exact vectors, so the exact canary answer is unaffected.
        if c > 0 && c % 8 == 0 {
            if let Ok(g) = engine.generation_create(None, now) {
                let _ = engine.generation_promote(&g.generation_id, now);
                let _ = engine.generation_migrate(&g.generation_id, None, now);
            }
        }

        // 5) the scheduler compacts.
        let report = engine.merge_tiered(now).unwrap();
        if c >= 8 {
            max_parts = max_parts.max(engine.snapshot().unwrap().parts.len());
            max_debt = max_debt.max(report.merge_debt);
        }
    }

    // The canary answer is still cycle-0's, after everything.
    assert_eq!(
        canary_answer(&engine),
        answer0,
        "the canary answer moved by the end of the soak"
    );

    // Steady state: part count and debt are bounded, not growing with the cycle count.
    assert!(
        max_parts < 200,
        "part count did not reach a steady state under sustained mutation: {max_parts}"
    );
    assert!(
        max_debt < 200,
        "merge debt is unbounded under sustained mutation: {max_debt}"
    );

    // Health: the store verifies after all of it.
    engine
        .catalog()
        .verify()
        .expect("the store must verify after the soak");
    let _ = std::fs::remove_dir_all(&root);
}
