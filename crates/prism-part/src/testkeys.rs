//! PEM/DER inspection for TLS test fixtures ([issue #34](https://github.com/Bobcatsfan33/PrismDB/issues/34)).
//!
//! **Why this exists.** The TLS fixtures across this workspace generate their certificates by
//! shelling out to `openssl`, and that is **deliberate interop coverage**, not convenience: these
//! are boundary tests whose subject is *"does the server accept the certificates a real PKI hands
//! it"*, and operators generate keys with OpenSSL or LibreSSL, not with an in-process Rust library.
//! Generating fixtures in-stack (`rcgen`/`ring`) would produce ring-friendly keys **by
//! construction** and would therefore have hidden both incompatibilities this workspace has already
//! hit — the explicit-curve-parameters trap, and the one below — while operators kept meeting them
//! in production.
//!
//! **The incompatibility.** LibreSSL DER-encodes an EC private key's scalar with **minimal length**:
//! when the scalar happens to have a leading zero byte, it emits **31 bytes instead of 32**. That
//! occurs for roughly **1 key in 300**. `ring`, the crypto provider behind rustls here, requires
//! exactly 32 bytes for P-256 and refuses the short form with *"failed to parse private key as RSA,
//! ECDSA, or EdDSA"* — a message that names neither the field nor the cause. `openssl pkey` accepts
//! the short form happily, so validating a fixture key with OpenSSL proves nothing about whether
//! rustls will take it.
//!
//! Fixtures therefore check the scalar length themselves and regenerate when it is short. That
//! removes the flake without weakening anything: the server still refuses short-scalar keys, and a
//! dedicated test proves it does.

/// P-256 private scalars are exactly this many bytes in the form `ring` accepts.
pub const P256_SCALAR_BYTES: usize = 32;

/// Decode the base64 body of a PEM document (ignoring the `-----` armour lines).
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in input.bytes() {
        if c == b'=' {
            break;
        }
        let Some(v) = value(c) else {
            if c.is_ascii_whitespace() {
                continue;
            }
            return None;
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// One DER tag-length-value header: returns `(tag, length, offset_of_value)`.
fn tlv(buf: &[u8], mut i: usize) -> Option<(u8, usize, usize)> {
    let tag = *buf.get(i)?;
    i += 1;
    let first = *buf.get(i)? as usize;
    i += 1;
    let len = if first & 0x80 == 0 {
        first
    } else {
        let n = first & 0x7f;
        if n == 0 || n > 4 {
            return None;
        }
        let mut v = 0usize;
        for k in 0..n {
            v = (v << 8) | *buf.get(i + k)? as usize;
        }
        i += n;
        v
    };
    Some((tag, len, i))
}

/// The byte length of the EC private scalar inside a PKCS#8 PEM private key.
///
/// `None` when the document is not a PKCS#8 EC key this walker understands — the caller treats that
/// as "cannot tell", never as "fine".
pub fn pkcs8_ec_scalar_len(pem: &str) -> Option<usize> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    let der = base64_decode(&body)?;

    // PrivateKeyInfo ::= SEQUENCE { version INTEGER, algorithm SEQUENCE, privateKey OCTET STRING }
    let (_, _, mut i) = tlv(&der, 0)?;
    let (_, l, next) = tlv(&der, i)?; // version
    i = next + l;
    let (_, l, next) = tlv(&der, i)?; // AlgorithmIdentifier
    i = next + l;
    let (_, l, next) = tlv(&der, i)?; // privateKey OCTET STRING
    let ec = der.get(next..next + l)?;

    // ECPrivateKey ::= SEQUENCE { version INTEGER, privateKey OCTET STRING, ... }
    let (_, _, mut j) = tlv(ec, 0)?;
    let (_, l, next) = tlv(ec, j)?; // version
    j = next + l;
    let (tag, l, _) = tlv(ec, j)?; // privateKey OCTET STRING
    if tag != 0x04 {
        return None;
    }
    Some(l)
}

/// Whether a PEM private key is in the form `ring` (and therefore rustls) will accept.
///
/// Deliberately conservative: a document this walker cannot interpret returns `false`, because
/// "I could not check" must never read as "it is fine".
pub fn is_ring_compatible_p256(pem: &str) -> bool {
    pkcs8_ec_scalar_len(pem) == Some(P256_SCALAR_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real LibreSSL-generated P-256 key whose scalar is **31 bytes**, captured from a run that
    /// hit the 1-in-300 case. This is the deterministic proof that the check fires — a fixture
    /// guard that has never been shown to reject anything is a guard nobody should trust.
    const SHORT_SCALAR_KEY: &str = include_str!("../testdata/short-scalar-p256.pem");

    #[test]
    fn a_short_scalar_key_is_rejected() {
        assert_eq!(pkcs8_ec_scalar_len(SHORT_SCALAR_KEY), Some(31));
        assert!(
            !is_ring_compatible_p256(SHORT_SCALAR_KEY),
            "a 31-byte scalar is exactly what ring refuses; the guard must reject it"
        );
    }

    #[test]
    fn a_normal_key_is_accepted() {
        // Generated the same way the fixtures do, retried until the scalar is full length.
        let dir = std::env::temp_dir().join(format!("prism-testkeys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut accepted = false;
        for _ in 0..16 {
            let out = std::process::Command::new("openssl")
                .current_dir(&dir)
                .args([
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
                    "1",
                    "-sha256",
                    "-subj",
                    "/CN=t",
                    "-keyout",
                    "k.pem",
                    "-out",
                    "c.pem",
                ])
                .output()
                .expect("run openssl");
            assert!(out.status.success());
            let pem = std::fs::read_to_string(dir.join("k.pem")).unwrap();
            if is_ring_compatible_p256(&pem) {
                assert_eq!(pkcs8_ec_scalar_len(&pem), Some(32));
                accepted = true;
                break;
            }
        }
        assert!(accepted, "16 attempts should find a full-length scalar");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_uninterpretable_document_is_not_reported_as_fine() {
        assert!(!is_ring_compatible_p256(
            "-----BEGIN PRIVATE KEY-----\nnot base64!!\n-----END PRIVATE KEY-----"
        ));
        assert!(!is_ring_compatible_p256(""));
    }
}
