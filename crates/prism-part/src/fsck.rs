//! The offline format validator (S1).
//!
//! `fsck` answers one question about one directory of bytes: *is this a part, and
//! is it intact?* It needs no catalog, no generation, no store, and no engine. An
//! operator holding a suspicious object out of a backup or an object store must
//! be able to condemn it — or clear it — without standing a database up first.
//!
//! It reports **every** problem it can find rather than dying on the first, and
//! each finding names what is wrong and where. "Corrupt" is not a useful thing to
//! tell someone at 3am; "column `body.data`, block 7, logical bytes 458752..524288
//! failed checksum" is.

use crate::io;
use crate::part::{ColumnStorage, PartManifest, PartReader};
use prism_types::error::Result;
use prism_types::vector::dot;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Finding {
    /// `manifest`, `column`, `block`, `structure`, `semantics`.
    pub kind: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FsckReport {
    pub path: String,
    pub ok: bool,
    pub part_id: Option<String>,
    pub format_version: Option<u32>,
    pub legacy: bool,
    pub rerank_encoding: Option<String>,
    pub row_count: Option<usize>,
    pub columns_checked: usize,
    pub blocks_checked: usize,
    pub logical_bytes: u64,
    pub findings: Vec<Finding>,
}

impl FsckReport {
    fn fail(path: &Path, kind: &str, detail: String) -> FsckReport {
        FsckReport {
            path: path.display().to_string(),
            ok: false,
            part_id: None,
            format_version: None,
            legacy: false,
            rerank_encoding: None,
            row_count: None,
            columns_checked: 0,
            blocks_checked: 0,
            logical_bytes: 0,
            findings: vec![Finding {
                kind: kind.to_string(),
                detail,
            }],
        }
    }
}

/// Validate a part directory from the bytes up.
///
/// Never returns `Err` for a *bad part* — a bad part is the answer, not an
/// accident. The report says so, and the caller decides what that is worth.
pub fn fsck_part(dir: &Path) -> FsckReport {
    // 1. Does it even claim to be a part?
    let reader = match PartReader::open(dir) {
        Ok(r) => r,
        Err(e) => return FsckReport::fail(dir, "manifest", e.to_string()),
    };

    let m: &PartManifest = &reader.manifest;
    let mut findings: Vec<Finding> = Vec::new();
    let mut blocks_checked = 0usize;
    let mut logical_bytes = 0u64;

    // 2. Every column, every block. Keep going after a failure: an operator wants
    //    the blast radius, not the first casualty.
    for c in &m.columns {
        logical_bytes += c.storage.logical_bytes();

        let path = dir.join(&c.file);
        if !path.exists() {
            findings.push(Finding {
                kind: "column".into(),
                detail: format!("column `{}` file `{}` is missing", c.name, c.file),
            });
            continue;
        }

        match &c.storage {
            ColumnStorage::Framed { blocks, .. } => {
                for (i, b) in blocks.iter().enumerate() {
                    // Read exactly this block and check its frame and its bytes.
                    match reader.read_range(
                        &c.name,
                        (i as u64) * crate::format::BLOCK_SIZE as u64,
                        b.payload_len as usize,
                    ) {
                        Ok(_) => blocks_checked += 1,
                        Err(e) => findings.push(Finding {
                            kind: "block".into(),
                            detail: e.to_string(),
                        }),
                    }
                }
            }
            ColumnStorage::Unframed { .. } => match reader.read_column_checked(&c.name) {
                Ok(_) => blocks_checked += 1,
                Err(e) => findings.push(Finding {
                    kind: "column".into(),
                    detail: e.to_string(),
                }),
            },
        }
    }

    // 3. Structure: do the bytes decode into the things they claim to be? This is
    //    what catches a checksum-valid offset array that points into space.
    if let Err(e) = reader.read_all() {
        findings.push(Finding {
            kind: "structure".into(),
            detail: e.to_string(),
        });
    } else if let Ok(rows) = reader.read_all() {
        // 4. Semantics: the engine assumes every stored vector is unit-norm,
        //    because it normalizes at ingest and never checks again. A part that
        //    breaks that assumption produces wrong scores rather than errors,
        //    which is the worst possible failure. So the audit checks it.
        let dim = m.dim;
        let mut bad_norms = 0usize;
        let mut nonfinite = 0usize;
        for i in 0..rows.events.len() {
            let v = &rows.vectors[i * dim..(i + 1) * dim];
            if v.iter().any(|x| !x.is_finite()) {
                nonfinite += 1;
                continue;
            }
            let n = dot(v, v);
            if (n - 1.0).abs() > 1e-3 {
                bad_norms += 1;
            }
        }
        if nonfinite > 0 {
            findings.push(Finding {
                kind: "semantics".into(),
                detail: format!("{nonfinite} stored vectors contain NaN or infinity"),
            });
        }
        if bad_norms > 0 {
            findings.push(Finding {
                kind: "semantics".into(),
                detail: format!(
                    "{bad_norms} stored vectors are not unit-norm; every score computed from \
                     this part would be silently wrong"
                ),
            });
        }

        // 5. The inner order is load-bearing: the scan reads a centroid as a
        //    contiguous byte range because rows are sorted by
        //    (centroid, event_time, event_id). If that is not true, probing
        //    returns the wrong rows and nothing else would ever tell us.
        let mut out_of_order = 0usize;
        for i in 1..rows.centroids.len() {
            let prev = (
                rows.centroids[i - 1],
                rows.events[i - 1].event_time,
                &rows.events[i - 1].event_id,
            );
            let cur = (
                rows.centroids[i],
                rows.events[i].event_time,
                &rows.events[i].event_id,
            );
            if prev > cur {
                out_of_order += 1;
            }
        }
        if out_of_order > 0 {
            findings.push(Finding {
                kind: "structure".into(),
                detail: format!(
                    "{out_of_order} rows violate the (centroid, event_time, event_id) inner order; \
                     centroid ranges are not contiguous and the scan would read the wrong rows"
                ),
            });
        }
    }

    FsckReport {
        path: dir.display().to_string(),
        ok: findings.is_empty(),
        part_id: Some(m.part_id.clone()),
        format_version: Some(m.format_version),
        legacy: reader.is_legacy(),
        rerank_encoding: Some(m.rerank.describe()),
        row_count: Some(m.row_count),
        columns_checked: m.columns.len(),
        blocks_checked,
        logical_bytes,
        findings,
    }
}

/// Validate every part in a store directory, catalog or no catalog.
pub fn fsck_store(root: &Path) -> Result<Vec<FsckReport>> {
    let parts = root.join("parts");
    let mut out = Vec::new();
    if !parts.exists() {
        return Ok(out);
    }
    let mut dirs: Vec<_> = std::fs::read_dir(&parts)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for d in dirs {
        out.push(fsck_part(&d));
    }
    Ok(out)
}

/// A part directory, or a store containing many.
pub fn fsck(path: &Path) -> Result<Vec<FsckReport>> {
    if path.join("store.json").exists() {
        fsck_store(path)
    } else {
        Ok(vec![fsck_part(path)])
    }
}

/// Load a part's raw manifest bytes without interpreting them. Used by the fuzz
/// harness to mutate a real manifest rather than a synthetic one.
pub fn raw_manifest(dir: &Path) -> Result<Vec<u8>> {
    let binary = dir.join(crate::part::MANIFEST_FILE);
    if binary.exists() {
        return io::read_file(&binary);
    }
    io::read_file(&dir.join(crate::part::LEGACY_MANIFEST_FILE))
}
