//! AWS KMS implementation of PrismDB's wrapping-key boundary.
//!
//! The provider sends only 32-byte data-encryption keys to KMS. Event data never
//! crosses this boundary. Requests use certificate-validating TLS 1.2+, SigV4,
//! a fixed encryption context, bounded sockets, and bounded response bodies.

use crate::keys::{KeyProvider, BACKEND_AWS_KMS};
use crate::storage::{s3, sigv4};
use native_tls::{Protocol, TlsConnector};
use prism_types::error::{PrismError, Result};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use zeroize::{Zeroize, Zeroizing};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const MAX_CIPHERTEXT_BYTES: usize = 64 * 1024;
const ENCRYPTION_CONTEXT_KEY: &str = "prism-purpose";
const ENCRYPTION_CONTEXT_VALUE: &str = "dek-wrap-v1";

/// Supplies AWS credentials at request time. File-backed implementations can
/// rotate short-lived credentials without restarting a database process.
pub trait AwsCredentialsProvider: Send + Sync {
    fn load(&self) -> Result<sigv4::Credentials>;
}

/// Explicit static credentials. Suitable for tests and externally managed
/// process environments; short-lived production credentials should use a
/// refreshable source.
pub struct StaticAwsCredentials(pub sigv4::Credentials);

impl AwsCredentialsProvider for StaticAwsCredentials {
    fn load(&self) -> Result<sigv4::Credentials> {
        Ok(self.0.clone())
    }
}

/// A refreshable AWS shared-credentials profile. The file is parsed anew for
/// every KMS call so an atomic credential rotation takes effect immediately.
pub struct SharedCredentialsFile {
    path: PathBuf,
    profile: String,
}

impl SharedCredentialsFile {
    pub fn new(path: PathBuf, profile: impl Into<String>) -> Result<Self> {
        if path.as_os_str().is_empty() {
            return Err(PrismError::Invalid(
                "AWS shared credentials path may not be empty".into(),
            ));
        }
        let profile = profile.into();
        if profile.trim().is_empty() || profile.contains(['\n', '\r', '[', ']']) {
            return Err(PrismError::Invalid(
                "AWS credentials profile is empty or malformed".into(),
            ));
        }
        Ok(Self { path, profile })
    }
}

impl AwsCredentialsProvider for SharedCredentialsFile {
    fn load(&self) -> Result<sigv4::Credentials> {
        read_shared_credentials(&self.path, &self.profile)
    }
}

/// Configuration for the production KMS transport.
pub struct AwsKmsConfig {
    region: String,
    /// Full immutable key ARN. Aliases are refused because an alias can be
    /// retargeted without changing bytes already stored in PrismDB envelopes.
    key_arn: String,
    /// `host:port`; defaults should use `kms.<region>.amazonaws.com:443`.
    endpoint: String,
    credentials: Arc<dyn AwsCredentialsProvider>,
    /// Active plus retired-but-authorized key ARNs accepted for unwrap.
    decrypt_key_arns: BTreeSet<String>,
    /// Present only for deterministic request-vector tests.
    fixed_amz_date: Option<String>,
}

impl AwsKmsConfig {
    pub fn production(
        region: impl Into<String>,
        key_arn: impl Into<String>,
        credentials: Arc<dyn AwsCredentialsProvider>,
    ) -> Result<Self> {
        let region = region.into();
        if region.trim().is_empty() {
            return Err(PrismError::Invalid(
                "AWS KMS region may not be empty".into(),
            ));
        }
        let key_arn = key_arn.into();
        let (partition, key_region) = key_arn_scope(&key_arn)?;
        if key_region != region {
            return Err(PrismError::Invalid(format!(
                "AWS KMS key ARN region `{key_region}` does not match configured region `{region}`"
            )));
        }
        let endpoint = if partition == "aws-cn" {
            format!("kms.{region}.amazonaws.com.cn:443")
        } else {
            format!("kms.{region}.amazonaws.com:443")
        };
        let config = Self {
            region,
            key_arn,
            endpoint,
            credentials,
            decrypt_key_arns: BTreeSet::new(),
            fixed_amz_date: None,
        };
        config.with_decrypt_key_arns(std::iter::empty::<String>())
    }

    pub fn with_decrypt_key_arns(
        mut self,
        key_arns: impl IntoIterator<Item = String>,
    ) -> Result<Self> {
        self.decrypt_key_arns.extend(key_arns);
        self.decrypt_key_arns.insert(self.key_arn.clone());
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<()> {
        let (partition, region) = key_arn_scope(&self.key_arn)?;
        if region != self.region {
            return Err(PrismError::Invalid("active KMS key region changed".into()));
        }
        for key in &self.decrypt_key_arns {
            let (candidate_partition, candidate_region) = key_arn_scope(key)?;
            if candidate_partition != partition || candidate_region != self.region {
                return Err(PrismError::Invalid(format!(
                    "every decrypt key must be an immutable KMS key ARN in partition {partition} and region {}",
                    self.region
                )));
            }
        }
        let (_, port) = endpoint_parts(&self.endpoint)?;
        if port != 443 {
            return Err(PrismError::Invalid(format!(
                "AWS KMS endpoint `{}` must use TLS port 443",
                self.endpoint
            )));
        }
        Ok(())
    }
}

struct KmsHttpResponse {
    status: u16,
    body: Vec<u8>,
}

trait KmsTransport: Send + Sync {
    fn call(&self, target: &str, body: &[u8]) -> Result<KmsHttpResponse>;
}

struct HttpsKmsTransport {
    config: AwsKmsConfig,
    tls: TlsConnector,
}

impl HttpsKmsTransport {
    fn new(config: AwsKmsConfig) -> Result<Self> {
        config.validate()?;
        let mut builder = TlsConnector::builder();
        builder.min_protocol_version(Some(Protocol::Tlsv12));
        let tls = builder.build().map_err(|error| {
            PrismError::Invalid(format!("could not initialize AWS KMS TLS: {error}"))
        })?;
        Ok(Self { config, tls })
    }

    fn connect(&self) -> Result<TcpStream> {
        let addresses = self
            .config
            .endpoint
            .to_socket_addrs()
            .map_err(|error| PrismError::Io(format!("key service unreachable: resolve: {error}")))?
            .collect::<Vec<_>>();
        let mut last = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
                Ok(stream) => {
                    stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(|error| {
                        PrismError::Io(format!("key service unreachable: read timeout: {error}"))
                    })?;
                    stream
                        .set_write_timeout(Some(IO_TIMEOUT))
                        .map_err(|error| {
                            PrismError::Io(format!(
                                "key service unreachable: write timeout: {error}"
                            ))
                        })?;
                    return Ok(stream);
                }
                Err(error) => last = Some(error),
            }
        }
        Err(PrismError::Io(format!(
            "key service unreachable: connect failed: {}",
            last.map(|error| error.to_string())
                .unwrap_or_else(|| "endpoint resolved to no addresses".into())
        )))
    }
}

impl KmsTransport for HttpsKmsTransport {
    fn call(&self, target: &str, body: &[u8]) -> Result<KmsHttpResponse> {
        let credentials = self.config.credentials.load()?;
        let amz_date = self
            .config
            .fixed_amz_date
            .clone()
            .unwrap_or_else(s3::now_amz_date);
        if amz_date.len() < 8 {
            return Err(PrismError::Invalid("AWS signing date is malformed".into()));
        }
        let date_stamp = &amz_date[..8];
        let request = build_request(
            &self.config.endpoint,
            &self.config.region,
            &credentials,
            target,
            body,
            &amz_date,
            date_stamp,
        );
        let tcp = self.connect()?;
        let (hostname, _) = endpoint_parts(&self.config.endpoint)?;
        let mut tls = self.tls.connect(&hostname, tcp).map_err(|error| {
            PrismError::Io(format!(
                "key service unreachable: TLS handshake or certificate validation failed: {error}"
            ))
        })?;
        tls.write_all(&request)
            .map_err(|error| PrismError::Io(format!("key service unreachable: write: {error}")))?;
        let mut raw = Zeroizing::new(Vec::new());
        std::io::Read::by_ref(&mut tls)
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut raw)
            .map_err(|error| PrismError::Io(format!("key service unreachable: read: {error}")))?;
        if raw.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(PrismError::Corrupt(
                "AWS KMS response exceeded the 1 MiB bound".into(),
            ));
        }
        let parsed = s3::parse_response(&raw)?;
        Ok(KmsHttpResponse {
            status: parsed.status,
            body: parsed.body,
        })
    }
}

/// AWS KMS-backed wrapping of PrismDB data-encryption keys.
pub struct AwsKmsProvider {
    key_arn: String,
    decrypt_key_arns: BTreeSet<String>,
    transport: Arc<dyn KmsTransport>,
}

impl AwsKmsProvider {
    pub fn new(config: AwsKmsConfig) -> Result<Self> {
        let key_arn = config.key_arn.clone();
        let decrypt_key_arns = config.decrypt_key_arns.clone();
        Ok(Self {
            key_arn,
            decrypt_key_arns,
            transport: Arc::new(HttpsKmsTransport::new(config)?),
        })
    }

    #[cfg(test)]
    fn with_transport(key_arn: &str, transport: Arc<dyn KmsTransport>) -> Self {
        Self {
            key_arn: key_arn.into(),
            decrypt_key_arns: [key_arn.to_string()].into_iter().collect(),
            transport,
        }
    }

    fn call(&self, target: &str, body: &[u8]) -> Result<Vec<u8>> {
        let response = self.transport.call(target, body)?;
        if response.status != 200 {
            return Err(kms_error(response.status, &response.body));
        }
        Ok(response.body)
    }
}

impl KeyProvider for AwsKmsProvider {
    fn backend(&self) -> &'static str {
        BACKEND_AWS_KMS
    }

    fn active_key_id(&self) -> Result<String> {
        Ok(self.key_arn.clone())
    }

    fn wrap(&self, dek: &[u8; 32]) -> Result<(String, Vec<u8>)> {
        let plaintext = base64_encode(dek);
        let mut body = Zeroizing::new(
            format!(
                "{{\"KeyId\":\"{}\",\"Plaintext\":\"{}\",\"EncryptionAlgorithm\":\"SYMMETRIC_DEFAULT\",\"EncryptionContext\":{{\"{}\":\"{}\"}}}}",
                self.key_arn, plaintext, ENCRYPTION_CONTEXT_KEY, ENCRYPTION_CONTEXT_VALUE
            )
            .into_bytes(),
        );
        let response = self.call("TrentService.Encrypt", &body)?;
        body.zeroize();
        let parsed: EncryptResponse = serde_json::from_slice(&response).map_err(|_| {
            PrismError::Corrupt("AWS KMS Encrypt response was not valid JSON".into())
        })?;
        if parsed.key_id != self.key_arn {
            return Err(PrismError::Policy(
                "AWS KMS Encrypt response named a different key than the configured immutable ARN"
                    .into(),
            ));
        }
        let ciphertext = base64_decode(&parsed.ciphertext_blob)?;
        if ciphertext.is_empty() || ciphertext.len() > MAX_CIPHERTEXT_BYTES {
            return Err(PrismError::Corrupt(
                "AWS KMS Encrypt ciphertext was empty or exceeded 64 KiB".into(),
            ));
        }
        Ok((self.key_arn.clone(), ciphertext))
    }

    fn unwrap(&self, key_id: &str, wrapped: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
        if !self.decrypt_key_arns.contains(key_id) {
            return Err(PrismError::Policy(format!(
                "AWS KMS provider for `{}` refuses envelope key `{key_id}`",
                self.key_arn
            )));
        }
        if wrapped.is_empty() || wrapped.len() > MAX_CIPHERTEXT_BYTES {
            return Err(PrismError::Corrupt(
                "wrapped DEK is empty or exceeds the 64 KiB KMS ciphertext bound".into(),
            ));
        }
        let ciphertext = base64_encode(wrapped);
        let body = Zeroizing::new(
            format!(
                "{{\"KeyId\":\"{}\",\"CiphertextBlob\":\"{}\",\"EncryptionAlgorithm\":\"SYMMETRIC_DEFAULT\",\"EncryptionContext\":{{\"{}\":\"{}\"}}}}",
                key_id, ciphertext, ENCRYPTION_CONTEXT_KEY, ENCRYPTION_CONTEXT_VALUE
            )
            .into_bytes(),
        );
        let response = Zeroizing::new(self.call("TrentService.Decrypt", &body)?);
        let mut parsed: DecryptResponse = serde_json::from_slice(&response).map_err(|_| {
            PrismError::Corrupt("AWS KMS Decrypt response was not valid JSON".into())
        })?;
        if parsed.key_id != key_id {
            parsed.plaintext.zeroize();
            return Err(PrismError::Policy(
                "AWS KMS Decrypt response named a different key than the envelope".into(),
            ));
        }
        let plaintext_b64 = Zeroizing::new(std::mem::take(&mut parsed.plaintext));
        let mut plaintext = base64_decode(&plaintext_b64)?;
        if plaintext.len() != 32 {
            plaintext.zeroize();
            return Err(PrismError::Corrupt(format!(
                "AWS KMS returned a {}-byte DEK; PrismDB requires exactly 32",
                plaintext.len()
            )));
        }
        let mut dek = Zeroizing::new([0u8; 32]);
        dek.copy_from_slice(&plaintext);
        plaintext.zeroize();
        Ok(dek)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct EncryptResponse {
    ciphertext_blob: String,
    key_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DecryptResponse {
    plaintext: String,
    key_id: String,
}

#[derive(Deserialize)]
struct ErrorResponse {
    #[serde(rename = "__type")]
    kind: Option<String>,
}

fn kms_error(status: u16, body: &[u8]) -> PrismError {
    let kind = serde_json::from_slice::<ErrorResponse>(body)
        .ok()
        .and_then(|error| error.kind)
        .and_then(|kind| kind.rsplit('#').next().map(str::to_string))
        .unwrap_or_else(|| format!("HTTP{status}"));
    match kind.as_str() {
        "AccessDeniedException" | "DisabledException" | "InvalidCiphertextException" => {
            PrismError::Policy(format!("AWS KMS refused the DEK operation: {kind}"))
        }
        "NotFoundException" => PrismError::NotFound(format!("AWS KMS key was not found: {kind}")),
        "ThrottlingException"
        | "DependencyTimeoutException"
        | "KMSInternalException"
        | "KeyUnavailableException" => PrismError::Io(format!("AWS KMS transient failure: {kind}")),
        _ if status == 429 || status >= 500 => {
            PrismError::Io(format!("AWS KMS transient HTTP failure: {status}"))
        }
        _ => PrismError::Policy(format!("AWS KMS refused the DEK operation: {kind}")),
    }
}

fn build_request(
    endpoint: &str,
    region: &str,
    credentials: &sigv4::Credentials,
    target: &str,
    body: &[u8],
    amz_date: &str,
    date_stamp: &str,
) -> Vec<u8> {
    let payload_hash = sigv4::payload_hash(body);
    let host = endpoint.strip_suffix(":443").unwrap_or(endpoint);
    let mut headers = vec![
        ("content-type".into(), "application/x-amz-json-1.1".into()),
        ("host".into(), host.into()),
        ("x-amz-content-sha256".into(), payload_hash.clone()),
        ("x-amz-date".into(), amz_date.into()),
        ("x-amz-target".into(), target.into()),
    ];
    if let Some(token) = &credentials.session_token {
        headers.push(("x-amz-security-token".into(), token.clone()));
    }
    let signed = sigv4::sign(
        "POST",
        "/",
        "",
        &headers,
        &payload_hash,
        region,
        "kms",
        amz_date,
        date_stamp,
        credentials,
    );
    let mut request = String::from("POST / HTTP/1.1\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("authorization: {}\r\n", signed.authorization));
    request.push_str(&format!("content-length: {}\r\n", body.len()));
    request.push_str("connection: close\r\n\r\n");
    let mut bytes = request.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn endpoint_parts(endpoint: &str) -> Result<(String, u16)> {
    let (host, port) = endpoint.rsplit_once(':').ok_or_else(|| {
        PrismError::Invalid(format!("AWS KMS endpoint `{endpoint}` must be host:port"))
    })?;
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let port = port.parse::<u16>().map_err(|_| {
        PrismError::Invalid(format!("AWS KMS endpoint `{endpoint}` has an invalid port"))
    })?;
    if host.is_empty() {
        return Err(PrismError::Invalid("AWS KMS endpoint host is empty".into()));
    }
    Ok((host.into(), port))
}

fn key_arn_scope(key_arn: &str) -> Result<(&str, &str)> {
    let fields = key_arn.split(':').collect::<Vec<_>>();
    let valid_partition = matches!(
        fields.get(1).copied(),
        Some("aws" | "aws-us-gov" | "aws-cn")
    );
    let valid_account = fields
        .get(4)
        .map(|account| account.len() == 12 && account.bytes().all(|byte| byte.is_ascii_digit()))
        .unwrap_or(false);
    let valid_key = fields
        .get(5)
        .and_then(|resource| resource.strip_prefix("key/"))
        .map(|id| {
            !id.is_empty()
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        .unwrap_or(false);
    if fields.len() != 6
        || fields.first() != Some(&"arn")
        || !valid_partition
        || fields.get(2) != Some(&"kms")
        || fields
            .get(3)
            .map(|region| region.is_empty())
            .unwrap_or(true)
        || !valid_account
        || !valid_key
    {
        return Err(PrismError::Invalid(
            "KMS key must be a full immutable commercial, GovCloud, or China key ARN; aliases and malformed ids are refused"
                .into(),
        ));
    }
    Ok((fields[1], fields[3]))
}

fn read_shared_credentials(path: &Path, profile: &str) -> Result<sigv4::Credentials> {
    // `metadata` deliberately follows one symlink. Kubernetes rotates projected
    // Secrets with an atomic symlink; refusing it would freeze credentials at
    // pod start. The resolved target still has to be a bounded regular file.
    let metadata = std::fs::metadata(path).map_err(|error| {
        PrismError::Invalid(format!("cannot inspect AWS credentials file: {error}"))
    })?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err(PrismError::Policy(
            "AWS credentials source must resolve to a regular file no larger than 64 KiB".into(),
        ));
    }
    let file = std::fs::File::open(path).map_err(|error| {
        PrismError::Invalid(format!("cannot open AWS credentials file: {error}"))
    })?;
    let mut raw = Zeroizing::new(Vec::new());
    file.take(64 * 1024 + 1)
        .read_to_end(&mut raw)
        .map_err(|error| {
            PrismError::Invalid(format!("cannot read AWS credentials file: {error}"))
        })?;
    if raw.len() > 64 * 1024 {
        return Err(PrismError::Policy(
            "AWS credentials source exceeded 64 KiB while being read".into(),
        ));
    }
    let text = Zeroizing::new(
        String::from_utf8(std::mem::take(&mut *raw))
            .map_err(|_| PrismError::Invalid("AWS credentials file is not UTF-8".into()))?,
    );
    let mut current = "";
    let mut values = std::collections::BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            current = line[1..line.len() - 1].trim();
            continue;
        }
        if current != profile {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            values.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    let required = |name: &str| {
        values
            .get(name)
            .filter(|value| !value.is_empty())
            .cloned()
            .ok_or_else(|| {
                PrismError::Invalid(format!(
                    "AWS credentials profile `{profile}` is missing {name}"
                ))
            })
    };
    Ok(sigv4::Credentials {
        access_key: required("aws_access_key_id")?,
        secret_key: required("aws_secret_access_key")?,
        session_token: values
            .get("aws_session_token")
            .filter(|value| !value.is_empty())
            .cloned(),
    })
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(a >> 2) as usize] as char);
        out.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(input: &str) -> Result<Vec<u8>> {
    if input.len() % 4 != 0 {
        return Err(PrismError::Corrupt(
            "AWS KMS response contains malformed base64".into(),
        ));
    }
    let value = |byte: u8| -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for (index, chunk) in input.as_bytes().chunks(4).enumerate() {
        let last = index + 1 == input.len() / 4;
        let pad2 = chunk[2] == b'=';
        let pad3 = chunk[3] == b'=';
        if (!last && (pad2 || pad3)) || pad2 && !pad3 {
            return Err(PrismError::Corrupt(
                "AWS KMS response contains malformed base64 padding".into(),
            ));
        }
        let a = value(chunk[0]);
        let b = value(chunk[1]);
        let c = if pad2 { Some(0) } else { value(chunk[2]) };
        let d = if pad3 { Some(0) } else { value(chunk[3]) };
        let (Some(a), Some(b), Some(c), Some(d)) = (a, b, c, d) else {
            return Err(PrismError::Corrupt(
                "AWS KMS response contains malformed base64".into(),
            ));
        };
        out.push((a << 2) | (b >> 4));
        if !pad2 {
            out.push((b << 4) | (c >> 2));
        }
        if !pad3 {
            out.push((c << 6) | d);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const KEY: &str = "arn:aws:kms:us-east-1:123456789012:key/abc";
    const OLD_KEY: &str = "arn:aws:kms:us-east-1:123456789012:key/old";

    struct FakeTransport {
        calls: Mutex<Vec<(String, Vec<u8>)>>,
        responses: Mutex<Vec<KmsHttpResponse>>,
    }

    impl KmsTransport for FakeTransport {
        fn call(&self, target: &str, body: &[u8]) -> Result<KmsHttpResponse> {
            self.calls
                .lock()
                .unwrap()
                .push((target.into(), body.into()));
            Ok(self.responses.lock().unwrap().remove(0))
        }
    }

    #[test]
    fn kms_wrap_and_unwrap_use_the_immutable_key_and_context() {
        let dek = [7u8; 32];
        let transport = Arc::new(FakeTransport {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(vec![
                KmsHttpResponse {
                    status: 200,
                    body: format!("{{\"CiphertextBlob\":\"Y2lwaGVy\",\"KeyId\":\"{KEY}\"}}")
                        .into_bytes(),
                },
                KmsHttpResponse {
                    status: 200,
                    body: format!(
                        "{{\"Plaintext\":\"{}\",\"KeyId\":\"{KEY}\"}}",
                        base64_encode(&dek)
                    )
                    .into_bytes(),
                },
            ]),
        });
        let provider = AwsKmsProvider::with_transport(KEY, transport.clone());
        let (key, wrapped) = provider.wrap(&dek).unwrap();
        assert_eq!(key, KEY);
        assert_eq!(wrapped, b"cipher");
        assert_eq!(&*provider.unwrap(KEY, &wrapped).unwrap(), &dek);
        let calls = transport.calls.lock().unwrap();
        assert_eq!(calls[0].0, "TrentService.Encrypt");
        assert_eq!(calls[1].0, "TrentService.Decrypt");
        for (_, body) in calls.iter() {
            let text = String::from_utf8_lossy(body);
            assert!(text.contains("dek-wrap-v1"));
            assert!(text.contains(KEY));
        }
    }

    #[test]
    fn a_provider_refuses_an_envelope_for_another_key_without_calling_kms() {
        let transport = Arc::new(FakeTransport {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(Vec::new()),
        });
        let provider = AwsKmsProvider::with_transport(KEY, transport.clone());
        let error = provider.unwrap("arn:aws:kms:us-east-1:123:key/other", b"x");
        assert!(matches!(error, Err(PrismError::Policy(_))));
        assert!(transport.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn an_explicit_previous_key_is_accepted_for_rotation_and_nothing_else_is() {
        let dek = [9u8; 32];
        let transport = Arc::new(FakeTransport {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(vec![KmsHttpResponse {
                status: 200,
                body: format!(
                    "{{\"Plaintext\":\"{}\",\"KeyId\":\"{OLD_KEY}\"}}",
                    base64_encode(&dek)
                )
                .into_bytes(),
            }]),
        });
        let provider = AwsKmsProvider {
            key_arn: KEY.into(),
            decrypt_key_arns: [KEY.to_string(), OLD_KEY.to_string()].into_iter().collect(),
            transport: transport.clone(),
        };
        assert_eq!(&*provider.unwrap(OLD_KEY, b"old-ciphertext").unwrap(), &dek);
        let calls = transport.calls.lock().unwrap();
        assert!(String::from_utf8_lossy(&calls[0].1).contains(OLD_KEY));
    }

    #[test]
    fn kms_failure_classes_map_to_the_existing_taxonomy() {
        assert!(matches!(
            kms_error(400, br#"{"__type":"AccessDeniedException"}"#),
            PrismError::Policy(_)
        ));
        assert!(matches!(
            kms_error(400, br#"{"__type":"ThrottlingException"}"#),
            PrismError::Io(_)
        ));
        assert!(matches!(
            kms_error(500, br#"{"__type":"KMSInternalException"}"#),
            PrismError::Io(_)
        ));
    }

    #[test]
    fn sigv4_request_signs_target_context_and_session_token() {
        let credentials = sigv4::Credentials {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "secret".into(),
            session_token: Some("token".into()),
        };
        let request = build_request(
            "kms.us-east-1.amazonaws.com:443",
            "us-east-1",
            &credentials,
            "TrentService.Encrypt",
            br#"{"KeyId":"x"}"#,
            "20260815T120000Z",
            "20260815",
        );
        let text = String::from_utf8(request).unwrap();
        assert!(text.contains("x-amz-target: TrentService.Encrypt"));
        assert!(text.contains("x-amz-security-token: token"));
        assert!(text.contains("Credential=AKIDEXAMPLE/20260815/us-east-1/kms/aws4_request"));
        assert!(!text.contains("secret"));
    }

    #[test]
    fn base64_vectors_and_malformed_input() {
        for (plain, encoded) in [
            (b"".as_slice(), ""),
            (b"f".as_slice(), "Zg=="),
            (b"fo".as_slice(), "Zm8="),
            (b"foo".as_slice(), "Zm9v"),
        ] {
            assert_eq!(base64_encode(plain), encoded);
            assert_eq!(base64_decode(encoded).unwrap(), plain);
        }
        assert!(base64_decode("a==a").is_err());
        assert!(base64_decode("abc").is_err());
    }

    #[test]
    fn a_shared_credentials_file_is_reloaded_after_rotation() {
        let root = std::env::temp_dir().join(format!("prism-kms-creds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("credentials");
        std::fs::write(
            &path,
            "[prism]\naws_access_key_id = first\naws_secret_access_key = secret1\n",
        )
        .unwrap();
        let source = SharedCredentialsFile::new(path.clone(), "prism").unwrap();
        assert_eq!(source.load().unwrap().access_key, "first");
        std::fs::write(
            &path,
            "[prism]\naws_access_key_id = second\naws_secret_access_key = secret2\naws_session_token = token2\n",
        )
        .unwrap();
        let rotated = source.load().unwrap();
        assert_eq!(rotated.access_key, "second");
        assert_eq!(rotated.session_token.as_deref(), Some("token2"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn aliases_and_cross_region_decrypt_keys_are_refused() {
        let credentials: Arc<dyn AwsCredentialsProvider> =
            Arc::new(StaticAwsCredentials(sigv4::Credentials {
                access_key: "a".into(),
                secret_key: "s".into(),
                session_token: None,
            }));
        assert!(AwsKmsConfig::production("us-east-1", "alias/prism", credentials.clone()).is_err());
        let config = AwsKmsConfig::production("us-east-1", KEY, credentials).unwrap();
        assert!(config
            .with_decrypt_key_arns(["arn:aws:kms:us-west-2:123456789012:key/x".into()])
            .is_err());
    }
}
