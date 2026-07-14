//! Reading format v1 — the JSON-manifest, unframed-column parts written by S0.
//!
//! v2 is the format we write. v1 is a format we still *read*, and that is the
//! whole point of the compatibility corpus: "today's build opens yesterday's
//! data" is a promise you keep by keeping it, not by saying it. The cost is this
//! file — a decoder and a mapping into the in-memory manifest — and it buys the
//! machinery that will be needed every time the format moves again.
//!
//! Nothing here can write. A v1 part is upgraded by being merged: read it,
//! write a v2 part, swap the catalog. Immutability means we never rewrite it in
//! place, so the migration is the same mechanism as everything else.

use crate::format::{RerankDescriptor, CODEC_RAW};
use crate::part::{CentroidRange, ColumnMeta, ColumnStorage, PartManifest};
use prism_types::error::{PrismError, Result};
use serde::Deserialize;

#[derive(Deserialize)]
struct V1ColumnMeta {
    name: String,
    file: String,
    bytes: usize,
    crc32: u32,
}

#[derive(Deserialize)]
struct V1CentroidRange {
    centroid: u32,
    first_row: usize,
    row_count: usize,
    pq_offset: u64,
    pq_len: usize,
    vec_offset: u64,
    vec_len: usize,
    time_min: i64,
    time_max: i64,
}

#[derive(Deserialize)]
struct V1Manifest {
    format_version: u32,
    part_id: String,
    generation_id: String,
    model_id: String,
    model_version: String,
    row_count: usize,
    dim: usize,
    pq_m: usize,
    byte_order: String,
    time_min: i64,
    time_max: i64,
    tenants: Vec<String>,
    cost_min: f64,
    cost_max: f64,
    has_error: bool,
    has_success: bool,
    centroid_ranges: Vec<V1CentroidRange>,
    columns: Vec<V1ColumnMeta>,
    created_at_ms: i64,
}

/// v1's logical column names, mapped onto v2's.
///
/// v1 called the exact-rerank column `vectors`, stored in `vectors.f32`. v2
/// calls it `rerank_vectors`, because the *encoding* of that column is now a
/// declared, versioned choice (D-003-resolved) and a name that hard-codes
/// `f32` would be a lie the day it becomes fp16.
fn map_column_name(v1: &str) -> &str {
    match v1 {
        "vectors" => "rerank_vectors",
        other => other,
    }
}

/// Decode a v1 `manifest.json` into the current in-memory manifest.
pub fn decode(bytes: &[u8]) -> Result<PartManifest> {
    let m: V1Manifest = serde_json::from_slice(bytes)
        .map_err(|e| PrismError::Corrupt(format!("v1 manifest will not parse: {e}")))?;

    if m.format_version != crate::format::LEGACY_FORMAT_VERSION {
        return Err(PrismError::Corrupt(format!(
            "manifest.json declares format version {}, but only version {} was ever \
             written as JSON",
            m.format_version,
            crate::format::LEGACY_FORMAT_VERSION
        )));
    }
    if m.byte_order != "little" {
        return Err(PrismError::Corrupt(format!(
            "v1 part declares byte order `{}`, which this build cannot read",
            m.byte_order
        )));
    }

    let columns = m
        .columns
        .into_iter()
        .map(|c| ColumnMeta {
            name: map_column_name(&c.name).to_string(),
            file: c.file,
            codec_id: CODEC_RAW,
            storage: ColumnStorage::Unframed {
                bytes: c.bytes,
                crc32: c.crc32,
            },
        })
        .collect();

    let centroid_ranges = m
        .centroid_ranges
        .into_iter()
        .map(|r| CentroidRange {
            centroid: r.centroid,
            first_row: r.first_row,
            row_count: r.row_count,
            pq_offset: r.pq_offset,
            pq_len: r.pq_len,
            rerank_offset: r.vec_offset,
            rerank_len: r.vec_len,
            time_min: r.time_min,
            time_max: r.time_max,
        })
        .collect();

    Ok(PartManifest {
        format_version: crate::format::LEGACY_FORMAT_VERSION,
        part_id: m.part_id,
        generation_id: m.generation_id,
        model_id: m.model_id,
        model_version: m.model_version,
        row_count: m.row_count,
        dim: m.dim,
        pq_m: m.pq_m,
        // v1 had no descriptor because it had no choice: full float32, exact
        // rerank, always. Saying so explicitly is what lets every reader above
        // this line stop caring which format it came from.
        rerank: RerankDescriptor::float32_exact(),
        time_min: m.time_min,
        time_max: m.time_max,
        tenants: m.tenants,
        cost_min: m.cost_min,
        cost_max: m.cost_max,
        has_error: m.has_error,
        has_success: m.has_success,
        centroid_ranges,
        columns,
        created_at_ms: m.created_at_ms,
    })
}
