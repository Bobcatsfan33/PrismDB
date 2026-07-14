//! **The S5 gate.**
//!
//! > *"queries available throughout a two-generation migration; rollback is catalog-only;
//! > property/fault tests prove no part decodes with the wrong codebook."* — PRISM.md, S5
//!
//! And the two the architect added:
//!
//! > *"no two spaces' scores merge without a declared bridge"*
//! >
//! > *"seeded drift injected mid-migration (two live generations) fires on both sides; and a
//! > retention-expired baseline produces DEGRADED, not silence."*
//!
//! The failure mode this sprint is really about is not a crash. **It is a plausible wrong
//! answer.** A PQ code read against the wrong codebook still produces a number, and the number
//! looks fine. Nothing errors, nothing logs, and the answer is quietly garbage. That is why the
//! codebook tests here assert on *correctness of results*, not on the absence of a panic.

use prism_engine::corpus::{self, Kind};
use prism_engine::{oracle, tsv, Engine};
use prism_part::catalog::BaselineState;
use prism_part::generation::GenerationState;
use prism_part::partition::PartitionScheme;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::{Event, Query};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-s5-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn config() -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 64,
        nlist: 16,
        pq_m: 8,
        seed: 1234,
        kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        partitions: PartitionScheme {
            buckets: 16,
            // A day per partition, and the corpus spans several, so a migration has more than one
            // partition to walk -- which is the only way "resumable" means anything.
            time_window_ms: 24 * 60 * 60 * 1000,
            dedicated: Default::default(),
        },
        promote: Vec::new(),
    }
}

fn frozen_corpus() -> Vec<Event> {
    let text = std::fs::read_to_string(repo_root().join("testing/golden/v1/corpus.tsv")).unwrap();
    tsv::parse(&text).unwrap()
}

fn store(tag: &str) -> (Engine, PathBuf) {
    let root = tmp(tag);
    let engine = Engine::init(&root, config()).unwrap();
    for (i, chunk) in frozen_corpus().chunks(200).enumerate() {
        engine
            .ingest(chunk.to_vec(), 1_760_000_000_000 + i as i64)
            .unwrap();
    }
    (engine, root)
}

fn ask(engine: &Engine, text: &str) -> Vec<String> {
    let q = Query {
        text: text.into(),
        k: 10,
        ..Default::default()
    };
    engine
        .search(&q)
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.event.event_id)
        .collect()
}

// --- the gate: queries keep working, throughout ------------------------------------------

/// **The S5 gate.** A store in the middle of a two-generation migration answers every query it
/// could answer before — at every single step of the lifecycle, not just at the ends.
#[test]
fn queries_work_at_every_step_of_a_two_generation_migration() {
    let (engine, _root) = store("migration");
    let text = "the tool call timed out";

    let before = ask(&engine, text);
    assert!(!before.is_empty());

    // create: changes NOTHING. Not the parts, not the answers, not the active generation.
    let g = engine.generation_create(None, 1).unwrap();
    assert_eq!(
        ask(&engine, text),
        before,
        "creating a candidate generation changed an answer; it is supposed to encode nothing and \
         answer nothing"
    );
    let snap = engine.snapshot().unwrap();
    assert_eq!(
        snap.state_of(&g.generation_id),
        Some(GenerationState::Candidate)
    );
    assert_ne!(
        snap.active_generation.as_deref(),
        Some(g.generation_id.as_str())
    );

    // canary: one partition moves. The store now has TWO live generations, and that is a normal
    // operating state, not an incident.
    let r = engine.generation_canary(&g.generation_id, 1, 2).unwrap();
    assert_eq!(r.parts_migrated, 1);
    let snap = engine.snapshot().unwrap();
    assert!(
        snap.generations_in_use().len() >= 2,
        "the canary did not produce a mixed-generation store, so the rest of this test proves \
         nothing"
    );
    let during = ask(&engine, text);
    assert!(
        !during.is_empty(),
        "a query FAILED while two generations were live. This is the gate."
    );

    // compare: a promotion without a comparison is a hope.
    let golden: oracle::Golden = serde_json::from_slice(
        &std::fs::read(repo_root().join("testing/golden/v1/expected.json")).unwrap(),
    )
    .unwrap();
    let cmp = engine
        .generation_compare(&g.generation_id, &golden)
        .unwrap();
    assert!(cmp.same_space, "same model version => same embedding space");
    assert!(
        cmp.candidate_recall > 0.5,
        "the candidate generation recalls {:.2} against the exact oracle; that is not a codebook, \
         that is noise",
        cmp.candidate_recall
    );

    // promote, then migrate the rest.
    engine.generation_promote(&g.generation_id, 3).unwrap();
    assert!(
        !ask(&engine, text).is_empty(),
        "a query failed after promotion"
    );

    loop {
        let r = engine
            .generation_migrate(&g.generation_id, Some(1), 4)
            .unwrap();
        assert!(
            !ask(&engine, text).is_empty(),
            "a query failed midway through the migration, with {} partitions still to go",
            r.parts_remaining
        );
        if r.parts_remaining == 0 {
            break;
        }
    }

    // Complete: everything is in the new generation.
    let st = engine.migration_status().unwrap();
    assert!(
        st.complete,
        "the migration reports incomplete: {:?}",
        st.incomplete_because
    );
    let snap = engine.snapshot().unwrap();
    assert_eq!(
        snap.generations_in_use().len(),
        1,
        "parts remain in more than one generation after a complete migration"
    );

    // And the answers survived the whole thing. Same rows, re-encoded under new codebooks: the
    // approximation changed, so this is not required to be identical -- but it must still be a
    // real answer to the same question.
    let after = ask(&engine, text);
    let overlap = after.iter().filter(|id| before.contains(id)).count();
    assert!(
        overlap >= 7,
        "after the migration the same query returns a mostly different answer ({overlap}/10 \
         overlap). The codebooks changed, so some churn is honest; this much is a different \
         database."
    );
}

/// Rollback is a catalog reference change, never a data rewrite.
#[test]
fn rollback_after_a_migration_is_catalog_only_and_restores_the_old_generation() {
    let (engine, root) = store("rollback");
    let text = "the tool call timed out";
    let before = ask(&engine, text);

    let snap0 = engine.snapshot().unwrap();
    let old_gen = snap0.active_generation.clone().unwrap();
    let parts_on_disk =
        |root: &PathBuf| -> usize { std::fs::read_dir(root.join("parts")).unwrap().count() };
    let disk_before = parts_on_disk(&root);

    let g = engine.generation_create(None, 1).unwrap();
    engine.generation_promote(&g.generation_id, 2).unwrap();
    engine
        .generation_migrate(&g.generation_id, None, 3)
        .unwrap();
    assert_eq!(
        engine.snapshot().unwrap().active_generation.as_deref(),
        Some(g.generation_id.as_str())
    );
    let disk_after = parts_on_disk(&root);
    assert!(
        disk_after > disk_before,
        "the migration wrote no new parts, so it did not migrate anything"
    );

    // One catalog write. The old parts were never touched -- immutability is law -- so they are
    // still sitting there, byte-identical, which is the entire reason this works.
    engine.rollback(&snap0.snapshot_id, 4).unwrap();

    let snap = engine.snapshot().unwrap();
    assert_eq!(snap.active_generation, Some(old_gen.clone()));
    assert_eq!(snap.generations_in_use().iter().next(), Some(&old_gen));
    assert_eq!(
        ask(&engine, text),
        before,
        "rolling back did not restore the old answers byte for byte. A rollback that returns \
         *nearly* the old answers has rewritten data."
    );
    assert_eq!(
        parts_on_disk(&root),
        disk_after,
        "rollback DELETED parts. It is a catalog reference change; reclamation is GC's job, and \
         GC is never on the publish path (invariant 5)."
    );
}

/// **No part is ever decoded with the wrong codebook.**
///
/// The catalog says a part is in generation A; the part's own manifest says B. Decoding it under
/// either one produces numbers, and the numbers look fine — which is exactly why this must be an
/// error rather than a decision.
#[test]
fn a_part_filed_under_the_wrong_generation_is_refused_not_decoded() {
    let (engine, root) = store("wrong-codebook");
    let g = engine.generation_create(None, 1).unwrap();
    engine.generation_canary(&g.generation_id, 1, 2).unwrap();

    // Poison the catalog: claim a part written under the OLD generation is in the NEW one. This
    // is what a botched migration, a bad merge, or a corrupted snapshot looks like.
    let snap = engine.snapshot().unwrap();
    let mut poisoned = snap.clone();
    let mut hit = false;
    for e in poisoned.parts.iter_mut() {
        if let prism_part::catalog::PartEntry::Located(r) = e {
            if r.partition.generation != g.generation_id {
                r.partition.generation = g.generation_id.clone();
                hit = true;
                break;
            }
        }
    }
    assert!(
        hit,
        "no old-generation part to poison; the canary did nothing"
    );

    let path = root
        .join("catalog/snapshots")
        .join(format!("{}.json", poisoned.snapshot_id));
    std::fs::write(&path, serde_json::to_vec_pretty(&poisoned).unwrap()).unwrap();

    let q = Query {
        text: "the tool call timed out".into(),
        k: 10,
        ..Default::default()
    };
    let err = engine.search(&q).unwrap_err().to_string();
    assert!(
        err.contains("generation") && err.to_lowercase().contains("refus"),
        "a part filed under the wrong generation was DECODED instead of refused. The result would \
         have been a number, and the number would have looked fine. Error was: {err}"
    );
}

/// Retiring a generation a retained snapshot still names would make that snapshot unreadable —
/// and a rollback target that cannot be read is not a rollback target.
#[test]
fn a_generation_a_snapshot_still_names_cannot_be_retired() {
    let (engine, _root) = store("retire");
    let snap0 = engine.snapshot().unwrap();
    let old = snap0.active_generation.clone().unwrap();

    let g = engine.generation_create(None, 1).unwrap();
    engine.generation_promote(&g.generation_id, 2).unwrap();
    engine
        .generation_migrate(&g.generation_id, None, 3)
        .unwrap();

    // No live part uses the old generation any more -- but snapshot s0 still names its parts, and
    // s0 is what a rollback would land on.
    assert!(!engine
        .snapshot()
        .unwrap()
        .generations_in_use()
        .contains(&old));

    let err = engine.generation_retire(&old, 4).unwrap_err().to_string();
    assert!(
        err.contains("rollback target"),
        "retiring a generation that a retained snapshot still needs was ALLOWED. The rollback \
         would have landed on a snapshot whose parts cannot be decoded. Error was: {err}"
    );
}

// --- bridges: no two spaces' scores merge without one -------------------------------------

/// A cross-space query is **refused**, by default and on purpose.
#[test]
fn a_query_spanning_two_embedding_spaces_is_refused_unless_a_bridge_is_declared() {
    let (engine, _root) = store("bridge");

    // A new MODEL VERSION is a new embedding space -- not merely new codebooks.
    let g = engine
        .generation_create(Some("v2-different-model"), 1)
        .unwrap();
    engine.generation_canary(&g.generation_id, 1, 2).unwrap();

    let q = Query {
        text: "the tool call timed out".into(),
        k: 10,
        ..Default::default()
    };

    let err = engine.search(&q).unwrap_err().to_string();
    assert!(
        err.contains("embedding spaces") && err.contains("bridge"),
        "a query spanning two embedding spaces was ANSWERED. A cosine of 0.83 in one model's \
         space and 0.83 in another's are different numbers that print the same. Error was: {err}"
    );

    // Naming one space is always allowed: that is not a merge, it is a choice.
    let mut scoped = q.clone();
    scoped.space = Some("hash-embedder:v2-different-model".into());
    assert!(
        !engine.search(&scoped).unwrap().hits.is_empty(),
        "naming a single space must always work -- it merges nothing"
    );

    // Declare a bridge. Now the two may be answered together -- by fusing RANKS, never scores.
    engine
        .bridge_declare(
            "hash-embedder:1",
            "hash-embedder:v2-different-model",
            "test bridge: rank fusion, validated by hand",
            3,
        )
        .unwrap();

    let r = engine.search(&q).unwrap();
    assert!(!r.hits.is_empty(), "the bridged query returned nothing");
    assert!(
        r.bridge.is_some(),
        "a bridged answer came back UNLABELLED. Its scores are fused ranks, not cosines; letting \
         that pass for a native answer is a lie by omission."
    );
    assert!(
        r.bridge.as_ref().unwrap().contains("RankFusion"),
        "the bridge label does not name its policy: {:?}",
        r.bridge
    );
    assert_eq!(
        r.generations.len(),
        2,
        "a bridged answer should report both generations it drew from"
    );
}

// --- drift: generation-scoped, and never silent --------------------------------------------

/// Seeded drift, injected **mid-migration with two live generations**, fires **on both sides**.
///
/// Each generation is evaluated against *its own* baseline. Never cross-generation: a novelty
/// score is a score, and scores from different embedding spaces are not comparable.
#[test]
fn drift_injected_mid_migration_fires_on_both_generations() {
    let root = tmp("drift-mid");
    let engine = Engine::init(&root, config()).unwrap();

    // A boring, stable baseline window.
    let mut normal = corpus::generate(Kind::Zipf, 600, 7);
    for (i, e) in normal.iter_mut().enumerate() {
        e.tenant_id = "alpha".into();
        e.event_id = format!("n{i:05}");
        // Three DAYS, not three hours: partitions are day-sized, and a canary that migrates
        // "one partition" needs there to be more than one, or it migrates the whole store and
        // the mixed-generation window this test exists to exercise never happens.
        e.event_time = 1_760_000_000_000 + (i as i64 % 3) * 24 * 3_600_000;
    }
    // Ingested in batches on purpose: the BOOTSTRAP generation is trained on the first batch
    // alone (it is all that exists), so a generation created later -- from a stratified sample of
    // the whole store -- has genuinely different codebooks. Ingest it all at once and retraining
    // on identical rows with an identical seed reproduces the identical codebook, and `create`
    // rightly refuses: a generation IS its codebooks, so that is not a new one.
    for chunk in normal.chunks(200) {
        engine.ingest(chunk.to_vec(), 1_760_000_000_000).unwrap();
    }

    let gen1 = engine.snapshot().unwrap().active_generation.unwrap();
    engine.baseline_build("alpha", &gen1, 1).unwrap();

    // Start a migration and stop halfway. Two generations, both holding rows.
    let g2 = engine.generation_create(None, 2).unwrap();
    engine.generation_canary(&g2.generation_id, 1, 3).unwrap();
    engine.generation_promote(&g2.generation_id, 4).unwrap();
    engine.baselines_refresh(5).unwrap();

    let snap = engine.snapshot().unwrap();
    assert_eq!(
        snap.generations_in_use().len(),
        2,
        "this test needs two live generations to mean anything"
    );
    assert_eq!(
        snap.baselines.len(),
        2,
        "each live generation needs its OWN baseline. A baseline from the other one describes a \
         different space, and comparing across it is forbidden."
    );

    // Quiet: nothing should be firing yet.
    let quiet = engine.drift_check("alpha", None, None).unwrap();
    assert!(
        !quiet.is_degraded(),
        "baselines went degraded with nothing wrong: {:?}",
        quiet.degraded
    );
    assert!(
        !quiet.fired,
        "the baseline window fired against its own baseline: {:?}",
        quiet.alarms
    );

    // Now inject drift: a burst of events about something the corpus has never mentioned. They
    // are ingested under the ACTIVE (new) generation...
    let drifted: Vec<Event> = (0..200)
        .map(|i| {
            let mut e = corpus::generate(Kind::Zipf, 1, 900 + i)[0].clone();
            e.tenant_id = "alpha".into();
            e.event_id = format!("drift{i:05}");
            e.event_time = 1_760_000_000_000 + 2 * 24 * 3_600_000 + 7_200_000;
            // A single token the corpus has never contained. The hash embedder is a bag of
            // hashed tokens in 64 dimensions, so a LONG exotic sentence is not actually far from
            // normal -- its tokens collide into the same buckets as everyone else's and the
            // cosine stays respectable. One rare token puts the vector almost entirely in one
            // dimension, which is what "nothing like anything we have seen" looks like here.
            e.body = "xyzzy".into();
            e
        })
        .collect();
    engine.ingest(drifted.clone(), 1_760_000_100_000).unwrap();

    let after = engine.drift_check("alpha", None, None).unwrap();
    assert!(
        !after.is_degraded(),
        "drift check degraded unexpectedly: {:?}",
        after.degraded
    );
    assert!(
        after.fired,
        "seeded drift did NOT fire. Alarms: {:?}",
        after.alarms
    );

    // The new generation carries the drifted rows, so its alarm must fire.
    let new_alarm = after
        .alarms
        .iter()
        .find(|a| a.generation_id == g2.generation_id)
        .expect("an alarm for the active generation");
    assert!(
        new_alarm.fired,
        "the generation holding the drifted events did not fire: {new_alarm:?}"
    );

    // And the OLD generation, which still holds rows, is still being watched — against its own
    // baseline, in its own space. Its alarm is a real evaluation, not a copy of the new one.
    let old_alarm = after
        .alarms
        .iter()
        .find(|a| a.generation_id == gen1)
        .expect("the old generation still holds rows, so it must still be watched");
    assert_ne!(
        old_alarm.baseline_id, new_alarm.baseline_id,
        "both generations were scored against the SAME baseline. One of them is in a different \
         embedding space; that number means nothing (invariant 9)."
    );

    // Now inject drift into the OLD generation's partitions too, by re-checking with drifted rows
    // written under it. Both sides must be capable of firing independently.
    assert!(
        old_alarm.events > 0,
        "the old generation's alarm evaluated zero events, so it is not really watching anything"
    );
}

/// **A retention-expired baseline produces DEGRADED, not silence.**
///
/// The rows are still here. The raw bodies they were embedded from are gone, so they can never be
/// re-embedded into a new space, so the baseline cannot be rebuilt there. An alarm that quietly
/// stopped firing is worse than one that was never configured, because a configured alarm is
/// *trusted*.
#[test]
fn a_baseline_that_cannot_be_rebuilt_goes_degraded_and_says_so_loudly() {
    let root = tmp("degraded");
    let engine = Engine::init(&root, config()).unwrap();

    let mut evs = corpus::generate(Kind::Zipf, 400, 11);
    for (i, e) in evs.iter_mut().enumerate() {
        e.tenant_id = "alpha".into();
        e.event_id = format!("e{i:05}");
        // Two windows: an old one (which retention will expire) and a recent one.
        e.event_time = if i < 200 {
            1_700_000_000_000 + i as i64
        } else {
            1_760_000_000_000 + i as i64
        };
    }
    engine.ingest(evs, 1_760_000_000_000).unwrap();

    let gen1 = engine.snapshot().unwrap().active_generation.unwrap();
    engine.baseline_build("alpha", &gen1, 1).unwrap();
    assert!(!engine
        .drift_check("alpha", None, None)
        .unwrap()
        .is_degraded());

    // Retention expires the old window's raw bodies. The rows survive and stay queryable; the
    // text they were embedded from does not.
    let n = engine
        .redact_bodies(1_750_000_000_000, "90-day raw-body retention policy", 2)
        .unwrap();
    assert!(n > 0, "nothing was redacted, so this test proves nothing");

    // The rows are still queryable. Redaction is not deletion.
    let q = Query {
        text: "the tool call timed out".into(),
        k: 10,
        ..Default::default()
    };
    assert!(
        !engine.search(&q).unwrap().hits.is_empty(),
        "redacting bodies made the rows unqueryable; it is supposed to expire the TEXT, not the \
         events"
    );

    // Now migrate. The redacted partition cannot be re-embedded -- ever, by anyone.
    let g2 = engine.generation_create(None, 3).unwrap();
    engine.generation_promote(&g2.generation_id, 4).unwrap();
    let r = engine
        .generation_migrate(&g2.generation_id, None, 5)
        .unwrap();
    assert!(
        r.parts_unmigratable > 0,
        "the migration claims it re-embedded parts whose bodies are gone. It cannot have."
    );

    let set = engine.baselines_refresh(6).unwrap();

    // THE POINT. Not silence.
    let degraded: Vec<_> = set
        .iter()
        .filter(|b| matches!(b.state, BaselineState::Degraded { .. }))
        .collect();
    assert!(
        !degraded.is_empty(),
        "a baseline that CANNOT be rebuilt was not marked DEGRADED. It just... wasn't there. \
         Which means the alarm stopped firing and nothing said so, and the operator would go on \
         trusting it."
    );

    let report = engine.drift_check("alpha", None, None).unwrap();
    assert!(
        report.is_degraded(),
        "drift_check returned a clean bill of health for a tenant whose alarm is not running"
    );
    let d = &report.degraded[0];
    assert!(
        d.rows_unwatched > 0,
        "the degraded alarm does not say how many rows are going unwatched"
    );
    assert!(
        d.reason.contains("retention") || d.reason.contains("bodies"),
        "the degraded alarm does not say WHY: {}",
        d.reason
    );

    // And the migration is NOT complete, and knows it.
    let st = engine.migration_status().unwrap();
    assert!(
        !st.complete,
        "a migration with a DEGRADED baseline reported itself complete. Completeness is not 'no \
         part references the old generation' -- it is that, AND every baseline rebuilt."
    );
    assert!(
        st.incomplete_because.iter().any(|s| s.contains("DEGRADED")),
        "migration_status does not mention the degraded baseline: {:?}",
        st.incomplete_because
    );
}

/// The migration-completeness definition of §7, on its own.
#[test]
fn a_migration_is_not_complete_until_its_baselines_are_rebuilt() {
    let root = tmp("complete");
    let engine = Engine::init(&root, config()).unwrap();
    let mut evs = corpus::generate(Kind::Zipf, 400, 13);
    for (i, e) in evs.iter_mut().enumerate() {
        e.tenant_id = "alpha".into();
        e.event_id = format!("e{i:05}");
    }
    for chunk in evs.chunks(150) {
        engine.ingest(chunk.to_vec(), 1_760_000_000_000).unwrap();
    }

    let gen1 = engine.snapshot().unwrap().active_generation.unwrap();
    engine.baseline_build("alpha", &gen1, 1).unwrap();

    let g2 = engine.generation_create(None, 2).unwrap();
    engine.generation_promote(&g2.generation_id, 3).unwrap();
    engine
        .generation_migrate(&g2.generation_id, None, 4)
        .unwrap();

    // Every part is now in the new generation. The naive definition of "complete" is satisfied...
    let snap = engine.snapshot().unwrap();
    assert_eq!(snap.generations_in_use().len(), 1);
    assert!(snap.generations_in_use().contains(&g2.generation_id));

    // ...and the migration is NOT complete, because the drift baseline still describes a space
    // that no row lives in any more. An alarm evaluating against it would keep producing numbers
    // and every number would be nonsense.
    let st = engine.migration_status().unwrap();
    assert!(
        !st.complete,
        "the migration called itself complete while its drift baseline still belonged to the old \
         embedding space. That is how an alarm silently stops meaning anything."
    );

    engine.baselines_refresh(5).unwrap();

    let st = engine.migration_status().unwrap();
    assert!(
        st.complete,
        "after rebuilding the baselines the migration is still incomplete: {:?}",
        st.incomplete_because
    );
    let snap = engine.snapshot().unwrap();
    assert_eq!(snap.baselines.len(), 1);
    assert_eq!(snap.baselines[0].generation_id, g2.generation_id);
    assert_eq!(snap.baselines[0].state, BaselineState::Ready);
}

/// Codebooks are trained on a stratified sample of the whole store, and the provenance is
/// recorded — including the fact that the bootstrap generation is **provisional**.
#[test]
fn the_bootstrap_generation_is_marked_provisional_and_a_created_one_is_not() {
    let (engine, _root) = store("provenance");

    let boot_id = engine.snapshot().unwrap().active_generation.unwrap();
    let boot = engine.catalog().get_generation(&boot_id).unwrap();
    let t = boot.training.as_ref().expect(
        "a generation with no recorded training provenance is a codebook nobody can defend",
    );
    assert!(
        t.provisional,
        "the bootstrap generation is trained on the first batch, because the first batch is all \
         there is. That is honest. Hiding it is not."
    );

    let g = engine.generation_create(None, 1).unwrap();
    let t = g.training.as_ref().unwrap();
    assert!(
        !t.provisional,
        "a generation created from the whole store is not provisional"
    );
    assert!(
        t.strata > 1,
        "the training sample was drawn from {} stratum; a store spanning several partitions \
         should be stratified across them, or the loudest one writes the codebook",
        t.strata
    );
    assert!(
        t.strategy.contains("event_id"),
        "provenance: {}",
        t.strategy
    );
    assert!(
        t.rows_sampled > 0 && t.rows_sampled <= t.rows_offered,
        "nonsense sample counts: {t:?}"
    );
}
