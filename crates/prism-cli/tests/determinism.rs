//! **The determinism gate (S6): the answer does not depend on which instruction set computed it.**
//!
//! > *"identical event IDs and identical ordering — not epsilon-close scores, identical ANSWERS
//! > — across scalar, AVX2, AVX-512, and NEON on the layout-variant golden fixtures."*
//!
//! This is [D-033](../../../docs/DECISIONS.md) in its float edition: the answer is a function of
//! the data, not of where the data is stored *and not of which CPU ran the query*. An analytical
//! database that answers a question differently on a different machine is not one database.
//!
//! The gate runs every kernel the current machine supports over the same frozen corpus the
//! [C-4 layout gate](../../../docs/DECISIONS.md) uses, and asserts **byte-identical** answers —
//! ids and scores. On aarch64 that is scalar vs NEON; on x86-64 with AVX2 it is scalar vs AVX2;
//! AVX-512 joins when a build enables `experimental-avx512` and a CPU supports it. The kernels
//! not present on this machine are exercised by the *other* machine in CI.

use prism_engine::{oracle, tsv, Engine};
use prism_part::partition::PartitionScheme;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_quantizer::kernel::{self, Isa};
use prism_types::{Event, Query};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

// The ISA ceiling is process-global, so any test that pins it must not run concurrently with
// another that does. One lock serializes them; nothing else in the suite touches the ceiling.
static CEILING_LOCK: Mutex<()> = Mutex::new(());
static N: AtomicU64 = AtomicU64::new(0);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-det-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn config(window_ms: i64) -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 64,
        nlist: 32,
        pq_m: 8,
        seed: 1234,
        kmeans_restarts: 1,
        block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
        partitions: PartitionScheme {
            buckets: 16,
            time_window_ms: window_ms,
            dedicated: Default::default(),
        },
        promote: Vec::new(),
    }
}

fn frozen_corpus() -> Vec<Event> {
    let text = std::fs::read_to_string(repo_root().join("testing/golden/v1/corpus.tsv")).unwrap();
    tsv::parse(&text).unwrap()
}

fn golden() -> oracle::Golden {
    let bytes = std::fs::read(repo_root().join("testing/golden/v1/expected.json")).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Answer every golden query under a forced ISA ceiling, returning (ids, scores) per query and
/// the kernel name that actually ran (so the gate can prove it was not silently scalar every
/// time).
fn answers_under(
    engine: &Engine,
    g: &oracle::Golden,
    ceil: Isa,
) -> (Vec<Vec<(String, f32)>>, String) {
    kernel::set_isa_ceiling(ceil);
    let mut out = Vec::new();
    let mut isa_used = String::new();
    for exp in &g.expectations {
        let r = engine.search(&exp.query.to_query()).unwrap();
        isa_used = r.counters.scan_isa.clone();
        out.push(
            r.hits
                .iter()
                .map(|h| (h.event.event_id.clone(), h.score))
                .collect(),
        );
    }
    kernel::clear_isa_ceiling();
    (out, isa_used)
}

/// **The gate.** Every kernel this machine supports answers every golden query identically —
/// byte for byte, ids and scores — over two physical layouts of the frozen corpus.
#[test]
fn every_kernel_returns_the_same_answer() {
    let _guard = CEILING_LOCK.lock().unwrap();
    let events = frozen_corpus();
    let g = golden();

    let isas = kernel::available();
    // On any real machine there is at least the reference plus one SIMD kernel; a machine with
    // only scalar (some CI containers) still runs the gate, it just has nothing to disagree with,
    // and the OTHER architecture in CI provides the comparison.
    assert!(!isas.is_empty());

    for window in [i64::MAX / 4, 24 * 60 * 60 * 1000] {
        let root = tmp("gate");
        let engine = Engine::init(&root, config(window)).unwrap();
        for (i, chunk) in events.chunks(200).enumerate() {
            engine
                .ingest(chunk.to_vec(), 1_760_000_000_000 + i as i64)
                .unwrap();
        }

        let (reference, ref_isa) = answers_under(&engine, &g, Isa::Scalar);
        assert_eq!(
            ref_isa, "scalar",
            "the scalar ceiling did not force the scalar kernel"
        );

        for &isa in &isas {
            if isa == Isa::Scalar {
                continue;
            }
            let (got, used) = answers_under(&engine, &g, isa);
            // The kernel actually ran -- otherwise this proves nothing. (On aarch64 NEON always
            // runs; on x86 the runner must actually have AVX2.)
            assert_eq!(
                used,
                isa.name(),
                "asked for kernel {} but the scan used {used}",
                isa.name()
            );
            assert_eq!(
                reference,
                got,
                "kernel {} returned a DIFFERENT answer than the scalar reference on the frozen \
                 corpus. The determinism contract requires byte-identical answers -- not \
                 epsilon-close scores, the same ordered event ids and the same scores. A query \
                 that answers differently on a different CPU is not one query.",
                isa.name()
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// **Feature-masking is a gate, not a hope (determinism contract §3).**
///
/// Force the dispatcher down to scalar — as if the CPU had no SIMD at all — and prove the store
/// still answers, and answers *identically* to the SIMD path. "The fallback exists" is worth
/// nothing; "the fallback is correct" is the property.
#[test]
fn masking_the_cpu_forces_the_fallback_and_it_still_answers_identically() {
    let _guard = CEILING_LOCK.lock().unwrap();
    let events = frozen_corpus();
    let g = golden();

    let root = tmp("mask");
    let engine = Engine::init(&root, config(24 * 60 * 60 * 1000)).unwrap();
    for (i, chunk) in events.chunks(200).enumerate() {
        engine
            .ingest(chunk.to_vec(), 1_760_000_000_000 + i as i64)
            .unwrap();
    }

    // The best kernel this machine has, whatever it is.
    let (fast, fast_isa) = answers_under(&engine, &g, kernel::best());
    // Everything masked off: scalar only.
    let (scalar, scalar_isa) = answers_under(&engine, &g, Isa::Scalar);

    assert_eq!(scalar_isa, "scalar");
    assert_eq!(
        fast, scalar,
        "the masked (scalar) fallback returned a different answer than the {fast_isa} kernel. A \
         fallback that answers differently is worse than no fallback, because a machine without \
         SIMD would silently return different results than one with it."
    );
}

/// **The boundary-tie stress corpus (determinism contract §2).**
///
/// The bounded candidate heap *decides* which rows survive the scan, and a one-`ulp` distance
/// disagreement between two kernels would flip that decision — turning a rounding difference into
/// a different answer. Real telemetry repeats bodies verbatim, so exactly-tied and
/// near-tied distances are ordinary, not exotic.
///
/// This corpus is built to be nothing *but* ties: many copies of a handful of bodies, so the
/// candidate width forces the heap to choose among rows at identical distances. If any kernel
/// selected a different set, the answers would diverge here first. (We ship strong-form, so they
/// do not — but this is the corpus a future weak-form kernel must also pass.)
#[test]
fn kernels_agree_even_when_the_scan_is_all_ties() {
    let _guard = CEILING_LOCK.lock().unwrap();

    // 12 distinct bodies, each repeated many times: exact-duplicate vectors, exact-tie distances.
    let bodies = [
        "the tool call timed out",
        "connection reset by peer",
        "rate limit exceeded retry after 30s",
        "model returned an empty completion",
        "invalid json in tool arguments",
        "context length exceeded",
        "authentication failed for api key",
        "the request was cancelled by the user",
        "downstream service unavailable 503",
        "token budget exhausted mid stream",
        "unexpected end of stream",
        "safety filter blocked the response",
    ];
    let events: Vec<Event> = (0..3_000)
        .map(|i| {
            let mut e =
                prism_engine::corpus::generate(prism_engine::corpus::Kind::Zipf, 1, i as u64)[0]
                    .clone();
            e.tenant_id = "alpha".into();
            e.event_id = format!("e{i:06}");
            e.body = bodies[i % bodies.len()].to_string();
            e
        })
        .collect();

    let root = tmp("ties");
    let engine = Engine::init(&root, config(24 * 60 * 60 * 1000)).unwrap();
    for chunk in events.chunks(250) {
        engine.ingest(chunk.to_vec(), 1_760_000_000_000).unwrap();
    }

    let q = Query {
        text: "the tool call timed out".into(),
        // A width far smaller than the number of tied rows, so the heap MUST choose among ties.
        k: 25,
        candidates: 40,
        rerank: 40,
        nprobe: 8,
        tenant: Some("alpha".into()),
        ..Default::default()
    };

    let ids = |ceil: Isa| -> Vec<(String, f32)> {
        kernel::set_isa_ceiling(ceil);
        let r = engine.search(&q).unwrap();
        kernel::clear_isa_ceiling();
        r.hits
            .into_iter()
            .map(|h| (h.event.event_id, h.score))
            .collect()
    };

    let reference = ids(Isa::Scalar);
    assert_eq!(reference.len(), 25);
    for &isa in &kernel::available() {
        assert_eq!(
            reference,
            ids(isa),
            "kernel {} selected a different set of tied rows than scalar. When every candidate is \
             at the same distance, SELECTION is decided entirely by the tie-break, and a kernel \
             that disagrees here has turned a rounding difference into a different answer.",
            isa.name()
        );
    }
}
