//! The S2 gate: duplicate/replay semantics, offsets, quotas, starvation.
//!
//! > **Gate:** replaying acknowledged input → no missing rows, documented duplicate
//! > behavior; offsets never advance pre-publication; one tenant cannot exceed quota
//! > or starve others.
//!
//! Every test here asserts a specific clause of
//! [`docs/INGESTION-CONTRACT.md`](../../../docs/INGESTION-CONTRACT.md). The contract
//! was written *before* the code, on the architect's instruction, because the risk in
//! this sprint was never the schema — it was these semantics.

use prism_engine::admission::KeyDictionary;
use prism_engine::source::{self, MemorySource, Source};
use prism_engine::{Engine, Ingestor};
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::attributes::AttrValue;
use prism_types::event::Event;
use prism_types::limits::{Quota, RejectReason, MAX_ATTRIBUTE_KEY_CARDINALITY};
use prism_types::Query;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-s2-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

const NOW: i64 = 1_760_000_000_000;

/// The real wall clock, in epoch millis.
///
/// The in-process tests pass their own `now_ms`, so a fixed epoch is fine there. The
/// tests that drive a real `prism` process cannot: the binary reads the actual clock,
/// and an event nine months in the past is — correctly — refused as `event_time_too_late`.
/// That skew check is not a nuisance to work around; it is the thing under test.
fn now_real() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// An event stamped at the real clock, for the tests that spawn a process.
fn ev_now(id: &str, tenant: &str, body: &str) -> Event {
    let mut e = ev(id, tenant, body);
    e.event_time = now_real();
    e
}

fn config() -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 32,
        nlist: 8,
        pq_m: 4,
        seed: 7,
        kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        partitions: Default::default(),
        promote: Vec::new(),
    }
}

fn ev(id: &str, tenant: &str, body: &str) -> Event {
    Event {
        event_id: id.into(),
        tenant_id: tenant.into(),
        event_time: NOW,
        observed_time: 0,
        event_name: "gen_ai.call".into(),
        cost: 0.01,
        error: false,
        body: body.into(),
        trace_id: "trace-1".into(),
        span_id: id.into(),
        attributes: Default::default(),
        idempotency_key: None,
    }
}

fn ingestor(root: &Path) -> Ingestor {
    let engine = Engine::init(root, config()).unwrap();
    Ingestor::open(engine).unwrap()
}

fn rows(ing: &Ingestor) -> usize {
    let snap = ing.engine.snapshot().unwrap();
    ing.engine
        .open_parts(&snap)
        .unwrap()
        .iter()
        .map(|r| r.manifest.row_count)
        .sum()
}

// ---------------------------------------------------- duplicates and replay

#[test]
fn a_replay_is_acknowledged_and_not_stored_again() {
    // Contract §2: same key, same content -> REPLAY. The producer retried after an
    // ack they never saw. They did exactly the right thing and must not be punished
    // for it: we acknowledge, and we store nothing.
    let root = tmp("replay");
    let mut ing = ingestor(&root);

    let batch: Vec<Event> = (0..10)
        .map(|i| ev(&format!("e{i}"), "t1", "the tool call timed out"))
        .collect();

    let first = ing.ingest(batch.clone(), None, None, NOW).unwrap();
    assert_eq!(first.published, 10);
    assert_eq!(first.duplicates_suppressed, 0);
    assert_eq!(rows(&ing), 10);

    // The exact same batch, again. And again.
    for _ in 0..3 {
        let r = ing.ingest(batch.clone(), None, None, NOW).unwrap();
        assert_eq!(r.published, 0, "a replay was stored a second time");
        assert_eq!(r.duplicates_suppressed, 10);
        assert_eq!(r.dead_lettered, 0, "a replay is not an error");
    }

    assert_eq!(
        rows(&ing),
        10,
        "replaying acknowledged input duplicated rows"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_conflict_is_refused_and_never_silently_rewrites_history() {
    // Contract §2: same key, DIFFERENT content -> CONFLICT. Last-write-wins here is
    // the seductive option and it is wrong: it silently rewrites history under a
    // reused id, and it makes behaviour depend on arrival order.
    let root = tmp("conflict");
    let mut ing = ingestor(&root);

    ing.ingest(vec![ev("e1", "t1", "the original body")], None, None, NOW)
        .unwrap();

    let mutated = ev("e1", "t1", "a COMPLETELY different body");
    let r = ing.ingest(vec![mutated], None, None, NOW).unwrap();

    assert_eq!(r.published, 0);
    assert_eq!(r.duplicates_suppressed, 0, "a conflict is not a replay");
    assert_eq!(r.dead_lettered, 1);
    assert_eq!(
        r.by_reason
            .get(&RejectReason::IdempotencyConflict.to_string()),
        Some(&1)
    );

    // The stored event is untouched. Both are visible: the stored one, and the
    // refused one in the dead-letter log, where a human can compare them.
    let hits = ing
        .engine
        .search(&Query {
            text: "the original body".into(),
            nprobe: 8,
            ..Default::default()
        })
        .unwrap();
    let stored = hits.hits.iter().find(|h| h.event.event_id == "e1").unwrap();
    assert_eq!(stored.event.body, "the original body");

    let dl = std::fs::read_to_string(ing.engine.store.deadletter_path()).unwrap();
    assert!(dl.contains("idempotency_conflict"));
    assert!(dl.contains("a COMPLETELY different body"));

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn idempotency_keys_are_scoped_per_tenant() {
    // One tenant must never be able to suppress another tenant's event by guessing
    // an id.
    let root = tmp("tenant-scope");
    let mut ing = ingestor(&root);

    ing.ingest(
        vec![ev("shared-id", "t1", "tenant one body")],
        None,
        None,
        NOW,
    )
    .unwrap();
    let r = ing
        .ingest(
            vec![ev("shared-id", "t2", "tenant two body")],
            None,
            None,
            NOW,
        )
        .unwrap();

    assert_eq!(r.published, 1, "t2's event was suppressed by t1's id");
    assert_eq!(r.dead_lettered, 0);
    assert_eq!(rows(&ing), 2);
    std::fs::remove_dir_all(root).ok();
}

// ---------------------------------------------------------------- offsets

#[test]
fn a_source_offset_never_advances_before_publication() {
    // Invariant 7, and the gate. Offsets may LAG reality; they must never LEAD it.
    let root = tmp("offsets");
    let mut ing = ingestor(&root);
    let state = root.join("sources");

    let events: Vec<Event> = (0..20)
        .map(|i| ev(&format!("e{i}"), "t1", "connection pool exhausted"))
        .collect();
    let src = MemorySource::new("kafka-like", events, &state).unwrap();

    assert_eq!(src.committed_offset().unwrap(), 0);

    let r = ing.poll_and_ingest(&src, 8, NOW).unwrap();
    assert_eq!(r.published, 8);
    assert_eq!(r.source_offset_before, Some(0));
    assert_eq!(r.source_offset_after, Some(8));
    assert_eq!(src.committed_offset().unwrap(), 8);
    assert_eq!(rows(&ing), 8);

    // Poll again: the source resumes from the committed offset, not from zero.
    let r = ing.poll_and_ingest(&src, 8, NOW).unwrap();
    assert_eq!(r.published, 8);
    assert_eq!(src.committed_offset().unwrap(), 16);
    assert_eq!(rows(&ing), 16);

    // Drain.
    ing.poll_and_ingest(&src, 8, NOW).unwrap();
    assert_eq!(src.committed_offset().unwrap(), 20);
    assert_eq!(rows(&ing), 20);

    // And an empty poll is a no-op, not an offset advance past the end.
    let r = ing.poll_and_ingest(&src, 8, NOW).unwrap();
    assert_eq!(r.published, 0);
    assert_eq!(src.committed_offset().unwrap(), 20);

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn an_offset_that_lags_reality_costs_a_replay_and_loses_nothing() {
    // The crash between publication and the offset commit. The source re-delivers
    // events we already have; idempotency must recognise every one as a replay.
    //
    // This is the *safe* direction. The unsafe direction -- an offset that leads --
    // is unrecoverable, and is why the offset is committed last.
    let root = tmp("lag");
    let mut ing = ingestor(&root);
    let state = root.join("sources");

    let events: Vec<Event> = (0..10)
        .map(|i| {
            ev(
                &format!("e{i}"),
                "t1",
                "deadlock detected on the orders table",
            )
        })
        .collect();
    let src = MemorySource::new("s", events, &state).unwrap();

    ing.poll_and_ingest(&src, 10, NOW).unwrap();
    assert_eq!(rows(&ing), 10);

    // Simulate the crash: the data is published, but the offset never advanced.
    source::commit_offset(&state, "s", 0).unwrap();
    assert_eq!(src.committed_offset().unwrap(), 0);

    // The source re-delivers everything. Nothing is duplicated.
    let r = ing.poll_and_ingest(&src, 10, NOW).unwrap();
    assert_eq!(r.duplicates_suppressed, 10);
    assert_eq!(r.published, 0);
    assert_eq!(rows(&ing), 10, "an offset that lagged duplicated rows");

    // And the offset catches up, so it does not replay forever.
    assert_eq!(src.committed_offset().unwrap(), 10);
    std::fs::remove_dir_all(root).ok();
}

// ------------------------------------------------- quotas and starvation

#[test]
fn a_tenant_cannot_exceed_its_quota() {
    let root = tmp("quota");
    let mut ing = ingestor(&root);
    ing.quotas.set_limit(
        "loud",
        Quota {
            events_per_sec: 5,
            ..Default::default()
        },
    );

    let events: Vec<Event> = (0..20)
        .map(|i| ev(&format!("e{i}"), "loud", "invalid bearer token"))
        .collect();
    let r = ing.ingest(events, None, None, NOW).unwrap();

    assert_eq!(r.published, 5);
    assert_eq!(r.dead_lettered, 15);
    assert_eq!(
        r.by_reason.get(&RejectReason::QuotaExceeded.to_string()),
        Some(&15)
    );
    assert_eq!(rows(&ing), 5);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_loud_tenant_cannot_starve_a_quiet_one() {
    // The other half of the gate, and the half that is easy to forget: a tenant that
    // is comfortably WITHIN quota can still monopolise a batch by being loud.
    let root = tmp("starve");
    let mut ing = ingestor(&root);

    let mut events: Vec<Event> = (0..2_000)
        .map(|i| ev(&format!("loud{i}"), "loud", "retrying the http request"))
        .collect();
    // The quiet tenant's single event arrives LAST.
    events.push(ev(
        "quiet1",
        "quiet",
        "why was my invoice higher this month",
    ));

    let r = ing.ingest(events, None, None, NOW).unwrap();
    assert_eq!(r.dead_lettered, 0, "nobody was over quota here");

    // It is stored, and it is queryable.
    let hits = ing
        .engine
        .search(&Query {
            text: "why was my invoice higher this month".into(),
            tenant: Some("quiet".into()),
            nprobe: 8,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.hits.len(), 1);
    assert_eq!(hits.hits[0].event.event_id, "quiet1");
    std::fs::remove_dir_all(root).ok();
}

// ----------------------------------------------- attributes are bounded

#[test]
fn an_unbounded_attribute_key_cardinality_is_refused_not_absorbed() {
    // Directive 1, and the limit that actually protects the format. A tenant using a
    // VALUE (a uuid) as a KEY would otherwise grow a dictionary the size of their
    // traffic, carried in every manifest, forever.
    let root = tmp("cardinality");
    let mut ing = ingestor(&root);

    // Fill the dictionary right up to the bound, a few keys at a time.
    let mut filled = 0usize;
    let mut batch = Vec::new();
    while filled < MAX_ATTRIBUTE_KEY_CARDINALITY {
        let mut e = ev(&format!("fill{filled}"), "t1", "planning the next step");
        for j in 0..8 {
            if filled + j >= MAX_ATTRIBUTE_KEY_CARDINALITY {
                break;
            }
            e.attributes
                .insert(format!("legit.key.{}", filled + j), AttrValue::Int(1));
        }
        filled += 8;
        batch.push(e);
    }
    let r = ing.ingest(batch, None, None, NOW).unwrap();
    assert_eq!(r.dead_lettered, 0);

    let dict: KeyDictionary = ing.key_dictionary().unwrap();
    assert_eq!(dict.len(), MAX_ATTRIBUTE_KEY_CARDINALITY);

    // Now the pathological producer: a uuid as a key.
    let mut bad = ev("bad1", "t1", "summarize this thread");
    bad.attributes
        .insert("user_id_9f3c1a7e-4d2b".into(), AttrValue::Str("x".into()));
    let r = ing.ingest(vec![bad], None, None, NOW).unwrap();

    assert_eq!(r.published, 0);
    assert_eq!(
        r.by_reason
            .get(&RejectReason::AttributeKeyCardinalityExceeded.to_string()),
        Some(&1)
    );

    // And the tenant is TOLD, in terms they can act on.
    let dl = std::fs::read_to_string(ing.engine.store.deadletter_path()).unwrap();
    assert!(dl.contains("attribute_key_cardinality_exceeded"));
    assert!(dl.contains("instrumentation bug"));

    // Crucially: the refused event did not widen the dictionary. Otherwise a
    // producer could exhaust a partition's budget using events never even stored.
    assert_eq!(
        ing.key_dictionary().unwrap().len(),
        MAX_ATTRIBUTE_KEY_CARDINALITY
    );

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn typed_attributes_survive_a_round_trip_through_storage() {
    let root = tmp("attrs");
    let mut ing = ingestor(&root);

    let mut e = ev("e1", "t1", "the payment api returned a rate limit error");
    e.attributes
        .insert("gen_ai.system".into(), AttrValue::Str("anthropic".into()));
    e.attributes
        .insert("gen_ai.usage.input_tokens".into(), AttrValue::Int(1200));
    e.attributes
        .insert("gen_ai.request.temperature".into(), AttrValue::Double(0.7));
    e.attributes.insert("stream".into(), AttrValue::Bool(true));

    ing.ingest(vec![e], None, None, NOW).unwrap();

    let hits = ing
        .engine
        .search(&Query {
            text: "the payment api returned a rate limit error".into(),
            nprobe: 8,
            ..Default::default()
        })
        .unwrap();
    let got = &hits.hits[0].event;

    // Types are preserved. An int is still an int -- not a string that used to be
    // one, which is what a schemaless map would have given us.
    assert_eq!(
        got.attributes.get("gen_ai.usage.input_tokens"),
        Some(&AttrValue::Int(1200))
    );
    assert_eq!(
        got.attributes.get("gen_ai.request.temperature"),
        Some(&AttrValue::Double(0.7))
    );
    assert_eq!(got.attributes.get("stream"), Some(&AttrValue::Bool(true)));
    assert_eq!(
        got.attributes.get("gen_ai.system"),
        Some(&AttrValue::Str("anthropic".into()))
    );

    // Trace context and both timestamps survive too.
    assert_eq!(got.trace_id, "trace-1");
    assert_eq!(got.span_id, "e1");
    assert_eq!(got.event_time, NOW);
    assert_eq!(
        got.observed_time, NOW,
        "observed_time is set by the boundary"
    );

    std::fs::remove_dir_all(root).ok();
}

// ------------------------------------------------------------------ time

#[test]
fn an_event_outside_the_skew_window_is_dead_lettered_never_clamped() {
    // Contract §4. Silently rewriting a producer's timestamp is falsifying their
    // data, and they will believe us.
    let root = tmp("skew");
    let mut ing = ingestor(&root);

    let mut ancient = ev("old", "t1", "a very late event");
    ancient.event_time = NOW - 30 * 24 * 60 * 60 * 1000; // 30 days late

    let mut future = ev("future", "t1", "a clock skewed event");
    future.event_time = NOW + 10 * 60 * 60 * 1000; // 10 hours ahead

    let ok = ev("fine", "t1", "an event that is on time");

    let r = ing
        .ingest(vec![ancient, future, ok], None, None, NOW)
        .unwrap();
    assert_eq!(r.published, 1);
    assert_eq!(r.dead_lettered, 2);
    assert_eq!(
        r.by_reason.get(&RejectReason::EventTimeTooLate.to_string()),
        Some(&1)
    );
    assert_eq!(
        r.by_reason
            .get(&RejectReason::EventTimeInFuture.to_string()),
        Some(&1)
    );

    // The rejected events still carry the timestamps their producer sent. We did not
    // "fix" them.
    let dl = std::fs::read_to_string(ing.engine.store.deadletter_path()).unwrap();
    assert!(dl.contains(&(NOW + 10 * 60 * 60 * 1000).to_string()));

    std::fs::remove_dir_all(root).ok();
}

// -------------------------------------------------- the crash that matters

/// Drive a real `prism` process and kill it at a named boundary.
fn run_killed(args: &[&str], fault: &str) -> bool {
    let out = Command::new(env!("CARGO_BIN_EXE_prism"))
        .args(args)
        .env("PRISM_FAULT", fault)
        .output()
        .expect("failed to run prism");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        return out.status.signal() == Some(6);
    }
    #[allow(unreachable_code)]
    {
        !out.status.success()
    }
}

fn run_ok(args: &[&str]) -> std::process::Output {
    let out = Command::new(env!("CARGO_BIN_EXE_prism"))
        .args(args)
        .env_remove("PRISM_FAULT")
        .output()
        .expect("failed to run prism");
    assert!(
        out.status.success(),
        "prism {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

#[test]
fn an_event_acked_then_crashed_before_the_part_write_reappears_exactly_once() {
    // **The crash the architect named**, and the one the whole WAL exists for.
    //
    // The batch is acknowledged (it is durable in the admission log), the embedding
    // has already been computed and the GPU time spent, and the events exist nowhere
    // else. Then the process dies.
    //
    // Recovery must bring them back: exactly once, and WITH their semantic columns.
    // Not lost. Not doubled. Not stored blind without an embedding.
    let root = tmp("wal-crash");
    let dir = root.join("store");
    let feed = root.join("feed.jsonl");
    std::fs::create_dir_all(&root).unwrap();

    let events: Vec<Event> = (0..12)
        .map(|i| {
            ev_now(
                &format!("e{i}"),
                "t1",
                "the agent failed to plan the next step",
            )
        })
        .collect();
    let lines: Vec<String> = events
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect();
    std::fs::write(&feed, lines.join("\n")).unwrap();

    run_ok(&[
        "init",
        "--path",
        dir.to_str().unwrap(),
        "--dim",
        "32",
        "--nlist",
        "8",
        "--pq-m",
        "4",
    ]);

    // Die AFTER the ack and AFTER the embedding, BEFORE the part is durable.
    let died = run_killed(
        &[
            "ingest-source",
            "--path",
            dir.to_str().unwrap(),
            "--file",
            feed.to_str().unwrap(),
            "--source",
            "feed",
        ],
        "ingest.after_embed_before_part",
    );
    assert!(
        died,
        "the kill point did not fire; this test proves nothing"
    );

    // Nothing is visible yet -- the catalog never committed.
    let engine = Engine::open(&dir).unwrap();
    let snap = engine.snapshot().unwrap();
    let visible: usize = engine
        .open_parts(&snap)
        .unwrap()
        .iter()
        .map(|r| r.manifest.row_count)
        .sum();
    assert_eq!(visible, 0, "an uncommitted part was visible");

    // ...but the events WERE acked, so they are in the admission log, waiting.
    let ing = Ingestor::open(Engine::open(&dir).unwrap()).unwrap();
    let outstanding = ing.wal.outstanding().unwrap();
    assert_eq!(outstanding.len(), 1, "the acked batch is not in the WAL");
    assert_eq!(outstanding[0].events.len(), 12);

    // The offset never advanced: a crash before publication must leave it behind.
    assert_eq!(
        source::committed_offset(&dir.join("sources"), "feed").unwrap(),
        0,
        "the source offset advanced before publication (invariant 7)"
    );

    // Recovery.
    let out = run_ok(&["recover", "--path", dir.to_str().unwrap()]);
    let rep: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rep["recovered_batches"], 1);
    assert_eq!(rep["recovered_events"], 12);

    // Exactly once...
    let engine = Engine::open(&dir).unwrap();
    let snap = engine.snapshot().unwrap();
    let visible: usize = engine
        .open_parts(&snap)
        .unwrap()
        .iter()
        .map(|r| r.manifest.row_count)
        .sum();
    assert_eq!(
        visible, 12,
        "recovery did not restore exactly the acked events"
    );

    // ...WITH their semantic columns. This is the clause that matters: an event
    // brought back without its embedding is an event that will never match a
    // semantic query, for reasons nobody could reconstruct later.
    let res = engine
        .search(&Query {
            text: "the agent failed to plan the next step".into(),
            nprobe: 8,
            k: 12,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        res.hits.len(),
        12,
        "the recovered events have no embeddings"
    );
    assert!(res.hits.iter().all(|h| h.score > 0.9));

    // And now the offset has caught up, so the source will not re-deliver forever.
    assert_eq!(
        source::committed_offset(&dir.join("sources"), "feed").unwrap(),
        12
    );

    // Re-running is a no-op: the WAL record is applied, and idempotency recognises
    // the source's re-delivery as a replay.
    let out = run_ok(&["recover", "--path", dir.to_str().unwrap()]);
    let rep: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rep["recovered_batches"], 0);

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_crash_before_the_wal_is_fsynced_loses_nothing_because_nothing_was_acked() {
    // The other side of the ack point. Before the fsync, no producer has been told
    // anything. The batch simply never happened, and the source will re-deliver it.
    let root = tmp("wal-nofsync");
    let dir = root.join("store");
    let feed = root.join("feed.jsonl");
    std::fs::create_dir_all(&root).unwrap();

    let lines: Vec<String> = (0..6)
        .map(|i| {
            serde_json::to_string(&ev_now(&format!("e{i}"), "t1", "replica lag exceeded")).unwrap()
        })
        .collect();
    std::fs::write(&feed, lines.join("\n")).unwrap();

    run_ok(&[
        "init",
        "--path",
        dir.to_str().unwrap(),
        "--dim",
        "32",
        "--nlist",
        "8",
        "--pq-m",
        "4",
    ]);
    assert!(run_killed(
        &[
            "ingest-source",
            "--path",
            dir.to_str().unwrap(),
            "--file",
            feed.to_str().unwrap(),
            "--source",
            "feed",
        ],
        "wal.after_append_before_fsync",
    ));

    // The offset did not move, so the source still owns these events.
    assert_eq!(
        source::committed_offset(&dir.join("sources"), "feed").unwrap(),
        0
    );

    // Re-deliver. Everything lands, exactly once.
    run_ok(&["recover", "--path", dir.to_str().unwrap()]);
    run_ok(&[
        "ingest-source",
        "--path",
        dir.to_str().unwrap(),
        "--file",
        feed.to_str().unwrap(),
        "--source",
        "feed",
    ]);

    let engine = Engine::open(&dir).unwrap();
    let snap = engine.snapshot().unwrap();
    let visible: usize = engine
        .open_parts(&snap)
        .unwrap()
        .iter()
        .map(|r| r.manifest.row_count)
        .sum();
    assert_eq!(visible, 6);

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_crash_after_publication_but_before_the_offset_commit_replays_and_does_not_duplicate() {
    // Offsets may LAG. They must never LEAD. This drives the lagging case through a
    // real process death.
    let root = tmp("offset-crash");
    let dir = root.join("store");
    let feed = root.join("feed.jsonl");
    std::fs::create_dir_all(&root).unwrap();

    let lines: Vec<String> = (0..8)
        .map(|i| {
            serde_json::to_string(&ev_now(
                &format!("e{i}"),
                "t1",
                "query timed out after thirty seconds",
            ))
            .unwrap()
        })
        .collect();
    std::fs::write(&feed, lines.join("\n")).unwrap();

    run_ok(&[
        "init",
        "--path",
        dir.to_str().unwrap(),
        "--dim",
        "32",
        "--nlist",
        "8",
        "--pq-m",
        "4",
    ]);
    assert!(run_killed(
        &[
            "ingest-source",
            "--path",
            dir.to_str().unwrap(),
            "--file",
            feed.to_str().unwrap(),
            "--source",
            "feed",
        ],
        "ingest.after_publish_before_offset_commit",
    ));

    // The data IS published -- the catalog committed before we died.
    let engine = Engine::open(&dir).unwrap();
    let snap = engine.snapshot().unwrap();
    let visible: usize = engine
        .open_parts(&snap)
        .unwrap()
        .iter()
        .map(|r| r.manifest.row_count)
        .sum();
    assert_eq!(visible, 8);

    // But the offset lagged. The source still thinks we have nothing.
    assert_eq!(
        source::committed_offset(&dir.join("sources"), "feed").unwrap(),
        0
    );

    // So it re-delivers everything. Idempotency recognises all eight as replays.
    let out = run_ok(&[
        "ingest-source",
        "--path",
        dir.to_str().unwrap(),
        "--file",
        feed.to_str().unwrap(),
        "--source",
        "feed",
    ]);
    let rep: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rep["duplicates_suppressed"], 8);
    assert_eq!(rep["published"], 0);

    let engine = Engine::open(&dir).unwrap();
    let snap = engine.snapshot().unwrap();
    let visible: usize = engine
        .open_parts(&snap)
        .unwrap()
        .iter()
        .map(|r| r.manifest.row_count)
        .sum();
    assert_eq!(visible, 8, "a lagging offset duplicated rows on replay");

    std::fs::remove_dir_all(root).ok();
}
