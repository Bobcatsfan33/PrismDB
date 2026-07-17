//! Free-space measurement — the input to merge admission ([merge contract §3](../../../docs/MERGE-CONTRACT.md)).
//!
//! A merge writes a second copy of the data it compacts before it can free the first, so it is the
//! disk-hungriest operation the engine runs, and it must be **refused before it starts** if the
//! output would not fit. That decision needs the free bytes on the device — which the standard
//! library does not expose, and which the serde-only charter forbids pulling a crate in for. So a
//! small per-OS `statvfs`/`statfs` shim reads it directly. The FFI is in [UNSAFE-INVENTORY.md].
//!
//! [`available_bytes`] returns `None` when it cannot determine free space (an unrecognised OS, or a
//! failed syscall). A `None` is treated by admission as "unknown, do not block" — the *real* ENOSPC
//! backstop ([`crate::faults::guard_space`] and the errno→[`prism_types::error::PrismError::OutOfSpace`]
//! mapping) still catches a genuine full disk gracefully, so an unreadable free count degrades to
//! the write-time guard, never to a corruption.

use std::path::Path;
use std::sync::Mutex;

/// A test override for the free-space reading, so admission can be driven without actually filling
/// a disk. `Some(bytes)` makes [`available_bytes`] report exactly that; `None` restores the real
/// syscall. Tests that use this must serialize (it is process-global).
static OVERRIDE: Mutex<Option<u64>> = Mutex::new(None);

/// Force [`available_bytes`] to report `bytes` free; `None` restores the real measurement.
pub fn set_available_override(bytes: Option<u64>) {
    *OVERRIDE.lock().expect("disk override lock") = bytes;
}

/// Free bytes available on the device holding `path`, or `None` if it cannot be determined.
pub fn available_bytes(path: &Path) -> Option<u64> {
    resolve_available_bytes(*OVERRIDE.lock().expect("disk override lock"), path)
}

/// The pure resolution rule: an injected override wins, else the real measurement. Split out from
/// the process-global [`OVERRIDE`] so tests exercise the rule by **passing the value in** rather than
/// racing a global — zero-flake by construction, not by serialization.
fn resolve_available_bytes(overridden: Option<u64>, path: &Path) -> Option<u64> {
    overridden.or_else(|| real_available_bytes(path))
}

#[cfg(target_os = "linux")]
fn real_available_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // glibc `struct statvfs` is ~112 bytes; over-allocate. Fields (x86-64/aarch64, all unsigned
    // long): f_bsize@0, f_frsize@8, f_blocks@16, f_bfree@24, f_bavail@32.
    let mut buf = [0u8; 256];
    extern "C" {
        fn statvfs(path: *const std::os::raw::c_char, buf: *mut u8) -> std::os::raw::c_int;
    }
    // SAFETY: `c` is a valid NUL-terminated path; `buf` is 256 bytes, larger than `struct statvfs`,
    // so the kernel cannot write out of bounds. We read two u64 fields at their POSIX offsets only
    // after a success return.
    let rc = unsafe { statvfs(c.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let f_frsize = u64::from_ne_bytes(buf[8..16].try_into().ok()?);
    let f_bavail = u64::from_ne_bytes(buf[32..40].try_into().ok()?);
    Some(f_bavail.saturating_mul(f_frsize))
}

#[cfg(target_os = "macos")]
fn real_available_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // macOS (64-bit-inode) `struct statfs` is ~2400 bytes; over-allocate. Fields: f_bsize:u32@0,
    // f_iosize:i32@4, f_blocks:u64@8, f_bfree:u64@16, f_bavail:u64@24.
    let mut buf = [0u8; 4096];
    extern "C" {
        fn statfs(path: *const std::os::raw::c_char, buf: *mut u8) -> std::os::raw::c_int;
    }
    // SAFETY: `c` is a valid NUL-terminated path; `buf` is 4096 bytes, larger than `struct statfs`,
    // so the kernel cannot write out of bounds. We read f_bsize and f_bavail at their fixed offsets
    // only after a success return.
    let rc = unsafe { statfs(c.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let f_bsize = u32::from_ne_bytes(buf[0..4].try_into().ok()?) as u64;
    let f_bavail = u64::from_ne_bytes(buf[24..32].try_into().ok()?);
    Some(f_bavail.saturating_mul(f_bsize))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn real_available_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // These test the pure resolution rule by INJECTING the override value, never touching the
    // process-global `OVERRIDE` — so they are zero-flake under parallel execution, where two tests
    // racing the global would otherwise clobber each other's setting. (The global is exercised
    // end-to-end by `tests/enospc.rs`, which drives merge admission.)

    #[test]
    fn reports_a_plausible_free_count_for_the_temp_dir() {
        let got = resolve_available_bytes(None, &std::env::temp_dir());
        // On the CI and dev targets this must succeed and be non-trivial; elsewhere None is allowed.
        if let Some(bytes) = got {
            assert!(bytes > 1024, "implausibly small free space: {bytes}");
        }
    }

    #[test]
    fn the_injected_override_wins_over_the_real_measurement() {
        // An injected value is returned verbatim, even for a path the syscall would fail on.
        assert_eq!(
            resolve_available_bytes(Some(4242), Path::new("/nonexistent")),
            Some(4242)
        );
        // With no override, the rule falls through to the real measurement.
        assert_eq!(
            resolve_available_bytes(None, Path::new("/nonexistent")),
            real_available_bytes(Path::new("/nonexistent"))
        );
    }
}
