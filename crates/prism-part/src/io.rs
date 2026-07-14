//! Durable file primitives.
//!
//! Nothing in PrismDB becomes visible by being written. It becomes visible by
//! being *renamed*, after its bytes and its parent directory are on the device.
//! Every path in and out of storage goes through here.

use prism_types::error::Result;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

/// fsync a directory so a rename inside it survives power loss.
pub fn fsync_dir(dir: &Path) -> Result<()> {
    let f = File::open(dir)?;
    f.sync_all()?;
    Ok(())
}

/// Write bytes to `path` durably: full write + fsync to a temp file, atomic
/// rename into place, fsync the directory.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_path(path);
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

pub fn tmp_path(path: &Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    std::path::PathBuf::from(s)
}

/// Read a byte range from a file without reading the rest of it.
///
/// This is the S0 stand-in for the coalesced ranged reads S11 issues against
/// object storage: the persisted centroid marks give us `(offset, len)` and we
/// fetch exactly that, so "we only scanned the ranges we selected" is a fact
/// about the syscalls, not a claim.
pub fn read_range(file: &File, offset: u64, len: usize) -> Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        let mut buf = vec![0u8; len];
        file.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    }
    #[cfg(not(unix))]
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = file.try_clone()?;
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }
}

pub fn read_file(path: &Path) -> Result<Vec<u8>> {
    Ok(fs::read(path)?)
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

// --- fixed-width column encoding ---
//
// Little-endian everywhere, declared in the manifest. A future big-endian
// reader must byte-swap; it must never guess.

pub fn encode_u32(vals: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vals.len() * 4);
    for v in vals {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

pub fn encode_i64(vals: &[i64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vals.len() * 8);
    for v in vals {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

pub fn encode_f64(vals: &[f64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vals.len() * 8);
    for v in vals {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

pub fn encode_f32(vals: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vals.len() * 4);
    for v in vals {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

pub fn decode_u32(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub fn decode_i64(bytes: &[u8]) -> Vec<i64> {
    bytes
        .chunks_exact(8)
        .map(|c| i64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
        .collect()
}

pub fn decode_f64(bytes: &[u8]) -> Vec<f64> {
    bytes
        .chunks_exact(8)
        .map(|c| f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
        .collect()
}

pub fn decode_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Variable-length strings: a data blob plus `n + 1` offsets.
pub fn encode_strings(vals: &[String]) -> (Vec<u8>, Vec<u8>) {
    let mut data = Vec::new();
    let mut offsets: Vec<i64> = Vec::with_capacity(vals.len() + 1);
    offsets.push(0);
    for v in vals {
        data.extend_from_slice(v.as_bytes());
        offsets.push(data.len() as i64);
    }
    (data, encode_i64(&offsets))
}

/// Borrow one row of a string column, validating its offsets first.
///
/// Every offset is validated against the blob length before it is used to slice:
/// an untrusted length must never index out of bounds and must never allocate on
/// trust (S1 gate). This borrows rather than allocating, so a caller that only
/// wants `k` of a million rows pays for `k`.
pub fn string_at<'a>(
    data: &'a [u8],
    offs: &[i64],
    row: usize,
    row_count: usize,
) -> Result<&'a str> {
    use prism_types::error::PrismError;

    if row >= row_count || row + 1 >= offs.len() {
        return Err(PrismError::Corrupt(format!(
            "string column row {row} is out of range ({row_count} rows)"
        )));
    }
    let (start, end) = (offs[row], offs[row + 1]);
    if start < 0 || end < start || end as usize > data.len() {
        return Err(PrismError::Corrupt(format!(
            "string column offset pair ({start}, {end}) is outside a {}-byte blob",
            data.len()
        )));
    }
    std::str::from_utf8(&data[start as usize..end as usize])
        .map_err(|e| PrismError::Corrupt(format!("string column row {row} is not utf-8: {e}")))
}

/// Decode and validate a string column's offset array.
pub fn string_offsets(offsets: &[u8], row_count: usize) -> Result<Vec<i64>> {
    use prism_types::error::PrismError;

    let offs = decode_i64(offsets);
    if offs.len() != row_count + 1 {
        return Err(PrismError::Corrupt(format!(
            "string column has {} offsets, expected {}",
            offs.len(),
            row_count + 1
        )));
    }
    Ok(offs)
}

/// Decode a whole string column.
pub fn decode_strings(data: &[u8], offsets: &[u8], row_count: usize) -> Result<Vec<String>> {
    let offs = string_offsets(offsets, row_count)?;
    let mut out = Vec::with_capacity(row_count);
    for i in 0..row_count {
        out.push(string_at(data, &offs, i, row_count)?.to_string());
    }
    Ok(out)
}

// --- attributes ---------------------------------------------------------------
//
// Keys are dictionary-encoded against the part's bounded key dictionary; values
// carry a type tag. Rows are variable-length, so the layout is the same
// data-blob + offsets pair the string columns use.
//
// Per row:  n:u32, then n x ( key_id:u32, type:u8, value )
//   Str    -> len:u32 + bytes
//   Int    -> i64
//   Double -> f64 bits
//   Bool   -> u8

use prism_types::attributes::{
    AttrValue, Attributes, ATTR_TYPE_BOOL, ATTR_TYPE_DOUBLE, ATTR_TYPE_INT, ATTR_TYPE_STR,
};
use std::collections::BTreeMap;

pub fn encode_attributes(
    rows: &[Attributes],
    dict: &BTreeMap<String, u32>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    use prism_types::error::PrismError;

    let mut data: Vec<u8> = Vec::new();
    let mut offsets: Vec<i64> = Vec::with_capacity(rows.len() + 1);
    offsets.push(0);

    for attrs in rows {
        data.extend_from_slice(&(attrs.len() as u32).to_le_bytes());
        for (k, v) in attrs {
            let id = dict.get(k).ok_or_else(|| {
                PrismError::Invariant(format!(
                    "attribute key `{k}` is not in the part's key dictionary; the dictionary is \
                     built from the rows being written, so this is a writer bug"
                ))
            })?;
            data.extend_from_slice(&id.to_le_bytes());
            data.push(v.type_tag());
            match v {
                AttrValue::Str(s) => {
                    data.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    data.extend_from_slice(s.as_bytes());
                }
                AttrValue::Int(i) => data.extend_from_slice(&i.to_le_bytes()),
                AttrValue::Double(d) => data.extend_from_slice(&d.to_bits().to_le_bytes()),
                AttrValue::Bool(b) => data.push(u8::from(*b)),
            }
        }
        offsets.push(data.len() as i64);
    }
    Ok((data, encode_i64(&offsets)))
}

/// Decode one row's attributes. Every length is validated against the bytes
/// actually present before it is used, and every key id against the dictionary —
/// a corrupt part must not be able to index out of either.
pub fn decode_attributes_at(
    data: &[u8],
    offs: &[i64],
    row: usize,
    row_count: usize,
    keys: &[String],
) -> Result<Attributes> {
    use prism_types::error::PrismError;

    if row >= row_count || row + 1 >= offs.len() {
        return Err(PrismError::Corrupt(format!(
            "attribute row {row} is out of range ({row_count} rows)"
        )));
    }
    let (start, end) = (offs[row], offs[row + 1]);
    if start < 0 || end < start || end as usize > data.len() {
        return Err(PrismError::Corrupt(format!(
            "attribute offset pair ({start}, {end}) is outside a {}-byte blob",
            data.len()
        )));
    }
    let buf = &data[start as usize..end as usize];
    let mut c = crate::format::Cursor::new(buf);

    // An attribute count is at least 4+1 bytes on the wire; the bound is what
    // stops a corrupt row claiming four billion attributes.
    let n = c.read_len(5, "attributes")?;
    let mut out = Attributes::new();
    for _ in 0..n {
        let id = c.u32()? as usize;
        let key = keys.get(id).ok_or_else(|| {
            PrismError::Corrupt(format!(
                "attribute key id {id} is not in the part's {}-key dictionary",
                keys.len()
            ))
        })?;
        let tag = c.u8()?;
        let value = match tag {
            ATTR_TYPE_STR => {
                let s = c.string()?;
                AttrValue::Str(s)
            }
            ATTR_TYPE_INT => AttrValue::Int(c.i64()?),
            ATTR_TYPE_DOUBLE => AttrValue::Double(c.f64()?),
            ATTR_TYPE_BOOL => AttrValue::Bool(c.u8()? != 0),
            other => {
                return Err(PrismError::Corrupt(format!(
                    "attribute `{key}` has type tag {other}, which this build cannot decode"
                )))
            }
        };
        out.insert(key.clone(), value);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_width_round_trips() {
        assert_eq!(decode_u32(&encode_u32(&[1, 2, 3])), vec![1, 2, 3]);
        assert_eq!(decode_i64(&encode_i64(&[-1, 0, 9])), vec![-1, 0, 9]);
        assert_eq!(decode_f64(&encode_f64(&[1.5, -2.25])), vec![1.5, -2.25]);
        assert_eq!(decode_f32(&encode_f32(&[1.5, -2.25])), vec![1.5, -2.25]);
    }

    #[test]
    fn strings_round_trip_including_empty_and_unicode() {
        let vals = vec![
            "hello".to_string(),
            String::new(),
            "héllo → 世界".to_string(),
        ];
        let (data, offs) = encode_strings(&vals);
        assert_eq!(decode_strings(&data, &offs, 3).unwrap(), vals);
    }

    #[test]
    fn corrupt_offsets_are_rejected_not_trusted() {
        let (data, _) = encode_strings(&["abc".to_string()]);
        // An offset that runs past the end of the blob.
        let bad = encode_i64(&[0, 1_000_000]);
        let err = decode_strings(&data, &bad, 1).unwrap_err();
        assert!(matches!(err, prism_types::error::PrismError::Corrupt(_)));

        // A negative offset.
        let neg = encode_i64(&[-5, 3]);
        assert!(decode_strings(&data, &neg, 1).is_err());

        // Wrong number of offsets for the row count.
        let short = encode_i64(&[0]);
        assert!(decode_strings(&data, &short, 1).is_err());
    }

    #[test]
    fn write_atomic_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("prism-io-{}", std::process::id()));
        ensure_dir(&dir).unwrap();
        let p = dir.join("x.bin");
        write_atomic(&p, b"hello").unwrap();
        assert_eq!(read_file(&p).unwrap(), b"hello");
        assert!(!tmp_path(&p).exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
