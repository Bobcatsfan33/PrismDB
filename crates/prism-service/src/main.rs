use prism_engine::shard_rpc::{
    client_tls_from_pem, server_tls_from_pem, RemoteReadCluster, RemoteReadTopology,
};
use prism_service::{AuthorizationPolicy, ReadService, DEFAULT_PUBLIC_TIMEOUT};
use prism_types::error::{PrismError, Result};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use signal_hook::consts::{SIGINT, SIGTERM};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

struct Args {
    command: String,
    flags: BTreeMap<String, String>,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut values = std::env::args().skip(1);
        let command = values
            .next()
            .ok_or_else(|| PrismError::Invalid("usage: prismd serve [flags]".into()))?;
        if !matches!(
            command.as_str(),
            "serve" | "read-serve" | "probe" | "version"
        ) {
            return Err(PrismError::Invalid(format!(
                "unknown command `{command}`; expected serve, probe, or version"
            )));
        }
        let mut flags = BTreeMap::new();
        while let Some(argument) = values.next() {
            let name = argument
                .strip_prefix("--")
                .ok_or_else(|| PrismError::Invalid(format!("unexpected argument `{argument}`")))?;
            let value = values
                .next()
                .ok_or_else(|| PrismError::Invalid(format!("missing value for flag `--{name}`")))?;
            if value.starts_with("--") || flags.insert(name.to_string(), value).is_some() {
                return Err(PrismError::Invalid(format!(
                    "missing value or duplicate flag `--{name}`"
                )));
            }
        }
        let allowed: BTreeSet<&str> = match command.as_str() {
            "serve" | "read-serve" => [
                "listen",
                "topology",
                "shard-cert",
                "shard-key",
                "shard-ca",
                "server-cert",
                "server-key",
                "client-ca",
                "auth-policy",
                "timeout-ms",
            ]
            .into_iter()
            .collect(),
            "probe" => [
                "address",
                "server-name",
                "cert",
                "key",
                "ca",
                "path",
                "timeout-ms",
            ]
            .into_iter()
            .collect(),
            "version" => BTreeSet::new(),
            _ => unreachable!(),
        };
        if let Some(unknown) = flags.keys().find(|name| !allowed.contains(name.as_str())) {
            return Err(PrismError::Invalid(format!("unknown flag `--{unknown}`")));
        }
        Ok(Self { command, flags })
    }

    fn required(&self, name: &str) -> Result<&str> {
        self.flags
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| PrismError::Invalid(format!("missing required flag `--{name}`")))
    }

    fn timeout(&self) -> Result<Duration> {
        let milliseconds = self
            .flags
            .get("timeout-ms")
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| PrismError::Invalid("`--timeout-ms` must be an integer".into()))
            })
            .transpose()?
            .unwrap_or(DEFAULT_PUBLIC_TIMEOUT.as_millis() as u64);
        Ok(Duration::from_millis(milliseconds))
    }
}

fn run_service(args: &Args) -> Result<()> {
    let timeout = args.timeout()?;
    let topology = RemoteReadTopology::load(Path::new(args.required("topology")?))?;
    let shard_tls = client_tls_from_pem(
        Path::new(args.required("shard-cert")?),
        Path::new(args.required("shard-key")?),
        Path::new(args.required("shard-ca")?),
    )?;
    let cluster = RemoteReadCluster::connect(topology, shard_tls, timeout)?;
    let shard_count = cluster.num_shards();
    let server_tls = server_tls_from_pem(
        Path::new(args.required("server-cert")?),
        Path::new(args.required("server-key")?),
        Path::new(args.required("client-ca")?),
    )?;
    let policy = Arc::new(AuthorizationPolicy::load(Path::new(
        args.required("auth-policy")?,
    ))?);
    let service = ReadService::new(Arc::new(cluster), policy, server_tls, timeout)?;
    let address: SocketAddr = args
        .required("listen")?
        .parse()
        .map_err(|_| PrismError::Invalid("`--listen` must be a socket address".into()))?;
    if address.port() == 0 {
        return Err(PrismError::Invalid(
            "`--listen` port 0 is not allowed in production".into(),
        ));
    }
    let listener = TcpListener::bind(address)
        .map_err(|error| PrismError::Io(format!("public API bind {address}: {error}")))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGTERM, Arc::clone(&shutdown))
        .and_then(|_| signal_hook::flag::register(SIGINT, Arc::clone(&shutdown)))
        .map_err(|error| PrismError::Io(format!("install shutdown signal handlers: {error}")))?;
    eprintln!(
        "{}",
        serde_json::json!({
            "event": "service_started",
            "service": "prismd",
            "api_version": prism_service::PUBLIC_API_VERSION,
            "transport": "mutual-tls",
            "listen": address.to_string(),
            "shards": shard_count,
        })
    );
    let result = service.serve(listener, Arc::clone(&shutdown));
    eprintln!(
        "{}",
        serde_json::json!({
            "event": "service_stopped",
            "service": "prismd",
            "graceful": shutdown.load(Ordering::SeqCst),
        })
    );
    result
}

fn run_probe(args: &Args) -> Result<()> {
    let timeout = args.timeout()?;
    let address: SocketAddr = args
        .required("address")?
        .parse()
        .map_err(|_| PrismError::Invalid("`--address` must be a socket address".into()))?;
    let server_name = args.required("server-name")?.to_string();
    let path = args.required("path")?;
    if !matches!(path, "/healthz" | "/readyz") {
        return Err(PrismError::Invalid(
            "`--path` must be /healthz or /readyz".into(),
        ));
    }
    let tls = client_tls_from_pem(
        Path::new(args.required("cert")?),
        Path::new(args.required("key")?),
        Path::new(args.required("ca")?),
    )?;
    let stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|error| PrismError::Io(format!("probe connect {address}: {error}")))?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|_| stream.set_write_timeout(Some(timeout)))
        .map_err(|error| PrismError::Io(format!("configure probe deadline: {error}")))?;
    let server_name = ServerName::try_from(server_name)
        .map_err(|_| PrismError::Invalid("`--server-name` is not a valid DNS name".into()))?;
    let connection = ClientConnection::new(tls, server_name)
        .map_err(|error| PrismError::Io(format!("create probe TLS session: {error}")))?;
    let mut tls = StreamOwned::new(connection, stream);
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {}\r\nX-Request-Id: prism-probe\r\nConnection: close\r\n\r\n",
        args.required("server-name")?
    );
    tls.write_all(request.as_bytes())
        .and_then(|_| tls.flush())
        .map_err(|error| PrismError::Io(format!("send probe request: {error}")))?;
    let mut response = Vec::new();
    tls.take(64 * 1024)
        .read_to_end(&mut response)
        .map_err(|error| PrismError::Io(format!("read probe response: {error}")))?;
    if !response.starts_with(b"HTTP/1.1 200 OK\r\n") {
        return Err(PrismError::Io(format!(
            "probe returned non-200 response: {}",
            String::from_utf8_lossy(&response[..response.len().min(256)])
        )));
    }
    Ok(())
}

fn run() -> Result<()> {
    let args = Args::parse()?;
    match args.command.as_str() {
        "serve" | "read-serve" => run_service(&args),
        "probe" => run_probe(&args),
        "version" => {
            println!(
                "prismd {} api={}",
                env!("CARGO_PKG_VERSION"),
                prism_service::PUBLIC_API_VERSION
            );
            Ok(())
        }
        _ => unreachable!(),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!(
            "{}",
            serde_json::json!({
                "event": "service_failed",
                "service": "prismd",
                "error": error.to_string(),
            })
        );
        std::process::exit(1);
    }
}
