//! Production authenticated read/write service boundary for PrismDB.
//!
//! The public boundary is intentionally smaller than [`prism_types::Query`]. A caller cannot
//! choose a tenant, physical plan, shard-partial mode, or internal tuning control outside the
//! authenticated policy attached to its client certificate.

use prism_engine::shard_rpc::RemoteReadCluster;
use prism_engine::IngestReport2;
use prism_types::error::{PrismError, Result};
use prism_types::hash::{hex, sha256};
use prism_types::{Event, Query, SearchResult};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const PUBLIC_API_VERSION: &str = "v1";
pub const MAX_POLICY_BYTES: u64 = 1024 * 1024;
pub const MAX_POLICY_CLIENTS: usize = 1024;
pub const MAX_TENANTS_PER_CLIENT: usize = 1024;
pub const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;
pub const MAX_HTTP_BODY_BYTES: usize = 256 * 1024;
pub const MAX_QUERY_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_RESULT_K: usize = 100;
pub const MAX_INGEST_EVENTS: usize = prism_engine::shard_rpc::MAX_SHARD_RPC_EVENTS;
pub const MAX_PUBLIC_CONNECTIONS: usize = 128;
pub const DEFAULT_PUBLIC_TIMEOUT: Duration = Duration::from_secs(15);

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn invalid(message: impl Into<String>) -> PrismError {
    PrismError::Invalid(format!("public API: {}", message.into()))
}

fn io_error(context: &str, error: impl std::fmt::Display) -> PrismError {
    PrismError::Io(format!("public API {context}: {error}"))
}

fn read_regular_file(path: &Path, label: &str, max_bytes: u64) -> Result<Vec<u8>> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| io_error(&format!("inspect {label}"), error))?;
    if !metadata.file_type().is_file() {
        return Err(invalid(format!(
            "{label} {} must be a regular, non-symlink file",
            path.display()
        )));
    }
    if metadata.len() == 0 || metadata.len() > max_bytes {
        return Err(invalid(format!(
            "{label} {} size {} is outside 1..={max_bytes}",
            path.display(),
            metadata.len()
        )));
    }
    fs::read(path).map_err(|error| io_error(&format!("read {label}"), error))
}

fn valid_name(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyDocument {
    version: u16,
    clients: Vec<ClientPolicy>,
}

/// The authorization attached to one exact client certificate.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientPolicy {
    pub identity_id: String,
    pub certificate_sha256: String,
    pub tenants: Vec<String>,
    pub scopes: Vec<String>,
    pub max_in_flight: usize,
}

impl ClientPolicy {
    fn validate(&self) -> Result<()> {
        if !valid_name(&self.identity_id, 128) {
            return Err(invalid(format!(
                "identity_id `{}` must be 1..128 safe ASCII characters",
                self.identity_id
            )));
        }
        if self.certificate_sha256.len() != 64
            || !self
                .certificate_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(invalid(format!(
                "identity `{}` certificate_sha256 must be 64 lowercase hexadecimal characters",
                self.identity_id
            )));
        }
        if self.tenants.len() > MAX_TENANTS_PER_CLIENT
            || ((self.permits("search") || self.permits("ingest")) && self.tenants.is_empty())
        {
            return Err(invalid(format!(
                "identity `{}` needs 1..={MAX_TENANTS_PER_CLIENT} tenants when search or ingest is granted",
                self.identity_id
            )));
        }
        if self.tenants.iter().any(|tenant| !valid_name(tenant, 128)) {
            return Err(invalid(format!(
                "identity `{}` has an invalid tenant; wildcards and untrusted names are refused",
                self.identity_id
            )));
        }
        if self.tenants.iter().collect::<BTreeSet<_>>().len() != self.tenants.len() {
            return Err(invalid(format!(
                "identity `{}` contains duplicate tenants",
                self.identity_id
            )));
        }
        let allowed_scopes = ["health", "ingest", "metrics", "search"];
        if self.scopes.is_empty()
            || self
                .scopes
                .iter()
                .any(|scope| !allowed_scopes.contains(&scope.as_str()))
        {
            return Err(invalid(format!(
                "identity `{}` scopes must be a non-empty subset of health, ingest, metrics, search",
                self.identity_id
            )));
        }
        if self.scopes.iter().collect::<BTreeSet<_>>().len() != self.scopes.len() {
            return Err(invalid(format!(
                "identity `{}` contains duplicate scopes",
                self.identity_id
            )));
        }
        if !(1..=64).contains(&self.max_in_flight) {
            return Err(invalid(format!(
                "identity `{}` max_in_flight must be between 1 and 64",
                self.identity_id
            )));
        }
        Ok(())
    }

    fn permits(&self, scope: &str) -> bool {
        self.scopes.iter().any(|candidate| candidate == scope)
    }

    fn permits_tenant(&self, tenant: &str) -> bool {
        self.tenants.iter().any(|candidate| candidate == tenant)
    }
}

#[derive(Debug)]
struct Identity {
    policy: ClientPolicy,
    in_flight: AtomicUsize,
}

/// Immutable, exact-certificate authorization policy.
#[derive(Debug)]
pub struct AuthorizationPolicy {
    identities: BTreeMap<String, Arc<Identity>>,
}

impl AuthorizationPolicy {
    pub fn load(path: &Path) -> Result<Self> {
        Self::from_slice(&read_regular_file(
            path,
            "authorization policy",
            MAX_POLICY_BYTES,
        )?)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() as u64 > MAX_POLICY_BYTES {
            return Err(invalid(format!(
                "authorization policy size is outside 1..={MAX_POLICY_BYTES}"
            )));
        }
        let document: PolicyDocument = serde_json::from_slice(bytes)?;
        if document.version != 1 {
            return Err(invalid(format!(
                "unsupported authorization policy version {}; expected 1",
                document.version
            )));
        }
        if document.clients.is_empty() || document.clients.len() > MAX_POLICY_CLIENTS {
            return Err(invalid(format!(
                "authorization policy needs 1..={MAX_POLICY_CLIENTS} clients"
            )));
        }
        let mut identities = BTreeMap::new();
        let mut names = BTreeSet::new();
        for client in document.clients {
            client.validate()?;
            if !names.insert(client.identity_id.clone()) {
                return Err(invalid(format!(
                    "duplicate identity_id `{}`",
                    client.identity_id
                )));
            }
            let fingerprint = client.certificate_sha256.clone();
            if identities
                .insert(
                    fingerprint.clone(),
                    Arc::new(Identity {
                        policy: client,
                        in_flight: AtomicUsize::new(0),
                    }),
                )
                .is_some()
            {
                return Err(invalid(format!(
                    "duplicate certificate fingerprint `{fingerprint}`"
                )));
            }
        }
        Ok(Self { identities })
    }

    fn authenticate(&self, certificate_der: &[u8]) -> Option<Arc<Identity>> {
        self.identities
            .get(&hex(&sha256(certificate_der)))
            .map(Arc::clone)
    }

    fn requires_writable_backend(&self) -> bool {
        self.identities
            .values()
            .any(|identity| identity.policy.permits("ingest"))
    }
}

/// Backend abstraction retained so the network and authorization boundary can be tested without
/// weakening the real remote coordinator.
pub trait ReadServiceBackend: Send + Sync {
    fn search(&self, query: &Query) -> Result<SearchResult>;
    fn ingest(&self, events: Vec<Event>, now_ms: i64) -> Result<IngestReport2>;
    fn readiness(&self, require_writable: bool) -> Result<()>;
}

impl ReadServiceBackend for RemoteReadCluster {
    fn search(&self, query: &Query) -> Result<SearchResult> {
        RemoteReadCluster::search(self, query)
    }

    fn ingest(&self, events: Vec<Event>, now_ms: i64) -> Result<IngestReport2> {
        RemoteReadCluster::ingest(self, events, now_ms)
    }

    fn readiness(&self, require_writable: bool) -> Result<()> {
        self.readiness(require_writable)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestRequest {
    tenant: String,
    events: Vec<PublicEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicEvent {
    event_id: String,
    event_time: i64,
    event_name: String,
    cost: f64,
    error: bool,
    body: String,
    #[serde(default)]
    trace_id: String,
    #[serde(default)]
    span_id: String,
    #[serde(default)]
    attributes: prism_types::attributes::Attributes,
    #[serde(default)]
    idempotency_key: Option<String>,
}

#[derive(Debug, Serialize)]
struct PublicIngestReport {
    offered: usize,
    published: usize,
    duplicates_suppressed: usize,
    dead_lettered: usize,
    by_reason: BTreeMap<String, usize>,
    snapshot_id: String,
}

impl From<IngestReport2> for PublicIngestReport {
    fn from(report: IngestReport2) -> Self {
        Self {
            offered: report.offered,
            published: report.published,
            duplicates_suppressed: report.duplicates_suppressed,
            dead_lettered: report.dead_lettered,
            by_reason: report.by_reason,
            snapshot_id: report.snapshot_id,
        }
    }
}

impl IngestRequest {
    fn into_events(self, observed_time: i64) -> Result<(String, Vec<Event>)> {
        if !valid_name(&self.tenant, 128) {
            return Err(invalid("tenant must be 1..128 safe ASCII characters"));
        }
        if self.events.is_empty() || self.events.len() > MAX_INGEST_EVENTS {
            return Err(invalid(format!(
                "events must contain 1..={MAX_INGEST_EVENTS} items"
            )));
        }
        let tenant = self.tenant;
        let events = self
            .events
            .into_iter()
            .map(|event| Event {
                event_id: event.event_id,
                tenant_id: tenant.clone(),
                event_time: event.event_time,
                observed_time,
                event_name: event.event_name,
                cost: event.cost,
                error: event.error,
                body: event.body,
                trace_id: event.trace_id,
                span_id: event.span_id,
                attributes: event.attributes,
                idempotency_key: event.idempotency_key,
            })
            .collect();
        Ok((tenant, events))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchRequest {
    tenant: String,
    text: String,
    #[serde(default)]
    time_from: Option<i64>,
    #[serde(default)]
    time_to: Option<i64>,
    #[serde(default = "default_k")]
    k: usize,
    #[serde(default)]
    group_k: Option<usize>,
    #[serde(default)]
    space: Option<String>,
    #[serde(default)]
    threshold: Option<f32>,
    #[serde(default)]
    explain: bool,
    #[serde(default)]
    fetch_budget_bytes: Option<usize>,
}

fn default_k() -> usize {
    10
}

impl SearchRequest {
    fn into_query(self) -> Result<Query> {
        if !valid_name(&self.tenant, 128) {
            return Err(invalid("tenant must be 1..128 safe ASCII characters"));
        }
        if self.text.trim().is_empty() || self.text.len() > MAX_QUERY_TEXT_BYTES {
            return Err(invalid(format!(
                "text length must be within 1..={MAX_QUERY_TEXT_BYTES} bytes"
            )));
        }
        if !(1..=MAX_RESULT_K).contains(&self.k) {
            return Err(invalid(format!("k must be within 1..={MAX_RESULT_K}")));
        }
        if let Some(group_k) = self.group_k {
            if group_k == 0 || group_k > self.k {
                return Err(invalid("group_k must be between 1 and k"));
            }
        }
        if let (Some(from), Some(to)) = (self.time_from, self.time_to) {
            if from > to {
                return Err(invalid("time_from must not be after time_to"));
            }
        }
        if self
            .threshold
            .is_some_and(|threshold| !threshold.is_finite() || !(-1.0..=1.0).contains(&threshold))
        {
            return Err(invalid("threshold must be finite and between -1 and 1"));
        }
        if self
            .space
            .as_deref()
            .is_some_and(|space| space.is_empty() || space.len() > 256)
        {
            return Err(invalid("space must be 1..256 characters when supplied"));
        }
        Ok(Query {
            text: self.text,
            tenant: Some(self.tenant),
            time_from: self.time_from,
            time_to: self.time_to,
            k: self.k,
            group_k: self.group_k,
            space: self.space,
            threshold: self.threshold,
            explain: self.explain,
            fetch_budget_bytes: self.fetch_budget_bytes,
            ..Query::default()
        })
    }
}

#[derive(Default)]
pub struct ServiceMetrics {
    accepted_connections: AtomicU64,
    rejected_connections: AtomicU64,
    requests: AtomicU64,
    searches: AtomicU64,
    ingests: AtomicU64,
    ingested_events: AtomicU64,
    duplicate_events: AtomicU64,
    dead_lettered_events: AtomicU64,
    failures: AtomicU64,
    unauthorized: AtomicU64,
    rate_limited: AtomicU64,
    in_flight: AtomicUsize,
    search_latency_micros: AtomicU64,
    ingest_latency_micros: AtomicU64,
}

impl ServiceMetrics {
    pub fn encode(&self) -> String {
        format!(
            "# TYPE prism_api_connections_total counter\n\
             prism_api_connections_total {}\n\
             # TYPE prism_api_connections_rejected_total counter\n\
             prism_api_connections_rejected_total {}\n\
             # TYPE prism_api_requests_total counter\n\
             prism_api_requests_total {}\n\
             # TYPE prism_api_searches_total counter\n\
             prism_api_searches_total {}\n\
             # TYPE prism_api_ingests_total counter\n\
             prism_api_ingests_total {}\n\
             # TYPE prism_api_ingested_events_total counter\n\
             prism_api_ingested_events_total {}\n\
             # TYPE prism_api_duplicate_events_total counter\n\
             prism_api_duplicate_events_total {}\n\
             # TYPE prism_api_dead_lettered_events_total counter\n\
             prism_api_dead_lettered_events_total {}\n\
             # TYPE prism_api_failures_total counter\n\
             prism_api_failures_total {}\n\
             # TYPE prism_api_unauthorized_total counter\n\
             prism_api_unauthorized_total {}\n\
             # TYPE prism_api_rate_limited_total counter\n\
             prism_api_rate_limited_total {}\n\
             # TYPE prism_api_in_flight gauge\n\
             prism_api_in_flight {}\n\
             # TYPE prism_api_search_latency_seconds_total counter\n\
             prism_api_search_latency_seconds_total {:.6}\n\
             # TYPE prism_api_ingest_latency_seconds_total counter\n\
             prism_api_ingest_latency_seconds_total {:.6}\n",
            self.accepted_connections.load(Ordering::Relaxed),
            self.rejected_connections.load(Ordering::Relaxed),
            self.requests.load(Ordering::Relaxed),
            self.searches.load(Ordering::Relaxed),
            self.ingests.load(Ordering::Relaxed),
            self.ingested_events.load(Ordering::Relaxed),
            self.duplicate_events.load(Ordering::Relaxed),
            self.dead_lettered_events.load(Ordering::Relaxed),
            self.failures.load(Ordering::Relaxed),
            self.unauthorized.load(Ordering::Relaxed),
            self.rate_limited.load(Ordering::Relaxed),
            self.in_flight.load(Ordering::Relaxed),
            self.search_latency_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            self.ingest_latency_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0,
        )
    }
}

struct InFlightGuard<'a> {
    counter: &'a AtomicUsize,
}

struct OwnedInFlightGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for OwnedInFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

fn try_enter(counter: &AtomicUsize, limit: usize) -> Option<InFlightGuard<'_>> {
    let previous = counter.fetch_add(1, Ordering::SeqCst);
    if previous >= limit {
        counter.fetch_sub(1, Ordering::SeqCst);
        None
    } else {
        Some(InFlightGuard { counter })
    }
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn read_http_request<R: Read>(reader: &mut R) -> Result<HttpRequest> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() >= MAX_HTTP_HEADER_BYTES {
            return Err(invalid(format!(
                "HTTP headers exceed {MAX_HTTP_HEADER_BYTES} bytes"
            )));
        }
        reader
            .read_exact(&mut byte)
            .map_err(|error| io_error("read HTTP headers", error))?;
        header.push(byte[0]);
    }
    let text = std::str::from_utf8(&header).map_err(|_| invalid("HTTP headers are not UTF-8"))?;
    let mut lines = text[..text.len() - 4].split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| invalid("HTTP request line is missing"))?;
    let fields: Vec<&str> = request_line.split(' ').collect();
    if fields.len() != 3 || fields[2] != "HTTP/1.1" {
        return Err(invalid("request line must use HTTP/1.1"));
    }
    if !matches!(fields[0], "GET" | "POST")
        || !fields[1].starts_with('/')
        || fields[1].len() > 128
        || !fields[1]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
    {
        return Err(invalid("unsupported HTTP method or request target"));
    }
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("malformed HTTP header"))?;
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || headers.insert(name.clone(), value).is_some()
        {
            return Err(invalid(format!("duplicate or empty HTTP header `{name}`")));
        }
    }
    if !matches!(headers.get("host"), Some(host) if !host.is_empty()) {
        return Err(invalid("HTTP/1.1 Host header is required"));
    }
    if headers.contains_key("transfer-encoding") {
        return Err(invalid("Transfer-Encoding is not supported"));
    }
    let content_length = headers
        .get("content-length")
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| invalid("Content-Length is not a valid integer"))
        })
        .transpose()?
        .unwrap_or(0);
    if content_length > MAX_HTTP_BODY_BYTES {
        return Err(invalid(format!(
            "HTTP body exceeds {MAX_HTTP_BODY_BYTES} bytes"
        )));
    }
    if fields[0] == "POST"
        && headers.get("content-type").map(String::as_str) != Some("application/json")
    {
        return Err(invalid("POST requires Content-Type: application/json"));
    }
    if fields[0] == "GET" && content_length != 0 {
        return Err(invalid("GET requests must not carry a body"));
    }
    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|error| io_error("read HTTP body", error))?;
    Ok(HttpRequest {
        method: fields[0].to_string(),
        path: fields[1].to_string(),
        headers,
        body,
    })
}

struct HttpResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl HttpResponse {
    fn json(status: u16, value: &impl Serialize) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_else(|_| b"{\"error\":\"encode\"}".to_vec()),
        }
    }

    fn text(status: u16, content_type: &'static str, body: String) -> Self {
        Self {
            status,
            content_type,
            body: body.into_bytes(),
        }
    }
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn write_http_response<W: Write>(writer: &mut W, response: &HttpResponse) -> Result<()> {
    let headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        response.status,
        status_reason(response.status),
        response.content_type,
        response.body.len()
    );
    writer
        .write_all(headers.as_bytes())
        .and_then(|_| writer.write_all(&response.body))
        .and_then(|_| writer.flush())
        .map_err(|error| io_error("write HTTP response", error))
}

fn write_http_response_and_close(
    tls: &mut StreamOwned<ServerConnection, TcpStream>,
    response: &HttpResponse,
) -> Result<()> {
    write_http_response(tls, response)?;
    tls.conn.send_close_notify();
    tls.flush()
        .map_err(|error| io_error("send TLS close notification", error))
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    message: &'a str,
    request_id: &'a str,
}

fn error_response(status: u16, code: &str, message: &str, request_id: &str) -> HttpResponse {
    HttpResponse::json(
        status,
        &ErrorBody {
            error: code,
            message,
            request_id,
        },
    )
}

fn request_id(request: &HttpRequest) -> Result<String> {
    match request.headers.get("x-request-id") {
        Some(value) if valid_name(value, 128) => Ok(value.clone()),
        Some(_) => Err(invalid(
            "X-Request-Id is not a safe 1..128 character identifier",
        )),
        None => Ok(format!(
            "prism-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )),
    }
}

/// Mutual-TLS HTTP/1.1 server for tenant-scoped reads and writes.
pub struct ReadService {
    backend: Arc<dyn ReadServiceBackend>,
    policy: Arc<AuthorizationPolicy>,
    tls: Arc<ServerConfig>,
    timeout: Duration,
    metrics: Arc<ServiceMetrics>,
}

impl ReadService {
    pub fn new(
        backend: Arc<dyn ReadServiceBackend>,
        policy: Arc<AuthorizationPolicy>,
        tls: Arc<ServerConfig>,
        timeout: Duration,
    ) -> Result<Self> {
        if !(Duration::from_millis(100)..=Duration::from_secs(60)).contains(&timeout) {
            return Err(invalid(
                "timeout must be between 100 milliseconds and 60 seconds",
            ));
        }
        Ok(Self {
            backend,
            policy,
            tls,
            timeout,
            metrics: Arc::new(ServiceMetrics::default()),
        })
    }

    pub fn metrics(&self) -> Arc<ServiceMetrics> {
        Arc::clone(&self.metrics)
    }

    pub fn bind_and_serve(self, address: SocketAddr) -> Result<()> {
        let listener =
            TcpListener::bind(address).map_err(|error| io_error("bind listener", error))?;
        self.serve(listener, Arc::new(AtomicBool::new(false)))
    }

    /// Serve until `shutdown` is set. The nonblocking accept loop gives supervisors a bounded,
    /// graceful drain door without asynchronous runtime state.
    pub fn serve(self, listener: TcpListener, shutdown: Arc<AtomicBool>) -> Result<()> {
        listener
            .set_nonblocking(true)
            .map_err(|error| io_error("set listener nonblocking", error))?;
        let active = Arc::new(AtomicUsize::new(0));
        let shutdown_timeout = self.timeout;
        let service = Arc::new(self);
        while !shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    if active.fetch_add(1, Ordering::SeqCst) >= MAX_PUBLIC_CONNECTIONS {
                        active.fetch_sub(1, Ordering::SeqCst);
                        service
                            .metrics
                            .rejected_connections
                            .fetch_add(1, Ordering::Relaxed);
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                    service
                        .metrics
                        .accepted_connections
                        .fetch_add(1, Ordering::Relaxed);
                    let service = Arc::clone(&service);
                    let connection_guard = OwnedInFlightGuard {
                        counter: Arc::clone(&active),
                    };
                    std::thread::spawn(move || {
                        let _connection_guard = connection_guard;
                        if let Err(error) = service.handle_connection(stream) {
                            service.metrics.failures.fetch_add(1, Ordering::Relaxed);
                            eprintln!(
                                "{}",
                                serde_json::json!({
                                    "event": "request_rejected",
                                    "error": error.to_string(),
                                })
                            );
                        }
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(io_error("accept connection", error)),
            }
        }
        let deadline = Instant::now() + shutdown_timeout;
        while active.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if active.load(Ordering::SeqCst) != 0 {
            return Err(io_error(
                "graceful shutdown",
                "request drain exceeded configured timeout",
            ));
        }
        Ok(())
    }

    /// Deterministic harness door: serve exactly one connection inline.
    #[doc(hidden)]
    pub fn serve_connection(&self, stream: TcpStream) -> Result<()> {
        self.handle_connection(stream)
    }

    fn handle_connection(&self, stream: TcpStream) -> Result<()> {
        stream
            .set_read_timeout(Some(self.timeout))
            .and_then(|_| stream.set_write_timeout(Some(self.timeout)))
            .map_err(|error| io_error("set socket deadline", error))?;
        let connection = ServerConnection::new(Arc::clone(&self.tls))
            .map_err(|error| io_error("create TLS session", error))?;
        let mut tls = StreamOwned::new(connection, stream);
        let request = match read_http_request(&mut tls) {
            Ok(request) => request,
            Err(error) => {
                let response =
                    error_response(400, "invalid_request", &error.to_string(), "unknown");
                let _ = write_http_response_and_close(&mut tls, &response);
                return Err(error);
            }
        };
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let request_id = match request_id(&request) {
            Ok(request_id) => request_id,
            Err(error) => {
                let response =
                    error_response(400, "invalid_request_id", &error.to_string(), "unknown");
                write_http_response_and_close(&mut tls, &response)?;
                return Ok(());
            }
        };
        let certificate = tls
            .conn
            .peer_certificates()
            .and_then(|certificates| certificates.first())
            .ok_or_else(|| invalid("authenticated client certificate is missing"))?;
        let Some(identity) = self.policy.authenticate(certificate.as_ref()) else {
            self.metrics.unauthorized.fetch_add(1, Ordering::Relaxed);
            let response = error_response(
                403,
                "unknown_client",
                "client certificate is not authorized",
                &request_id,
            );
            write_http_response_and_close(&mut tls, &response)?;
            return Ok(());
        };
        let response = self.route(&identity, &request, &request_id);
        eprintln!(
            "{}",
            serde_json::json!({
                "event": "api_request",
                "request_id": request_id,
                "identity_id": identity.policy.identity_id,
                "method": request.method,
                "path": request.path,
                "status": response.status,
            })
        );
        write_http_response_and_close(&mut tls, &response)
    }

    fn route(
        &self,
        identity: &Arc<Identity>,
        request: &HttpRequest,
        request_id: &str,
    ) -> HttpResponse {
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/healthz") => {
                if !identity.policy.permits("health") {
                    return error_response(
                        403,
                        "scope_denied",
                        "health scope is required",
                        request_id,
                    );
                }
                HttpResponse::json(
                    200,
                    &serde_json::json!({"status": "ok", "version": PUBLIC_API_VERSION}),
                )
            }
            ("GET", "/readyz") => {
                if !identity.policy.permits("health") {
                    return error_response(
                        403,
                        "scope_denied",
                        "health scope is required",
                        request_id,
                    );
                }
                match self
                    .backend
                    .readiness(self.policy.requires_writable_backend())
                {
                    Ok(()) => HttpResponse::json(
                        200,
                        &serde_json::json!({"status": "ready", "version": PUBLIC_API_VERSION}),
                    ),
                    Err(_) => error_response(
                        503,
                        "not_ready",
                        "one or more required shards are unavailable",
                        request_id,
                    ),
                }
            }
            ("GET", "/metrics") => {
                if !identity.policy.permits("metrics") {
                    return error_response(
                        403,
                        "scope_denied",
                        "metrics scope is required",
                        request_id,
                    );
                }
                HttpResponse::text(200, "text/plain; version=0.0.4", self.metrics.encode())
            }
            ("POST", "/v1/search") => self.search(identity, request, request_id),
            ("POST", "/v1/events") => self.ingest(identity, request, request_id),
            ("GET" | "POST", _) => {
                error_response(404, "not_found", "route does not exist", request_id)
            }
            _ => error_response(
                405,
                "method_not_allowed",
                "method is not allowed",
                request_id,
            ),
        }
    }

    fn search(
        &self,
        identity: &Arc<Identity>,
        request: &HttpRequest,
        request_id: &str,
    ) -> HttpResponse {
        if !identity.policy.permits("search") {
            return error_response(403, "scope_denied", "search scope is required", request_id);
        }
        let parsed: SearchRequest = match serde_json::from_slice(&request.body) {
            Ok(parsed) => parsed,
            Err(error) => {
                return error_response(400, "invalid_json", &error.to_string(), request_id);
            }
        };
        let query = match parsed.into_query() {
            Ok(query) => query,
            Err(error) => {
                return error_response(400, "invalid_query", &error.to_string(), request_id);
            }
        };
        let tenant = query.tenant.as_deref().unwrap_or_default();
        if !identity.policy.permits_tenant(tenant) {
            self.metrics.unauthorized.fetch_add(1, Ordering::Relaxed);
            return error_response(
                403,
                "tenant_denied",
                "identity is not authorized for the requested tenant",
                request_id,
            );
        }
        let Some(_identity_guard) = try_enter(&identity.in_flight, identity.policy.max_in_flight)
        else {
            self.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
            return error_response(
                429,
                "concurrency_limit",
                "identity concurrency limit reached",
                request_id,
            );
        };
        let Some(_global_guard) = try_enter(&self.metrics.in_flight, MAX_PUBLIC_CONNECTIONS) else {
            self.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
            return error_response(
                429,
                "concurrency_limit",
                "service concurrency limit reached",
                request_id,
            );
        };
        self.metrics.searches.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let outcome = self.backend.search(&query);
        self.metrics.search_latency_micros.fetch_add(
            started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        match outcome {
            Ok(result) => HttpResponse::json(
                200,
                &serde_json::json!({"request_id": request_id, "result": result}),
            ),
            Err(error) => {
                self.metrics.failures.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "event": "search_failed",
                        "request_id": request_id,
                        "identity_id": identity.policy.identity_id,
                        "tenant": tenant,
                        "error": error.to_string(),
                    })
                );
                error_response(
                    503,
                    "search_unavailable",
                    "search could not be completed",
                    request_id,
                )
            }
        }
    }

    fn ingest(
        &self,
        identity: &Arc<Identity>,
        request: &HttpRequest,
        request_id: &str,
    ) -> HttpResponse {
        if !identity.policy.permits("ingest") {
            return error_response(403, "scope_denied", "ingest scope is required", request_id);
        }
        let parsed: IngestRequest = match serde_json::from_slice(&request.body) {
            Ok(parsed) => parsed,
            Err(error) => {
                return error_response(400, "invalid_json", &error.to_string(), request_id);
            }
        };
        let observed_time = prism_engine::engine::now_ms();
        let (tenant, events) = match parsed.into_events(observed_time) {
            Ok(value) => value,
            Err(error) => {
                return error_response(400, "invalid_ingest", &error.to_string(), request_id);
            }
        };
        if !identity.policy.permits_tenant(&tenant) {
            self.metrics.unauthorized.fetch_add(1, Ordering::Relaxed);
            return error_response(
                403,
                "tenant_denied",
                "identity is not authorized for the requested tenant",
                request_id,
            );
        }
        let Some(_identity_guard) = try_enter(&identity.in_flight, identity.policy.max_in_flight)
        else {
            self.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
            return error_response(
                429,
                "concurrency_limit",
                "identity concurrency limit reached",
                request_id,
            );
        };
        let Some(_global_guard) = try_enter(&self.metrics.in_flight, MAX_PUBLIC_CONNECTIONS) else {
            self.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
            return error_response(
                429,
                "concurrency_limit",
                "service concurrency limit reached",
                request_id,
            );
        };
        self.metrics.ingests.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let outcome = self.backend.ingest(events, observed_time);
        self.metrics.ingest_latency_micros.fetch_add(
            started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        match outcome {
            Ok(report) => {
                self.metrics
                    .ingested_events
                    .fetch_add(report.published as u64, Ordering::Relaxed);
                self.metrics
                    .duplicate_events
                    .fetch_add(report.duplicates_suppressed as u64, Ordering::Relaxed);
                self.metrics
                    .dead_lettered_events
                    .fetch_add(report.dead_lettered as u64, Ordering::Relaxed);
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "event": "ingest_completed",
                        "request_id": request_id,
                        "identity_id": identity.policy.identity_id,
                        "tenant": tenant,
                        "offered": report.offered,
                        "published": report.published,
                        "duplicates_suppressed": report.duplicates_suppressed,
                        "dead_lettered": report.dead_lettered,
                        "snapshot_id": report.snapshot_id,
                    })
                );
                let report = PublicIngestReport::from(report);
                HttpResponse::json(
                    200,
                    &serde_json::json!({"request_id": request_id, "result": report}),
                )
            }
            Err(error) => {
                self.metrics.failures.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "event": "ingest_failed",
                        "request_id": request_id,
                        "identity_id": identity.policy.identity_id,
                        "tenant": tenant,
                        "error": error.to_string(),
                    })
                );
                error_response(
                    503,
                    "ingest_unavailable",
                    "ingest could not be completed",
                    request_id,
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_engine::shard_rpc::{client_tls_from_pem, server_tls_from_pem};
    use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName};
    use rustls::{ClientConnection, StreamOwned};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Mutex;

    struct FakeBackend {
        tenants: Mutex<Vec<String>>,
        ingested: Mutex<Vec<Vec<Event>>>,
        ready: bool,
        writable: bool,
    }

    impl ReadServiceBackend for FakeBackend {
        fn search(&self, query: &Query) -> Result<SearchResult> {
            self.tenants
                .lock()
                .unwrap()
                .push(query.tenant.clone().unwrap());
            Ok(SearchResult {
                hits: Vec::new(),
                clusters: None,
                counters: Default::default(),
                generations: Vec::new(),
                bridge: None,
                explain: None,
                missing_shards: Vec::new(),
                snapshot_id: "s00000000".into(),
            })
        }

        fn readiness(&self, require_writable: bool) -> Result<()> {
            if self.ready && (!require_writable || self.writable) {
                Ok(())
            } else {
                Err(PrismError::Io("not ready".into()))
            }
        }

        fn ingest(&self, events: Vec<Event>, _now_ms: i64) -> Result<IngestReport2> {
            let published = events.len();
            self.ingested.lock().unwrap().push(events);
            Ok(IngestReport2 {
                offered: published,
                published,
                snapshot_id: "s00000001".into(),
                ..Default::default()
            })
        }
    }

    /// Fixture directories get their **own** counter.
    ///
    /// They used to share `REQUEST_SEQUENCE` with runtime request ids, so a fixture's directory
    /// name depended on how many requests other concurrently-running tests happened to have made.
    /// Unique, but **not reproducible by design** — and a fixture nobody can reproduce is a fixture
    /// nobody can debug ([issue #34](https://github.com/Bobcatsfan33/PrismDB/issues/34)).
    static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "prism-service-{tag}-{}-{}",
            std::process::id(),
            FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Every key below pins `ec_param_enc:named_curve`, because the fixture must not depend on
    /// which OpenSSL the host happens to ship. LibreSSL — what macOS puts on `PATH` as
    /// `/usr/bin/openssl` — defaults to writing EC keys with *explicit* curve parameters, while
    /// OpenSSL 3 defaults to the `prime256v1` OID. rustls accepts only the named form, so without
    /// this flag the service refuses its own test certificate and these tests fail against a TLS
    /// parse error instead of the behaviour they mean to test.
    fn openssl(dir: &Path, arguments: &[&str]) {
        let output = Command::new("openssl")
            .current_dir(dir)
            .args(arguments)
            .output()
            .expect("run openssl");
        assert!(
            output.status.success(),
            "openssl {:?}: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    const KEY_ATTEMPTS: usize = 8;

    /// Run `openssl`, then keep the key **only if rustls could actually use it**
    /// ([issue #34](https://github.com/Bobcatsfan33/PrismDB/issues/34)).
    ///
    /// A short private scalar is chance and is retried: LibreSSL emits 31 bytes whenever the value
    /// has a leading zero (~1 key in 300) and `ring` requires exactly 32. Explicit curve parameters
    /// are a generator defect and fail loudly, because retrying would hide a dropped
    /// `ec_param_enc:named_curve` pin.
    fn openssl_key(dir: &Path, key_file: &str, arguments: &[&str]) {
        for _ in 0..KEY_ATTEMPTS {
            openssl(dir, arguments);
            let pem = fs::read_to_string(dir.join(key_file))
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

    fn client_request(
        address: SocketAddr,
        tls: Arc<rustls::ClientConfig>,
        request: &[u8],
    ) -> Vec<u8> {
        let stream = TcpStream::connect(address).unwrap();
        let connection =
            ClientConnection::new(tls, ServerName::try_from("api.test").unwrap()).unwrap();
        let mut tls = StreamOwned::new(connection, stream);
        tls.write_all(request).unwrap();
        let mut response = Vec::new();
        tls.read_to_end(&mut response).unwrap();
        response
    }

    fn policy() -> AuthorizationPolicy {
        AuthorizationPolicy::from_slice(
            br#"{
                "version": 1,
                "clients": [{
                    "identity_id": "reader-a",
                    "certificate_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "tenants": ["tenant-a"],
                    "scopes": ["health", "ingest", "search"],
                    "max_in_flight": 2
                }]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn policy_refuses_duplicate_certificates_and_wildcards() {
        let duplicate = br#"{
            "version": 1,
            "clients": [
                {"identity_id":"a","certificate_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","tenants":["t"],"scopes":["search"],"max_in_flight":1},
                {"identity_id":"b","certificate_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","tenants":["t"],"scopes":["search"],"max_in_flight":1}
            ]
        }"#;
        assert!(AuthorizationPolicy::from_slice(duplicate)
            .unwrap_err()
            .to_string()
            .contains("duplicate certificate"));

        let wildcard = br#"{
            "version": 1,
            "clients": [{
                "identity_id":"a",
                "certificate_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "tenants":["*"],
                "scopes":["search"],
                "max_in_flight":1
            }]
        }"#;
        assert!(AuthorizationPolicy::from_slice(wildcard)
            .unwrap_err()
            .to_string()
            .contains("invalid tenant"));
        let health_only = AuthorizationPolicy::from_slice(
            br#"{
                "version": 1,
                "clients": [{
                    "identity_id":"probe",
                    "certificate_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "tenants":[],
                    "scopes":["health"],
                    "max_in_flight":1
                }]
            }"#,
        )
        .unwrap();
        assert!(!health_only.requires_writable_backend());
        assert!(policy().requires_writable_backend());
    }

    #[test]
    fn public_query_surface_is_tenant_scoped_and_bounded() {
        let request: SearchRequest =
            serde_json::from_slice(br#"{"tenant":"tenant-a","text":"timeouts","k":10}"#).unwrap();
        let query = request.into_query().unwrap();
        assert_eq!(query.tenant.as_deref(), Some("tenant-a"));
        assert!(!query.best_effort);
        assert!(query.plan.is_none());
        assert!(query.force_route.is_none());

        let oversized = format!(
            "{{\"tenant\":\"tenant-a\",\"text\":\"{}\"}}",
            "x".repeat(MAX_QUERY_TEXT_BYTES + 1)
        );
        assert!(serde_json::from_str::<SearchRequest>(&oversized)
            .unwrap()
            .into_query()
            .is_err());
        assert!(serde_json::from_slice::<SearchRequest>(
            br#"{"tenant":"tenant-a","text":"x","best_effort":true}"#
        )
        .is_err());
    }

    #[test]
    fn public_ingest_injects_tenant_and_observed_time() {
        let request: IngestRequest = serde_json::from_slice(
            br#"{
                "tenant":"tenant-a",
                "events":[{
                    "event_id":"e1",
                    "event_time":123,
                    "event_name":"llm.call",
                    "cost":0.01,
                    "error":false,
                    "body":"payment timeout"
                }]
            }"#,
        )
        .unwrap();
        let (_, events) = request.into_events(456).unwrap();
        assert_eq!(events[0].tenant_id, "tenant-a");
        assert_eq!(events[0].observed_time, 456);
        assert!(serde_json::from_slice::<IngestRequest>(
            br#"{"tenant":"tenant-a","events":[{"tenant_id":"tenant-b","event_id":"e1","event_time":123,"event_name":"x","cost":0,"error":false,"body":"x"}]}"#
        )
        .is_err());
    }

    #[test]
    fn parser_refuses_smuggling_and_oversized_bodies() {
        let duplicate = b"GET /healthz HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n";
        assert!(read_http_request(&mut duplicate.as_slice())
            .unwrap_err()
            .to_string()
            .contains("duplicate"));

        let chunked =
            b"POST /v1/search HTTP/1.1\r\nHost: api.test\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n";
        assert!(read_http_request(&mut chunked.as_slice())
            .unwrap_err()
            .to_string()
            .contains("Transfer-Encoding"));

        let oversized = format!(
            "POST /v1/search HTTP/1.1\r\nHost: api.test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            MAX_HTTP_BODY_BYTES + 1
        );
        assert!(read_http_request(&mut oversized.as_bytes())
            .unwrap_err()
            .to_string()
            .contains("exceeds"));
    }

    #[test]
    fn metrics_have_fixed_cardinality() {
        let metrics = ServiceMetrics::default();
        let encoded = metrics.encode();
        assert!(encoded.contains("prism_api_requests_total 0"));
        assert!(!encoded.contains("tenant"));
        assert!(!encoded.contains("identity"));
    }

    #[test]
    fn mutual_tls_identity_is_bound_to_tenant_policy() {
        let directory = temp("mtls");
        fs::create_dir_all(&directory).unwrap();
        generate_ca(&directory, "server-ca");
        generate_ca(&directory, "client-ca");
        generate_leaf(&directory, "server", "api.test", "server-ca", "serverAuth");
        generate_leaf(
            &directory,
            "allowed",
            "allowed.test",
            "client-ca",
            "clientAuth",
        );
        generate_leaf(
            &directory,
            "unknown",
            "unknown.test",
            "client-ca",
            "clientAuth",
        );
        let allowed_der =
            CertificateDer::from_pem_slice(&fs::read(directory.join("allowed.pem")).unwrap())
                .unwrap();
        let policy = AuthorizationPolicy::from_slice(
            serde_json::to_string(&serde_json::json!({
                "version": 1,
                "clients": [{
                    "identity_id": "allowed-reader",
                    "certificate_sha256": hex(&sha256(allowed_der.as_ref())),
                    "tenants": ["tenant-a"],
                    "scopes": ["health", "ingest", "search"],
                    "max_in_flight": 2
                }]
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        let server_tls = server_tls_from_pem(
            &directory.join("server.pem"),
            &directory.join("server-key.pem"),
            &directory.join("client-ca.pem"),
        )
        .unwrap();
        let allowed_tls = client_tls_from_pem(
            &directory.join("allowed.pem"),
            &directory.join("allowed-key.pem"),
            &directory.join("server-ca.pem"),
        )
        .unwrap();
        let unknown_tls = client_tls_from_pem(
            &directory.join("unknown.pem"),
            &directory.join("unknown-key.pem"),
            &directory.join("server-ca.pem"),
        )
        .unwrap();
        let backend = Arc::new(FakeBackend {
            tenants: Mutex::new(Vec::new()),
            ingested: Mutex::new(Vec::new()),
            ready: true,
            writable: true,
        });
        let service = ReadService::new(
            backend.clone(),
            Arc::new(policy),
            server_tls,
            Duration::from_secs(2),
        )
        .unwrap();

        let search = b"POST /v1/search HTTP/1.1\r\nHost: api.test\r\nContent-Type: application/json\r\nContent-Length: 46\r\n\r\n{\"tenant\":\"tenant-a\",\"text\":\"payment timeout\"}";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let response = std::thread::scope(|scope| {
            scope.spawn(|| {
                let (stream, _) = listener.accept().unwrap();
                service.serve_connection(stream).unwrap();
            });
            client_request(address, allowed_tls, search)
        });
        assert!(
            response.starts_with(b"HTTP/1.1 200 OK"),
            "{}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(backend.tenants.lock().unwrap().as_slice(), ["tenant-a"]);

        let ingest_body = br#"{"tenant":"tenant-a","events":[{"event_id":"e2","event_time":1760000000000,"event_name":"llm.call","cost":0.01,"error":false,"body":"payment timeout"}]}"#;
        let ingest_request = format!(
            "POST /v1/events HTTP/1.1\r\nHost: api.test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            ingest_body.len(),
            std::str::from_utf8(ingest_body).unwrap()
        );
        let allowed_tls = client_tls_from_pem(
            &directory.join("allowed.pem"),
            &directory.join("allowed-key.pem"),
            &directory.join("server-ca.pem"),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let response = std::thread::scope(|scope| {
            scope.spawn(|| {
                let (stream, _) = listener.accept().unwrap();
                service.serve_connection(stream).unwrap();
            });
            client_request(address, allowed_tls, ingest_request.as_bytes())
        });
        assert!(
            response.starts_with(b"HTTP/1.1 200 OK"),
            "{}",
            String::from_utf8_lossy(&response)
        );
        let response_text = String::from_utf8_lossy(&response);
        assert!(response_text.contains("\"published\":1"));
        assert!(!response_text.contains("wal_record"));
        assert!(!response_text.contains("part_id"));
        assert!(!response_text.contains("source_offset"));
        let ingested = backend.ingested.lock().unwrap();
        assert_eq!(ingested.len(), 1);
        assert_eq!(ingested[0][0].tenant_id, "tenant-a");
        assert!(ingested[0][0].observed_time > 0);
        drop(ingested);

        let denied_body = br#"{"tenant":"tenant-b","events":[{"event_id":"e3","event_time":1760000000000,"event_name":"llm.call","cost":0.01,"error":false,"body":"other tenant"}]}"#;
        let denied_request = format!(
            "POST /v1/events HTTP/1.1\r\nHost: api.test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            denied_body.len(),
            std::str::from_utf8(denied_body).unwrap()
        );
        let allowed_tls = client_tls_from_pem(
            &directory.join("allowed.pem"),
            &directory.join("allowed-key.pem"),
            &directory.join("server-ca.pem"),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let response = std::thread::scope(|scope| {
            scope.spawn(|| {
                let (stream, _) = listener.accept().unwrap();
                service.serve_connection(stream).unwrap();
            });
            client_request(address, allowed_tls, denied_request.as_bytes())
        });
        assert!(
            response.starts_with(b"HTTP/1.1 403 Forbidden"),
            "{}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(backend.ingested.lock().unwrap().len(), 1);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let response = std::thread::scope(|scope| {
            scope.spawn(|| {
                let (stream, _) = listener.accept().unwrap();
                service.serve_connection(stream).unwrap();
            });
            client_request(
                address,
                unknown_tls,
                b"GET /healthz HTTP/1.1\r\nHost: api.test\r\n\r\n",
            )
        });
        assert!(
            response.starts_with(b"HTTP/1.1 403 Forbidden"),
            "{}",
            String::from_utf8_lossy(&response)
        );
    }
}
