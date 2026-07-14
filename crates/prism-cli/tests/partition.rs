//! The S4 gate: isolation as an **I/O property**, and promotion as a **dual door**.
//!
//! > *"'Physically impossible' must be testable, so define it as an I/O property: a query's
//! > execution trace never touches a byte range belonging to another tenant's partition. The
//! > strongest gate test: fill other tenants' partitions with unreadable garbage — every
//! > tenant-A query still returns correct results, because it never looked."*
//!
//! That is exactly what `a_query_never_touches_another_tenants_partition_even_if_it_is_garbage`
//! does, and it is the reason partition metadata lives in the **catalog** rather than in the
//! part manifests. Pruning that has to open a manifest to decide whether to open a manifest is
//! not isolation.

use prism_engine::corpus::{self, Kind};
use prism_engine::Engine;
use prism_part::partition::{Bucket, PartitionScheme};
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_sql::{compile, Session};
use prism_types::rng::Rng;
use prism_types::{AttrValue, Event, Query};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-s4-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn config(scheme: PartitionScheme, promote: Vec<String>) -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 64,
        nlist: 16,
        pq_m: 8,
        seed: 9,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        partitions: scheme,
        promote,
    }
}

/// A corpus whose tenants are explicit, so a test can name them.
fn events(n: usize, tenants: &[&str], seed: u64) -> Vec<Event> {
    corpus::generate(Kind::Zipf, n, seed)
        .into_iter()
        .enumerate()
        .map(|(i, mut e)| {
            e.tenant_id = tenants[i % tenants.len()].to_string();
            e.event_id = format!("{}-{i}", e.tenant_id);
            e
        })
        .collect()
}

fn store(tag: &str, n: usize, tenants: &[&str], promote: Vec<String>) -> (Engine, PathBuf) {
    let root = tmp(tag);
    let engine = Engine::init(&root, config(PartitionScheme::default(), promote)).unwrap();
    engine
        .ingest(events(n, tenants, 5), 1_760_000_000_000)
        .unwrap();
    (engine, root)
}

fn sess(t: &str) -> Session {
    Session {
        tenant: t.to_string(),
    }
}

fn count(engine: &Engine, tenant: &str, sql: &str) -> usize {
    let plan = compile(sql, &sess(tenant)).unwrap();
    let r = engine.run_sql(&plan, None).unwrap();
    r.rows[0][0].1.as_u64().unwrap() as usize
}

// ================================================================= directive 2

#[test]
fn a_query_never_touches_another_tenants_partition_even_if_it_is_garbage() {
    // **The S4 gate.** Isolation is not a filter we promise to apply; it is a set of bytes we
    // never read.
    //
    // Fill every partition that does not belong to tenant `alpha` with unreadable garbage --
    // shredded manifests, truncated columns, the lot -- and then run alpha's queries. They must
    // all still answer, correctly, because they never looked.
    //
    // If pruning still opened every part's manifest to decide what to skip (as it did until
    // S4), every one of these would fail. That is the whole point.
    let root = tmp("garbage");
    let engine = Engine::init(&root, config(PartitionScheme::default(), vec![])).unwrap();
    engine
        .ingest(
            events(3000, &["alpha", "bravo", "charlie", "delta"], 5),
            1_760_000_000_000,
        )
        .unwrap();

    // The truth, measured before anything is destroyed.
    let truth_rows = count(&engine, "alpha", "SELECT count(*) FROM events");
    assert!(truth_rows > 100, "alpha needs real data: {truth_rows}");

    let q = Query {
        text: "the tool call timed out".into(),
        tenant: Some("alpha".into()),
        k: 10,
        ..Default::default()
    };
    let truth_hits: Vec<String> = engine
        .search(&q)
        .unwrap()
        .hits
        .iter()
        .map(|h| h.event.event_id.clone())
        .collect();
    assert!(!truth_hits.is_empty());

    // --- now destroy everything that is not alpha's ---
    let snap = engine.snapshot().unwrap();
    let scheme = &engine.store.config.partitions;
    let alpha_bucket = scheme.bucket_of("alpha");

    let mut shredded = 0usize;
    for e in &snap.parts {
        let r = e.located().expect("S4 parts are located");
        if r.partition.bucket == alpha_bucket {
            continue;
        }
        // Not alpha's. Shred it: every file, every byte.
        let dir = engine.store.part_dir(&r.part_id);
        for f in std::fs::read_dir(&dir).unwrap() {
            let f = f.unwrap().path();
            std::fs::write(
                &f,
                b"THIS IS NOT A PART. IF YOU READ THIS, ISOLATION IS A LIE.",
            )
            .unwrap();
        }
        shredded += 1;
    }
    assert!(
        shredded >= 2,
        "the test destroyed only {shredded} foreign partitions; it is not proving much"
    );

    // --- and alpha carries on as if nothing happened ---
    let engine = Engine::open(&root).unwrap();

    assert_eq!(
        count(&engine, "alpha", "SELECT count(*) FROM events"),
        truth_rows,
        "a scalar query changed its answer because ANOTHER tenant's bytes were destroyed"
    );

    let hits: Vec<String> = engine
        .search(&q)
        .unwrap()
        .hits
        .iter()
        .map(|h| h.event.event_id.clone())
        .collect();
    assert_eq!(
        hits, truth_hits,
        "a semantic query changed its answer because ANOTHER tenant's bytes were destroyed"
    );

    // Hybrid, aggregate, time-bounded: none of them looked either.
    for sql in [
        "SELECT count(*) FROM events WHERE cost > 0.01",
        "SELECT count(*) FROM events WHERE event_time >= 1760000000000",
        "SELECT count(*) FROM events WHERE event_name = 'tool.retry'",
    ] {
        let plan = compile(sql, &sess("alpha")).unwrap();
        engine
            .run_sql(&plan, None)
            .unwrap_or_else(|e| panic!("alpha's query read a foreign partition: {sql}\n  {e}"));
    }

    // And the shredded tenants get an *error*, not silence. Their data really is gone, and
    // saying so is the only honest answer.
    let bravo = compile("SELECT count(*) FROM events", &sess("bravo")).unwrap();
    assert!(
        engine.run_sql(&bravo, None).is_err(),
        "bravo's data was destroyed; reporting zero rows would be a lie"
    );

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn corrupting_one_tenants_partition_has_a_blast_radius_of_exactly_that_tenant() {
    // The S1 corruption suite, scoped per tenant. Damage is *attributable*: an operator can say
    // "tenant bravo lost data between 09:00 and 10:00", and mean it, rather than "the store is
    // corrupt".
    let root = tmp("blast");
    let engine = Engine::init(&root, config(PartitionScheme::default(), vec![])).unwrap();
    engine
        .ingest(events(2000, &["alpha", "bravo"], 7), 1_760_000_000_000)
        .unwrap();

    let before_alpha = count(&engine, "alpha", "SELECT count(*) FROM events");
    let before_bravo = count(&engine, "bravo", "SELECT count(*) FROM events");
    assert!(before_alpha > 0 && before_bravo > 0);

    // Flip one bit inside one of bravo's parts -- the S1 corruption, aimed.
    let snap = engine.snapshot().unwrap();
    let bravo_bucket = engine.store.config.partitions.bucket_of("bravo");
    let victim = snap
        .parts
        .iter()
        .find_map(|e| {
            let r = e.located().unwrap();
            (r.partition.bucket == bravo_bucket).then(|| r.part_id.clone())
        })
        .expect("bravo has a part");

    let f = engine.store.part_dir(&victim).join("pq.codes");
    let mut bytes = std::fs::read(&f).unwrap();
    let n = bytes.len();
    bytes[n / 2] ^= 0x01;
    std::fs::write(&f, &bytes).unwrap();

    let engine = Engine::open(&root).unwrap();

    // Alpha does not notice. Not "mostly": at all.
    assert_eq!(
        count(&engine, "alpha", "SELECT count(*) FROM events"),
        before_alpha,
        "corrupting bravo's bytes changed alpha's answer"
    );

    // Bravo gets a specific, attributable error naming the damaged block -- the S1 discipline,
    // now with a tenant attached to it.
    //
    // Note *which* query fails. We damaged `pq.codes`, so bravo's SEMANTIC queries are broken --
    // and bravo's `count(*)` still answers, because a count does not read the compressed codes.
    // Blast radius is localized per tenant AND per column: an operator can say "tenant bravo
    // cannot run similarity search on this partition", which is far more actionable than "the
    // store is corrupt".
    let q = Query {
        text: "the tool call timed out".into(),
        tenant: Some("bravo".into()),
        ..Default::default()
    };
    let err = engine
        .search(&q)
        .expect_err("bravo's codes are damaged; a semantic query must not silently under-report")
        .to_string();
    assert!(err.contains("checksum"), "{err}");
    assert!(
        err.contains(&victim),
        "the error does not name the damaged part: {err}"
    );

    // Alpha's semantic queries are untouched.
    let qa = Query {
        text: "the tool call timed out".into(),
        tenant: Some("alpha".into()),
        ..Default::default()
    };
    assert!(
        !engine.search(&qa).unwrap().hits.is_empty(),
        "corrupting bravo's codes broke alpha's semantic search"
    );

    // And bravo's count still works, because a count never reads the codes.
    assert_eq!(
        count(&engine, "bravo", "SELECT count(*) FROM events"),
        before_bravo,
        "the damage was not localized to the column that actually holds it"
    );

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_dedicated_bucket_shares_a_part_with_nobody() {
    let root = tmp("dedicated");
    let mut scheme = PartitionScheme::default();
    scheme.dedicated.insert("whale".into(), 0);

    let engine = Engine::init(&root, config(scheme, vec![])).unwrap();
    engine
        .ingest(
            events(1500, &["whale", "minnow", "krill"], 3),
            1_760_000_000_000,
        )
        .unwrap();

    let snap = engine.snapshot().unwrap();
    for e in &snap.parts {
        let r = e.located().unwrap();
        if matches!(r.partition.bucket, Bucket::Dedicated(_)) {
            assert_eq!(
                r.tenants,
                vec!["whale".to_string()],
                "a DEDICATED bucket holds another tenant's rows; every isolation claim resting \
                 on it is now false"
            );
        } else {
            assert!(!r.tenants.iter().any(|t| t == "whale"));
        }
    }
    std::fs::remove_dir_all(root).ok();
}

// ================================================================= directive 3

#[test]
fn a_shared_bucket_leaks_no_metadata_through_any_query_surface() {
    // The shared-bucket seam. Part-level metadata describes the BUCKET, not the tenant -- one
    // time range, one cost range, one union key dictionary. Every one of those tells tenant A
    // something about tenant B, so the metadata a QUERY can observe is scoped per tenant.
    let root = tmp("seam");
    let engine = Engine::init(&root, config(PartitionScheme::default(), vec![])).unwrap();

    // Force co-tenancy: find two tenants that hash into the same bucket.
    let scheme = PartitionScheme::default();
    let mut pair: Option<(String, String)> = None;
    for i in 0..500 {
        for j in (i + 1)..500 {
            let (a, b) = (format!("t{i}"), format!("t{j}"));
            if scheme.bucket_of(&a) == scheme.bucket_of(&b) {
                pair = Some((a, b));
                break;
            }
        }
        if pair.is_some() {
            break;
        }
    }
    let (a, b) = pair.expect("two tenants must share a bucket somewhere");
    assert!(scheme.bucket_of(&a).is_shared());

    // `a` has narrow time and cost ranges and one attribute key. `b` has wide ones and a
    // *different* key. If part-level metadata were used, `a` would inherit `b`'s ranges.
    // Both tenants inside ONE time window, so they genuinely land in the same part -- but with
    // disjoint time ranges inside it, which is what makes the per-tenant zone map matter.
    let day = prism_part::partition::DEFAULT_TIME_WINDOW_MS;
    let base = (1_760_000_000_000i64 / day) * day; // start of a window

    let mut evs = Vec::new();
    for i in 0..40 {
        let mut e = corpus::generate(Kind::Uniform, 1, 100 + i)[0].clone();
        e.tenant_id = a.clone();
        e.event_id = format!("a-{i}");
        e.event_time = base + i as i64;
        e.cost = 0.001;
        e.attributes.clear();
        e.attributes.insert("a.only.key".into(), AttrValue::Int(1));
        evs.push(e);
    }
    for i in 0..40 {
        let mut e = corpus::generate(Kind::Uniform, 1, 500 + i)[0].clone();
        e.tenant_id = b.clone();
        e.event_id = format!("b-{i}");
        // Same window (same part), a much later instant, a much larger cost.
        e.event_time = base + 3_600_000 + i as i64;
        e.cost = 99.0;
        e.attributes.clear();
        e.attributes
            .insert("b.secret.key".into(), AttrValue::Str("shhh".into()));
        evs.push(e);
    }
    engine.ingest(evs, base).unwrap();

    // They really are co-tenants in one part.
    let snap = engine.snapshot().unwrap();
    let shared = snap
        .parts
        .iter()
        .find(|e| e.located().unwrap().tenants.len() > 1)
        .expect("the two tenants should share a part");
    let r = shared.located().unwrap();
    assert!(r.tenants.contains(&a) && r.tenants.contains(&b));

    // --- the per-tenant sections are what a query sees ---
    let m = prism_part::part::PartReader::open(&engine.store.part_dir(&r.part_id))
        .unwrap()
        .manifest
        .s4()
        .unwrap();

    let sa = m.stats_for(&a).unwrap();
    let sb = m.stats_for(&b).unwrap();

    // "Does this part contain key X?" is answerable PER TENANT.
    assert!(sa.has_key("a.only.key"));
    assert!(
        !sa.has_key("b.secret.key"),
        "tenant {a} can see tenant {b}'s attribute keys through the part metadata"
    );
    assert!(sb.has_key("b.secret.key"));
    assert!(!sb.has_key("a.only.key"));

    // A zone map is a zone map FOR ONE TENANT. `a`'s rows all sit in the first minute; a query
    // for `a` outside that window must be able to skip the part, even though the part as a whole
    // -- because of `b` -- spans an hour.
    assert!(sa.time_max < sb.time_min);
    assert!(!sa.may_match(Some(sb.time_min), None));
    assert!(sb.may_match(Some(sb.time_min), None));

    // And cost: `a` never sees a hint of `b`'s 99.0.
    assert!(sa.cost_max < 1.0);
    assert!(sb.cost_min > 1.0);

    // Nothing about `b` is reachable from a query as `a` -- not a row, not a count, not a
    // key.
    assert_eq!(
        count(
            &engine,
            &a,
            "SELECT count(*) FROM events WHERE attributes['b.secret.key'] = 'shhh'"
        ),
        0
    );
    assert_eq!(count(&engine, &a, "SELECT count(*) FROM events"), 40);
    assert_eq!(count(&engine, &b, "SELECT count(*) FROM events"), 40);

    std::fs::remove_dir_all(root).ok();
}

// ================================================================= directive 4

#[test]
fn a_promoted_key_and_a_mapped_key_are_the_same_door() {
    // **The S4 gate that matters most.** Promotion is a versioned schema event, so the two
    // representations -- typed column and attribute map -- COEXIST across parts of different
    // ages. A query over a promoted key must not be able to tell which one it landed on.
    //
    // Two stores, identical in every way except that one promotes `gen_ai.system` and
    // `gen_ai.usage.input_tokens` to typed columns. Every query must agree, row for row and
    // counter for counter.
    let tenants = ["alpha", "bravo"];
    let (mapped, r1) = store("promo-map", 2000, &tenants, vec![]);
    let (promoted, r2) = store(
        "promo-col",
        2000,
        &tenants,
        vec![
            "gen_ai.system".to_string(),
            "gen_ai.usage.input_tokens".to_string(),
        ],
    );

    // The promoting store really did promote.
    let snap = promoted.snapshot().unwrap();
    let pr =
        prism_part::part::PartReader::open(&promoted.store.part_dir(&snap.part_ids()[0])).unwrap();
    let s4 = pr.manifest.s4().unwrap();
    assert_eq!(s4.promoted.len(), 2, "the store did not promote anything");
    // ...and the promoted keys are GONE from the attribute map. Storing them twice would make
    // promotion cost storage rather than save it, and leave two sources of truth for one value.
    assert!(!pr
        .manifest
        .attribute_keys
        .contains(&"gen_ai.system".to_string()));

    let queries = [
        "SELECT count(*) FROM events WHERE attributes['gen_ai.system'] = 'anthropic'",
        "SELECT count(*) FROM events WHERE attributes['gen_ai.usage.input_tokens'] > 2000",
        "SELECT count(*) FROM events WHERE attributes['gen_ai.system'] = 'openai' \
         AND attributes['gen_ai.usage.input_tokens'] < 500",
        // A key that was NOT promoted must still work, in both stores.
        "SELECT count(*) FROM events WHERE attributes['stream'] = true",
        // An absent key is absent in both. Absent is not zero, and it is not false.
        "SELECT count(*) FROM events WHERE attributes['never.existed'] = 'x'",
        // A promoted key combined with a scalar predicate.
        "SELECT count(*) FROM events WHERE attributes['gen_ai.system'] = 'local' AND cost > 0.02",
    ];

    for sql in queries {
        let pm = compile(sql, &sess("alpha")).unwrap();
        let a = mapped.run_sql(&pm, None).unwrap();
        let b = promoted.run_sql(&pm, None).unwrap();

        assert_eq!(a.rows, b.rows, "promotion changed the ANSWER:\n  {sql}");

        // Every LOGICAL counter must be identical: same parts opened, same rows scanned, same
        // rows passing the filter. If promotion changed what the engine *considered*, it would
        // be a different query wearing the same text.
        let logical = |c: &prism_types::Counters| {
            (
                c.parts_total,
                c.parts_pruned,
                c.parts_opened,
                c.rows_eligible,
                c.rows_passing_filter,
            )
        };
        assert_eq!(
            logical(&a.counters),
            logical(&b.counters),
            "promotion changed the logical execution:\n  {sql}"
        );

        // `physical_bytes_read` is the ONE counter that legitimately differs -- and it must
        // differ downward, or promotion bought nothing. A predicate over promoted keys never
        // decodes the attribute map at all.
        if !sql.contains("stream") && !sql.contains("never.existed") {
            assert!(
                b.counters.physical_bytes_read < a.counters.physical_bytes_read,
                "promotion read MORE bytes, so it bought nothing:\n  {sql}\n  map={} promoted={}",
                a.counters.physical_bytes_read,
                b.counters.physical_bytes_read
            );
        }
    }

    // And a semantic query returns identical rows and scores either way.
    let q = Query {
        text: "the tool call timed out".into(),
        tenant: Some("alpha".into()),
        k: 10,
        predicate: Some(prism_types::Predicate::Cmp(
            Box::new(prism_types::Predicate::Attribute("gen_ai.system".into())),
            prism_types::CmpOp::Eq,
            Box::new(prism_types::Predicate::Literal(prism_types::Literal::Str(
                "anthropic".into(),
            ))),
        )),
        ..Default::default()
    };
    let ha: Vec<(String, f32)> = mapped
        .search(&q)
        .unwrap()
        .hits
        .iter()
        .map(|h| (h.event.event_id.clone(), h.score))
        .collect();
    let hb: Vec<(String, f32)> = promoted
        .search(&q)
        .unwrap()
        .hits
        .iter()
        .map(|h| (h.event.event_id.clone(), h.score))
        .collect();
    assert_eq!(ha, hb, "promotion changed a semantic query's answer");

    std::fs::remove_dir_all(r1).ok();
    std::fs::remove_dir_all(r2).ok();
}

#[test]
fn a_promoted_event_reads_back_identical_to_an_unpromoted_one() {
    // Promotion is a STORAGE decision, not a schema change. An event read out of a part that
    // promoted a key must be byte-identical to the same event read out of a part that did not
    // -- or every equivalence in the system quietly stops holding.
    let tenants = ["alpha"];
    let (mapped, r1) = store("readback-map", 400, &tenants, vec![]);
    let (promoted, r2) = store(
        "readback-col",
        400,
        &tenants,
        vec!["gen_ai.system".to_string()],
    );

    let read = |e: &Engine| -> Vec<Event> {
        let snap = e.snapshot().unwrap();
        let mut all: Vec<Event> = e
            .open_parts(&snap)
            .unwrap()
            .iter()
            .flat_map(|r| r.read_all().unwrap().events)
            .collect();
        all.sort_by(|a, b| a.event_id.cmp(&b.event_id));
        all
    };

    assert_eq!(
        read(&mapped),
        read(&promoted),
        "an event read out of a promoted part differs from the same event read out of a mapped \
         one; promotion is not supposed to be observable"
    );

    std::fs::remove_dir_all(r1).ok();
    std::fs::remove_dir_all(r2).ok();
}

#[test]
fn promoting_a_key_used_with_two_types_is_refused() {
    // A promoted column is TYPED; that is the point. A key used as an int on one row and a
    // string on another is not a column, it is a map entry pretending to be one, and promoting
    // it would silently coerce or drop values.
    let root = tmp("mixed-type");
    let engine = Engine::init(
        &root,
        config(PartitionScheme::default(), vec!["mixed".to_string()]),
    )
    .unwrap();

    let mut evs = corpus::generate(Kind::Uniform, 2, 1);
    evs[0].tenant_id = "t".into();
    evs[1].tenant_id = "t".into();
    evs[0].attributes.insert("mixed".into(), AttrValue::Int(1));
    evs[1]
        .attributes
        .insert("mixed".into(), AttrValue::Str("one".into()));

    let e = engine
        .ingest(evs, 1_760_000_000_000)
        .unwrap_err()
        .to_string();
    assert!(e.contains("more than one value type"), "{e}");
    std::fs::remove_dir_all(root).ok();
}

// ================================================================= directive 5

#[test]
fn pruning_never_produces_a_false_negative_on_randomized_metadata() {
    // **The property that must never break.** Pruning may be too generous -- it may open a part
    // that turns out to contribute nothing, and that costs a scan. It may NEVER be too eager:
    // a part excluded that held a matching row is a lost row, and pruning that can lose a row
    // is not pruning, it is sampling.
    //
    // Randomized metadata, randomized queries, ten thousand times: every part the brute-force
    // truth says could match MUST survive pruning.
    use prism_part::partition::{PartRef, PartitionKey};

    let mut rng = Rng::new(0x5A4B);
    let tenants = ["a", "b", "c", "d"];

    for iter in 0..10_000 {
        let n_parts = 1 + rng.below(6);
        let parts: Vec<PartRef> = (0..n_parts)
            .map(|i| {
                let n_t = 1 + rng.below(3);
                let mut ts: Vec<String> = (0..n_t)
                    .map(|_| tenants[rng.below(tenants.len())].to_string())
                    .collect();
                ts.sort();
                ts.dedup();
                let lo = rng.below(1000) as i64;
                let hi = lo + rng.below(1000) as i64;
                PartRef {
                    part_id: format!("p{i}"),
                    partition: PartitionKey {
                        bucket: Bucket::Shared(rng.below(4) as u32),
                        window: 0,
                        generation: "g".into(),
                    },
                    rows: 1,
                    tenants: ts,
                    time_min: lo,
                    time_max: hi,
                }
            })
            .collect();

        let tenant = tenants[rng.below(tenants.len())];
        let from = if rng.next_f32() < 0.5 {
            Some(rng.below(1200) as i64)
        } else {
            None
        };
        let to = if rng.next_f32() < 0.5 {
            Some(rng.below(2200) as i64)
        } else {
            None
        };

        for p in &parts {
            // The brute-force truth: could this part hold a row for `tenant` in `[from, to]`?
            // A part *could* if it names the tenant and its time range overlaps the query's.
            let overlaps = {
                let lo = from.unwrap_or(i64::MIN);
                let hi = to.unwrap_or(i64::MAX);
                p.tenants.iter().any(|t| t == tenant) && p.time_max >= lo && p.time_min <= hi
            };

            if overlaps {
                assert!(
                    p.may_match(tenant, from, to),
                    "iteration {iter}: pruning EXCLUDED a part that could hold a matching row.\n  \
                     part {:?} tenants={:?} time=[{}, {}]\n  query tenant={tenant} from={from:?} \
                     to={to:?}",
                    p.part_id,
                    p.tenants,
                    p.time_min,
                    p.time_max
                );
            }
        }
    }
}

#[test]
fn partition_metadata_is_untrusted_input() {
    // The S3 fuzz corpus moves down a layer, as planned. Every length, count and offset in the
    // partition extension arrives from a file a stranger may have edited: it must decode, or be
    // refused with an error naming the byte. It may never panic, and it may never allocate on
    // the strength of a number it just read.
    use prism_part::ext::S4Ext;

    let good = S4Ext {
        partition: Some(prism_part::partition::PartitionKey {
            bucket: Bucket::Shared(2),
            window: 1_760_000_000_000,
            generation: "gen0".into(),
        }),
        tenant_stats: vec![prism_part::ext::TenantStats {
            tenant: "alpha".into(),
            rows: 100,
            time_min: 1,
            time_max: 999,
            cost_min: 0.0,
            cost_max: 1.0,
            has_error: true,
            has_success: true,
            attribute_keys: vec!["k1".into(), "k2".into()],
        }],
        promoted: vec![prism_part::ext::PromotedColumn {
            key: "gen_ai.system".into(),
            column: "attr.gen_ai.system".into(),
            type_tag: 1,
        }],
    };
    let bytes = good.encode();
    S4Ext::decode(&bytes).expect("the pristine extension must decode");

    let mut rng = Rng::new(0xBEEF);

    // Byte flips.
    for _ in 0..5000 {
        let mut b = bytes.clone();
        for _ in 0..(1 + rng.below(3)) {
            let at = rng.below(b.len());
            b[at] ^= 1u8 << rng.below(8);
        }
        assert_no_panic(&b);
    }

    // Every truncation length.
    for len in 0..bytes.len() {
        assert_no_panic(&bytes[..len]);
    }

    // Absurd lengths planted at every 4-byte-aligned offset.
    for at in (0..bytes.len().saturating_sub(4)).step_by(4) {
        let mut b = bytes.clone();
        b[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_no_panic(&b);
    }

    // Total garbage.
    for _ in 0..5000 {
        let n = rng.below(256);
        let b: Vec<u8> = (0..n).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
        assert_no_panic(&b);
    }
}

fn assert_no_panic(bytes: &[u8]) {
    use prism_part::ext::S4Ext;
    match S4Ext::decode(bytes) {
        Ok(e) => {
            // If it decoded, it must be coherent: a zone map that cannot be true would prune
            // rows that exist, which is worse than no zone map because it is *trusted*.
            for t in &e.tenant_stats {
                assert!(t.time_min <= t.time_max);
            }
        }
        Err(err) => {
            assert!(
                matches!(err, prism_types::PrismError::Corrupt(_)),
                "expected a specific Corrupt error, got {err:?}"
            );
            assert!(!err.to_string().is_empty());
        }
    }
}

// ================================================================= data skipping

#[test]
fn a_selective_query_reads_only_the_partitions_it_needs() {
    // "Selective benchmarks read only eligible partitions" -- as a fact about the counters, not
    // a claim.
    let root = tmp("skip");
    let scheme = PartitionScheme {
        // One-hour windows, so a day of data becomes many time partitions.
        time_window_ms: 60 * 60 * 1000,
        ..Default::default()
    };
    let engine = Engine::init(&root, config(scheme, vec![])).unwrap();

    // Spread events across many hours and four tenants.
    let mut evs = events(4000, &["alpha", "bravo", "charlie", "delta"], 11);
    for (i, e) in evs.iter_mut().enumerate() {
        e.event_time = 1_760_000_000_000 + (i as i64 % 24) * 60 * 60 * 1000;
    }
    engine.ingest(evs, 1_760_000_000_000).unwrap();

    let total_parts = engine.snapshot().unwrap().parts.len();
    assert!(
        total_parts > 20,
        "need many partitions to skip: {total_parts}"
    );

    // A query for one tenant, in one hour. It should read a tiny fraction of the parts.
    let plan = compile(
        "SELECT count(*) FROM events WHERE event_time >= 1760000000000 \
         AND event_time < 1760003600000",
        &sess("alpha"),
    )
    .unwrap();
    let res = engine.run_sql(&plan, None).unwrap();

    assert_eq!(res.counters.parts_total, total_parts);
    assert!(
        res.counters.parts_opened <= 2,
        "a query for one tenant in one hour opened {} of {total_parts} parts",
        res.counters.parts_opened
    );
    assert!(
        res.counters.parts_pruned >= total_parts - 2,
        "pruning left {} parts unpruned",
        total_parts - res.counters.parts_pruned
    );

    std::fs::remove_dir_all(root).ok();
}
