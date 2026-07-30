//! Authenticated coordinator↔shard transport.
//!
//! The wire is deliberately small: one mutual-TLS connection carries one bounded,
//! length-prefixed JSON request and one response. The operations are read-only and
//! mirror the three fragment calls used by [`crate::sharded::Cluster`]. A remote
//! mutation or ownership takeover is intentionally absent until the admission log is
//! remote-durable; exposing writes here would weaken the ack contract.

use crate::search::{ShardCandidate, ShardScored};
use crate::Engine;
use prism_part::catalog::Snapshot;
use prism_types::error::{PrismError, Result};
use prism_types::{Event, Query};
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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const SHARD_RPC_PROTOCOL_VERSION: u16 = 1;
pub const MAX_SHARD_RPC_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SHARD_RPC_SELECTIONS: usize = 10_000;
pub const MAX_SHARD_RPC_CONNECTIONS: usize = 64;
pub const MAX_SHARD_RPC_TLS_FILE_BYTES: u64 = 4 * 1024 * 1024;
pub const DEFAULT_SHARD_RPC_TIMEOUT: Duration = Duration::from_secs(5);

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
    Candidates {
        snapshot: Snapshot,
        query: Query,
    },
    Rerank {
        query: Query,
        selected: Vec<(String, usize)>,
    },
    Materialize {
        selected: Vec<(String, usize)>,
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
    Ok { payload: RpcPayload },
    Error { code: String, message: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum RpcPayload {
    Health(ShardHealth),
    Snapshot(Snapshot),
    Candidates(Vec<ShardCandidate>),
    Rerank(Vec<ShardScored>),
    Materialize(Vec<(Event, u32)>),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShardHealth {
    pub protocol_version: u16,
    pub shard_id: usize,
    pub snapshot_id: String,
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
        if mode & 0o077 != 0 {
            return Err(invalid_transport(format!(
                "private key {} permissions are {mode:03o}; expected 0600 or stricter",
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
/// The private-key path must be a regular non-symlink file with mode 0600 or stricter on Unix.
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

/// A read-only shard RPC server. Every accepted connection must complete a mutual-TLS handshake.
pub struct ShardRpcServer {
    shard_id: usize,
    engine: Arc<Engine>,
    tls: Arc<ServerConfig>,
    timeout: Duration,
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
        })
    }

    pub fn bind_and_serve(self, address: SocketAddr) -> Result<()> {
        let listener =
            TcpListener::bind(address).map_err(|e| transport_error("bind listener", e))?;
        self.serve(listener)
    }

    /// Serve forever with a fixed upper bound on concurrent TLS handshakes and requests.
    pub fn serve(self, listener: TcpListener) -> Result<()> {
        let active = Arc::new(AtomicUsize::new(0));
        let server = Arc::new(self);
        for accepted in listener.incoming() {
            let stream = accepted.map_err(|e| transport_error("accept connection", e))?;
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
                outcome: RpcOutcome::Ok { payload },
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
            })),
            RpcOperation::Snapshot => Ok(RpcPayload::Snapshot(self.engine.snapshot()?)),
            RpcOperation::Candidates { snapshot, query } => Ok(RpcPayload::Candidates(
                self.engine.search_candidates(&snapshot, &query)?,
            )),
            RpcOperation::Rerank { query, selected } => {
                validate_selection(&selected)?;
                Ok(RpcPayload::Rerank(
                    self.engine.search_rerank_selected(&query, &selected)?,
                ))
            }
            RpcOperation::Materialize { selected } => {
                validate_selection(&selected)?;
                Ok(RpcPayload::Materialize(
                    self.engine.search_materialize(&selected)?,
                ))
            }
        }
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

    pub fn candidates(&self, snapshot: Snapshot, query: Query) -> Result<Vec<ShardCandidate>> {
        match self.call(RpcOperation::Candidates { snapshot, query })? {
            RpcPayload::Candidates(candidates) => Ok(candidates),
            _ => Err(invalid_transport(
                "candidates returned the wrong payload type",
            )),
        }
    }

    pub fn rerank(&self, query: Query, selected: Vec<(String, usize)>) -> Result<Vec<ShardScored>> {
        validate_selection(&selected)?;
        match self.call(RpcOperation::Rerank { query, selected })? {
            RpcPayload::Rerank(scored) => Ok(scored),
            _ => Err(invalid_transport("rerank returned the wrong payload type")),
        }
    }

    pub fn materialize(&self, selected: Vec<(String, usize)>) -> Result<Vec<(Event, u32)>> {
        validate_selection(&selected)?;
        match self.call(RpcOperation::Materialize { selected })? {
            RpcPayload::Materialize(events) => Ok(events),
            _ => Err(invalid_transport(
                "materialize returned the wrong payload type",
            )),
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
            RpcOutcome::Ok { payload } => Ok(payload),
            RpcOutcome::Error { code, message } => Err(PrismError::Io(format!(
                "remote shard {} returned {code}: {message}",
                self.shard_id
            ))),
        }
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
    use prism_part::partition::PartitionScheme;
    use prism_part::store::{StoreConfig, STORE_VERSION};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::mpsc;

    fn temp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "prism-rpc-{tag}-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
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

    fn generate_ca(dir: &Path, prefix: &str) {
        let key = format!("{prefix}-key.pem");
        let cert = format!("{prefix}.pem");
        let subject = format!("/CN={prefix}");
        openssl(
            dir,
            &[
                "req",
                "-x509",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
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
        openssl(
            dir,
            &[
                "req",
                "-new",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = ShardRpcServer::new(7, engine, tls, Duration::from_millis(500)).unwrap();
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
        server.join().unwrap().unwrap();
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
        let (address, server) = connection_server(server_tls, shard, 5);
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
        let candidates = client.candidates(snapshot.clone(), query.clone()).unwrap();
        assert!(!candidates.is_empty());
        let selected: Vec<_> = candidates
            .iter()
            .map(|candidate| (candidate.part_id.clone(), candidate.row))
            .collect();
        let scored = client.rerank(query, selected).unwrap();
        assert!(!scored.is_empty());
        let materialized = client
            .materialize(
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
    fn private_key_file_must_not_be_group_or_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp("permissive-key");
        fs::write(&path, b"not-a-key").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = read_private_key(&path).unwrap_err().to_string();
        assert!(error.contains("permissions are 644"), "{error}");
        fs::remove_file(path).unwrap();
    }
}
