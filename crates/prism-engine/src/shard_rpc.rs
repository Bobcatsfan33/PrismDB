//! Authenticated coordinator↔shard transport.
//!
//! The wire is deliberately small: one mutual-TLS connection carries one bounded,
//! length-prefixed JSON request and one response. Read operations mirror the fragment calls used
//! by [`crate::sharded::Cluster`]. Ingest is available only on a server constructed with the
//! remote-durable admission log; the read-only constructor remains incapable of mutation.

use crate::ingestor::{IngestReport2, Ingestor};
use crate::search::{ShardCandidates, ShardScored};
use crate::sharded::{Cluster, ReadShard};
use crate::Engine;
use prism_part::catalog::Snapshot;
use prism_part::partition::{Bucket, PartitionScheme};
use prism_part::store::StoreConfig;
use prism_types::error::{PrismError, Result};
use prism_types::{Event, Query, SearchResult};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const SHARD_RPC_PROTOCOL_VERSION: u16 = 4;
pub const MAX_SHARD_RPC_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SHARD_RPC_SELECTIONS: usize = 10_000;
pub const MAX_SHARD_RPC_CONNECTIONS: usize = 64;
pub const MAX_SHARD_RPC_EVENTS: usize = 1_000;
pub const MAX_REMOTE_READ_SHARDS: usize = 256;
pub const MAX_REMOTE_TOPOLOGY_BYTES: u64 = 1024 * 1024;
pub const MAX_SHARD_RPC_TLS_FILE_BYTES: u64 = 4 * 1024 * 1024;
pub const DEFAULT_SHARD_RPC_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval for the nonblocking accept loop and the shutdown drain.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(10);

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Serialize, Deserialize)]
struct RpcRequest {
    version: u16,
    request_id: String,
    target_shard: usize,
    operation: RpcOperation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum RpcOperation {
    Health,
    Snapshot,
    SnapshotById {
        snapshot_id: String,
    },
    ValidateSnapshot {
        snapshot: Snapshot,
    },
    SearchAt {
        snapshot: Snapshot,
        query: Query,
    },
    Candidates {
        snapshot: Snapshot,
        query: Query,
    },
    Rerank {
        snapshot: Snapshot,
        query: Query,
        selected: Vec<(String, usize)>,
    },
    Materialize {
        snapshot: Snapshot,
        selected: Vec<(String, usize)>,
    },
    Ingest {
        events: Vec<Event>,
        now_ms: i64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct RpcResponse {
    version: u16,
    request_id: String,
    shard_id: usize,
    outcome: RpcOutcome,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum RpcOutcome {
    Ok { payload: Box<RpcPayload> },
    Error { code: String, message: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum RpcPayload {
    Health(ShardHealth),
    Snapshot(Snapshot),
    Validated(String),
    Search(Box<SearchResult>),
    Candidates(ShardCandidates),
    Rerank(Vec<ShardScored>),
    Materialize(Vec<(Event, u32)>),
    Ingest(IngestReport2),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShardHealth {
    pub protocol_version: u16,
    pub shard_id: usize,
    pub snapshot_id: String,
    pub store_config: StoreConfig,
    pub writable: bool,
}

fn transport_error(context: &str, error: impl std::fmt::Display) -> PrismError {
    PrismError::Io(format!("shard RPC {context}: {error}"))
}

fn invalid_transport(message: impl Into<String>) -> PrismError {
    PrismError::Invalid(format!("shard RPC: {}", message.into()))
}

fn error_code(error: &PrismError) -> &'static str {
    match error {
        PrismError::Io(_) => "io",
        PrismError::Corrupt(_) => "corrupt",
        PrismError::Invalid(_) => "invalid",
        PrismError::Policy(_) => "policy",
        PrismError::NotFound(_) => "not_found",
        PrismError::Decode(_) => "decode",
        PrismError::Invariant(_) => "invariant",
        PrismError::OutOfSpace(_) => "out_of_space",
    }
}

fn validate_request_id(request_id: &str) -> Result<()> {
    if request_id.is_empty()
        || request_id.len() > 128
        || !request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(invalid_transport(
            "request_id must be 1..128 ASCII letters, digits, dots, underscores, or hyphens",
        ));
    }
    Ok(())
}

fn validate_snapshot_id(snapshot_id: &str) -> Result<()> {
    if snapshot_id.len() != 9
        || !snapshot_id.starts_with('s')
        || !snapshot_id.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(invalid_transport(
            "snapshot_id must use the canonical `s` plus eight decimal digits format",
        ));
    }
    Ok(())
}

fn read_frame<R: Read>(reader: &mut R) -> Result<Vec<u8>> {
    let mut prefix = [0u8; 4];
    reader
        .read_exact(&mut prefix)
        .map_err(|e| transport_error("read frame length", e))?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 || length > MAX_SHARD_RPC_FRAME_BYTES {
        return Err(invalid_transport(format!(
            "frame length {length} is outside 1..={MAX_SHARD_RPC_FRAME_BYTES}"
        )));
    }
    let mut bytes = vec![0u8; length];
    reader
        .read_exact(&mut bytes)
        .map_err(|e| transport_error("read frame body", e))?;
    Ok(bytes)
}

fn write_frame<W: Write>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() || bytes.len() > MAX_SHARD_RPC_FRAME_BYTES {
        return Err(invalid_transport(format!(
            "response length {} is outside 1..={MAX_SHARD_RPC_FRAME_BYTES}",
            bytes.len()
        )));
    }
    writer
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .and_then(|_| writer.write_all(bytes))
        .and_then(|_| writer.flush())
        .map_err(|e| transport_error("write frame", e))
}

fn read_regular_file(path: &Path, label: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| transport_error(&format!("inspect {label} {}", path.display()), e))?;
    if !metadata.file_type().is_file() {
        return Err(invalid_transport(format!(
            "{label} {} must be a regular, non-symlink file",
            path.display()
        )));
    }
    if metadata.len() == 0 || metadata.len() > MAX_SHARD_RPC_TLS_FILE_BYTES {
        return Err(invalid_transport(format!(
            "{label} {} size {} is outside 1..={MAX_SHARD_RPC_TLS_FILE_BYTES}",
            path.display(),
            metadata.len()
        )));
    }
    fs::read(path).map_err(|e| transport_error(&format!("read {label} {}", path.display()), e))
}

fn read_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let bytes = read_regular_file(path, "private key")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::symlink_metadata(path)
            .map_err(|e| transport_error("inspect private-key permissions", e))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o037 != 0 {
            return Err(invalid_transport(format!(
                "private key {} permissions are {mode:03o}; expected 0640 or stricter \
                 (group-read is allowed, group-write/execute and all other access are refused)",
                path.display()
            )));
        }
    }

    PrivateKeyDer::from_pem_slice(&bytes).map_err(|e| {
        transport_error(
            &format!(
                "parse private key {}; expected one supported PEM key",
                path.display()
            ),
            e,
        )
    })
}

fn read_certificates(path: &Path, label: &str) -> Result<Vec<CertificateDer<'static>>> {
    let bytes = read_regular_file(path, label)?;
    let certificates = CertificateDer::pem_slice_iter(&bytes)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| transport_error(&format!("parse {label}"), e))?;
    if certificates.is_empty() {
        return Err(invalid_transport(format!(
            "{label} {} contains no certificates",
            path.display()
        )));
    }
    Ok(certificates)
}

fn root_store(path: &Path, label: &str) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    for certificate in read_certificates(path, label)? {
        roots
            .add(certificate)
            .map_err(|e| transport_error(&format!("add {label}"), e))?;
    }
    Ok(roots)
}

/// Load a shard server identity and the dedicated CA allowed to issue coordinator certificates.
///
/// The private-key path must be a regular non-symlink file with mode 0640 or stricter on Unix.
/// Group-read supports an isolated Kubernetes `fsGroup`; group write/execute and all access for
/// other users are refused.
/// There is no "no client auth" constructor: production cannot accidentally downgrade mTLS.
pub fn server_tls_from_pem(
    certificate_chain: &Path,
    private_key: &Path,
    coordinator_ca: &Path,
) -> Result<Arc<ServerConfig>> {
    let verifier = WebPkiClientVerifier::builder(Arc::new(root_store(
        coordinator_ca,
        "coordinator trust root",
    )?))
    .build()
    .map_err(|e| transport_error("build coordinator certificate verifier", e))?;
    let config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            read_certificates(certificate_chain, "server certificate chain")?,
            read_private_key(private_key)?,
        )
        .map_err(|e| transport_error("build server TLS configuration", e))?;
    Ok(Arc::new(config))
}

/// Refuse a shard node whose server identity and coordinator-client trust share a root.
///
/// A shard server proves *it is the shard*; a coordinator client proves *it may call the shard*.
/// If one CA issues both, any coordinator certificate can also impersonate a shard endpoint and the
/// two roles stop being separable. This compares the two bundles as bytes: it refuses the same file
/// used twice and refuses a coordinator trust root that also appears in the shard's served chain.
/// It cannot see a root that signs the leaf without appearing in either file, so the deployment
/// contract — two distinct Secrets, documented in `SHARD-TRANSPORT.md` — remains the outer control.
pub fn assert_distinct_trust_roles(certificate_chain: &Path, coordinator_ca: &Path) -> Result<()> {
    if fs::canonicalize(certificate_chain).ok() == fs::canonicalize(coordinator_ca).ok() {
        return Err(invalid_transport(format!(
            "shard server chain and coordinator trust root are the same file {}; \
             the server identity and the coordinator-client trust must be separate bundles",
            coordinator_ca.display()
        )));
    }
    let served = read_certificates(certificate_chain, "server certificate chain")?;
    for root in read_certificates(coordinator_ca, "coordinator trust root")? {
        if served.contains(&root) {
            return Err(invalid_transport(
                "a coordinator trust root also appears in the shard server chain; \
                 issue the shard-server and coordinator-client identities from separate CAs"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

/// Load a coordinator client identity and the dedicated CA allowed to issue shard certificates.
pub fn client_tls_from_pem(
    certificate_chain: &Path,
    private_key: &Path,
    shard_ca: &Path,
) -> Result<Arc<ClientConfig>> {
    let config = ClientConfig::builder()
        .with_root_certificates(root_store(shard_ca, "shard trust root")?)
        .with_client_auth_cert(
            read_certificates(certificate_chain, "client certificate chain")?,
            read_private_key(private_key)?,
        )
        .map_err(|e| transport_error("build client TLS configuration", e))?;
    Ok(Arc::new(config))
}

fn validate_timeout(timeout: Duration) -> Result<()> {
    if !(Duration::from_millis(10)..=Duration::from_secs(60)).contains(&timeout) {
        return Err(invalid_transport(
            "timeout must be between 10 milliseconds and 60 seconds",
        ));
    }
    Ok(())
}

/// A write-enabled shard that has finished replicated recovery and has not yet been bound.
///
/// Holding the recovered count next to the not-yet-serving server is what lets a node report
/// exactly how much history it replayed in the same record that announces it began listening.
pub struct ShardNodeStartup {
    pub server: ShardRpcServer,
    pub recovered_wal_records: usize,
}

/// A read-only shard RPC server. Every accepted connection must complete a mutual-TLS handshake.
pub struct ShardRpcServer {
    shard_id: usize,
    engine: Arc<Engine>,
    tls: Arc<ServerConfig>,
    timeout: Duration,
    writer: Option<Arc<Mutex<Ingestor>>>,
}

impl ShardRpcServer {
    pub fn new(
        shard_id: usize,
        engine: Arc<Engine>,
        tls: Arc<ServerConfig>,
        timeout: Duration,
    ) -> Result<Self> {
        validate_timeout(timeout)?;
        Ok(Self {
            shard_id,
            engine,
            tls,
            timeout,
            writer: None,
        })
    }

    /// Construct a shard endpoint whose mutation door is backed by the replicated admission log.
    pub fn new_replicated_writer(
        shard_id: usize,
        ingestor: Ingestor,
        tls: Arc<ServerConfig>,
        timeout: Duration,
    ) -> Result<Self> {
        validate_timeout(timeout)?;
        let engine = Arc::clone(&ingestor.engine);
        Ok(Self {
            shard_id,
            engine,
            tls,
            timeout,
            writer: Some(Arc::new(Mutex::new(ingestor))),
        })
    }

    /// Recover the replicated admission log, then hand back a shard that is ready to be bound.
    ///
    /// The ordering is the guarantee, and it is expressed in the return type: this yields a
    /// [`ShardNodeStartup`], never a listener. A caller cannot admit a single connection before
    /// replicated recovery has returned, because until then there is nothing to accept on. A
    /// recovery that fails returns `Err` and no socket is ever bound, so an incompletely recovered
    /// write node is unreachable rather than quietly serving a truncated history.
    pub fn recover_then_ready(
        shard_id: usize,
        mut ingestor: Ingestor,
        tls: Arc<ServerConfig>,
        timeout: Duration,
        now_ms: i64,
    ) -> Result<ShardNodeStartup> {
        validate_timeout(timeout)?;
        let recovered_wal_records = ingestor.recover(now_ms)?.len();
        Ok(ShardNodeStartup {
            server: Self::new_replicated_writer(shard_id, ingestor, tls, timeout)?,
            recovered_wal_records,
        })
    }

    pub fn bind_and_serve(self, address: SocketAddr) -> Result<()> {
        let listener =
            TcpListener::bind(address).map_err(|e| transport_error("bind listener", e))?;
        self.serve(listener)
    }

    /// Serve forever with a fixed upper bound on concurrent TLS handshakes and requests.
    pub fn serve(self, listener: TcpListener) -> Result<()> {
        self.serve_with_shutdown(listener, Arc::new(AtomicBool::new(false)))
    }

    /// Serve until `shutdown` is set, then drain.
    ///
    /// Draining is what makes termination safe for a *write* node: the listener stops accepting
    /// first, so no new admission can begin, and the loop then waits for every in-flight request to
    /// reach its own durability boundary before returning. An ingest that has been acknowledged has
    /// already crossed the WAL fsync ([`crate::wal`]), so a drained shutdown cannot strand an
    /// acknowledged write; an ingest still in flight either completes and is acknowledged or never
    /// acknowledges at all. Exceeding the drain budget is a named failure, never a silent exit.
    pub fn serve_with_shutdown(
        self,
        listener: TcpListener,
        shutdown: Arc<AtomicBool>,
    ) -> Result<()> {
        listener
            .set_nonblocking(true)
            .map_err(|e| transport_error("set shard listener nonblocking", e))?;
        let active = Arc::new(AtomicUsize::new(0));
        let drain_budget = self.timeout;
        let server = Arc::new(self);
        while !shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    // BSD and macOS hand back an accepted socket that inherited the listener's
                    // O_NONBLOCK; Linux does not. Clearing it explicitly makes the per-connection
                    // read/write deadlines mean the same thing on every platform, instead of
                    // turning a blocking read into a spurious `WouldBlock` rejection.
                    if let Err(error) = stream.set_nonblocking(false) {
                        return Err(transport_error("clear accepted socket nonblocking", error));
                    }
                    if active.fetch_add(1, Ordering::SeqCst) >= MAX_SHARD_RPC_CONNECTIONS {
                        active.fetch_sub(1, Ordering::SeqCst);
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                    let server = Arc::clone(&server);
                    let active = Arc::clone(&active);
                    std::thread::spawn(move || {
                        if let Err(error) = server.handle_connection(stream) {
                            eprintln!("prism shard RPC connection rejected: {error}");
                        }
                        active.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL_INTERVAL);
                }
                Err(error) => return Err(transport_error("accept connection", error)),
            }
        }
        let deadline = std::time::Instant::now() + drain_budget;
        while active.load(Ordering::SeqCst) != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(ACCEPT_POLL_INTERVAL);
        }
        if active.load(Ordering::SeqCst) != 0 {
            return Err(invalid_transport(
                "shard shutdown drain exceeded the configured request timeout".to_string(),
            ));
        }
        Ok(())
    }

    /// Deterministic harness door: serve exactly `count` accepted connections inline.
    #[doc(hidden)]
    pub fn serve_connections(&self, listener: TcpListener, count: usize) -> Result<()> {
        for _ in 0..count {
            let (stream, _) = listener
                .accept()
                .map_err(|e| transport_error("accept test connection", e))?;
            self.handle_connection(stream)?;
        }
        Ok(())
    }

    fn handle_connection(&self, stream: TcpStream) -> Result<()> {
        stream
            .set_read_timeout(Some(self.timeout))
            .and_then(|_| stream.set_write_timeout(Some(self.timeout)))
            .map_err(|e| transport_error("set server socket deadline", e))?;
        let connection = ServerConnection::new(Arc::clone(&self.tls))
            .map_err(|e| transport_error("create server TLS session", e))?;
        let mut tls = StreamOwned::new(connection, stream);
        let request_bytes = read_frame(&mut tls)?;
        let request: RpcRequest = serde_json::from_slice(&request_bytes)?;
        let request_id = request.request_id.clone();
        let response = match self.process(request) {
            Ok(payload) => RpcResponse {
                version: SHARD_RPC_PROTOCOL_VERSION,
                request_id,
                shard_id: self.shard_id,
                outcome: RpcOutcome::Ok {
                    payload: Box::new(payload),
                },
            },
            Err(error) => RpcResponse {
                version: SHARD_RPC_PROTOCOL_VERSION,
                request_id,
                shard_id: self.shard_id,
                outcome: RpcOutcome::Error {
                    code: error_code(&error).to_string(),
                    message: error.to_string(),
                },
            },
        };
        let mut response_bytes = serde_json::to_vec(&response)?;
        if response_bytes.len() > MAX_SHARD_RPC_FRAME_BYTES {
            response_bytes = serde_json::to_vec(&RpcResponse {
                version: SHARD_RPC_PROTOCOL_VERSION,
                request_id: response.request_id,
                shard_id: self.shard_id,
                outcome: RpcOutcome::Error {
                    code: "response_too_large".into(),
                    message: format!(
                        "shard RPC response exceeds {MAX_SHARD_RPC_FRAME_BYTES} bytes"
                    ),
                },
            })?;
        }
        write_frame(&mut tls, &response_bytes)
    }

    fn process(&self, request: RpcRequest) -> Result<RpcPayload> {
        validate_request_id(&request.request_id)?;
        if request.version != SHARD_RPC_PROTOCOL_VERSION {
            return Err(invalid_transport(format!(
                "unsupported protocol version {}; expected {SHARD_RPC_PROTOCOL_VERSION}",
                request.version
            )));
        }
        if request.target_shard != self.shard_id {
            return Err(invalid_transport(format!(
                "request targets shard {}, but this endpoint serves shard {}",
                request.target_shard, self.shard_id
            )));
        }
        match request.operation {
            RpcOperation::Health => Ok(RpcPayload::Health(ShardHealth {
                protocol_version: SHARD_RPC_PROTOCOL_VERSION,
                shard_id: self.shard_id,
                snapshot_id: self.engine.snapshot()?.snapshot_id,
                store_config: self.engine.store.config.clone(),
                writable: self.writer.is_some(),
            })),
            RpcOperation::Snapshot => Ok(RpcPayload::Snapshot(self.engine.snapshot()?)),
            RpcOperation::SnapshotById { snapshot_id } => {
                validate_snapshot_id(&snapshot_id)?;
                Ok(RpcPayload::Snapshot(self.load_snapshot(&snapshot_id)?))
            }
            RpcOperation::ValidateSnapshot { snapshot } => {
                let snapshot = self.trusted_snapshot(&snapshot)?;
                <Engine as ReadShard>::validate_snapshot(&self.engine, &snapshot)?;
                Ok(RpcPayload::Validated(snapshot.snapshot_id))
            }
            RpcOperation::SearchAt { snapshot, query } => {
                let snapshot = self.trusted_snapshot(&snapshot)?;
                Ok(RpcPayload::Search(Box::new(
                    self.engine.search_at(&snapshot, &query)?,
                )))
            }
            RpcOperation::Candidates { snapshot, query } => Ok(RpcPayload::Candidates(
                self.engine
                    .search_candidates(&self.trusted_snapshot(&snapshot)?, &query)?,
            )),
            RpcOperation::Rerank {
                snapshot,
                query,
                selected,
            } => {
                let snapshot = self.trusted_snapshot(&snapshot)?;
                validate_selection_in_snapshot(&snapshot, &selected)?;
                Ok(RpcPayload::Rerank(
                    self.engine.search_rerank_selected(&query, &selected)?,
                ))
            }
            RpcOperation::Materialize { snapshot, selected } => {
                let snapshot = self.trusted_snapshot(&snapshot)?;
                validate_selection_in_snapshot(&snapshot, &selected)?;
                Ok(RpcPayload::Materialize(
                    self.engine.search_materialize(&selected)?,
                ))
            }
            RpcOperation::Ingest { events, now_ms } => {
                if events.is_empty() || events.len() > MAX_SHARD_RPC_EVENTS {
                    return Err(invalid_transport(format!(
                        "ingest batch needs 1..={MAX_SHARD_RPC_EVENTS} events"
                    )));
                }
                let tenant = &events[0].tenant_id;
                if tenant.is_empty() || events.iter().any(|event| event.tenant_id != *tenant) {
                    return Err(invalid_transport(
                        "ingest batch must contain one non-empty tenant",
                    ));
                }
                let writer = self.writer.as_ref().ok_or_else(|| {
                    PrismError::Policy(
                        "shard RPC ingest is disabled because this endpoint has no replicated WAL"
                            .into(),
                    )
                })?;
                let mut writer = writer.lock().map_err(|_| {
                    PrismError::Invariant("shard RPC ingest lock was poisoned".into())
                })?;
                Ok(RpcPayload::Ingest(
                    writer.ingest(events, None, None, now_ms)?,
                ))
            }
        }
    }

    fn load_snapshot(&self, snapshot_id: &str) -> Result<Snapshot> {
        let current = self.engine.snapshot()?;
        if current.snapshot_id == snapshot_id {
            Ok(current)
        } else {
            self.engine.catalog().load_snapshot(snapshot_id)
        }
    }

    fn trusted_snapshot(&self, requested: &Snapshot) -> Result<Snapshot> {
        validate_snapshot_id(&requested.snapshot_id)?;
        let stored = self.load_snapshot(&requested.snapshot_id)?;
        if &stored != requested {
            return Err(invalid_transport(format!(
                "snapshot `{}` does not match the shard's immutable catalog bytes",
                requested.snapshot_id
            )));
        }
        Ok(stored)
    }
}

fn validate_selection(selected: &[(String, usize)]) -> Result<()> {
    if selected.len() > MAX_SHARD_RPC_SELECTIONS {
        return Err(invalid_transport(format!(
            "selection has {} rows; limit is {MAX_SHARD_RPC_SELECTIONS}",
            selected.len()
        )));
    }
    if selected.iter().any(|(part_id, _)| {
        part_id.is_empty()
            || part_id.len() > 256
            || !part_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    }) {
        return Err(invalid_transport("selection contains an invalid part id"));
    }
    Ok(())
}

fn validate_selection_in_snapshot(snapshot: &Snapshot, selected: &[(String, usize)]) -> Result<()> {
    validate_selection(selected)?;
    let live: std::collections::BTreeSet<String> = snapshot.part_ids().into_iter().collect();
    if let Some((part_id, _)) = selected.iter().find(|(part_id, _)| !live.contains(part_id)) {
        return Err(invalid_transport(format!(
            "selection names part `{part_id}`, which is not in pinned snapshot `{}`",
            snapshot.snapshot_id
        )));
    }
    Ok(())
}

/// A mutual-TLS shard client. Hostname verification is mandatory and there is no insecure
/// constructor.
pub struct TlsShardClient {
    shard_id: usize,
    address: String,
    server_name: String,
    tls: Arc<ClientConfig>,
    timeout: Duration,
}

impl TlsShardClient {
    pub fn new(
        shard_id: usize,
        address: impl Into<String>,
        server_name: impl Into<String>,
        tls: Arc<ClientConfig>,
        timeout: Duration,
    ) -> Result<Self> {
        validate_timeout(timeout)?;
        let address = address.into();
        if address.trim().is_empty() {
            return Err(invalid_transport("address must not be empty"));
        }
        let server_name = server_name.into();
        ServerName::try_from(server_name.as_str())
            .map_err(|_| invalid_transport("server_name is not a valid DNS name"))?;
        Ok(Self {
            shard_id,
            address,
            server_name,
            tls,
            timeout,
        })
    }

    pub fn health(&self) -> Result<ShardHealth> {
        match self.call(RpcOperation::Health)? {
            RpcPayload::Health(health) => Ok(health),
            _ => Err(invalid_transport("health returned the wrong payload type")),
        }
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        match self.call(RpcOperation::Snapshot)? {
            RpcPayload::Snapshot(snapshot) => Ok(snapshot),
            _ => Err(invalid_transport(
                "snapshot returned the wrong payload type",
            )),
        }
    }

    pub fn snapshot_by_id(&self, snapshot_id: &str) -> Result<Snapshot> {
        validate_snapshot_id(snapshot_id)?;
        match self.call(RpcOperation::SnapshotById {
            snapshot_id: snapshot_id.to_string(),
        })? {
            RpcPayload::Snapshot(snapshot) => Ok(snapshot),
            _ => Err(invalid_transport(
                "snapshot_by_id returned the wrong payload type",
            )),
        }
    }

    pub fn validate_snapshot(&self, snapshot: Snapshot) -> Result<()> {
        let expected = snapshot.snapshot_id.clone();
        match self.call(RpcOperation::ValidateSnapshot { snapshot })? {
            RpcPayload::Validated(snapshot_id) if snapshot_id == expected => Ok(()),
            RpcPayload::Validated(_) => Err(invalid_transport(
                "validate_snapshot returned a different snapshot id",
            )),
            _ => Err(invalid_transport(
                "validate_snapshot returned the wrong payload type",
            )),
        }
    }

    pub fn search_at(&self, snapshot: Snapshot, query: Query) -> Result<SearchResult> {
        match self.call(RpcOperation::SearchAt { snapshot, query })? {
            RpcPayload::Search(result) => Ok(*result),
            _ => Err(invalid_transport(
                "search_at returned the wrong payload type",
            )),
        }
    }

    pub fn candidates(&self, snapshot: Snapshot, query: Query) -> Result<ShardCandidates> {
        match self.call(RpcOperation::Candidates { snapshot, query })? {
            RpcPayload::Candidates(candidates) => Ok(candidates),
            _ => Err(invalid_transport(
                "candidates returned the wrong payload type",
            )),
        }
    }

    pub fn rerank(
        &self,
        snapshot: Snapshot,
        query: Query,
        selected: Vec<(String, usize)>,
    ) -> Result<Vec<ShardScored>> {
        validate_selection_in_snapshot(&snapshot, &selected)?;
        match self.call(RpcOperation::Rerank {
            snapshot,
            query,
            selected,
        })? {
            RpcPayload::Rerank(scored) => Ok(scored),
            _ => Err(invalid_transport("rerank returned the wrong payload type")),
        }
    }

    pub fn materialize(
        &self,
        snapshot: Snapshot,
        selected: Vec<(String, usize)>,
    ) -> Result<Vec<(Event, u32)>> {
        validate_selection_in_snapshot(&snapshot, &selected)?;
        match self.call(RpcOperation::Materialize { snapshot, selected })? {
            RpcPayload::Materialize(events) => Ok(events),
            _ => Err(invalid_transport(
                "materialize returned the wrong payload type",
            )),
        }
    }

    pub fn ingest(&self, events: Vec<Event>, now_ms: i64) -> Result<IngestReport2> {
        if events.is_empty() || events.len() > MAX_SHARD_RPC_EVENTS {
            return Err(invalid_transport(format!(
                "ingest batch needs 1..={MAX_SHARD_RPC_EVENTS} events"
            )));
        }
        match self.call(RpcOperation::Ingest { events, now_ms })? {
            RpcPayload::Ingest(report) => Ok(report),
            _ => Err(invalid_transport("ingest returned the wrong payload type")),
        }
    }

    fn call(&self, operation: RpcOperation) -> Result<RpcPayload> {
        let request_id = format!(
            "{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let request = RpcRequest {
            version: SHARD_RPC_PROTOCOL_VERSION,
            request_id: request_id.clone(),
            target_shard: self.shard_id,
            operation,
        };
        let request_bytes = serde_json::to_vec(&request)?;
        if request_bytes.len() > MAX_SHARD_RPC_FRAME_BYTES {
            return Err(invalid_transport(format!(
                "request exceeds {MAX_SHARD_RPC_FRAME_BYTES} bytes"
            )));
        }

        let address = resolve_address(&self.address)?;
        let stream = TcpStream::connect_timeout(&address, self.timeout)
            .map_err(|e| transport_error("connect", e))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .and_then(|_| stream.set_write_timeout(Some(self.timeout)))
            .map_err(|e| transport_error("set client socket deadline", e))?;
        let server_name = ServerName::try_from(self.server_name.clone())
            .map_err(|_| invalid_transport("server_name is not a valid DNS name"))?;
        let connection = ClientConnection::new(Arc::clone(&self.tls), server_name)
            .map_err(|e| transport_error("create client TLS session", e))?;
        let mut tls = StreamOwned::new(connection, stream);
        write_frame(&mut tls, &request_bytes)?;
        let response_bytes = read_frame(&mut tls)?;
        let response: RpcResponse = serde_json::from_slice(&response_bytes)?;
        if response.version != SHARD_RPC_PROTOCOL_VERSION {
            return Err(invalid_transport(format!(
                "response protocol version {} does not match {SHARD_RPC_PROTOCOL_VERSION}",
                response.version
            )));
        }
        if response.request_id != request_id {
            return Err(invalid_transport("response request_id does not match"));
        }
        if response.shard_id != self.shard_id {
            return Err(invalid_transport(format!(
                "response came from shard {}, expected {}",
                response.shard_id, self.shard_id
            )));
        }
        match response.outcome {
            RpcOutcome::Ok { payload } => Ok(*payload),
            RpcOutcome::Error { code, message } => {
                let message = format!("remote shard {} returned {code}: {message}", self.shard_id);
                Err(match code.as_str() {
                    "io" => PrismError::Io(message),
                    "corrupt" => PrismError::Corrupt(message),
                    "invalid" => PrismError::Invalid(message),
                    "policy" => PrismError::Policy(message),
                    "not_found" => PrismError::NotFound(message),
                    "decode" => PrismError::Decode(message),
                    "invariant" => PrismError::Invariant(message),
                    "out_of_space" => PrismError::OutOfSpace(message),
                    _ => PrismError::Io(format!(
                        "{message}; the remote error code is not recognized by this client"
                    )),
                })
            }
        }
    }
}

impl ReadShard for TlsShardClient {
    fn validate_snapshot(&self, snapshot: &Snapshot) -> Result<()> {
        TlsShardClient::validate_snapshot(self, snapshot.clone())
    }

    fn candidates(&self, snapshot: &Snapshot, query: &Query) -> Result<ShardCandidates> {
        TlsShardClient::candidates(self, snapshot.clone(), query.clone())
    }

    fn rerank(
        &self,
        snapshot: &Snapshot,
        query: &Query,
        selected: &[(String, usize)],
    ) -> Result<Vec<ShardScored>> {
        TlsShardClient::rerank(self, snapshot.clone(), query.clone(), selected.to_vec())
    }

    fn materialize(
        &self,
        snapshot: &Snapshot,
        selected: &[(String, usize)],
    ) -> Result<Vec<(Event, u32)>> {
        TlsShardClient::materialize(self, snapshot.clone(), selected.to_vec())
    }
}

/// One authenticated shard endpoint in a remote read topology. Shard identifiers must be the
/// contiguous range `0..N`; routing by tenant bucket therefore cannot silently disagree with the
/// configured endpoint order.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteShardEndpoint {
    pub shard_id: usize,
    pub address: String,
    pub server_name: String,
}

/// Versioned, bounded topology consumed by the read coordinator.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteReadTopology {
    pub version: u16,
    pub shards: Vec<RemoteShardEndpoint>,
}

impl RemoteReadTopology {
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| transport_error("inspect coordinator topology", error))?;
        if !metadata.file_type().is_file() {
            return Err(invalid_transport(format!(
                "coordinator topology {} must be a regular, non-symlink file",
                path.display()
            )));
        }
        if metadata.len() == 0 || metadata.len() > MAX_REMOTE_TOPOLOGY_BYTES {
            return Err(invalid_transport(format!(
                "coordinator topology {} size {} is outside 1..={MAX_REMOTE_TOPOLOGY_BYTES}",
                path.display(),
                metadata.len()
            )));
        }
        let topology: Self = serde_json::from_slice(
            &fs::read(path).map_err(|error| transport_error("read coordinator topology", error))?,
        )?;
        topology.validate()?;
        Ok(topology)
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(invalid_transport(format!(
                "unsupported coordinator topology version {}; expected 1",
                self.version
            )));
        }
        if self.shards.is_empty() || self.shards.len() > MAX_REMOTE_READ_SHARDS {
            return Err(invalid_transport(format!(
                "coordinator topology needs 1..={MAX_REMOTE_READ_SHARDS} shards"
            )));
        }
        let mut ids: Vec<usize> = self.shards.iter().map(|shard| shard.shard_id).collect();
        ids.sort_unstable();
        let expected: Vec<usize> = (0..self.shards.len()).collect();
        if ids != expected {
            return Err(invalid_transport(format!(
                "coordinator shard ids must be the contiguous range 0..{}; found {ids:?}",
                self.shards.len()
            )));
        }
        Ok(())
    }

    /// Validate this topology from one shard node's point of view.
    ///
    /// A shard that does not know the cluster shape cannot notice that it was deployed with the
    /// wrong identity. Reusing the coordinator's own contiguity rule here means a topology that a
    /// coordinator would refuse cannot be the topology a shard silently starts under, and a
    /// `shard_id` outside `0..N` fails by name at startup instead of answering for a range of the
    /// keyspace no coordinator will ever route to it.
    pub fn endpoint_for(&self, shard_id: usize) -> Result<&RemoteShardEndpoint> {
        self.validate()?;
        self.shards
            .iter()
            .find(|endpoint| endpoint.shard_id == shard_id)
            .ok_or_else(|| {
                invalid_transport(format!(
                    "shard id {shard_id} is not a member of the configured topology of {} shards",
                    self.shards.len()
                ))
            })
    }
}

/// A multi-node, read-only coordinator over mutually authenticated shard clients.
///
/// Construction preflights every endpoint and refuses mixed store configurations. Query planning
/// pins a snapshot vector, routes tenant-scoped reads to one shard, and runs the same two-round
/// coordinator implementation as the in-process cluster for cross-tenant reads.
pub struct RemoteReadCluster {
    shards: Vec<TlsShardClient>,
    scheme: PartitionScheme,
    dim: usize,
    seed: u64,
    writable: Vec<bool>,
}

impl RemoteReadCluster {
    pub fn connect(
        topology: RemoteReadTopology,
        tls: Arc<ClientConfig>,
        timeout: Duration,
    ) -> Result<Self> {
        topology.validate()?;
        let mut endpoints = topology.shards;
        endpoints.sort_by_key(|endpoint| endpoint.shard_id);
        let shards = endpoints
            .into_iter()
            .map(|endpoint| {
                TlsShardClient::new(
                    endpoint.shard_id,
                    endpoint.address,
                    endpoint.server_name,
                    Arc::clone(&tls),
                    timeout,
                )
            })
            .collect::<Result<Vec<_>>>()?;

        let health_results: Vec<Result<ShardHealth>> = std::thread::scope(|scope| {
            let handles: Vec<_> = shards
                .iter()
                .map(|shard| scope.spawn(|| shard.health()))
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().map_err(|_| {
                        PrismError::Invariant("a remote shard health-check thread panicked".into())
                    })?
                })
                .collect()
        });
        let mut health = Vec::with_capacity(health_results.len());
        for (shard_id, result) in health_results.into_iter().enumerate() {
            health.push(result.map_err(|error| {
                PrismError::Io(format!(
                    "remote coordinator preflight could not reach shard {shard_id}: {error}"
                ))
            })?);
        }
        let config = health[0].store_config.clone();
        config.validate()?;
        for (shard_id, item) in health.iter().enumerate() {
            if item.protocol_version != SHARD_RPC_PROTOCOL_VERSION || item.shard_id != shard_id {
                return Err(PrismError::Invariant(format!(
                    "remote coordinator preflight identity mismatch for shard {shard_id}"
                )));
            }
            if item.store_config != config {
                return Err(PrismError::Invariant(format!(
                    "remote shard {shard_id} has a different immutable store configuration; \
                     dimensions, seed, partition routing, and format must agree"
                )));
            }
        }

        Ok(Self {
            shards,
            scheme: config.partitions,
            dim: config.dim,
            seed: config.seed,
            writable: health.iter().map(|item| item.writable).collect(),
        })
    }

    pub fn num_shards(&self) -> usize {
        self.shards.len()
    }

    pub fn shard_index(&self, tenant: &str) -> usize {
        let bucket = self.scheme.bucket_of(tenant);
        let ordinal = match bucket {
            Bucket::Shared(index) => index as u64,
            Bucket::Dedicated(index) => self.scheme.buckets as u64 + index as u64,
        };
        (ordinal % self.shards.len() as u64) as usize
    }

    /// Revalidate every authenticated shard identity and immutable store configuration.
    ///
    /// The public service calls this from readiness rather than treating a successful startup
    /// preflight as permanent. A partition therefore removes the pod from ready endpoints before
    /// callers receive a plausibly healthy but incomplete service.
    pub fn readiness(&self, require_writable: bool) -> Result<()> {
        let health_results: Vec<Result<ShardHealth>> = std::thread::scope(|scope| {
            let handles: Vec<_> = self
                .shards
                .iter()
                .map(|shard| scope.spawn(|| shard.health()))
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().map_err(|_| {
                        PrismError::Invariant("a remote shard readiness thread panicked".into())
                    })?
                })
                .collect()
        });
        for (shard_id, health) in health_results.into_iter().enumerate() {
            let health = health.map_err(|error| {
                PrismError::Io(format!(
                    "remote coordinator readiness could not reach shard {shard_id}: {error}"
                ))
            })?;
            if health.protocol_version != SHARD_RPC_PROTOCOL_VERSION
                || health.shard_id != shard_id
                || health.store_config.dim != self.dim
                || health.store_config.seed != self.seed
                || health.store_config.partitions != self.scheme
                || health.writable != self.writable[shard_id]
            {
                return Err(PrismError::Invariant(format!(
                    "remote shard {shard_id} failed readiness identity/configuration/mode validation"
                )));
            }
            if require_writable && !health.writable {
                return Err(PrismError::Policy(format!(
                    "remote shard {shard_id} is read-only while the public policy grants ingest"
                )));
            }
        }
        Ok(())
    }

    pub fn search(&self, query: &Query) -> Result<SearchResult> {
        if let Some(tenant) = query.tenant.as_deref() {
            let shard_id = self.shard_index(tenant);
            let snapshot = self.shards[shard_id].snapshot().map_err(|error| {
                PrismError::NotFound(format!(
                    "shard {shard_id} unreachable while pinning its snapshot: {error}; \
                     a tenant-scoped query is all-or-nothing"
                ))
            })?;
            return self.shards[shard_id]
                .search_at(snapshot, query.clone())
                .map_err(|error| {
                    PrismError::NotFound(format!(
                        "shard {shard_id} unreachable while serving the tenant query: {error}; \
                         a tenant-scoped query is all-or-nothing"
                    ))
                });
        }

        let snapshot_results: Vec<Result<Snapshot>> = std::thread::scope(|scope| {
            let handles: Vec<_> = self
                .shards
                .iter()
                .map(|shard| scope.spawn(|| shard.snapshot()))
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().map_err(|_| {
                        PrismError::Invariant("a remote shard snapshot thread panicked".into())
                    })?
                })
                .collect()
        });
        let mut vector = Vec::with_capacity(snapshot_results.len());
        let mut missing = Vec::new();
        for (shard_id, result) in snapshot_results.into_iter().enumerate() {
            match result {
                Ok(snapshot) => vector.push(snapshot),
                Err(error) if query.best_effort => {
                    missing.push(prism_types::MissingShard {
                        shard: shard_id,
                        reason: format!("snapshot pin failed: {error}"),
                    });
                    vector.push(Snapshot::empty());
                }
                Err(error) => {
                    return Err(PrismError::NotFound(format!(
                        "shard {shard_id} unreachable while pinning the distributed snapshot \
                         vector: {error}. The query did not opt in to a partial answer."
                    )));
                }
            }
        }

        Cluster::coordinate_cross_shard(&self.shards, self.dim, self.seed, &vector, query, missing)
    }

    /// Route a single-tenant batch to its authenticated writable shard.
    pub fn ingest(&self, events: Vec<Event>, now_ms: i64) -> Result<IngestReport2> {
        if events.is_empty() || events.len() > MAX_SHARD_RPC_EVENTS {
            return Err(invalid_transport(format!(
                "ingest batch needs 1..={MAX_SHARD_RPC_EVENTS} events"
            )));
        }
        let tenant = events[0].tenant_id.clone();
        if tenant.is_empty() || events.iter().any(|event| event.tenant_id != tenant) {
            return Err(invalid_transport(
                "coordinator ingest batch must contain exactly one non-empty tenant",
            ));
        }
        let shard_id = self.shard_index(&tenant);
        if !self.writable[shard_id] {
            return Err(PrismError::Policy(format!(
                "remote shard {shard_id} is read-only; replicated WAL ingest is required"
            )));
        }
        self.shards[shard_id].ingest(events, now_ms)
    }
}

fn resolve_address(address: &str) -> Result<SocketAddr> {
    let mut resolved = address
        .to_socket_addrs()
        .map_err(|e| transport_error("resolve address", e))?;
    let address = resolved
        .next()
        .ok_or_else(|| invalid_transport("address resolved to no endpoints"))?;
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::object::{CachedObjectStore, LocalObjectStore, ObjectStore};
    use crate::storage::CACHE_QUOTA_BYTES;
    use prism_part::partition::PartitionScheme;
    use prism_part::store::{StoreConfig, STORE_VERSION};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::mpsc;

    /// Fixture directories get their **own** counter.
    ///
    /// They used to share [`REQUEST_SEQUENCE`] with runtime RPC request ids, which meant a
    /// fixture's directory name depended on how many RPC calls other concurrently-running tests
    /// happened to have made. The names stayed unique — `fetch_add` sees to that — but they were
    /// **not reproducible by design**, and a fixture nobody can reproduce is a fixture nobody can
    /// debug when it misbehaves ([issue #34](https://github.com/Bobcatsfan33/PrismDB/issues/34)).
    /// Removing the coupling is the point; uniqueness was never the problem.
    static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "prism-rpc-{tag}-{}-{}",
            std::process::id(),
            FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn engine() -> Arc<Engine> {
        Arc::new(
            Engine::init(
                &temp("store"),
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
                },
            )
            .unwrap(),
        )
    }

    /// Every key below pins `ec_param_enc:named_curve`, because the fixture must not depend on
    /// which OpenSSL the host happens to ship. LibreSSL — what macOS puts on `PATH` as
    /// `/usr/bin/openssl` — defaults to writing EC keys with *explicit* curve parameters, while
    /// OpenSSL 3 defaults to the `prime256v1` OID. rustls accepts only the named form, so without
    /// this flag the server refuses its own test certificate and these tests fail against a TLS
    /// parse error instead of the behaviour they mean to test.
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

    /// How many times a generator will retry a key whose scalar came out short.
    ///
    /// The short case is ~1 in 300, so two retries already leave a ~1-in-27-million residue; the
    /// bound exists so a genuinely broken generator fails loudly instead of spinning.
    const KEY_ATTEMPTS: usize = 8;

    /// Run `openssl`, then keep the key **only if rustls could actually use it**
    /// ([issue #34](https://github.com/Bobcatsfan33/PrismDB/issues/34)).
    ///
    /// Two failure modes, deliberately handled differently:
    ///
    /// * **A short private scalar is chance, so it is retried.** LibreSSL emits a 31-byte scalar
    ///   whenever the value has a leading zero — about 1 key in 300 — and `ring` requires exactly
    ///   32. Regenerating draws a new key; there is nothing to fix.
    /// * **Explicit curve parameters are a generator defect, so they fail loudly.** That is the
    ///   `ec_param_enc:named_curve` pin having been dropped, and retrying would just hide it.
    fn openssl_key(dir: &Path, key_file: &str, args: &[&str]) {
        for _ in 0..KEY_ATTEMPTS {
            openssl(dir, args);
            let pem = fs::read_to_string(dir.join(key_file))
                .unwrap_or_else(|e| panic!("fixture key {key_file} could not be read: {e}"));
            assert!(
                !pem.is_empty(),
                "fixture key {key_file} is empty; openssl exited zero but wrote nothing"
            );
            assert!(
                pem.contains("BEGIN PRIVATE KEY"),
                "fixture key {key_file} is not a PKCS#8 private key"
            );
            let asn1 = Command::new("openssl")
                .args(["asn1parse", "-in", key_file])
                .current_dir(dir)
                .output()
                .expect("run openssl asn1parse");
            assert!(
                String::from_utf8_lossy(&asn1.stdout).contains("prime256v1"),
                "fixture key {key_file} is not in NAMED-CURVE form; rustls refuses that even \
                 though openssl parses it. The generator must pin `ec_param_enc:named_curve`."
            );
            if prism_part::testkeys::is_ring_compatible_p256(&pem) {
                return;
            }
        }
        panic!(
            "fixture key {key_file} still had a short private scalar after {KEY_ATTEMPTS} \
             attempts; that is far past chance and means the generator is broken"
        );
    }

    fn generate_ca(dir: &Path, prefix: &str) {
        let key = format!("{prefix}-key.pem");
        let cert = format!("{prefix}.pem");
        let subject = format!("/CN={prefix}");
        openssl_key(
            dir,
            &key,
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
                &subject,
                "-keyout",
                &key,
                "-out",
                &cert,
            ],
        );
    }

    fn generate_leaf(dir: &Path, prefix: &str, common_name: &str, ca_prefix: &str, usage: &str) {
        let key = format!("{prefix}-key.pem");
        let csr = format!("{prefix}.csr");
        let cert = format!("{prefix}.pem");
        let ext = format!("{prefix}.ext");
        let ca_cert = format!("{ca_prefix}.pem");
        let ca_key = format!("{ca_prefix}-key.pem");
        let ca_serial = format!("{ca_prefix}.srl");
        let subject = format!("/CN={common_name}");
        openssl_key(
            dir,
            &key,
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
                &subject,
                "-keyout",
                &key,
                "-out",
                &csr,
            ],
        );
        fs::write(
            dir.join(&ext),
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
                &csr,
                "-CA",
                &ca_cert,
                "-CAkey",
                &ca_key,
                "-CAserial",
                &ca_serial,
                "-CAcreateserial",
                "-days",
                "3650",
                "-sha256",
                "-extfile",
                &ext,
                "-out",
                &cert,
            ],
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.join(key), fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn tls_pair() -> (Arc<ServerConfig>, Arc<ClientConfig>, Arc<ClientConfig>) {
        let dir = temp("certificates");
        fs::create_dir_all(&dir).unwrap();
        generate_ca(&dir, "shard-ca");
        generate_ca(&dir, "coordinator-ca");
        generate_leaf(&dir, "server", "shard.test", "shard-ca", "serverAuth");
        generate_leaf(
            &dir,
            "client",
            "coordinator.test",
            "coordinator-ca",
            "clientAuth",
        );
        generate_ca(&dir, "rogue-ca");
        generate_leaf(
            &dir,
            "rogue-client",
            "rogue-coordinator.test",
            "rogue-ca",
            "clientAuth",
        );
        let server = server_tls_from_pem(
            &dir.join("server.pem"),
            &dir.join("server-key.pem"),
            &dir.join("coordinator-ca.pem"),
        )
        .unwrap();
        let client = client_tls_from_pem(
            &dir.join("client.pem"),
            &dir.join("client-key.pem"),
            &dir.join("shard-ca.pem"),
        )
        .unwrap();
        let rogue = client_tls_from_pem(
            &dir.join("rogue-client.pem"),
            &dir.join("rogue-client-key.pem"),
            &dir.join("shard-ca.pem"),
        )
        .unwrap();
        (server, client, rogue)
    }

    fn one_connection_server(
        tls: Arc<ServerConfig>,
    ) -> (SocketAddr, std::thread::JoinHandle<Result<()>>) {
        connection_server(tls, engine(), 1)
    }

    fn connection_server(
        tls: Arc<ServerConfig>,
        engine: Arc<Engine>,
        count: usize,
    ) -> (SocketAddr, std::thread::JoinHandle<Result<()>>) {
        connection_server_for(7, tls, engine, count)
    }

    fn connection_server_for(
        shard_id: usize,
        tls: Arc<ServerConfig>,
        engine: Arc<Engine>,
        count: usize,
    ) -> (SocketAddr, std::thread::JoinHandle<Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server =
            ShardRpcServer::new(shard_id, engine, tls, Duration::from_millis(500)).unwrap();
        let handle = std::thread::spawn(move || server.serve_connections(listener, count));
        (address, handle)
    }

    fn event(id: &str, body: &str) -> Event {
        Event {
            event_id: id.into(),
            tenant_id: "tenant-a".into(),
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

    fn event_for(id: &str, tenant: &str, body: &str) -> Event {
        Event {
            tenant_id: tenant.to_string(),
            ..event(id, body)
        }
    }

    type TwoShardFixture = (
        PathBuf,
        SearchResult,
        RemoteReadTopology,
        Arc<ClientConfig>,
        [std::thread::JoinHandle<Result<()>>; 2],
    );

    fn two_shard_fixture(server_connections: [usize; 2]) -> TwoShardFixture {
        let root = temp("remote-cluster");
        let config = StoreConfig {
            format_version: STORE_VERSION,
            dim: 8,
            nlist: 2,
            pq_m: 2,
            seed: 42,
            kmeans_restarts: 2,
            block_size: 4096,
            partitions: PartitionScheme::default(),
            promote: Vec::new(),
        };
        let cluster = crate::sharded::Cluster::init(&root, 2, config).unwrap();
        let tenant0 = (0..10_000)
            .map(|index| format!("tenant-{index}"))
            .find(|tenant| cluster.shard_index(tenant) == 0)
            .unwrap();
        let tenant1 = (0..10_000)
            .map(|index| format!("tenant-{index}"))
            .find(|tenant| cluster.shard_index(tenant) == 1)
            .unwrap();
        cluster
            .ingest(
                vec![
                    event_for("a1", &tenant0, "payment service timeout"),
                    event_for("a2", &tenant0, "payment queue growing"),
                    event_for("b1", &tenant1, "payment service recovered"),
                    event_for("b2", &tenant1, "search latency high"),
                ],
                2,
            )
            .unwrap();
        let query = Query {
            text: "payment service".into(),
            k: 4,
            candidates: 8,
            rerank: 8,
            ..Query::default()
        };
        let expected = cluster.search(&query).unwrap();
        drop(cluster);

        let shard0 = Arc::new(Engine::open(&root.join("shard-0")).unwrap());
        let shard1 = Arc::new(Engine::open(&root.join("shard-1")).unwrap());
        let (server_tls, client_tls, _) = tls_pair();
        let (address0, server0) =
            connection_server_for(0, Arc::clone(&server_tls), shard0, server_connections[0]);
        let (address1, server1) =
            connection_server_for(1, server_tls, shard1, server_connections[1]);
        let topology = RemoteReadTopology {
            version: 1,
            shards: vec![
                RemoteShardEndpoint {
                    shard_id: 0,
                    address: address0.to_string(),
                    server_name: "shard.test".into(),
                },
                RemoteShardEndpoint {
                    shard_id: 1,
                    address: address1.to_string(),
                    server_name: "shard.test".into(),
                },
            ],
        };
        (root, expected, topology, client_tls, [server0, server1])
    }

    #[test]
    fn mutual_tls_health_names_the_protocol_shard_and_snapshot() {
        let (server_tls, client_tls, _) = tls_pair();
        let (address, server) = one_connection_server(server_tls);
        let client = TlsShardClient::new(
            7,
            address.to_string(),
            "shard.test",
            client_tls,
            Duration::from_millis(500),
        )
        .unwrap();
        let health = client.health().unwrap();
        assert_eq!(health.protocol_version, SHARD_RPC_PROTOCOL_VERSION);
        assert_eq!(health.shard_id, 7);
        assert!(!health.snapshot_id.is_empty());
        assert!(!health.writable);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn read_only_server_refuses_ingest_by_construction() {
        let (tls, _, _) = tls_pair();
        let server = ShardRpcServer::new(7, engine(), tls, Duration::from_millis(500)).unwrap();
        let error = server
            .process(RpcRequest {
                version: SHARD_RPC_PROTOCOL_VERSION,
                request_id: "no-write".into(),
                target_shard: 7,
                operation: RpcOperation::Ingest {
                    events: vec![event("e1", "payment timeout")],
                    now_ms: 2,
                },
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("disabled"), "{error}");
        assert!(error.contains("replicated WAL"), "{error}");
    }

    #[test]
    fn authenticated_remote_ingest_uses_the_replicated_wal() {
        let root = temp("write-node");
        let remote_root = temp("write-remote");
        fs::create_dir_all(&remote_root).unwrap();
        let backend: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(&remote_root));
        let raw_engine = Engine::init(
            &root,
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
            },
        )
        .unwrap()
        .with_cold(Arc::new(CachedObjectStore::new(
            Arc::clone(&backend),
            CACHE_QUOTA_BYTES,
        )));
        let ingestor = Ingestor::open_replicated(raw_engine, 0).unwrap();
        let engine = Arc::clone(&ingestor.engine);
        let (server_tls, client_tls, _) = tls_pair();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server =
            ShardRpcServer::new_replicated_writer(0, ingestor, server_tls, Duration::from_secs(2))
                .unwrap();
        let handle = std::thread::spawn(move || server.serve_connections(listener, 2));
        let cluster = RemoteReadCluster::connect(
            RemoteReadTopology {
                version: 1,
                shards: vec![RemoteShardEndpoint {
                    shard_id: 0,
                    address: address.to_string(),
                    server_name: "shard.test".into(),
                }],
            },
            client_tls,
            Duration::from_secs(2),
        )
        .unwrap();
        let report = cluster
            .ingest(vec![event("e1", "payment timeout")], 2)
            .unwrap();
        assert_eq!(report.published, 1);
        assert!(report.wal_record.unwrap() >= 1u64 << 32);
        assert!(!engine.snapshot().unwrap().parts.is_empty());
        assert_eq!(
            backend.list("wal/shard-0/records/").unwrap().len(),
            1,
            "the network write must leave a remote-durable admission record"
        );
        handle.join().unwrap().unwrap();
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(remote_root);
    }

    /// A write node with one acknowledged-but-unpublished admission record, plus its backend.
    fn pending_write_node(tag: &str) -> (PathBuf, PathBuf, Ingestor) {
        let root = temp(tag);
        let remote_root = temp(&format!("{tag}-remote"));
        fs::create_dir_all(&remote_root).unwrap();
        let backend: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(&remote_root));
        let engine = Engine::init(
            &root,
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
            },
        )
        .unwrap()
        .with_cold(Arc::new(CachedObjectStore::new(
            Arc::clone(&backend),
            CACHE_QUOTA_BYTES,
        )));
        let ingestor = Ingestor::open_replicated(engine, 0).unwrap();
        // Cross the durability boundary without publishing: exactly the state a node dies in.
        let remote = crate::wal::RemoteWal::new(Arc::clone(&backend), 0);
        let record_id = remote
            .next_record_id(ingestor.engine.ownership_epoch(), &[])
            .unwrap();
        let record = crate::wal::WalRecord {
            record_id,
            events: vec![event("pending-1", "payment service timeout")],
            source: None,
            source_offset: None,
            created_at_ms: 1_760_000_000_000,
        };
        ingestor.wal.append_record(&record).unwrap();
        remote.append(&record).unwrap();
        (root, remote_root, ingestor)
    }

    #[test]
    fn replicated_recovery_completes_before_a_shard_can_be_bound() {
        let (root, remote_root, ingestor) = pending_write_node("recover-order");
        let (server_tls, _, _) = tls_pair();

        let startup = ShardRpcServer::recover_then_ready(
            0,
            ingestor,
            server_tls,
            Duration::from_secs(2),
            1_760_000_000_001,
        )
        .unwrap();

        // The acknowledged record was replayed, and the value proving it is in hand BEFORE any
        // listener exists: `recover_then_ready` yields a server, never a bound socket, so there is
        // no window in which a coordinator could be served from an unrecovered history.
        assert_eq!(
            startup.recovered_wal_records, 1,
            "the acknowledged admission record must be replayed at startup"
        );
        assert!(
            !startup.server.engine.snapshot().unwrap().parts.is_empty(),
            "recovery must have published the replayed batch before the shard is bindable"
        );

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(remote_root);
    }

    #[test]
    fn a_shard_whose_recovery_fails_is_never_bindable() {
        let (root, remote_root, ingestor) = pending_write_node("recover-fail");
        let (server_tls, _, _) = tls_pair();

        // Corrupt the remote admission log the way a mutable backend would: a well-formed record
        // filed under a key that no longer matches its body. Recovery must refuse rather than serve
        // a history it cannot vouch for.
        let records = fs::read_dir(remote_root.join("wal").join("shard-0").join("records"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1);
        let mut record: crate::wal::WalRecord =
            serde_json::from_slice(&fs::read(&records[0]).unwrap()).unwrap();
        record.record_id += 1;
        fs::write(&records[0], serde_json::to_vec(&record).unwrap()).unwrap();

        let outcome = ShardRpcServer::recover_then_ready(
            0,
            ingestor,
            server_tls,
            Duration::from_secs(2),
            1_760_000_000_001,
        );
        let error = match outcome {
            Ok(_) => panic!("a corrupt admission log must not produce a bindable shard"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("key id") && error.contains("body id"),
            "a failed recovery must name the disagreement it found: {error}"
        );

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(remote_root);
    }

    #[test]
    fn shutdown_drains_in_flight_work_before_returning() {
        let shard = engine();
        shard
            .ingest(vec![event("e1", "payment service timeout")], 2)
            .unwrap();
        let (server_tls, client_tls, _) = tls_pair();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = ShardRpcServer::new(0, shard, server_tls, Duration::from_secs(5)).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let serving = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || server.serve_with_shutdown(listener, serving));

        // A request served through the draining accept loop behaves exactly as before.
        let client = TlsShardClient::new(
            0,
            address.to_string(),
            "shard.test",
            client_tls,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(client.health().is_ok());

        shutdown.store(true, Ordering::SeqCst);
        // The drain returns Ok only when nothing is still in flight; exceeding the budget is a
        // named error, so a clean Ok here is the durability-preserving outcome, not a timeout.
        handle
            .join()
            .unwrap()
            .expect("a drained shutdown must not report an error");
    }

    #[test]
    fn one_bundle_cannot_serve_both_trust_roles() {
        let dir = temp("shared-trust");
        fs::create_dir_all(&dir).unwrap();
        generate_ca(&dir, "shared-ca");
        generate_leaf(&dir, "server", "shard.test", "shared-ca", "serverAuth");

        let same_file =
            assert_distinct_trust_roles(&dir.join("shared-ca.pem"), &dir.join("shared-ca.pem"))
                .unwrap_err()
                .to_string();
        assert!(same_file.contains("same file"), "{same_file}");

        // A chain that carries the coordinator root is the subtler shape: two paths, one trust.
        let mut chain = fs::read(dir.join("server.pem")).unwrap();
        chain.extend_from_slice(&fs::read(dir.join("shared-ca.pem")).unwrap());
        fs::write(dir.join("server-chain.pem"), &chain).unwrap();
        let shared_root =
            assert_distinct_trust_roles(&dir.join("server-chain.pem"), &dir.join("shared-ca.pem"))
                .unwrap_err()
                .to_string();
        assert!(shared_root.contains("separate CAs"), "{shared_root}");

        // Distinct CAs, as the deployment contract requires, are accepted.
        generate_ca(&dir, "distinct-coordinator-ca");
        assert_distinct_trust_roles(
            &dir.join("server.pem"),
            &dir.join("distinct-coordinator-ca.pem"),
        )
        .unwrap();

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_shard_refuses_a_topology_it_is_not_a_member_of() {
        let topology = RemoteReadTopology {
            version: 1,
            shards: vec![
                RemoteShardEndpoint {
                    shard_id: 0,
                    address: "prism-shard-0.internal:7443".into(),
                    server_name: "prism-shard-0.internal".into(),
                },
                RemoteShardEndpoint {
                    shard_id: 1,
                    address: "prism-shard-1.internal:7443".into(),
                    server_name: "prism-shard-1.internal".into(),
                },
            ],
        };
        assert_eq!(topology.endpoint_for(1).unwrap().shard_id, 1);
        let error = topology.endpoint_for(2).unwrap_err().to_string();
        assert!(error.contains("not a member"), "{error}");

        let gapped = RemoteReadTopology {
            version: 1,
            shards: vec![RemoteShardEndpoint {
                shard_id: 4,
                address: "prism-shard-4.internal:7443".into(),
                server_name: "prism-shard-4.internal".into(),
            }],
        };
        let error = gapped.endpoint_for(4).unwrap_err().to_string();
        assert!(error.contains("contiguous range"), "{error}");
    }

    #[test]
    fn all_three_query_fragments_cross_the_authenticated_wire() {
        let shard = engine();
        shard
            .ingest(
                vec![
                    event("e1", "payment service timeout"),
                    event("e2", "payment service recovered"),
                    event("e3", "search latency high"),
                    event("e4", "payment queue growing"),
                ],
                2,
            )
            .unwrap();
        let (server_tls, client_tls, _) = tls_pair();
        let (address, server) = connection_server(server_tls, shard, 6);
        let client = TlsShardClient::new(
            7,
            address.to_string(),
            "shard.test",
            client_tls,
            Duration::from_millis(500),
        )
        .unwrap();
        let snapshot = client.snapshot().unwrap();
        let query = Query {
            text: "payment service".into(),
            tenant: Some("tenant-a".into()),
            ..Query::default()
        };
        let complete = client.search_at(snapshot.clone(), query.clone()).unwrap();
        assert!(!complete.hits.is_empty());
        let candidates = client.candidates(snapshot.clone(), query.clone()).unwrap();
        assert!(!candidates.candidates.is_empty());
        let selected: Vec<_> = candidates
            .candidates
            .iter()
            .map(|candidate| (candidate.part_id.clone(), candidate.row))
            .collect();
        let scored = client.rerank(snapshot.clone(), query, selected).unwrap();
        assert!(!scored.is_empty());
        let materialized = client
            .materialize(
                snapshot.clone(),
                scored
                    .iter()
                    .map(|row| (row.part_id.clone(), row.row))
                    .collect(),
            )
            .unwrap();
        assert_eq!(materialized.len(), scored.len());
        assert!(materialized
            .iter()
            .all(|(event, _)| event.tenant_id == "tenant-a"));
        assert_eq!(snapshot.snapshot_id, client.health().unwrap().snapshot_id);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn coordinator_cannot_mutate_catalog_snapshot_bytes() {
        let shard = engine();
        shard
            .ingest(vec![event("e1", "payment service timeout")], 2)
            .unwrap();
        let (server_tls, client_tls, _) = tls_pair();
        let (address, server) = connection_server(server_tls, shard, 2);
        let client = TlsShardClient::new(
            7,
            address.to_string(),
            "shard.test",
            client_tls,
            Duration::from_millis(500),
        )
        .unwrap();
        let mut snapshot = client.snapshot().unwrap();
        snapshot.created_at_ms += 1;
        let error = client
            .candidates(snapshot, Query::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("immutable catalog bytes"), "{error}");
        server.join().unwrap().unwrap();
    }

    #[test]
    fn selected_parts_must_belong_to_the_pinned_snapshot() {
        let shard = engine();
        shard
            .ingest(vec![event("e1", "payment service timeout")], 2)
            .unwrap();
        let snapshot = shard.snapshot().unwrap();
        let (server_tls, _, _) = tls_pair();
        let server = ShardRpcServer::new(7, shard, server_tls, Duration::from_millis(500)).unwrap();
        let error = server
            .process(RpcRequest {
                version: SHARD_RPC_PROTOCOL_VERSION,
                request_id: "foreign-part".into(),
                target_shard: 7,
                operation: RpcOperation::Materialize {
                    snapshot,
                    selected: vec![("p99999999-deadbeef".into(), 0)],
                },
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("not in pinned snapshot"), "{error}");
    }

    #[test]
    fn remote_coordinator_is_byte_identical_to_the_in_process_cluster() {
        let (root, expected, topology, client_tls, servers) = two_shard_fixture([8, 8]);
        let remote =
            RemoteReadCluster::connect(topology, client_tls, Duration::from_millis(500)).unwrap();
        assert_eq!(remote.num_shards(), 2);
        remote.readiness(false).unwrap();
        let error = remote.readiness(true).unwrap_err().to_string();
        assert!(error.contains("read-only"), "{error}");
        assert!(error.contains("policy grants ingest"), "{error}");
        let actual = remote
            .search(&Query {
                text: "payment service".into(),
                k: 4,
                candidates: 8,
                rerank: 8,
                ..Query::default()
            })
            .unwrap();
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        for server in servers {
            server.join().unwrap().unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remote_partition_is_fail_named_or_explicitly_labelled() {
        // Shard 1 accepts only the construction health check and then disappears. Shard 0 serves
        // the default failure attempt, one best-effort query, and one refused partial GROUP BY.
        let (root, _, topology, client_tls, servers) = two_shard_fixture([11, 1]);
        let remote =
            RemoteReadCluster::connect(topology, client_tls, Duration::from_millis(500)).unwrap();
        let base = Query {
            text: "payment service".into(),
            k: 4,
            candidates: 8,
            rerank: 8,
            ..Query::default()
        };
        let error = remote.search(&base).unwrap_err().to_string();
        assert!(error.contains("shard 1 unreachable"), "{error}");
        assert!(error.contains("did not opt in"), "{error}");

        let partial = remote
            .search(&Query {
                best_effort: true,
                ..base.clone()
            })
            .unwrap();
        assert_eq!(partial.missing_shards.len(), 1);
        assert_eq!(partial.missing_shards[0].shard, 1);
        assert_eq!(partial.counters.shards_missing, 1);

        let error = remote
            .search(&Query {
                best_effort: true,
                group_k: Some(2),
                ..base
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("GROUP BY"), "{error}");
        assert!(error.contains("refused"), "{error}");

        for server in servers {
            server.join().unwrap().unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn materialization_partition_recomputes_the_best_effort_topk() {
        // Shard 1 survives health, snapshot pin, validation, candidates, and rerank, then disappears
        // before final survivor bodies are fetched. Shard 0 is materialized once before that failure
        // and once again after the coordinator removes shard 1's scores and recomputes the top-k.
        let (root, _, topology, client_tls, servers) = two_shard_fixture([7, 5]);
        let remote =
            RemoteReadCluster::connect(topology, client_tls, Duration::from_millis(500)).unwrap();
        let partial = remote
            .search(&Query {
                text: "payment service".into(),
                k: 4,
                candidates: 8,
                rerank: 8,
                best_effort: true,
                ..Query::default()
            })
            .unwrap();
        assert_eq!(partial.missing_shards.len(), 1);
        assert_eq!(partial.missing_shards[0].shard, 1);
        assert_eq!(partial.counters.shards_missing, 1);
        assert!(!partial.hits.is_empty());
        assert!(partial
            .hits
            .iter()
            .all(|hit| remote.shard_index(&hit.event.tenant_id) == 0));
        for server in servers {
            server.join().unwrap().unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remote_topology_is_versioned_bounded_and_contiguous() {
        let invalid = RemoteReadTopology {
            version: 1,
            shards: vec![RemoteShardEndpoint {
                shard_id: 1,
                address: "127.0.0.1:1".into(),
                server_name: "shard.test".into(),
            }],
        };
        let error = invalid.validate().unwrap_err().to_string();
        assert!(error.contains("contiguous range"), "{error}");

        let invalid = RemoteReadTopology {
            version: 2,
            shards: vec![RemoteShardEndpoint {
                shard_id: 0,
                address: "127.0.0.1:1".into(),
                server_name: "shard.test".into(),
            }],
        };
        let error = invalid.validate().unwrap_err().to_string();
        assert!(error.contains("topology version"), "{error}");
    }

    #[test]
    fn remote_preflight_refuses_mixed_store_configuration() {
        let first = engine();
        let second_root = temp("mixed-config");
        let second = Arc::new(
            Engine::init(
                &second_root,
                StoreConfig {
                    format_version: STORE_VERSION,
                    dim: 8,
                    nlist: 2,
                    pq_m: 2,
                    seed: 99,
                    kmeans_restarts: 2,
                    block_size: 4096,
                    partitions: PartitionScheme::default(),
                    promote: Vec::new(),
                },
            )
            .unwrap(),
        );
        let (server_tls, client_tls, _) = tls_pair();
        let (address0, server0) = connection_server_for(0, Arc::clone(&server_tls), first, 1);
        let (address1, server1) = connection_server_for(1, server_tls, second, 1);
        let error = match RemoteReadCluster::connect(
            RemoteReadTopology {
                version: 1,
                shards: vec![
                    RemoteShardEndpoint {
                        shard_id: 0,
                        address: address0.to_string(),
                        server_name: "shard.test".into(),
                    },
                    RemoteShardEndpoint {
                        shard_id: 1,
                        address: address1.to_string(),
                        server_name: "shard.test".into(),
                    },
                ],
            },
            client_tls,
            Duration::from_millis(500),
        ) {
            Ok(_) => panic!("mixed store configurations must be refused"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("different immutable store configuration"),
            "{error}"
        );
        server0.join().unwrap().unwrap();
        server1.join().unwrap().unwrap();
        fs::remove_dir_all(second_root).unwrap();
    }

    #[test]
    fn untrusted_coordinator_certificate_is_rejected() {
        let (server_tls, _, rogue_tls) = tls_pair();
        let (address, server) = one_connection_server(server_tls);
        let client = TlsShardClient::new(
            7,
            address.to_string(),
            "shard.test",
            rogue_tls,
            Duration::from_millis(500),
        )
        .unwrap();
        assert!(client
            .health()
            .unwrap_err()
            .to_string()
            .contains("shard RPC"));
        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn wrong_server_name_is_rejected() {
        let (server_tls, client_tls, _) = tls_pair();
        let (address, server) = one_connection_server(server_tls);
        let client = TlsShardClient::new(
            7,
            address.to_string(),
            "other-shard.test",
            client_tls,
            Duration::from_millis(500),
        )
        .unwrap();
        assert!(client
            .health()
            .unwrap_err()
            .to_string()
            .contains("shard RPC"));
        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn half_open_peer_hits_the_read_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let peer = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            accepted_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(250));
        });
        let (_, client_tls, _) = tls_pair();
        let client = TlsShardClient::new(
            7,
            address.to_string(),
            "shard.test",
            client_tls,
            Duration::from_millis(50),
        )
        .unwrap();
        let started = std::time::Instant::now();
        let error = client.health().unwrap_err().to_string();
        accepted_rx.recv().unwrap();
        assert!(error.contains("shard RPC"));
        assert!(started.elapsed() < Duration::from_millis(500));
        peer.join().unwrap();
    }

    #[test]
    fn framing_refuses_an_oversized_length_before_allocating() {
        let prefix = ((MAX_SHARD_RPC_FRAME_BYTES as u32) + 1).to_be_bytes();
        let mut input = prefix.as_slice();
        let error = read_frame(&mut input).unwrap_err().to_string();
        assert!(error.contains("outside"));
        assert!(error.contains(&MAX_SHARD_RPC_FRAME_BYTES.to_string()));
    }

    #[test]
    fn protocol_version_and_target_shard_are_bound_before_dispatch() {
        let (tls, _, _) = tls_pair();
        let server = ShardRpcServer::new(7, engine(), tls, Duration::from_millis(500)).unwrap();
        let wrong_version = server
            .process(RpcRequest {
                version: SHARD_RPC_PROTOCOL_VERSION + 1,
                request_id: "version-mismatch".into(),
                target_shard: 7,
                operation: RpcOperation::Health,
            })
            .unwrap_err()
            .to_string();
        assert!(wrong_version.contains("unsupported protocol version"));

        let wrong_shard = server
            .process(RpcRequest {
                version: SHARD_RPC_PROTOCOL_VERSION,
                request_id: "shard-mismatch".into(),
                target_shard: 8,
                operation: RpcOperation::Health,
            })
            .unwrap_err()
            .to_string();
        assert!(wrong_shard.contains("targets shard 8"));
        assert!(wrong_shard.contains("serves shard 7"));
    }

    #[test]
    fn tls_config_files_are_bounded_before_allocation() {
        let path = temp("oversized-pem");
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_SHARD_RPC_TLS_FILE_BYTES + 1).unwrap();
        let error = read_regular_file(&path, "test PEM")
            .unwrap_err()
            .to_string();
        assert!(error.contains("size"));
        assert!(error.contains(&MAX_SHARD_RPC_TLS_FILE_BYTES.to_string()));
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_key_file_allows_group_read_but_refuses_other_access() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp("permissive-key");
        fs::write(&path, b"not-a-key").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let parse_error = read_private_key(&path).unwrap_err().to_string();
        assert!(parse_error.contains("parse private key"), "{parse_error}");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = read_private_key(&path).unwrap_err().to_string();
        assert!(error.contains("permissions are 644"), "{error}");
        fs::remove_file(path).unwrap();
    }
}
