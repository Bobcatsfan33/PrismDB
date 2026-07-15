//! The binary part format (v2) — S1.
//!
//! Three things v1 (a JSON manifest and unframed column files) could not do:
//!
//! 1. **Refuse what it does not understand.** An explicit header carries the
//!    format version, the byte order, and a feature bitset. A part that needs a
//!    feature this build has never heard of is *refused*, not guessed at. Codecs
//!    are ids, so a column encoded with something we cannot decode says so
//!    instead of returning plausible garbage.
//! 2. **Localize damage.** Column files are sequences of checksummed blocks. A
//!    flipped byte condemns one 64 KiB block and names it, rather than
//!    condemning a whole column.
//! 3. **Never allocate on trust.** Every length in this file arrives from a
//!    stranger. [`Cursor`] validates each one against the bytes actually
//!    present *before* it reserves anything. A manifest claiming four billion
//!    centroid ranges must produce an error, not an OOM kill.
//!
//! Everything is little-endian and says so in the header.

use prism_types::error::{PrismError, Result};
use prism_types::hash::crc32;
use serde::{Deserialize, Serialize};

/// `PRSMPART`. If these eight bytes are not first, this is not a part.
pub const MAGIC: &[u8; 8] = b"PRSMPART";

/// Bumped whenever the *core row shape* changes. Every released version keeps a
/// fixture in `testing/compat/`, and the reader dispatches on this.
///
/// **v3 is designed to be the last cheap bump.** The compat corpus grows with
/// every major version, and each one is a decoder we carry forever — so v3 adds
/// two mechanisms whose whole job is to make a v4 unnecessary:
///
/// 1. **Feature-flag bits** ([`SUPPORTED_FEATURES`]) for additions that change
///    what stored bytes *mean*. An older reader refuses a bit it does not know,
///    rather than misreading the part — which is the only thing a version bump was
///    ever really buying.
/// 2. **A TLV extension section** ([`Extension`]) for additions that add *new*
///    metadata without changing the meaning of what is already there. Extensions
///    carry a required/optional bit, so a reader knows whether it may skip one.
///
/// S3's typed columns and S4's partitioning metadata are therefore *flagged
/// extensions on v3*, not v4 and v5.
pub const FORMAT_VERSION: u32 = 3;

/// Binary formats this build can still read. v1 is JSON and handled separately.
pub const SUPPORTED_BINARY_VERSIONS: &[u32] = &[2, 3];

/// The JSON-manifest format S0 wrote.
pub const LEGACY_FORMAT_VERSION: u32 = 1;

pub const BYTE_ORDER_LITTLE: u8 = 0;

/// The **default** logical bytes per block, used when a store does not say
/// otherwise.
///
/// A *tuned* constant under charter amendment C-1: it is derived from measurement,
/// and its receipt is `testing/evidence/block-size.json`. The trade-off it sits in
/// the middle of is real and two-sided:
///
/// * **Bigger blocks** waste bytes on a small ranged read — asking for a 300-byte
///   centroid range from a 1 MiB block reads 1 MiB. Read amplification.
/// * **Smaller blocks** grow the block directory in *every manifest*, which every
///   reader pays on every open, and add a 24-byte frame header per block.
///
/// The block size is stored per column in the manifest, so this is a default and
/// not a law: a part written under one block size is readable forever, whatever
/// this constant later becomes.
///
/// **It was 64 KiB in S1, and 64 KiB was wrong.** It had been chosen because 64 KiB
/// is what people choose. The derivation (`testing/evidence/block-size.json`) shows
/// it cost a **247x read amplification** on the golden query set — a 300-byte
/// centroid range and a 256-byte rerank vector were each dragging a 64 KiB block off
/// the disk behind them. At 4 KiB the same queries move **5.6x fewer bytes** and run
/// **4.4x faster** at p50.
///
/// The rule is constrained, not naive: minimise bytes physically read, *subject to*
/// the manifest block directory staying under 4 bytes per row. Without the
/// constraint the sweep collapses onto its smallest candidate — but the directory is
/// read in full on every part open, including the opens that prune the part away, so
/// 512-byte blocks would give a billion-row part a ~16 GB directory that every reader
/// must load before it can decide the part is irrelevant.
// Re-derived from 4 KiB to 2 KiB in S6: the S4/S5 manifest extensions grew the fixed
// per-part overhead, so the block-size sweep's budget was corrected to the DIRECTORY term alone
// and re-run against the current engine (docs/DETERMINISM-CONTRACT.md §5; testing/evidence/block-size.json).
pub const DEFAULT_BLOCK_SIZE: u32 = 2 * 1024;

/// Retained as the v2 block size: v2 parts have no per-column block size field, so
/// they are all this, forever.
pub const BLOCK_SIZE: u32 = 64 * 1024;

/// `PBLK` — the frame marker at the head of every block.
pub const BLOCK_MAGIC: u32 = 0x4B4C_4250;

/// Frame header: magic + block_index + logical_offset + payload_len + crc32.
pub const FRAME_HEADER_BYTES: usize = 4 + 4 + 8 + 4 + 4;

// --- feature flags -----------------------------------------------------------
//
// A bit set here that this build does not know is a hard refusal. That is the
// whole point of the field: it lets a future version add something that changes
// the meaning of stored bytes and be *certain* an older reader will not silently
// misread it.

/// Column files are block-framed (set by every v2 and v3 part).
pub const FEATURE_BLOCK_FRAMING: u64 = 1 << 0;

/// The part carries bounded typed attributes and a partition key dictionary (S2).
pub const FEATURE_ATTRIBUTES: u64 = 1 << 1;

/// The part carries `observed_time` and W3C trace context (S2).
pub const FEATURE_TRACE_CONTEXT: u64 = 1 << 2;

// --- reserved, and deliberately unimplemented --------------------------------
//
// These are declared so that the *number* is nailed down now and two sprints
// cannot quietly claim the same bit. They are NOT in SUPPORTED_FEATURES: a part
// that sets one is refused by this build, which is exactly right — we cannot read
// it, and guessing would be worse than failing.

/// S4: hot attributes promoted to typed top-level columns (issue #2).
pub const FEATURE_PROMOTED_COLUMNS: u64 = 1 << 3;
/// S4: outer-partition metadata (tenant-bucket x time-window x generation).
pub const FEATURE_PARTITION_META: u64 = 1 << 4;
/// S4: dictionary / delta / general column compression.
pub const FEATURE_COLUMN_COMPRESSION: u64 = 1 << 5;
/// S14: envelope encryption key id.
pub const FEATURE_ENCRYPTION: u64 = 1 << 6;

/// Every feature this build understands. Anything outside this mask is refused.
pub const SUPPORTED_FEATURES: u64 = FEATURE_BLOCK_FRAMING
    | FEATURE_ATTRIBUTES
    | FEATURE_TRACE_CONTEXT
    | FEATURE_PARTITION_META
    | FEATURE_PROMOTED_COLUMNS;

// --- TLV extensions -----------------------------------------------------------

/// An extension id with this bit set is **required**: a reader that does not know
/// it must refuse the part. Without the bit, the extension is optional metadata
/// and an old reader may skip it.
///
/// This is what lets a future sprint add manifest metadata without a version bump
/// *and* without the risk that an old reader silently ignores something essential.
pub const EXT_REQUIRED: u16 = 0x8000;

/// A manifest extension: a typed, length-prefixed blob the core layout knows
/// nothing about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extension {
    pub id: u16,
    pub bytes: Vec<u8>,
}

impl Extension {
    pub fn is_required(&self) -> bool {
        self.id & EXT_REQUIRED != 0
    }
}

/// Extension ids this build understands.
///
/// The mechanism shipped in S2 with this list **empty**, on purpose, so that its first user
/// would not also be a format break. S4 is that first user: partition metadata, per-tenant
/// scoped statistics and promoted columns land here as a flagged extension on v3 — not a v4
/// (D-020).
///
/// Forgetting to register an extension here is not a subtle bug: the build refuses its own
/// parts, loudly, at the commit that would have published them. It did, during S4.
pub const SUPPORTED_EXTENSIONS: &[u16] =
    &[crate::ext::EXT_S4_PARTITION, crate::ext::EXT_S5_LINEAGE];

/// Fixed reserved manifest words. Must be zero; a non-zero value means a writer
/// put something there that this build cannot interpret, and we refuse rather
/// than ignore it. A cheap slot for a future fixed-width field, guarded by a
/// feature bit.
pub const RESERVED_WORDS: usize = 4;

// --- codec ids ---------------------------------------------------------------

/// Raw little-endian fixed-width values, or raw bytes. The only codec v2 writes.
/// Dictionary, delta and general compression arrive in S4 as new ids — which is
/// why this is an id and not a boolean.
pub const CODEC_RAW: u16 = 1;

pub fn codec_name(id: u16) -> &'static str {
    match id {
        CODEC_RAW => "raw",
        _ => "unknown",
    }
}

// --- the rerank-tier descriptor (D-003-resolved) -----------------------------
//
// The exact-rerank tier is the biggest open cost question in the project: full
// float32 is ~32x the size of the compressed scan tier and dominates the storage
// bill. PRISM.md deliberately does not choose between float32-cold, fp16 with a
// stated accuracy contract, re-embed-on-demand, and residual quantization.
//
// So the format does not choose either. It *describes*. Every part declares
// which encoding its rerank vectors use and which accuracy contract that
// encoding owes the caller, and the reader dispatches on the pair. float32-exact
// is the only encoding implemented today; adding fp16 later means a new
// encoding id and a generation migration that rewrites parts — never a format
// break, and never a silent change in what a stored byte means.

/// Full IEEE-754 binary32, stored cold, never on the scan path.
pub const RERANK_ENCODING_FLOAT32: u16 = 1;

/// IEEE-754 binary16 (half precision) — 2 bytes/dim, half the exact-tier storage bill (S7). Lossy
/// by construction, so it may enter the format *only* under a negotiated accuracy contract.
pub const RERANK_ENCODING_FLOAT16: u16 = 2;

/// The re-rank score is the exact cosine against the stored vector. Zero
/// approximation error: the vector *is* the vector that was embedded.
pub const RERANK_CONTRACT_EXACT: u16 = 1;

/// The **fp16 cosine accuracy contract** (S7, [D-049](../../../docs/DECISIONS.md)) — the first
/// negotiated accuracy contract in the system, and the precedent for every lossy encoding after
/// it.
///
/// It promises: a rerank score computed from an fp16-stored vector differs from the fp32-exact
/// score by no more than the contract's tolerance, and — the property that actually matters —
/// **selection is stable**: the ordered set of returned event ids is identical to fp32-exact on
/// the golden corpus at this tolerance. The tolerance and the selection-stability evidence are
/// committed as a receipt (`testing/evidence/fp16.json`), and this build refuses any part whose
/// accuracy contract it does not implement rather than guessing at what its scores mean.
pub const RERANK_CONTRACT_FP16_COSINE: u16 = 2;

/// The tolerance the fp16 cosine contract promises. **Tuned** (charter C-1): measured as the
/// worst |fp16 − fp32| cosine gap over the golden corpus, with headroom, and committed in the
/// receipt. An absolute bound because a cosine near zero has no relative scale.
pub const FP16_COSINE_TOLERANCE: f32 = 2e-3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RerankDescriptor {
    pub encoding_id: u16,
    pub accuracy_contract_id: u16,
}

impl RerankDescriptor {
    /// What v1 parts implicitly were, and what v2 writes today.
    pub fn float32_exact() -> Self {
        RerankDescriptor {
            encoding_id: RERANK_ENCODING_FLOAT32,
            accuracy_contract_id: RERANK_CONTRACT_EXACT,
        }
    }

    /// fp16 storage under the fp16-cosine accuracy contract (S7). Never a default -- fp32-exact
    /// stays the default; a store opts into fp16 deliberately, accepting the contract's tolerance.
    pub fn float16_cosine() -> Self {
        RerankDescriptor {
            encoding_id: RERANK_ENCODING_FLOAT16,
            accuracy_contract_id: RERANK_CONTRACT_FP16_COSINE,
        }
    }

    /// Bytes on disk for one vector under this encoding.
    pub fn bytes_per_vector(&self, dim: usize) -> Result<usize> {
        match self.encoding_id {
            RERANK_ENCODING_FLOAT32 => Ok(dim * 4),
            RERANK_ENCODING_FLOAT16 => Ok(dim * 2),
            other => Err(PrismError::Corrupt(format!(
                "part declares rerank encoding id {other}, which this build cannot decode; \
                 refusing to guess at the meaning of its stored vectors"
            ))),
        }
    }

    /// Decode one vector. Dispatched on the declared encoding — never assumed.
    pub fn decode_vector(&self, bytes: &[u8], dim: usize) -> Result<Vec<f32>> {
        match self.encoding_id {
            RERANK_ENCODING_FLOAT32 => {
                if bytes.len() != dim * 4 {
                    return Err(PrismError::Corrupt(format!(
                        "rerank vector is {} bytes, expected {} for a {dim}-dim float32 vector",
                        bytes.len(),
                        dim * 4
                    )));
                }
                Ok(bytes
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect())
            }
            RERANK_ENCODING_FLOAT16 => {
                if bytes.len() != dim * 2 {
                    return Err(PrismError::Corrupt(format!(
                        "rerank vector is {} bytes, expected {} for a {dim}-dim float16 vector",
                        bytes.len(),
                        dim * 2
                    )));
                }
                // Widen each half back to f32. This IS the lossy value the rerank sees, and the
                // fp16-cosine contract bounds how far the resulting score can be from fp32-exact.
                Ok(bytes
                    .chunks_exact(2)
                    .map(|c| prism_types::half::f16_bits_to_f32(u16::from_le_bytes([c[0], c[1]])))
                    .collect())
            }
            other => Err(PrismError::Corrupt(format!(
                "part declares rerank encoding id {other}, which this build cannot decode"
            ))),
        }
    }

    /// Encode one vector.
    pub fn encode_vector(&self, v: &[f32]) -> Result<Vec<u8>> {
        match self.encoding_id {
            RERANK_ENCODING_FLOAT32 => {
                let mut out = Vec::with_capacity(v.len() * 4);
                for x in v {
                    out.extend_from_slice(&x.to_le_bytes());
                }
                Ok(out)
            }
            RERANK_ENCODING_FLOAT16 => {
                let mut out = Vec::with_capacity(v.len() * 2);
                for x in v {
                    out.extend_from_slice(&prism_types::half::f32_to_f16_bits(*x).to_le_bytes());
                }
                Ok(out)
            }
            other => Err(PrismError::Invariant(format!(
                "this build cannot write rerank encoding id {other}"
            ))),
        }
    }

    pub fn describe(&self) -> String {
        let enc = match self.encoding_id {
            RERANK_ENCODING_FLOAT32 => "float32",
            RERANK_ENCODING_FLOAT16 => "float16",
            _ => "unknown",
        };
        let contract = match self.accuracy_contract_id {
            RERANK_CONTRACT_EXACT => "exact",
            RERANK_CONTRACT_FP16_COSINE => "fp16-cosine",
            _ => "unknown",
        };
        format!("{enc}/{contract}")
    }

    /// Refuse a descriptor this build cannot honour, at open time, loudly.
    pub fn validate(&self) -> Result<()> {
        // The (encoding, contract) pairs this build implements. A pair not on this list is refused
        // at open time, loudly -- never decoded into numbers that mean something other than what
        // the caller expects.
        let known = matches!(
            (self.encoding_id, self.accuracy_contract_id),
            (RERANK_ENCODING_FLOAT32, RERANK_CONTRACT_EXACT)
                | (RERANK_ENCODING_FLOAT16, RERANK_CONTRACT_FP16_COSINE)
        );
        if !known {
            return Err(PrismError::Corrupt(format!(
                "part declares rerank encoding id {} with accuracy contract id {}, a pairing this \
                 build does not implement; its re-rank scores would not mean what the caller \
                 expects. Upgrade, or migrate the part to an encoding/contract this build supports.",
                self.encoding_id, self.accuracy_contract_id
            )));
        }
        Ok(())
    }
}

// --- a reader that never trusts a length -------------------------------------

/// A bounds-checked cursor over untrusted bytes.
///
/// Every `read_*` validates against the bytes actually remaining before it does
/// anything. `read_len` additionally refuses a count that could not possibly be
/// backed by the remaining bytes — which is what stops a corrupt manifest
/// claiming `u64::MAX` centroid ranges from reserving 400 exabytes and taking
/// the process with it. The S1 gate says: *no untrusted length allocates
/// unbounded.* This type is where that is true.
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(PrismError::Corrupt(format!(
                "truncated: wanted {n} bytes at offset {}, only {} remain",
                self.pos,
                self.remaining()
            )));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    pub fn f64(&mut self) -> Result<f64> {
        let bits = self.u64()?;
        let v = f64::from_bits(bits);
        if !v.is_finite() {
            return Err(PrismError::Corrupt(format!(
                "manifest holds a non-finite float at offset {}",
                self.pos - 8
            )));
        }
        Ok(v)
    }

    /// A count of elements, each at least `min_elem_bytes` on the wire.
    ///
    /// This is the guard. A length is only believable if the bytes to back it
    /// are actually here. `usize::MAX` centroid ranges in a 200-byte manifest is
    /// a corrupt manifest, and it is refused *before* anything is reserved.
    pub fn read_len(&mut self, min_elem_bytes: usize, what: &str) -> Result<usize> {
        let n = self.u32()? as usize;
        let need = n.saturating_mul(min_elem_bytes.max(1));
        if need > self.remaining() {
            return Err(PrismError::Corrupt(format!(
                "manifest claims {n} {what} (at least {need} bytes) but only {} bytes remain; \
                 refusing to allocate on an untrusted length",
                self.remaining()
            )));
        }
        Ok(n)
    }

    pub fn string(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        if n > self.remaining() {
            return Err(PrismError::Corrupt(format!(
                "manifest claims a {n}-byte string but only {} bytes remain",
                self.remaining()
            )));
        }
        let b = self.take(n)?;
        std::str::from_utf8(b)
            .map(|s| s.to_string())
            .map_err(|e| PrismError::Corrupt(format!("manifest string is not utf-8: {e}")))
    }
}

/// The mirror-image writer. Deliberately dumb: everything the reader validates,
/// the writer simply emits.
#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer { buf: Vec::new() }
    }
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_bits().to_le_bytes());
    }
    pub fn len(&mut self, n: usize) {
        self.u32(n as u32);
    }
    pub fn string(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
    }
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

// --- the manifest header -----------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub format_version: u32,
    pub byte_order: u8,
    pub feature_flags: u64,
    pub body_len: u32,
    pub body_crc32: u32,
}

/// magic(8) + version(4) + byte_order(1) + reserved(3) + features(8)
/// + body_len(4) + body_crc32(4) + header_crc32(4)
pub const HEADER_BYTES: usize = 8 + 4 + 1 + 3 + 8 + 4 + 4 + 4;

impl Header {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.buf.extend_from_slice(MAGIC);
        w.u32(self.format_version);
        w.u8(self.byte_order);
        w.buf.extend_from_slice(&[0u8; 3]); // reserved, must be zero
        w.u64(self.feature_flags);
        w.u32(self.body_len);
        w.u32(self.body_crc32);
        let hc = crc32(&w.buf);
        w.u32(hc);
        debug_assert_eq!(w.buf.len(), HEADER_BYTES);
        w.buf
    }

    /// Parse and validate. Order matters: magic, then the header's own checksum,
    /// then the version, then the features. Each failure is distinguishable,
    /// because "this is not a part file" and "this is a part from the future"
    /// are different problems with different fixes.
    pub fn decode(bytes: &[u8]) -> Result<Header> {
        if bytes.len() < HEADER_BYTES {
            return Err(PrismError::Corrupt(format!(
                "part manifest is {} bytes, shorter than the {HEADER_BYTES}-byte header",
                bytes.len()
            )));
        }
        if &bytes[..8] != MAGIC {
            return Err(PrismError::Corrupt(
                "part manifest does not begin with the PRSMPART magic; this is not a part file"
                    .into(),
            ));
        }

        let stated = u32::from_le_bytes([
            bytes[HEADER_BYTES - 4],
            bytes[HEADER_BYTES - 3],
            bytes[HEADER_BYTES - 2],
            bytes[HEADER_BYTES - 1],
        ]);
        let actual = crc32(&bytes[..HEADER_BYTES - 4]);
        if stated != actual {
            return Err(PrismError::Corrupt(format!(
                "part manifest header failed checksum: expected {stated:#010x}, computed {actual:#010x}"
            )));
        }

        let mut c = Cursor::new(&bytes[8..HEADER_BYTES - 4]);
        let format_version = c.u32()?;
        let byte_order = c.u8()?;
        let _reserved = c.take(3)?;
        let feature_flags = c.u64()?;
        let body_len = c.u32()?;
        let body_crc32 = c.u32()?;

        if !SUPPORTED_BINARY_VERSIONS.contains(&format_version) {
            return Err(PrismError::Corrupt(format!(
                "part is format version {format_version}; this build writes version \
                 {FORMAT_VERSION} and reads {SUPPORTED_BINARY_VERSIONS:?}"
            )));
        }
        if byte_order != BYTE_ORDER_LITTLE {
            return Err(PrismError::Corrupt(format!(
                "part declares byte order {byte_order}, which this build cannot read"
            )));
        }
        let unknown = feature_flags & !SUPPORTED_FEATURES;
        if unknown != 0 {
            return Err(PrismError::Corrupt(format!(
                "part requires feature bits {unknown:#x} that this build does not implement; \
                 refusing to read it rather than misinterpret its bytes"
            )));
        }
        if feature_flags & FEATURE_BLOCK_FRAMING == 0 {
            return Err(PrismError::Corrupt(
                "a binary part must declare block framing".into(),
            ));
        }

        Ok(Header {
            format_version,
            byte_order,
            feature_flags,
            body_len,
            body_crc32,
        })
    }
}

/// Split a manifest file into its validated header and its checksum-verified
/// body bytes.
pub fn split_manifest(bytes: &[u8]) -> Result<(Header, &[u8])> {
    let h = Header::decode(bytes)?;
    let start = HEADER_BYTES;
    let end = start
        .checked_add(h.body_len as usize)
        .ok_or_else(|| PrismError::Corrupt("manifest body length overflows".into()))?;
    if end > bytes.len() {
        return Err(PrismError::Corrupt(format!(
            "manifest declares a {}-byte body but only {} bytes follow the header",
            h.body_len,
            bytes.len() - start
        )));
    }
    let body = &bytes[start..end];
    let actual = crc32(body);
    if actual != h.body_crc32 {
        return Err(PrismError::Corrupt(format!(
            "part manifest body failed checksum: expected {:#010x}, computed {actual:#010x}",
            h.body_crc32
        )));
    }
    Ok((h, body))
}

// --- block framing -----------------------------------------------------------

/// Where one block lives in its column file, and what it should hash to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockRef {
    pub file_offset: u64,
    pub payload_len: u32,
    pub crc32: u32,
}

/// Frame one logical byte stream into checksummed blocks.
pub fn frame_column(logical: &[u8], block_size: u32) -> (Vec<u8>, Vec<BlockRef>) {
    let bs = block_size as usize;
    let mut file = Vec::with_capacity(logical.len() + FRAME_HEADER_BYTES);
    let mut refs = Vec::new();

    // A zero-length column is one zero-length block, not zero blocks: the reader
    // should never have to special-case "no blocks at all".
    let chunks: Vec<&[u8]> = if logical.is_empty() {
        vec![&[]]
    } else {
        logical.chunks(bs).collect()
    };

    for (i, payload) in chunks.iter().enumerate() {
        let file_offset = file.len() as u64;
        let crc = crc32(payload);
        let mut w = Writer::new();
        w.u32(BLOCK_MAGIC);
        w.u32(i as u32);
        w.u64((i * bs) as u64);
        w.u32(payload.len() as u32);
        w.u32(crc);
        debug_assert_eq!(w.buf.len(), FRAME_HEADER_BYTES);
        file.extend_from_slice(&w.buf);
        file.extend_from_slice(payload);
        refs.push(BlockRef {
            file_offset,
            payload_len: payload.len() as u32,
            crc32: crc,
        });
    }
    (file, refs)
}

/// Validate one framed block's raw file bytes and return its payload.
///
/// Everything the manifest said about this block is re-checked against what the
/// frame itself says, and then against the bytes. A block that passes this is a
/// block whose payload is exactly what was written; a block that fails names
/// itself, so damage is attributable to 64 KiB rather than to a column.
#[allow(clippy::too_many_arguments)]
pub fn read_block<'a>(
    raw: &'a [u8],
    index: usize,
    expect: &BlockRef,
    column: &str,
    part_id: &str,
    block_size: u32,
) -> Result<&'a [u8]> {
    let mut c = Cursor::new(raw);
    let magic = c.u32()?;
    if magic != BLOCK_MAGIC {
        return Err(PrismError::Corrupt(format!(
            "part {part_id} column `{column}` block {index}: bad frame magic {magic:#010x}"
        )));
    }
    let idx = c.u32()? as usize;
    if idx != index {
        return Err(PrismError::Corrupt(format!(
            "part {part_id} column `{column}` block {index}: frame says it is block {idx}"
        )));
    }
    let logical_offset = c.u64()?;
    let expected_offset = (index as u64) * (block_size as u64);
    if logical_offset != expected_offset {
        return Err(PrismError::Corrupt(format!(
            "part {part_id} column `{column}` block {index}: frame claims logical offset \
             {logical_offset}, expected {expected_offset}"
        )));
    }
    let payload_len = c.u32()? as usize;
    if payload_len != expect.payload_len as usize {
        return Err(PrismError::Corrupt(format!(
            "part {part_id} column `{column}` block {index}: frame says {payload_len} payload \
             bytes, manifest says {}",
            expect.payload_len
        )));
    }
    let stated_crc = c.u32()?;
    if stated_crc != expect.crc32 {
        return Err(PrismError::Corrupt(format!(
            "part {part_id} column `{column}` block {index}: frame checksum {stated_crc:#010x} \
             disagrees with the manifest's {:#010x}",
            expect.crc32
        )));
    }

    if c.remaining() < payload_len {
        return Err(PrismError::Corrupt(format!(
            "part {part_id} column `{column}` block {index}: truncated, {} payload bytes present \
             of {payload_len}",
            c.remaining()
        )));
    }
    let start = c.position();
    let payload = &raw[start..start + payload_len];

    let actual = crc32(payload);
    if actual != expect.crc32 {
        return Err(PrismError::Corrupt(format!(
            "part {part_id} column `{column}` block {index} failed checksum: expected \
             {:#010x}, computed {actual:#010x} (logical bytes {}..{})",
            expect.crc32,
            expected_offset,
            expected_offset + payload_len as u64
        )));
    }
    Ok(payload)
}

/// Which blocks cover a logical byte range.
pub fn blocks_for_range(offset: u64, len: usize, block_size: u32) -> (usize, usize) {
    let bs = block_size as u64;
    if len == 0 {
        let b = (offset / bs) as usize;
        return (b, b);
    }
    let first = (offset / bs) as usize;
    let last = ((offset + len as u64 - 1) / bs) as usize;
    (first, last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = Header {
            format_version: FORMAT_VERSION,
            byte_order: BYTE_ORDER_LITTLE,
            feature_flags: FEATURE_BLOCK_FRAMING,
            body_len: 123,
            body_crc32: 0xDEAD_BEEF,
        };
        assert_eq!(Header::decode(&h.encode()).unwrap(), h);
    }

    #[test]
    fn a_file_without_the_magic_is_not_a_part() {
        let mut b = Header {
            format_version: FORMAT_VERSION,
            byte_order: BYTE_ORDER_LITTLE,
            feature_flags: FEATURE_BLOCK_FRAMING,
            body_len: 0,
            body_crc32: 0,
        }
        .encode();
        b[0] = b'X';
        let e = Header::decode(&b).unwrap_err();
        assert!(e.to_string().contains("not a part file"), "{e}");
    }

    #[test]
    fn a_damaged_header_is_caught_by_its_own_checksum() {
        let mut b = Header {
            format_version: FORMAT_VERSION,
            byte_order: BYTE_ORDER_LITTLE,
            feature_flags: FEATURE_BLOCK_FRAMING,
            body_len: 10,
            body_crc32: 7,
        }
        .encode();
        b[12] ^= 0xFF; // inside the header, before its crc
        let e = Header::decode(&b).unwrap_err();
        assert!(e.to_string().contains("header failed checksum"), "{e}");
    }

    #[test]
    fn a_part_from_the_future_is_refused_not_guessed_at() {
        let h = Header {
            format_version: FORMAT_VERSION + 1,
            byte_order: BYTE_ORDER_LITTLE,
            feature_flags: FEATURE_BLOCK_FRAMING,
            body_len: 0,
            body_crc32: 0,
        };
        let e = Header::decode(&h.encode()).unwrap_err();
        assert!(e.to_string().contains("format version"), "{e}");
    }

    #[test]
    fn an_unknown_feature_bit_is_refused() {
        // This is the forward-compatibility escape hatch doing its job: a future
        // version can change what stored bytes mean, and an older reader will
        // refuse rather than misread them.
        let h = Header {
            format_version: FORMAT_VERSION,
            byte_order: BYTE_ORDER_LITTLE,
            feature_flags: FEATURE_BLOCK_FRAMING | (1 << 33),
            body_len: 0,
            body_crc32: 0,
        };
        let e = Header::decode(&h.encode()).unwrap_err();
        assert!(e.to_string().contains("feature bits"), "{e}");
    }

    #[test]
    fn a_big_endian_part_is_refused() {
        let h = Header {
            format_version: FORMAT_VERSION,
            byte_order: 1,
            feature_flags: FEATURE_BLOCK_FRAMING,
            body_len: 0,
            body_crc32: 0,
        };
        assert!(Header::decode(&h.encode())
            .unwrap_err()
            .to_string()
            .contains("byte order"));
    }

    #[test]
    fn cursor_refuses_a_length_the_bytes_cannot_back() {
        // The S1 gate, in one test: an untrusted length must not allocate.
        let mut buf = Writer::new();
        buf.u32(u32::MAX); // "there are 4 billion ranges"
        buf.u32(1); // ...in 4 bytes.
        let mut c = Cursor::new(&buf.buf);
        let e = c.read_len(56, "centroid ranges").unwrap_err();
        assert!(e.to_string().contains("refusing to allocate"), "{e}");
    }

    #[test]
    fn cursor_refuses_a_string_longer_than_the_buffer() {
        let mut w = Writer::new();
        w.u32(1_000_000);
        w.buf.extend_from_slice(b"abc");
        let mut c = Cursor::new(&w.buf);
        assert!(c.string().is_err());
    }

    #[test]
    fn cursor_rejects_non_finite_floats() {
        let mut w = Writer::new();
        w.f64(f64::NAN);
        let mut c = Cursor::new(&w.buf);
        assert!(c.f64().is_err());
        let mut w = Writer::new();
        w.f64(f64::INFINITY);
        assert!(Cursor::new(&w.buf).f64().is_err());
    }

    #[test]
    fn cursor_never_reads_past_the_end() {
        let w = Writer::new();
        let mut c = Cursor::new(&w.buf);
        assert!(c.u64().is_err());
        assert_eq!(c.remaining(), 0);
    }

    #[test]
    fn framing_round_trips_across_block_boundaries() {
        for len in [
            0usize,
            1,
            100,
            BLOCK_SIZE as usize - 1,
            BLOCK_SIZE as usize,
            BLOCK_SIZE as usize + 1,
            3 * BLOCK_SIZE as usize + 7,
        ] {
            let logical: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let (file, refs) = frame_column(&logical, BLOCK_SIZE);

            let mut rebuilt = Vec::new();
            for (i, r) in refs.iter().enumerate() {
                let raw = &file[r.file_offset as usize..];
                let payload = read_block(raw, i, r, "test", "p", BLOCK_SIZE).unwrap();
                rebuilt.extend_from_slice(payload);
            }
            assert_eq!(rebuilt, logical, "round trip failed at len {len}");
        }
    }

    #[test]
    fn a_flipped_byte_condemns_one_block_and_names_it() {
        let logical: Vec<u8> = (0..(3 * BLOCK_SIZE as usize))
            .map(|i| (i % 251) as u8)
            .collect();
        let (mut file, refs) = frame_column(&logical, BLOCK_SIZE);

        // Damage a byte inside block 1's payload.
        let target = refs[1].file_offset as usize + FRAME_HEADER_BYTES + 10;
        file[target] ^= 0x01;

        // Block 0 and block 2 are untouched and still read.
        for i in [0usize, 2] {
            let raw = &file[refs[i].file_offset as usize..];
            assert!(
                read_block(raw, i, &refs[i], "col", "p", BLOCK_SIZE).is_ok(),
                "block {i} was collateral damage"
            );
        }
        // Block 1 is condemned, and says so.
        let raw = &file[refs[1].file_offset as usize..];
        let e = read_block(raw, 1, &refs[1], "col", "p", BLOCK_SIZE).unwrap_err();
        let m = e.to_string();
        assert!(m.contains("block 1"), "{m}");
        assert!(m.contains("failed checksum"), "{m}");
    }

    #[test]
    fn a_block_that_lies_about_its_index_is_rejected() {
        let logical = vec![7u8; 100];
        let (mut file, refs) = frame_column(&logical, BLOCK_SIZE);
        file[4] = 9; // block_index field
        let e = read_block(&file, 0, &refs[0], "col", "p", BLOCK_SIZE).unwrap_err();
        assert!(e.to_string().contains("is block 9"), "{e}");
    }

    #[test]
    fn block_range_math_is_right() {
        let b = BLOCK_SIZE;
        assert_eq!(blocks_for_range(0, 1, b), (0, 0));
        assert_eq!(blocks_for_range(0, b as usize, b), (0, 0));
        assert_eq!(blocks_for_range(0, b as usize + 1, b), (0, 1));
        assert_eq!(blocks_for_range(b as u64, 1, b), (1, 1));
        assert_eq!(blocks_for_range(b as u64 - 1, 2, b), (0, 1));
        assert_eq!(blocks_for_range(100, 0, b), (0, 0));
    }

    #[test]
    fn rerank_descriptor_dispatches_and_refuses_the_unknown() {
        let d = RerankDescriptor::float32_exact();
        assert_eq!(d.bytes_per_vector(64).unwrap(), 256);
        assert_eq!(d.describe(), "float32/exact");
        d.validate().unwrap();

        let v = vec![1.0f32, -2.0, 0.5];
        let bytes = d.encode_vector(&v).unwrap();
        assert_eq!(d.decode_vector(&bytes, 3).unwrap(), v);

        // An encoding this build does not implement is refused at open time,
        // rather than producing numbers that mean nothing. Encoding 2 (fp16) is now KNOWN, so the
        // unknown-encoding fixture moved to encoding 3 -- the unknown-encoding fixture must be
        // updated in the same change that adds an encoding (D-049), or it silently stops testing
        // anything.
        let future = RerankDescriptor {
            encoding_id: 3, // reserved for a future encoding, not yet implemented
            accuracy_contract_id: 1,
        };
        assert!(future.validate().is_err());
        assert!(future.bytes_per_vector(64).is_err());
        assert!(future.decode_vector(&bytes, 3).is_err());

        // A known encoding paired with a contract this build cannot honour is also refused: the
        // PAIR must be implemented, not just the encoding.
        let odd = RerankDescriptor {
            encoding_id: RERANK_ENCODING_FLOAT32,
            accuracy_contract_id: 99,
        };
        assert!(odd.validate().is_err());
        // fp16 with the EXACT contract is a lie -- fp16 cannot be exact -- and is refused.
        let mislabelled = RerankDescriptor {
            encoding_id: RERANK_ENCODING_FLOAT16,
            accuracy_contract_id: RERANK_CONTRACT_EXACT,
        };
        assert!(mislabelled.validate().is_err());
    }

    #[test]
    fn fp16_is_a_valid_lossy_encoding_under_its_contract() {
        let d = RerankDescriptor::float16_cosine();
        assert_eq!(d.describe(), "float16/fp16-cosine");
        assert_eq!(d.bytes_per_vector(64).unwrap(), 128); // half of float32's 256
        d.validate().unwrap();

        // Round-trips through fp16: small integers are exact, and the decode is the LOSSY value a
        // rerank sees -- which is the whole point, and what the accuracy contract bounds.
        let v = vec![1.0f32, -2.0, 0.5, 0.0, 8.0];
        let bytes = d.encode_vector(&v).unwrap();
        assert_eq!(bytes.len(), v.len() * 2);
        assert_eq!(d.decode_vector(&bytes, v.len()).unwrap(), v);
    }
}
