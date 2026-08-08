//! The packaged shard node refuses every misconfiguration by name, before it can serve anything.
//!
//! These are process-level gates on the exact command the container image runs
//! (`deploy/prism-shard/Dockerfile` → `prism shard-serve`). A shard that starts under the wrong
//! identity, an unvalidated cluster shape, a shared trust bundle, or a durability target that
//! cannot survive losing the node is a supported-distribution defect, not an operator mistake, so
//! each of those must be a named refusal rather than a running server.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn prism() -> &'static str {
    env!("CARGO_BIN_EXE_prism")
}

/// A port unlikely to collide with a concurrently running test binary.
///
/// `shard-serve` refuses port 0 on purpose — an ephemeral production port is a misconfiguration —
/// so a test that needs a live listener has to name one. Deriving it from this process keeps the
/// debug and release suites, which run at the same time in CI, off each other's sockets.
fn port(offset: u16) -> u16 {
    20_000 + ((std::process::id() as u16) % 20_000) + offset
}

fn workspace(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "prism-shard-dist-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&path).expect("create test workspace");
    path
}

/// Every key below pins `ec_param_enc:named_curve`, because the fixture must not depend on which
/// OpenSSL the host happens to ship. LibreSSL — what macOS puts on `PATH` as `/usr/bin/openssl` —
/// defaults to writing EC keys with *explicit* curve parameters (the whole prime field spelled
/// out), while OpenSSL 3 defaults to the `prime256v1` OID. rustls accepts only the named form, so
/// without this flag the shard refuses its own test certificate on a developer's Mac and every
/// assertion below fails against a TLS parse error instead of the behaviour it means to test.
fn openssl(dir: &Path, args: &[&str]) {
    let output = Command::new("openssl")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run openssl");
    assert!(
        output.status.success(),
        "openssl {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

const KEY_ATTEMPTS: usize = 8;

/// Run `openssl`, then keep the key **only if rustls could actually use it**
/// ([issue #34](https://github.com/Bobcatsfan33/PrismDB/issues/34)).
///
/// A short private scalar is chance and is retried: LibreSSL emits 31 bytes whenever the value has
/// a leading zero (~1 key in 300) and `ring` requires exactly 32. Explicit curve parameters are a
/// generator defect and fail loudly, because retrying would hide a dropped
/// `ec_param_enc:named_curve` pin. Across a process boundary this matters even more: the `prism`
/// subprocess would otherwise die with an opaque TLS error nothing here could attribute.
fn openssl_key(dir: &Path, key_file: &str, args: &[&str]) {
    for _ in 0..KEY_ATTEMPTS {
        openssl(dir, args);
        let pem = std::fs::read_to_string(dir.join(key_file))
            .unwrap_or_else(|e| panic!("fixture key {key_file} could not be read: {e}"));
        assert!(
            !pem.is_empty(),
            "fixture key {key_file} is empty; openssl exited zero but wrote nothing"
        );
        let asn1 = Command::new("openssl")
            .args(["asn1parse", "-in", key_file])
            .current_dir(dir)
            .output()
            .expect("run openssl asn1parse");
        assert!(
            String::from_utf8_lossy(&asn1.stdout).contains("prime256v1"),
            "fixture key {key_file} is not in NAMED-CURVE form; rustls refuses that even though \
             openssl parses it. The generator must pin `ec_param_enc:named_curve`."
        );
        if prism_part::testkeys::is_ring_compatible_p256(&pem) {
            return;
        }
    }
    panic!(
        "fixture key {key_file} still had a short private scalar after {KEY_ATTEMPTS} attempts; \
         that is far past chance and means the generator is broken"
    );
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

/// A complete, valid shard-node deployment: store, distinct CAs, and a two-shard topology.
struct Fixture {
    dir: PathBuf,
    store: PathBuf,
    topology: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = workspace(tag);
        let store = dir.join("shard");
        let status = Command::new(prism())
            .args([
                "init",
                "--path",
                store.to_str().unwrap(),
                "--dim",
                "8",
                "--nlist",
                "2",
                "--pq-m",
                "2",
            ])
            .status()
            .expect("run prism init");
        assert!(status.success(), "prism init failed");

        generate_ca(&dir, "shard-ca");
        generate_ca(&dir, "coordinator-ca");
        generate_leaf(
            &dir,
            "server",
            "prism-shard-0.internal",
            "shard-ca",
            "serverAuth",
        );

        let topology = dir.join("topology.json");
        std::fs::write(
            &topology,
            r#"{
              "version": 1,
              "shards": [
                {"shard_id": 0, "address": "prism-shard-0.internal:7443", "server_name": "prism-shard-0.internal"},
                {"shard_id": 1, "address": "prism-shard-1.internal:7443", "server_name": "prism-shard-1.internal"}
              ]
            }"#,
        )
        .unwrap();

        Self {
            dir,
            store,
            topology,
        }
    }

    /// The arguments the container image runs, with each value overridable per test.
    fn args(&self, shard_id: &str, port: u16) -> Vec<String> {
        vec![
            "shard-serve".into(),
            "--path".into(),
            self.store.to_str().unwrap().into(),
            "--listen".into(),
            format!("127.0.0.1:{port}"),
            "--shard-id".into(),
            shard_id.into(),
            "--topology".into(),
            self.topology.to_str().unwrap().into(),
            "--cert".into(),
            self.dir.join("server.pem").to_str().unwrap().into(),
            "--key".into(),
            self.dir.join("server-key.pem").to_str().unwrap().into(),
            "--client-ca".into(),
            self.dir.join("coordinator-ca.pem").to_str().unwrap().into(),
            "--timeout-ms".into(),
            "1000".into(),
        ]
    }

    fn path(&self, name: &str) -> String {
        self.dir.join(name).to_str().unwrap().into()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Run `prism shard-serve` expecting it to refuse, and return the message it refused with.
fn refusal(args: &[String], env: &[(&str, Option<&str>)]) -> String {
    let mut command = Command::new(prism());
    command.args(args);
    for (key, value) in env {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    let output = command.output().expect("run prism shard-serve");
    assert!(
        !output.status.success(),
        "shard-serve started when it should have refused: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_shard_without_a_topology_refuses_to_start() {
    let fixture = Fixture::new("no-topology");
    let mut args = fixture.args("0", 7443);
    let flag = args
        .iter()
        .position(|value| value == "--topology")
        .expect("the packaged command passes --topology");
    args.drain(flag..flag + 2);
    let error = refusal(&args, &[]);
    assert!(
        error.contains("missing required flag --topology"),
        "{error}"
    );
}

#[test]
fn a_shard_refuses_a_non_contiguous_topology() {
    let fixture = Fixture::new("gapped-topology");
    std::fs::write(
        &fixture.topology,
        r#"{
          "version": 1,
          "shards": [
            {"shard_id": 0, "address": "prism-shard-0.internal:7443", "server_name": "prism-shard-0.internal"},
            {"shard_id": 7, "address": "prism-shard-7.internal:7443", "server_name": "prism-shard-7.internal"}
          ]
        }"#,
    )
    .unwrap();
    let error = refusal(&fixture.args("0", 7443), &[]);
    assert!(error.contains("contiguous range"), "{error}");
}

#[test]
fn a_shard_refuses_an_identity_outside_its_topology() {
    let fixture = Fixture::new("stranger");
    let error = refusal(&fixture.args("5", 7443), &[]);
    assert!(error.contains("not a member"), "{error}");
}

#[test]
fn a_shard_refuses_an_empty_shard_id_from_an_unresolved_pod_ordinal() {
    // On a cluster without the pod-index label the downward-API reference expands to an empty
    // string. A shard that guessed zero there would make every pod claim shard 0.
    let fixture = Fixture::new("empty-ordinal");
    let error = refusal(&fixture.args("", 7443), &[]);
    assert!(error.contains("--shard-id"), "{error}");
}

#[test]
fn a_shard_refuses_one_bundle_serving_both_trust_roles() {
    let fixture = Fixture::new("shared-trust");
    let mut args = fixture.args("0", 7443);
    // Point the coordinator trust root at the shard's own server chain.
    let client_ca = args.iter().position(|a| a == "--client-ca").unwrap() + 1;
    args[client_ca] = fixture.path("server.pem");
    let error = refusal(&args, &[]);
    assert!(error.contains("separate"), "{error}");
}

#[test]
fn a_write_enabled_shard_requires_a_durable_object_store() {
    let fixture = Fixture::new("no-remote");
    let mut args = fixture.args("0", 7443);
    args.extend(["--write-enabled".into(), "true".into()]);
    let error = refusal(&args, &[("PRISM_S3_ENDPOINT", None)]);
    assert!(
        error.contains("PRISM_S3_ENDPOINT") && error.contains("survives node loss"),
        "{error}"
    );
}

#[test]
fn a_write_enabled_shard_refuses_the_loopback_development_store() {
    let fixture = Fixture::new("insecure-remote");
    let mut args = fixture.args("0", 7443);
    args.extend(["--write-enabled".into(), "true".into()]);
    let error = refusal(
        &args,
        &[
            ("PRISM_S3_ENDPOINT", Some("127.0.0.1:9000")),
            ("PRISM_ALLOW_INSECURE_S3", Some("true")),
        ],
    );
    assert!(error.contains("PRISM_ALLOW_INSECURE_S3"), "{error}");
}

#[test]
fn a_read_only_shard_announces_that_it_cannot_write() {
    // The read-only constructor has no mutation door at all (proven in `shard_rpc`); this asserts
    // the packaged command reports that state rather than starting a silently writable node.
    let fixture = Fixture::new("read-only");
    let port = port(0);
    let mut child = Command::new(prism())
        .args(fixture.args("1", port))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn prism shard-serve");

    let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut record = String::new();
    // `emit` pretty-prints, so read until the closing brace of the startup object.
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).expect("read startup record");
        assert!(read > 0, "shard exited before announcing: {record}");
        record.push_str(&line);
        if line.starts_with('}') {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    let startup: serde_json::Value = serde_json::from_str(&record).expect("startup record is JSON");
    assert_eq!(startup["status"], "listening");
    assert_eq!(startup["transport"], "mutual-tls");
    assert_eq!(startup["shard_id"], 1);
    assert_eq!(startup["server_name"], "prism-shard-1.internal");
    assert_eq!(startup["topology_shards"], 2);
    assert_eq!(startup["writable"], false);
    assert_eq!(startup["recovered_wal_records"], 0);
}

#[test]
fn a_shard_drains_on_sigterm_instead_of_being_killed_mid_request() {
    let fixture = Fixture::new("drain");
    let port = port(1);
    let mut child = Command::new(prism())
        .args(fixture.args("0", port))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn prism shard-serve");

    let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));
    loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).expect("read startup record") > 0,
            "shard exited before announcing"
        );
        if line.starts_with('}') {
            break;
        }
    }

    #[cfg(unix)]
    {
        // SIGTERM is what a kubelet sends at the start of the termination grace period. The shard
        // must stop accepting, drain, and exit zero — not die with a signal status.
        let pid = child.id() as i32;
        // SAFETY: `pid` names this test's own child process, which is still alive because we have
        // not reaped it, and SIGTERM has no effect on any other process.
        let sent = unsafe { libc_kill(pid, 15) };
        assert_eq!(sent, 0, "failed to signal the shard");
    }
    #[cfg(not(unix))]
    let _ = child.kill();

    let status = child.wait().expect("wait for shard");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("piped stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");

    #[cfg(unix)]
    {
        assert!(
            status.success(),
            "a drained shutdown must exit zero, got {status:?}: {stderr}"
        );
        let stopped: serde_json::Value =
            serde_json::from_str(stderr.lines().last().unwrap_or_default())
                .expect("stop record is JSON");
        assert_eq!(stopped["event"], "shard_stopped");
        assert_eq!(
            stopped["graceful"], true,
            "the shard must record that it drained rather than died"
        );
    }
    #[cfg(not(unix))]
    let _ = status;
}

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, signal: i32) -> i32;
}
