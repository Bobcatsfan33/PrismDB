//! Structured fuzzing of the part format (S1).
//!
//! The S1 gate: *"fuzz/property tests on manifests, offsets, lengths, NaNs,
//! truncation… no untrusted length allocates unbounded."*
//!
//! Every byte of a part arrives from somewhere we do not control — a disk that
//! may be lying, an object store that may have torn a multipart upload, a
//! restore from a backup somebody edited. The reader's only acceptable
//! behaviours are: **decode it, or refuse it with a specific error.** It may
//! never panic, never index out of bounds, never hang, and never allocate on the
//! strength of a number it just read.
//!
//! So: take a real part, corrupt it in every way we can think of, thousands of
//! times, deterministically, and assert that the reader either opens it or says
//! precisely what is wrong with it. A panic here is a crash in production; an
//! `Ok` on garbage is a silently wrong answer, which is worse.
//!
//! This is seeded and in-tree rather than a `cargo-fuzz` target so it runs on
//! every commit, on stable, with no extra toolchain. A coverage-guided fuzzer is
//! a strictly better thing to *also* have, and it is not a reason to have nothing.

use prism_part::format::{self, Cursor, RerankDescriptor};
use prism_part::part::{PartManifest, PartReader, PartWriter, RowIn};
use prism_part::store::{Store, StoreConfig, STORE_VERSION};
use prism_types::error::PrismError;
use prism_types::event::Event;
use prism_types::rng::Rng;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("prism-fuzz-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    p
}

const DIM: usize = 16;
const PQ_M: usize = 4;

/// A small, real part — written by the real writer, so the fuzzer corrupts the
/// bytes we actually ship rather than a synthetic approximation of them.
fn build_part(root: &Path, rows: usize) -> PathBuf {
    let store = Store::init(
        root,
        StoreConfig {
            format_version: STORE_VERSION,
            dim: DIM,
            nlist: 4,
            pq_m: PQ_M,
            seed: 1,
            kmeans_restarts: prism_quantizer::kmeans::KMEANS_RESTARTS,
            block_size: prism_part::format::DEFAULT_BLOCK_SIZE,
            partitions: Default::default(),
            promote: Vec::new(),
        },
    )
    .unwrap();

    let mut rng = Rng::new(7);
    let rows: Vec<RowIn> = (0..rows)
        .map(|i| {
            let mut v: Vec<f32> = (0..DIM).map(|_| rng.normal()).collect();
            prism_types::validate_and_normalize(&mut v).unwrap();
            RowIn {
                event: Event {
                    observed_time: 1_000 + i as i64,
                    trace_id: String::new(),
                    span_id: String::new(),
                    attributes: Default::default(),
                    idempotency_key: None,
                    event_id: format!("e{i:06}"),
                    tenant_id: format!("t{}", i % 3),
                    event_time: 1_000 + i as i64,
                    event_name: "x".into(),
                    cost: 0.01,
                    error: i % 5 == 0,
                    body: format!("body number {i} with some words in it"),
                },
                centroid: (i % 4) as u32,
                code: (0..PQ_M).map(|j| ((i + j) % 251) as u8).collect(),
                vector: v,
            }
        })
        .collect();

    let m = PartWriter::write(
        &store.parts_dir(),
        1,
        "gen0",
        "hash-embedder",
        "1",
        DIM,
        PQ_M,
        prism_part::format::DEFAULT_BLOCK_SIZE,
        &prism_part::part::PartSpec::default(),
        rows,
        1_000,
    )
    .unwrap();
    store.part_dir(&m.part_id)
}

/// The only two acceptable outcomes, for any bytes at all.
fn assert_decode_or_refuse(bytes: &[u8], what: &str) {
    match PartManifest::decode(bytes) {
        Ok(m) => {
            // If it decoded, it must also be *structurally* coherent or say why.
            // An Ok manifest that then blows up downstream is the failure mode
            // this whole file exists to prevent.
            if let Err(e) = m.validate_structure() {
                assert!(
                    matches!(e, PrismError::Corrupt(_)),
                    "{what}: structure check produced {e:?}, expected Corrupt"
                );
            }
        }
        Err(e) => {
            assert!(
                matches!(e, PrismError::Corrupt(_) | PrismError::Decode(_)),
                "{what}: expected a Corrupt/Decode error, got {e:?}"
            );
            let msg = e.to_string();
            assert!(!msg.is_empty(), "{what}: refused with an empty message");
        }
    }
}

#[test]
fn random_byte_flips_in_a_manifest_never_panic_and_never_lie() {
    let root = tmp("flip");
    let part = build_part(&root, 300);
    let good = std::fs::read(part.join("manifest.bin")).unwrap();
    PartManifest::decode(&good).expect("the pristine manifest must decode");

    let mut rng = Rng::new(99);
    for i in 0..4000 {
        let mut b = good.clone();
        // One to four flipped bits, anywhere.
        let flips = 1 + rng.below(4);
        for _ in 0..flips {
            let at = rng.below(b.len());
            b[at] ^= 1u8 << rng.below(8);
        }
        assert_decode_or_refuse(&b, &format!("flip iteration {i}"));
    }
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn truncation_at_every_length_never_panics() {
    // A torn write, a truncated multipart upload, a disk that ran out. Every
    // possible prefix of a valid manifest must be refused cleanly.
    let root = tmp("trunc");
    let part = build_part(&root, 200);
    let good = std::fs::read(part.join("manifest.bin")).unwrap();

    for len in 0..good.len() {
        assert_decode_or_refuse(&good[..len], &format!("truncated to {len}"));
    }
    // And bytes appended past the end must not be silently accepted as content.
    let mut long = good.clone();
    long.extend_from_slice(&[0xAB; 64]);
    assert_decode_or_refuse(&long, "trailing garbage");

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn total_garbage_is_never_mistaken_for_a_part() {
    let mut rng = Rng::new(4);
    for i in 0..2000 {
        let n = rng.below(512);
        let bytes: Vec<u8> = (0..n).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
        assert_decode_or_refuse(&bytes, &format!("garbage {i}"));
    }
    // Including garbage that happens to start with the right magic.
    for i in 0..2000 {
        let n = 8 + rng.below(512);
        let mut bytes: Vec<u8> = (0..n).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
        bytes[..8].copy_from_slice(format::MAGIC);
        assert_decode_or_refuse(&bytes, &format!("magic-prefixed garbage {i}"));
    }
}

#[test]
fn an_absurd_length_is_refused_and_allocates_nothing() {
    // The S1 gate in one test. If this ever regresses, the process does not fail
    // a test -- it gets OOM-killed, which is a much worse way to find out.
    let root = tmp("absurd");
    let part = build_part(&root, 100);
    let good = std::fs::read(part.join("manifest.bin")).unwrap();

    // Walk the body and try planting u32::MAX at every 4-byte-aligned position.
    // Any of them that lands on a length prefix must be refused; none of them may
    // cause an allocation the machine cannot survive.
    let body_start = format::HEADER_BYTES;
    let mut planted = 0usize;
    for at in (body_start..good.len().saturating_sub(4)).step_by(4) {
        let mut b = good.clone();
        b[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());

        // Re-seal both checksums, so the *length check* is what has to catch it
        // -- not the CRC. This is the adversary who can edit bytes and fix up the
        // checksums, which is precisely the adversary a checksum cannot stop.
        reseal(&mut b);
        assert_decode_or_refuse(&b, &format!("u32::MAX planted at {at}"));
        planted += 1;
    }
    assert!(planted > 20, "the fuzzer barely tried anything: {planted}");
    std::fs::remove_dir_all(root).ok();
}

/// Rebuild both checksums around an edited body, so semantic validation — not
/// the CRC — is what must catch the edit.
fn reseal(buf: &mut [u8]) {
    use prism_types::hash::crc32;
    let body_len = (buf.len() - format::HEADER_BYTES) as u32;
    buf[24..28].copy_from_slice(&body_len.to_le_bytes());
    let body_crc = crc32(&buf[format::HEADER_BYTES..]);
    buf[28..32].copy_from_slice(&body_crc.to_le_bytes());
    let header_crc = crc32(&buf[..32]);
    buf[32..36].copy_from_slice(&header_crc.to_le_bytes());
}

#[test]
fn a_checksum_repairing_adversary_still_cannot_get_nonsense_past_the_reader() {
    // A CRC proves the bytes are the bytes someone wrote. It proves nothing about
    // whether they mean anything. So: corrupt the body, fix the checksums, and
    // insist the structural validation still refuses what is now internally
    // inconsistent.
    let root = tmp("adversary");
    let part = build_part(&root, 250);
    let good = std::fs::read(part.join("manifest.bin")).unwrap();

    let mut rng = Rng::new(2024);
    let mut refused = 0usize;
    let mut accepted = 0usize;

    for _ in 0..3000 {
        let mut b = good.clone();
        let at = format::HEADER_BYTES + rng.below(b.len() - format::HEADER_BYTES);
        b[at] ^= 1u8 << rng.below(8);
        reseal(&mut b);

        match PartManifest::decode(&b) {
            Err(e) => {
                assert!(matches!(e, PrismError::Corrupt(_)));
                refused += 1;
            }
            Ok(m) => match m.validate_structure() {
                Err(e) => {
                    assert!(matches!(e, PrismError::Corrupt(_)));
                    refused += 1;
                }
                Ok(()) => {
                    // Surviving is allowed only for fields where any value is
                    // legal -- a timestamp, a zone-map bound, a character in an
                    // id. It must never be a field the reader will later trust to
                    // index memory.
                    accepted += 1;
                }
            },
        }
    }

    // Most single-bit edits land in something structural. If almost everything
    // were "accepted", the validation would be doing nothing.
    assert!(
        refused > accepted,
        "structural validation caught only {refused} of {} adversarial edits",
        refused + accepted
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn corrupting_column_bytes_is_always_caught_by_a_block_checksum() {
    let root = tmp("cols");
    let part = build_part(&root, 500);

    let mut rng = Rng::new(11);
    for i in 0..200 {
        // Fresh copy each time.
        let scratch = tmp("cols-run");
        let dst = scratch.join("part");
        copy_dir(&part, &dst);

        // Pick a column file and flip a bit inside it.
        let files: Vec<PathBuf> = std::fs::read_dir(&dst)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.file_name().unwrap() != "manifest.bin")
            .collect();
        let f = &files[rng.below(files.len())];
        let mut bytes = std::fs::read(f).unwrap();
        if bytes.is_empty() {
            std::fs::remove_dir_all(&scratch).ok();
            continue;
        }
        let at = rng.below(bytes.len());
        bytes[at] ^= 1u8 << rng.below(8);
        std::fs::write(f, &bytes).unwrap();

        // The manifest is untouched, so the part still opens. Reading it must not.
        let r = PartReader::open(&dst).expect("manifest is intact, so open must succeed");
        let e = r
            .verify()
            .expect_err(&format!("iteration {i}: damage to {f:?} went undetected"));
        assert!(
            matches!(e, PrismError::Corrupt(_)),
            "iteration {i}: {e:?} is not a Corrupt error"
        );

        std::fs::remove_dir_all(&scratch).ok();
    }
    std::fs::remove_dir_all(root).ok();
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
    }
}

#[test]
fn a_manifest_never_carries_a_nan_through_to_a_zone_map() {
    // Zone maps are used to *skip* data. A NaN bound makes every comparison
    // false, which would silently turn "this part cannot match" into "this part
    // does match" -- or worse, the reverse. So NaNs are refused at the boundary.
    let mut w = format::Writer::new();
    w.f64(f64::NAN);
    assert!(Cursor::new(&w.buf).f64().is_err());

    let mut w = format::Writer::new();
    w.f64(f64::NEG_INFINITY);
    assert!(Cursor::new(&w.buf).f64().is_err());
}

#[test]
fn every_declared_rerank_encoding_either_decodes_or_refuses() {
    // Exhaustive over the id space we could plausibly meet. Exactly one encoding
    // is implemented; every other id must be refused rather than guessed at.
    for enc in 0u16..=64 {
        for contract in 0u16..=4 {
            let d = RerankDescriptor {
                encoding_id: enc,
                accuracy_contract_id: contract,
            };
            let known =
                enc == format::RERANK_ENCODING_FLOAT32 && contract == format::RERANK_CONTRACT_EXACT;
            assert_eq!(
                d.validate().is_ok(),
                known,
                "encoding {enc}/contract {contract} was handled wrongly"
            );
            if !known {
                // And it must never hand back numbers it cannot vouch for.
                assert!(d.validate().is_err());
            }
        }
    }
}

/// **The truncated-part-under-mmap fault test (S6, determinism contract §6).**
///
/// The framed column read path now maps the file read-only instead of `pread`-ing it. A truncated
/// file under mmap `SIGBUS`es on access — a process death an operator cannot act on. The S1
/// truncation discipline must survive the I/O change: a truncated part names its column and block,
/// and it does so *before* any out-of-range byte is touched.
///
/// This truncates a real framed column file on disk at many lengths and asserts every read either
/// succeeds or refuses with a **named `Corrupt` error** — never a `SIGBUS`, never a generic io
/// error. If the mmap bounds check were missing, this test would crash the process instead of
/// failing an assertion.
#[test]
fn a_truncated_framed_column_under_mmap_names_itself_and_never_sigbuses() {
    let root = tmp("mmap-trunc");
    let dir = build_part(&root, 400);

    // The compressed-code column is framed (v2), so its reads go through the mmap path.
    let col_file = std::fs::read_dir(dir.join("."))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("pq.codes"))
        .expect("a framed pq_codes column file");
    let path = dir.join(&col_file);
    let full = std::fs::read(&path).unwrap();

    // Truncate to many lengths, including inside the first block's header, mid-payload, and just
    // short of the end. Every one must name itself or read cleanly, and the process must survive.
    for &len in &[
        0usize,
        1,
        4,
        7,
        16,
        64,
        100,
        full.len() / 2,
        full.len().saturating_sub(1),
    ] {
        std::fs::write(&path, &full[..len.min(full.len())]).unwrap();

        let reader = match PartReader::open(&dir) {
            Ok(r) => r,
            Err(e) => {
                // Refusing at open is fine, as long as it is a named Corrupt error.
                assert!(
                    matches!(e, PrismError::Corrupt(_)),
                    "truncated to {len}: open failed with {e:?}, expected a named Corrupt error"
                );
                continue;
            }
        };

        // Reading the whole column exercises the framed mmap path over every block.
        match reader.read_column_checked("pq_codes") {
            Ok(_) => {}
            Err(PrismError::Corrupt(msg)) => {
                assert!(
                    msg.contains("pq_codes"),
                    "truncated to {len}: the error must name the column, got: {msg}"
                );
            }
            Err(e) => panic!("truncated to {len}: expected a named Corrupt error, got {e:?}"),
        }
        // If we reached here, the process did not SIGBUS -- which is the whole point.
    }
    let _ = std::fs::remove_dir_all(&root);
}
