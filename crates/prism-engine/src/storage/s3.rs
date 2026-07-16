//! A minimal hand-rolled S3 client (S11) — [D-065](../../../../docs/DECISIONS.md).
//!
//! HTTP/1.1 over a raw `TcpStream`, SigV4-signed with [`super::sigv4`], the `GET`-Range / `PUT` /
//! conditional-`PUT` / `HEAD` / `DELETE` / list subset the engine needs. **Plain HTTP** — CI's
//! MinIO speaks it, and TLS-over-WAN is the one deferred dependency ([D-065](../../../../docs/DECISIONS.md)).
//!
//! The wire-format-independent pieces — request construction, response parsing, list-XML scanning —
//! are pure functions, unit-tested locally against real HTTP bytes. The socket I/O and the S3
//! semantics (conditional-put, ranged 206, XML shape) are verified end-to-end against a real MinIO
//! server in CI; there is **no mock of S3 behavior** anywhere in the gate path (storage contract §1).

use super::object::{MultipartUpload, ObjectMeta, ObjectStore};
use super::sigv4::{self, Credentials};
use super::MULTIPART_THRESHOLD_BYTES;
use prism_types::error::{PrismError, Result};
use std::io::{Read, Write};
use std::net::TcpStream;

/// Where the store lives and who it is. One backend, one region (scope guard §8).
#[derive(Clone)]
pub struct S3Config {
    /// `host:port` of the endpoint (e.g. `127.0.0.1:9000` for CI MinIO).
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub credentials: Credentials,
    /// A fixed `YYYYMMDDTHHMMSSZ` for deterministic tests; `None` uses the wall clock.
    pub fixed_amz_date: Option<String>,
}

/// A parsed HTTP response.
#[derive(Debug, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        let lname = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| n.to_ascii_lowercase() == lname)
            .map(|(_, v)| v.as_str())
    }
}

/// Parse an HTTP/1.1 response from raw bytes: status line, headers, and a body sized by
/// `Content-Length`. A truncated body (fewer bytes than `Content-Length`) is a **named** error, the
/// same discipline the local reader has (storage §1).
pub fn parse_response(buf: &[u8]) -> Result<HttpResponse> {
    // Skip any `HTTP/1.1 100 Continue\r\n\r\n` interstitial a server may send before the real
    // response (some paths do, even without an `Expect` header).
    let mut buf = buf;
    loop {
        let split = find_subslice(buf, b"\r\n\r\n")
            .ok_or_else(|| PrismError::Corrupt("HTTP response has no header terminator".into()))?;
        let is_continue = buf
            .get(..split)
            .and_then(|h| std::str::from_utf8(h).ok())
            .map(|h| h.starts_with("HTTP/1.1 100") || h.starts_with("HTTP/1.0 100"))
            .unwrap_or(false);
        if is_continue {
            buf = &buf[split + 4..];
        } else {
            break;
        }
    }

    let split = find_subslice(buf, b"\r\n\r\n").unwrap();
    let head = std::str::from_utf8(&buf[..split])
        .map_err(|_| PrismError::Corrupt("HTTP response head is not UTF-8".into()))?;
    let raw_body = &buf[split + 4..];

    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| PrismError::Corrupt("HTTP response has no status line".into()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            PrismError::Corrupt(format!("HTTP status line malformed: `{status_line}`"))
        })?;

    let mut headers = Vec::new();
    for line in lines {
        if let Some((n, v)) = line.split_once(':') {
            headers.push((n.trim().to_string(), v.trim().to_string()));
        }
    }

    let chunked = headers.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked")
    });
    let content_len = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok());

    let body = if chunked {
        decode_chunked(raw_body)?
    } else if let Some(cl) = content_len {
        if raw_body.len() < cl {
            return Err(PrismError::Corrupt(format!(
                "HTTP body is truncated: Content-Length {cl}, but only {} bytes arrived",
                raw_body.len()
            )));
        }
        raw_body[..cl].to_vec()
    } else {
        raw_body.to_vec()
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// Decode an HTTP/1.1 chunked body: `<hexlen>\r\n<bytes>\r\n`... terminated by a `0\r\n`.
fn decode_chunked(mut buf: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = find_subslice(buf, b"\r\n")
            .ok_or_else(|| PrismError::Corrupt("chunked body: missing chunk-size line".into()))?;
        let size_str = std::str::from_utf8(&buf[..line_end])
            .ok()
            .and_then(|s| s.split(';').next())
            .map(str::trim)
            .ok_or_else(|| PrismError::Corrupt("chunked body: bad chunk size".into()))?;
        let size = usize::from_str_radix(size_str, 16).map_err(|_| {
            PrismError::Corrupt(format!("chunked body: bad chunk size `{size_str}`"))
        })?;
        buf = &buf[line_end + 2..];
        if size == 0 {
            break;
        }
        if buf.len() < size {
            return Err(PrismError::Corrupt(format!(
                "chunked body: chunk declares {size} bytes, only {} present",
                buf.len()
            )));
        }
        out.extend_from_slice(&buf[..size]);
        buf = &buf[size + 2..]; // skip the trailing CRLF after the chunk data
    }
    Ok(out)
}

/// Extract the `<Key>` values from an S3 `ListBucketResult` XML body — a small scanner, not a full
/// XML parser (scope guard: only the fields the responses carry).
pub fn parse_list_xml(body: &[u8]) -> Vec<String> {
    let s = String::from_utf8_lossy(body);
    let mut keys = Vec::new();
    let mut rest = s.as_ref();
    while let Some(start) = rest.find("<Key>") {
        let after = &rest[start + 5..];
        if let Some(end) = after.find("</Key>") {
            keys.push(xml_unescape(&after[..end]));
            rest = &after[end + 6..];
        } else {
            break;
        }
    }
    keys
}

/// Extract `(key, size, last-modified-ms)` from each `<Contents>` of a `ListBucketResult` — the
/// fields age-aware reconciliation needs. Still a small scanner over the exact fields S3/MinIO
/// return, not a full XML parser (same scope guard as [`parse_list_xml`]). A `<Contents>` whose
/// `LastModified` does not parse is skipped rather than assigned a wrong age — an unparseable time
/// must never make a live object look old enough to sweep.
pub fn parse_list_xml_meta(body: &[u8]) -> Vec<(String, u64, i64)> {
    let s = String::from_utf8_lossy(body);
    let mut out = Vec::new();
    let mut rest = s.as_ref();
    while let Some(start) = rest.find("<Contents>") {
        let after = &rest[start + "<Contents>".len()..];
        let end = match after.find("</Contents>") {
            Some(e) => e,
            None => break,
        };
        let block = &after[..end];
        rest = &after[end + "</Contents>".len()..];
        let (Some(key), Some(lm)) = (
            inner_tag(block, "Key").map(|v| xml_unescape(&v)),
            inner_tag(block, "LastModified"),
        ) else {
            continue;
        };
        let size = inner_tag(block, "Size")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if let Some(ms) = epoch_ms_from_iso8601(&lm) {
            out.push((key, size, ms));
        }
    }
    out
}

/// The `UploadId` from an `InitiateMultipartUploadResult`.
pub fn parse_multipart_initiate(body: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(body);
    inner_tag(&s, "UploadId").map(|v| xml_unescape(&v))
}

/// Each in-progress upload `(key, upload_id, initiated-ms)` from a `ListMultipartUploadsResult`.
/// An `<Upload>` whose `Initiated` does not parse is skipped rather than assigned a wrong age.
pub fn parse_multipart_list(body: &[u8]) -> Vec<(String, String, i64)> {
    let s = String::from_utf8_lossy(body);
    let mut out = Vec::new();
    let mut rest = s.as_ref();
    while let Some(start) = rest.find("<Upload>") {
        let after = &rest[start + "<Upload>".len()..];
        let end = match after.find("</Upload>") {
            Some(e) => e,
            None => break,
        };
        let block = &after[..end];
        rest = &after[end + "</Upload>".len()..];
        let (Some(key), Some(uid), Some(init)) = (
            inner_tag(block, "Key").map(|v| xml_unescape(&v)),
            inner_tag(block, "UploadId").map(|v| xml_unescape(&v)),
            inner_tag(block, "Initiated"),
        ) else {
            continue;
        };
        if let Some(ms) = epoch_ms_from_iso8601(&init) {
            out.push((key, uid, ms));
        }
    }
    out
}

fn inner_tag(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&close)? + start;
    Some(block[start..end].to_string())
}

/// Parse an S3 `LastModified` (`YYYY-MM-DDThh:mm:ss(.fff)?Z`, UTC) to epoch milliseconds. Returns
/// `None` on any shape it does not recognise — the caller then skips that object rather than sweep
/// it on a guessed age.
pub fn epoch_ms_from_iso8601(s: &str) -> Option<i64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let mo: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    let h: i64 = s[11..13].parse().ok()?;
    let mi: i64 = s[14..16].parse().ok()?;
    let sec: i64 = s[17..19].parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // Fractional seconds, if present (`.fff`), to milliseconds.
    let millis: i64 = if bytes.get(19) == Some(&b'.') {
        let frac: String = s[20..].chars().take_while(|c| c.is_ascii_digit()).collect();
        let mut f = frac;
        f.truncate(3);
        while f.len() < 3 {
            f.push('0');
        }
        f.parse().unwrap_or(0)
    } else {
        0
    };
    let days = days_from_civil(y, mo, d);
    let secs = days * 86400 + h * 3600 + mi * 60 + sec;
    Some(secs * 1000 + millis)
}

/// Days since the Unix epoch for a civil (proleptic Gregorian) date — the inverse of
/// [`civil_from_epoch`]'s date half (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Build the exact request bytes for a signed S3 call — pure, so a test can assert the wire form.
/// `path` is the object path under the bucket (e.g. `/parts/p/rerank.vec`), already URI-safe.
pub fn build_request(
    cfg: &S3Config,
    method: &str,
    path_and_query: &str,
    extra_headers: &[(String, String)],
    body: &[u8],
    amz_date: &str,
    date_stamp: &str,
) -> Vec<u8> {
    let host = cfg.endpoint.clone();
    let payload_sha = sigv4::payload_hash(body);
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path_and_query, ""),
    };

    // The headers that are always signed, plus any extra (Range, If-None-Match, ...).
    let mut headers: Vec<(String, String)> = vec![
        ("host".to_string(), host.clone()),
        ("x-amz-content-sha256".to_string(), payload_sha.clone()),
        ("x-amz-date".to_string(), amz_date.to_string()),
    ];
    for (n, v) in extra_headers {
        headers.push((n.to_ascii_lowercase(), v.clone()));
    }

    let signed = sigv4::sign(
        method,
        path,
        query,
        &headers,
        &payload_sha,
        &cfg.region,
        "s3",
        amz_date,
        date_stamp,
        &cfg.credentials,
    );

    let mut req = format!("{method} {path_and_query} HTTP/1.1\r\n");
    // Emit headers (host first, then the rest), plus Authorization, Content-Length, Connection.
    for (n, v) in &headers {
        req.push_str(&format!("{n}: {v}\r\n"));
    }
    req.push_str(&format!("authorization: {}\r\n", signed.authorization));
    req.push_str(&format!("content-length: {}\r\n", body.len()));
    req.push_str("connection: close\r\n\r\n");

    let mut out = req.into_bytes();
    out.extend_from_slice(body);
    out
}

/// Read one HTTP response from a stream, length-delimited. Reads headers, then the body by
/// `Content-Length` / `Transfer-Encoding: chunked` / to-EOF — correct on a kept-alive connection,
/// where `read_to_end` would block. A HEAD response (`is_head`) carries `Content-Length` but no body.
fn read_http(stream: &mut TcpStream, is_head: bool) -> Result<HttpResponse> {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 8192];
    // 1) headers.
    let header_end = loop {
        if let Some(p) = find_subslice(&buf, b"\r\n\r\n") {
            break p;
        }
        let n = stream
            .read(&mut tmp)
            .map_err(|e| PrismError::Io(format!("remote unavailable: read: {e}")))?;
        if n == 0 {
            return Err(PrismError::Corrupt(
                "connection closed before the response headers were complete".into(),
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| PrismError::Corrupt("HTTP response head is not UTF-8".into()))?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| PrismError::Corrupt("HTTP status line malformed".into()))?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((n, v)) = line.split_once(':') {
            headers.push((n.trim().to_string(), v.trim().to_string()));
        }
    }
    let chunked = headers.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked")
    });
    let content_len = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok());

    let mut body = buf[header_end + 4..].to_vec();
    if !is_head {
        if chunked {
            // Read until the terminating `0\r\n\r\n`, then decode.
            while find_subslice(&body, b"0\r\n\r\n").is_none() {
                let n = stream
                    .read(&mut tmp)
                    .map_err(|e| PrismError::Io(format!("remote unavailable: read: {e}")))?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&tmp[..n]);
            }
            body = decode_chunked(&body)?;
        } else if let Some(cl) = content_len {
            while body.len() < cl {
                let n = stream
                    .read(&mut tmp)
                    .map_err(|e| PrismError::Io(format!("remote unavailable: read: {e}")))?;
                if n == 0 {
                    return Err(PrismError::Corrupt(format!(
                        "HTTP body is truncated: Content-Length {cl}, connection closed after {} bytes",
                        body.len()
                    )));
                }
                body.extend_from_slice(&tmp[..n]);
            }
            body.truncate(cl);
        } else {
            // No length signalled: read to EOF.
            loop {
                let n = stream
                    .read(&mut tmp)
                    .map_err(|e| PrismError::Io(format!("remote unavailable: read: {e}")))?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&tmp[..n]);
            }
        }
    } else {
        body.clear();
    }
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// A real S3 object store over the hand-rolled client.
pub struct S3ObjectStore {
    cfg: S3Config,
}

impl S3ObjectStore {
    pub fn new(cfg: S3Config) -> Self {
        S3ObjectStore { cfg }
    }

    fn dates(&self) -> (String, String) {
        // A fixed date for tests; otherwise the wall clock. Clock-skew handling (a named
        // RequestTimeTooSkewed with a bounded retry after resync) lives in `send` on a 403.
        let amz = self.cfg.fixed_amz_date.clone().unwrap_or_else(now_amz_date);
        let stamp = amz[..8].to_string();
        (amz, stamp)
    }

    fn object_path(&self, key: &str) -> String {
        format!(
            "/{}/{}",
            self.cfg.bucket,
            sigv4::uri_encode(key, true).trim_start_matches('/')
        )
    }

    /// Send a signed request over a fresh connection and parse the response.
    fn send(
        &self,
        method: &str,
        path_and_query: &str,
        extra_headers: &[(String, String)],
        body: &[u8],
    ) -> Result<HttpResponse> {
        let (amz, stamp) = self.dates();
        let bytes = build_request(
            &self.cfg,
            method,
            path_and_query,
            extra_headers,
            body,
            &amz,
            &stamp,
        );
        let mut stream = TcpStream::connect(&self.cfg.endpoint).map_err(|e| {
            PrismError::Io(format!(
                "remote unavailable: connect {}: {e}",
                self.cfg.endpoint
            ))
        })?;
        stream
            .write_all(&bytes)
            .map_err(|e| PrismError::Io(format!("remote unavailable: write: {e}")))?;
        // Read length-delimited (not read_to_end, which hangs on a kept-alive connection). A HEAD
        // response carries Content-Length but NO body by the HTTP spec, so it must not be treated as
        // a truncation.
        let parsed = read_http(&mut stream, method == "HEAD")?;
        // Clock skew: S3 answers a badly-skewed request with 403 RequestTimeTooSkewed. Name it so a
        // caller resyncs and retries within bounds rather than treating it as an auth failure.
        if parsed.status == 403
            && String::from_utf8_lossy(&parsed.body).contains("RequestTimeTooSkewed")
        {
            return Err(PrismError::Invalid(
                "RequestTimeTooSkewed: local clock differs from the S3 server beyond tolerance; \
                 resync the clock and retry"
                    .into(),
            ));
        }
        Ok(parsed)
    }

    // --- multipart upload (initiate / part / complete / abort) — plain SigV4, one part per chunk.

    /// Begin a multipart upload; returns the `UploadId` that identifies its server-side parts.
    pub fn initiate_multipart(&self, key: &str) -> Result<String> {
        let path = format!("{}?uploads=", self.object_path(key));
        let r = self.send("POST", &path, &[], b"")?;
        if r.status != 200 {
            return Err(PrismError::Io(format!(
                "initiate multipart `{key}` returned HTTP {}",
                r.status
            )));
        }
        parse_multipart_initiate(&r.body).ok_or_else(|| {
            PrismError::Corrupt(format!(
                "initiate multipart `{key}`: no UploadId in response"
            ))
        })
    }

    /// Upload one part (1-based `part_number`); returns its `ETag`, needed to complete the upload.
    pub fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
        bytes: &[u8],
    ) -> Result<String> {
        let path = format!(
            "{}?partNumber={}&uploadId={}",
            self.object_path(key),
            part_number,
            sigv4::uri_encode(upload_id, false)
        );
        let r = self.send("PUT", &path, &[], bytes)?;
        if r.status != 200 {
            return Err(PrismError::Io(format!(
                "upload part {part_number} of `{key}` returned HTTP {}",
                r.status
            )));
        }
        r.header("etag").map(str::to_string).ok_or_else(|| {
            PrismError::Corrupt(format!("upload part {part_number} of `{key}`: no ETag"))
        })
    }

    /// Complete the upload, assembling the parts (each `(number, etag)`) into the final object.
    pub fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        parts: &[(u32, String)],
    ) -> Result<()> {
        let mut body = String::from("<CompleteMultipartUpload>");
        for (n, etag) in parts {
            body.push_str(&format!(
                "<Part><PartNumber>{n}</PartNumber><ETag>{etag}</ETag></Part>"
            ));
        }
        body.push_str("</CompleteMultipartUpload>");
        let path = format!(
            "{}?uploadId={}",
            self.object_path(key),
            sigv4::uri_encode(upload_id, false)
        );
        let r = self.send("POST", &path, &[], body.as_bytes())?;
        if r.status != 200 {
            return Err(PrismError::Io(format!(
                "complete multipart `{key}` returned HTTP {}",
                r.status
            )));
        }
        // S3 can answer a completion with HTTP 200 and an `<Error>` in the body — name it, never
        // treat it as success.
        if String::from_utf8_lossy(&r.body).contains("<Error>") {
            return Err(PrismError::Io(format!(
                "complete multipart `{key}` failed: {}",
                String::from_utf8_lossy(&r.body)
            )));
        }
        Ok(())
    }

    /// Put a large object as a multipart upload, one part per [`MULTIPART_THRESHOLD_BYTES`] chunk. A
    /// failure mid-upload aborts the upload so no server-side parts leak, and propagates the cause.
    fn put_multipart(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let upload_id = self.initiate_multipart(key)?;
        let mut parts = Vec::new();
        for (i, chunk) in bytes.chunks(MULTIPART_THRESHOLD_BYTES).enumerate() {
            let n = i as u32 + 1;
            match self.upload_part(key, &upload_id, n, chunk) {
                Ok(etag) => parts.push((n, etag)),
                Err(e) => {
                    let _ = self.abort_multipart(key, &upload_id);
                    return Err(e);
                }
            }
        }
        self.complete_multipart(key, &upload_id, &parts)
    }
}

impl ObjectStore for S3ObjectStore {
    fn get(&self, key: &str) -> Result<Vec<u8>> {
        let r = self.send("GET", &self.object_path(key), &[], b"")?;
        match r.status {
            200 => Ok(r.body),
            404 => Err(PrismError::NotFound(format!(
                "object `{key}` does not exist"
            ))),
            s => Err(PrismError::Io(format!("GET `{key}` returned HTTP {s}"))),
        }
    }

    fn get_range(&self, key: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        let range = (
            "range".to_string(),
            format!("bytes={}-{}", offset, offset + len as u64 - 1),
        );
        let r = self.send("GET", &self.object_path(key), &[range], b"")?;
        match r.status {
            200 | 206 => {
                if r.body.len() < len {
                    return Err(PrismError::Corrupt(format!(
                        "object `{key}` is truncated: range needed {len} bytes at offset {offset}, \
                         the remote returned {}",
                        r.body.len()
                    )));
                }
                Ok(r.body[..len].to_vec())
            }
            404 => Err(PrismError::NotFound(format!(
                "object `{key}` does not exist"
            ))),
            s => Err(PrismError::Io(format!(
                "GET range `{key}` returned HTTP {s}"
            ))),
        }
    }

    fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        // A large object goes up as multipart, so a mid-upload crash resumes and its server-side
        // parts are sweepable, rather than re-sent whole (storage §2).
        if bytes.len() >= MULTIPART_THRESHOLD_BYTES {
            return self.put_multipart(key, bytes);
        }
        let r = self.send("PUT", &self.object_path(key), &[], bytes)?;
        match r.status {
            200 | 201 => Ok(()),
            s => Err(PrismError::Io(format!("PUT `{key}` returned HTTP {s}"))),
        }
    }

    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<bool> {
        let cond = ("if-none-match".to_string(), "*".to_string());
        let r = self.send("PUT", &self.object_path(key), &[cond], bytes)?;
        match r.status {
            200 | 201 => Ok(true),
            412 => Ok(false), // Precondition Failed: the object already existed.
            s => Err(PrismError::Io(format!(
                "conditional PUT `{key}` returned HTTP {s}"
            ))),
        }
    }

    fn head(&self, key: &str) -> Result<Option<u64>> {
        let r = self.send("HEAD", &self.object_path(key), &[], b"")?;
        match r.status {
            200 => Ok(r.header("content-length").and_then(|v| v.parse().ok())),
            404 => Ok(None),
            s => Err(PrismError::Io(format!("HEAD `{key}` returned HTTP {s}"))),
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        let r = self.send("DELETE", &self.object_path(key), &[], b"")?;
        match r.status {
            200 | 204 | 404 => Ok(()),
            s => Err(PrismError::Io(format!("DELETE `{key}` returned HTTP {s}"))),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let path = format!(
            "/{}?list-type=2&prefix={}",
            self.cfg.bucket,
            sigv4::uri_encode(prefix, false)
        );
        let r = self.send("GET", &path, &[], b"")?;
        match r.status {
            200 => Ok(parse_list_xml(&r.body)),
            s => Err(PrismError::Io(format!("LIST returned HTTP {s}"))),
        }
    }

    fn list_meta(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let path = format!(
            "/{}?list-type=2&prefix={}",
            self.cfg.bucket,
            sigv4::uri_encode(prefix, false)
        );
        let r = self.send("GET", &path, &[], b"")?;
        match r.status {
            200 => Ok(parse_list_xml_meta(&r.body)
                .into_iter()
                .map(|(key, size, last_modified_ms)| ObjectMeta {
                    key,
                    size,
                    last_modified_ms,
                })
                .collect()),
            s => Err(PrismError::Io(format!("LIST returned HTTP {s}"))),
        }
    }

    fn list_multipart(&self, prefix: &str) -> Result<Vec<MultipartUpload>> {
        // Enumerate all in-progress uploads and filter by key prefix client-side: MinIO's
        // ListMultipartUploads honours a server-side `prefix` inconsistently, so we do not depend on
        // it (and there are few in-progress uploads to enumerate — a crashed publication's leftovers,
        // not steady-state traffic). The valueless `uploads` is written `uploads=` so the request
        // line and the signed canonical query match.
        let path = format!("/{}?uploads=", self.cfg.bucket);
        let r = self.send("GET", &path, &[], b"")?;
        match r.status {
            200 => Ok(parse_multipart_list(&r.body)
                .into_iter()
                .filter(|(key, _, _)| key.starts_with(prefix))
                .map(|(key, upload_id, initiated_ms)| MultipartUpload {
                    key,
                    upload_id,
                    initiated_ms,
                })
                .collect()),
            s => Err(PrismError::Io(format!("LIST multipart returned HTTP {s}"))),
        }
    }

    fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<()> {
        let path = format!(
            "{}?uploadId={}",
            self.object_path(key),
            sigv4::uri_encode(upload_id, false)
        );
        let r = self.send("DELETE", &path, &[], b"")?;
        match r.status {
            200 | 204 | 404 => Ok(()),
            s => Err(PrismError::Io(format!(
                "abort multipart `{key}` returned HTTP {s}"
            ))),
        }
    }
}

fn now_amz_date() -> String {
    // A deterministic-enough UTC stamp without a time dependency: seconds since epoch → date/time.
    // Good enough to sign against a server whose clock is within tolerance; a real skew surfaces as
    // the named RequestTimeTooSkewed from S3 (handled in `send`).
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_from_epoch(secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// Convert epoch seconds to (year, month, day, hour, min, sec) UTC — a small, dependency-free
/// civil-time conversion (Howard Hinnant's algorithm).
fn civil_from_epoch(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86400) as i64;
    let rem = (secs % 86400) as u32;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> S3Config {
        S3Config {
            endpoint: "127.0.0.1:9000".into(),
            region: "us-east-1".into(),
            bucket: "prism".into(),
            credentials: Credentials {
                access_key: "AKIDEXAMPLE".into(),
                secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            },
            fixed_amz_date: Some("20150830T123600Z".into()),
        }
    }

    #[test]
    fn iso8601_epoch_anchor_and_roundtrip() {
        // Absolute anchor.
        assert_eq!(epoch_ms_from_iso8601("1970-01-01T00:00:00Z"), Some(0));
        // Fractional seconds → milliseconds.
        assert_eq!(epoch_ms_from_iso8601("1970-01-01T00:00:00.250Z"), Some(250));
        // Inverse of civil_from_epoch across a wide span of dates.
        for secs in [1u64, 1_000_000, 1_440_938_160, 1_760_000_000, 4_102_444_800] {
            let (y, mo, d, h, mi, s) = civil_from_epoch(secs);
            let iso = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
            assert_eq!(
                epoch_ms_from_iso8601(&iso),
                Some(secs as i64 * 1000),
                "roundtrip failed for {iso}"
            );
        }
        // Unparseable shapes yield None rather than a wrong (sweepable) age.
        assert_eq!(epoch_ms_from_iso8601("not-a-date"), None);
        assert_eq!(epoch_ms_from_iso8601("2026/07/16 20:10:10"), None);
    }

    #[test]
    fn parses_multipart_initiate_and_list() {
        let init = br#"<?xml version="1.0"?><InitiateMultipartUploadResult><Bucket>prism</Bucket><Key>parts/a/rerank.vec</Key><UploadId>abc-123-xyz</UploadId></InitiateMultipartUploadResult>"#;
        assert_eq!(
            parse_multipart_initiate(init),
            Some("abc-123-xyz".to_string())
        );

        let list = br#"<?xml version="1.0"?><ListMultipartUploadsResult>
          <Upload><Key>parts/p1/rerank.vec</Key><UploadId>uid-1</UploadId><Initiated>2026-07-16T20:10:10.000Z</Initiated></Upload>
          <Upload><Key>parts/p2/rerank.vec</Key><UploadId>uid-2</UploadId><Initiated>2026-07-16T20:11:00Z</Initiated></Upload>
        </ListMultipartUploadsResult>"#;
        let got = parse_multipart_list(list);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "parts/p1/rerank.vec");
        assert_eq!(got[0].1, "uid-1");
        assert_eq!(
            got[0].2,
            epoch_ms_from_iso8601("2026-07-16T20:10:10.000Z").unwrap()
        );
        assert_eq!(got[1].1, "uid-2");
    }

    #[test]
    fn parses_list_contents_with_size_and_time() {
        let body = br#"<?xml version="1.0"?><ListBucketResult>
          <Contents><Key>parts/a/rerank.vec</Key><LastModified>2026-07-16T20:10:10.000Z</LastModified><Size>512</Size></Contents>
          <Contents><Key>catalog/SNAPSHOT-s00000003</Key><LastModified>2026-07-16T20:11:00Z</LastModified><Size>2048</Size></Contents>
        </ListBucketResult>"#;
        let got = parse_list_xml_meta(body);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "parts/a/rerank.vec");
        assert_eq!(got[0].1, 512);
        assert_eq!(
            got[0].2,
            epoch_ms_from_iso8601("2026-07-16T20:10:10.000Z").unwrap()
        );
        assert_eq!(got[1].0, "catalog/SNAPSHOT-s00000003");
        assert_eq!(got[1].1, 2048);
    }

    #[test]
    fn builds_a_signed_request() {
        let bytes = build_request(
            &cfg(),
            "GET",
            "/prism/parts/p/rerank.vec",
            &[],
            b"",
            "20150830T123600Z",
            "20150830",
        );
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.starts_with("GET /prism/parts/p/rerank.vec HTTP/1.1\r\n"));
        assert!(s.contains("host: 127.0.0.1:9000\r\n"));
        assert!(s.contains("x-amz-date: 20150830T123600Z\r\n"));
        assert!(s.contains("authorization: AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/s3/aws4_request"));
        assert!(s.contains("connection: close\r\n\r\n"));
    }

    #[test]
    fn parses_a_ranged_response() {
        let raw = b"HTTP/1.1 206 Partial Content\r\nContent-Length: 5\r\nContent-Range: bytes 0-4/100\r\n\r\nhello";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 206);
        assert_eq!(r.body, b"hello");
        assert_eq!(r.header("content-length"), Some("5"));
    }

    #[test]
    fn a_short_body_is_named() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhi"; // says 10, has 2
        let err = parse_response(raw).unwrap_err().to_string();
        assert!(
            err.contains("truncated") && err.contains("Content-Length 10"),
            "{err}"
        );
    }

    #[test]
    fn parses_a_chunked_response_and_skips_100_continue() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello world");
    }

    #[test]
    fn parses_list_xml_keys() {
        let body = br#"<?xml version="1.0"?><ListBucketResult><Contents><Key>parts/a/rerank.vec</Key></Contents><Contents><Key>parts/b/rerank.vec</Key></Contents></ListBucketResult>"#;
        assert_eq!(
            parse_list_xml(body),
            vec![
                "parts/a/rerank.vec".to_string(),
                "parts/b/rerank.vec".to_string()
            ]
        );
    }

    #[test]
    fn civil_time_is_correct_for_a_known_epoch() {
        // 1440938160 = 2015-08-30T12:36:00Z (the get-vanilla date).
        assert_eq!(civil_from_epoch(1_440_938_160), (2015, 8, 30, 12, 36, 0));
    }
}
