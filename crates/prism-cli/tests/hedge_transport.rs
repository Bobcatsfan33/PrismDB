//! **The S12 latency/jitter/async-hedge campaign** — D-079's timing, finally honoured for real
//! ([D-098](../../../docs/DECISIONS.md), [query §21](../../../docs/QUERY-CONTRACT.md)).
//!
//! The in-process gate (`hedging.rs`) proves the hedge *semantics* through a seam, because a
//! synchronous coordinator has no latency to race. This binary proves the *timing* over the real
//! mutual-TLS transport: real `ShardRpcServer`s on loopback TCP, a deterministic server-side
//! fragment delay (the jitter), and the remote coordinator's per-query hedged client racing a
//! genuine straggler on a fresh connection. Four properties, each a phase of one test (the delay
//! and cap seams are process-global, so this is its own binary and one serial test):
//!
//! 1. un-jittered, the remote answer is byte-identical to the in-process cluster and **no hedge
//!    is ever issued** — hedging adds no load in the common case;
//! 2. with an injected tail and hedging suppressed, every answer is *still* byte-identical —
//!    jitter is a latency event, never a correctness event — and slow;
//! 3. with hedging live, the same jitter schedule is **at least twice as fast at the median**,
//!    hedges are actually issued, and every answer is still byte-identical — a hedge changes
//!    latency, never the answer;
//! 4. a duplicate landing inside the dedup window is **absorbed and compared bit-for-bit**
//!    (observable), and the blast-radius cap suppresses hedging rather than letting a slow
//!    cluster amplify itself.
//!
//! The measured receipt (`testing/evidence/s12-hedge-campaign.json`) is written by the ignored
//! release-mode variant below, the same convention as `scaling.rs`.

use prism_engine::shard_rpc::{
    client_tls_from_pem, rpc_fault, server_tls_from_pem, RemoteReadCluster, RemoteReadTopology,
    RemoteShardEndpoint, ShardRpcServer,
};
use prism_engine::sharded::{inject_max_inflight, Cluster};
use prism_engine::Engine;
use prism_part::partition::PartitionScheme;
use prism_part::store::{StoreConfig, STORE_VERSION};
use prism_types::{Event, Query, SearchResult};
use rustls::{ClientConfig, ServerConfig};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static N: AtomicU64 = AtomicU64::new(0);
const TS: i64 = 1_760_000_000_000;

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!(
        "prism-hedge-tp-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

// ---------------------------------------------------------------- TLS fixture
//
// The same two-CA discipline the deployment contract requires (D-088): the shard's server
// identity and the coordinator's client identity come from *separate* roots. Key generation
// pins `ec_param_enc:named_curve` and retries short scalars, for the reasons recorded in
// `shard_distribution.rs` (issue #34).

fn openssl(dir: &Path, args: &[&str]) {
    let output = Command::new("openssl")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run openssl");
    assert!(
        output.status.success(),
        "openssl {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

const KEY_ATTEMPTS: usize = 8;

fn openssl_key(dir: &Path, key_file: &str, args: &[&str]) {
    for _ in 0..KEY_ATTEMPTS {
        openssl(dir, args);
        let pem = std::fs::read_to_string(dir.join(key_file)).expect("fixture key readable");
        if prism_part::testkeys::is_ring_compatible_p256(&pem) {
            return;
        }
    }
    panic!("fixture key {key_file} still had a short private scalar after {KEY_ATTEMPTS} attempts");
}

fn generate_ca(dir: &Path, prefix: &str) {
    openssl_key(
        dir,
        &format!("{prefix}-key.pem"),
        &[
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-pkeyopt",
            "ec_param_enc:named_curve",
            "-nodes",
            "-days",
            "3650",
            "-sha256",
            "-subj",
            &format!("/CN={prefix}"),
            "-keyout",
            &format!("{prefix}-key.pem"),
            "-out",
            &format!("{prefix}.pem"),
        ],
    );
}

fn generate_leaf(dir: &Path, prefix: &str, common_name: &str, ca_prefix: &str, usage: &str) {
    openssl_key(
        dir,
        &format!("{prefix}-key.pem"),
        &[
            "req",
            "-new",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-pkeyopt",
            "ec_param_enc:named_curve",
            "-nodes",
            "-sha256",
            "-subj",
            &format!("/CN={common_name}"),
            "-keyout",
            &format!("{prefix}-key.pem"),
            "-out",
            &format!("{prefix}.csr"),
        ],
    );
    std::fs::write(
        dir.join(format!("{prefix}.ext")),
        format!(
            "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n\
             extendedKeyUsage={usage}\nsubjectAltName=DNS:{common_name}\n"
        ),
    )
    .unwrap();
    openssl(
        dir,
        &[
            "x509",
            "-req",
            "-in",
            &format!("{prefix}.csr"),
            "-CA",
            &format!("{ca_prefix}.pem"),
            "-CAkey",
            &format!("{ca_prefix}-key.pem"),
            "-CAserial",
            &format!("{ca_prefix}.srl"),
            "-CAcreateserial",
            "-days",
            "3650",
            "-sha256",
            "-extfile",
            &format!("{prefix}.ext"),
            "-out",
            &format!("{prefix}.pem"),
        ],
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dir.join(format!("{prefix}-key.pem")),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
}

fn tls_pair(dir: &Path) -> (Arc<ServerConfig>, Arc<ClientConfig>) {
    generate_ca(dir, "shard-ca");
    generate_ca(dir, "coordinator-ca");
    generate_leaf(dir, "shard", "shard.test", "shard-ca", "serverAuth");
    generate_leaf(
        dir,
        "coordinator",
        "coordinator.test",
        "coordinator-ca",
        "clientAuth",
    );
    let server = server_tls_from_pem(
        &dir.join("shard.pem"),
        &dir.join("shard-key.pem"),
        &dir.join("coordinator-ca.pem"),
    )
    .unwrap();
    let client = client_tls_from_pem(
        &dir.join("coordinator.pem"),
        &dir.join("coordinator-key.pem"),
        &dir.join("shard-ca.pem"),
    )
    .unwrap();
    (server, client)
}

// ---------------------------------------------------------------- store fixture

fn config() -> StoreConfig {
    StoreConfig {
        format_version: STORE_VERSION,
        dim: 8,
        nlist: 2,
        pq_m: 2,
        seed: 42,
        kmeans_restarts: 2,
        block_size: 4096,
        partitions: PartitionScheme::default(),
        promote: Vec::new(),
    }
}

fn event_for(id: &str, tenant: &str, body: &str) -> Event {
    Event {
        event_id: id.into(),
        tenant_id: tenant.into(),
        event_time: 1,
        observed_time: 1,
        event_name: "test".into(),
        cost: 1.0,
        error: false,
        body: body.into(),
        trace_id: String::new(),
        span_id: String::new(),
        attributes: Default::default(),
        idempotency_key: None,
    }
}

fn query() -> Query {
    Query {
        text: "payment service timeout".into(),
        k: 8,
        candidates: 32,
        rerank: 32,
        nprobe: 2,
        ..Query::default()
    }
}

fn fp(result: &SearchResult) -> Vec<(String, u32)> {
    result
        .hits
        .iter()
        .map(|hit| (hit.event.event_id.clone(), hit.score.to_bits()))
        .collect()
}

/// A served shard: the shutdown flag and the join handle, so teardown is explicit.
struct Served {
    shutdown: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<prism_types::error::Result<()>>,
    address: SocketAddr,
}

fn serve(shard_id: usize, tls: Arc<ServerConfig>, engine: Arc<Engine>) -> Served {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = ShardRpcServer::new(shard_id, engine, tls, Duration::from_secs(5)).unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let serving = Arc::clone(&shutdown);
    let handle = std::thread::spawn(move || server.serve_with_shutdown(listener, serving));
    Served {
        shutdown,
        handle,
        address,
    }
}

struct Campaign {
    expected: SearchResult,
    remote: RemoteReadCluster,
    servers: Vec<Served>,
}

fn campaign_fixture(root: &Path) -> Campaign {
    let cluster = Cluster::init(root, 2, config()).unwrap();
    let tenant0 = (0..10_000)
        .map(|index| format!("tenant-{index}"))
        .find(|tenant| cluster.shard_index(tenant) == 0)
        .unwrap();
    let tenant1 = (0..10_000)
        .map(|index| format!("tenant-{index}"))
        .find(|tenant| cluster.shard_index(tenant) == 1)
        .unwrap();
    let mut events = Vec::new();
    for i in 0..30 {
        events.push(event_for(
            &format!("a{i:03}"),
            &tenant0,
            if i % 3 == 0 {
                "payment service timeout retrying"
            } else {
                "checkout flow healthy"
            },
        ));
        events.push(event_for(
            &format!("b{i:03}"),
            &tenant1,
            if i % 3 == 0 {
                "payment service timeout escalated"
            } else {
                "search latency nominal"
            },
        ));
    }
    cluster.ingest(events, TS).unwrap();
    let expected = cluster.search(&query()).unwrap();
    assert!(
        !expected.hits.is_empty(),
        "the fixture corpus must produce a non-empty cross-shard answer"
    );
    drop(cluster);

    let (server_tls, client_tls) = tls_pair(root);
    let shard0 = Arc::new(Engine::open(&root.join("shard-0")).unwrap());
    let shard1 = Arc::new(Engine::open(&root.join("shard-1")).unwrap());
    let servers = vec![
        serve(0, Arc::clone(&server_tls), shard0),
        serve(1, server_tls, shard1),
    ];
    let topology = RemoteReadTopology {
        version: 1,
        shards: servers
            .iter()
            .enumerate()
            .map(|(shard_id, served)| RemoteShardEndpoint {
                shard_id,
                address: served.address.to_string(),
                server_name: "shard.test".into(),
            })
            .collect(),
    };
    let remote = RemoteReadCluster::connect(topology, client_tls, Duration::from_secs(5)).unwrap();
    Campaign {
        expected,
        remote,
        servers,
    }
}

fn teardown(servers: Vec<Served>) {
    for served in &servers {
        served.shutdown.store(true, Ordering::SeqCst);
    }
    for served in servers {
        served.handle.join().expect("server thread").unwrap();
    }
}

/// Run `runs` queries, asserting each answer byte-identical to `expected`; return per-query wall
/// times and the total hedges issued.
fn measure(
    remote: &RemoteReadCluster,
    expected: &SearchResult,
    runs: usize,
) -> (Vec<Duration>, usize) {
    let mut times = Vec::with_capacity(runs);
    let mut hedges = 0usize;
    for _ in 0..runs {
        let started = Instant::now();
        let result = remote.search(&query()).unwrap();
        times.push(started.elapsed());
        assert_eq!(
            fp(&result),
            fp(expected),
            "a transport answer diverged — jitter and hedging may change latency, never a byte"
        );
        hedges += result.counters.hedges_issued;
    }
    times.sort();
    (times, hedges)
}

fn median(times: &[Duration]) -> Duration {
    times[times.len() / 2]
}

/// The injected tail models what hedging exists for: **rare, huge** stalls (a p99, not a mean).
/// Every 5th fragment request (candidates/rerank, across both shards) stalls 900ms server-side.
/// Deterministic by arrival sequence — and because the stall spacing (5) exceeds a query's
/// fragment fan-out, two adjacent requests can never both stall, so a stalled original's hedge
/// (the next fragment request) is **deterministically fast**. An unhedged run pays the stall in
/// full; a hedged run pays `HEDGE_DELAY_MS` plus at most the dedup window.
const JITTER_EVERY: u64 = 5;
const JITTER_MS: u64 = 900;

#[test]
fn the_latency_jitter_async_hedge_campaign_over_the_real_transport() {
    let root = tmp("campaign");
    let campaign = campaign_fixture(&root);

    // --- phase 1: un-jittered, the transport is exact and hedging is silent -----------------
    rpc_fault::inject_fragment_delay(None);
    rpc_fault::reset_counters();
    let (calm_times, calm_hedges) = measure(&campaign.remote, &campaign.expected, 4);
    assert_eq!(
        calm_hedges, 0,
        "an un-jittered transport must never hedge — hedging adds no load in the common case"
    );

    // --- phase 2: jitter with hedging suppressed — slow, and still exact --------------------
    inject_max_inflight(Some(1)); // one original fills the budget; no hedge is ever admitted
    rpc_fault::inject_fragment_delay(Some((JITTER_EVERY, JITTER_MS)));
    let (slow_times, slow_hedges) = measure(&campaign.remote, &campaign.expected, 6);
    assert_eq!(
        slow_hedges, 0,
        "the in-flight cap must suppress every hedge"
    );
    assert!(
        median(&slow_times) >= Duration::from_millis(JITTER_MS * 8 / 10),
        "the injected tail did not land: unhedged median {:?} is not in the stall's neighbourhood",
        median(&slow_times)
    );

    // --- phase 3: the same jitter schedule, hedging live — fast, hedged, and still exact ----
    inject_max_inflight(None);
    rpc_fault::inject_fragment_delay(Some((JITTER_EVERY, JITTER_MS)));
    let (hedged_times, hedged_hedges) = measure(&campaign.remote, &campaign.expected, 6);
    assert!(
        hedged_hedges > 0,
        "the straggler never triggered a hedge — the timing path was not exercised"
    );
    assert!(
        median(&hedged_times) * 2 < median(&slow_times),
        "hedging did not cut the injected tail: hedged median {:?} vs unhedged {:?}",
        median(&hedged_times),
        median(&slow_times)
    );
    assert!(
        median(&hedged_times) < median(&calm_times) + Duration::from_millis(JITTER_MS / 2),
        "a hedged query under jitter should sit near the calm baseline, not near the stall: \
         hedged {:?}, calm {:?}",
        median(&hedged_times),
        median(&calm_times)
    );

    // --- phase 4a: a duplicate inside the dedup window is absorbed and compared -------------
    // Every fragment stalls 120ms (< HEDGE_DEDUP_WINDOW_MS): the hedge is also stalled, so the
    // loser lands inside the window after the winner — the bit-for-bit compare must actually run.
    rpc_fault::inject_fragment_delay(Some((1, 120)));
    rpc_fault::reset_counters();
    let result = campaign.remote.search(&query()).unwrap();
    assert_eq!(fp(&result), fp(&campaign.expected));
    assert!(
        result.counters.hedges_issued > 0,
        "every-fragment jitter must hedge"
    );
    assert!(
        rpc_fault::hedge_duplicates_absorbed() > 0,
        "no late duplicate was absorbed inside the dedup window — the bit-for-bit compare \
         path was never exercised"
    );

    // --- phase 4b: the blast-radius cap bounds hedging under jitter, answers unchanged ------
    inject_max_inflight(Some(3));
    rpc_fault::inject_fragment_delay(Some((1, 120)));
    let capped = campaign.remote.search(&query()).unwrap();
    assert_eq!(fp(&capped), fp(&campaign.expected));
    assert!(
        capped.counters.hedges_issued < result.counters.hedges_issued,
        "the in-flight cap did not bound transport hedging (capped {} vs uncapped {})",
        capped.counters.hedges_issued,
        result.counters.hedges_issued
    );

    inject_max_inflight(None);
    rpc_fault::inject_fragment_delay(None);
    teardown(campaign.servers);
}

/// The measured receipt — run release, by hand, like the scaling worksheet:
///
/// ```text
/// cargo test --release -p prism-cli --test hedge_transport -- --ignored --nocapture
/// ```
///
/// Writes `testing/evidence/s12-hedge-campaign.json`: calm / jittered-unhedged / jittered-hedged
/// latency quartiles over the real mutual-TLS transport, hedges issued, duplicates absorbed.
/// The substrate is honest about what it is: loopback TCP on one host — the *timing mechanism's*
/// receipt, not a wide-area number.
#[test]
#[ignore]
fn write_the_hedge_campaign_receipt() {
    let root = tmp("receipt");
    let campaign = campaign_fixture(&root);
    const RUNS: usize = 40;

    rpc_fault::inject_fragment_delay(None);
    rpc_fault::reset_counters();
    let (calm, _) = measure(&campaign.remote, &campaign.expected, RUNS);

    inject_max_inflight(Some(1));
    rpc_fault::inject_fragment_delay(Some((JITTER_EVERY, JITTER_MS)));
    let (unhedged, _) = measure(&campaign.remote, &campaign.expected, RUNS);

    inject_max_inflight(None);
    rpc_fault::reset_counters();
    rpc_fault::inject_fragment_delay(Some((JITTER_EVERY, JITTER_MS)));
    let (hedged, hedges) = measure(&campaign.remote, &campaign.expected, RUNS);

    inject_max_inflight(None);
    rpc_fault::inject_fragment_delay(None);

    let quartiles = |t: &[Duration]| {
        serde_json::json!({
            "p50_ms": t[t.len() / 2].as_secs_f64() * 1e3,
            "p90_ms": t[t.len() * 9 / 10].as_secs_f64() * 1e3,
            "max_ms": t[t.len() - 1].as_secs_f64() * 1e3,
        })
    };
    let receipt = serde_json::json!({
        "what": "S12 latency/jitter/async-hedge campaign over the mutual-TLS shard transport (D-098)",
        "substrate": "loopback TCP, one host, in-process ShardRpcServer x2, real rustls mTLS — \
                      the timing mechanism's receipt, not a wide-area number",
        "jitter": { "rule": format!("every {JITTER_EVERY}th fragment request"), "stall_ms": JITTER_MS },
        "constants": {
            "hedge_delay_ms": prism_engine::hedge::HEDGE_DELAY_MS,
            "hedge_fanout": prism_engine::hedge::HEDGE_FANOUT,
            "hedge_dedup_window_ms": prism_engine::hedge::HEDGE_DEDUP_WINDOW_MS,
            "max_inflight_fragments": prism_engine::hedge::MAX_INFLIGHT_FRAGMENTS,
        },
        "runs_per_config": RUNS,
        "calm": quartiles(&calm),
        "jittered_unhedged": quartiles(&unhedged),
        "jittered_hedged": quartiles(&hedged),
        "hedges_issued_over_hedged_runs": hedges,
        "answers": "every run byte-identical to the in-process cluster answer (asserted per run)",
    });
    let path = "../../testing/evidence/s12-hedge-campaign.json";
    std::fs::write(path, serde_json::to_string_pretty(&receipt).unwrap()).unwrap();
    eprintln!("receipt written to testing/evidence/s12-hedge-campaign.json");
    teardown(campaign.servers);
}
