//! Read-only memory mapping over an immutable part file (S6).
//!
//! Parts are immutable once published, so a read-only mapping is the easy case: nothing mutates
//! the bytes under us, and several readers can share one page cache. There is exactly one sharp
//! edge, and the architect named it: **a truncated file under mmap `SIGBUS`es on access** rather
//! than returning an error, and a `SIGBUS` is a process death — it cannot be caught, reported, or
//! attributed to the read that caused it.
//!
//! The [S1 truncation discipline](../../../docs/DECISIONS.md) — a truncated part names its column,
//! block and byte range — must survive the move from `read()` to mmap. It does, **by
//! construction**, not by a signal handler:
//!
//! 1. The map's length is the file's *real* length, stat'd once at map time.
//! 2. Every byte a caller reads goes through [`Mmap::slice`], which is bounds-checked against that
//!    length and returns a named `Corrupt` error for anything past the end.
//! 3. mmap backs every byte in `[0, file_len)`; bytes in the last partial page beyond `file_len`
//!    read as zero and pages entirely beyond it are never mapped — but we never reach them,
//!    because the bounds check fires first.
//!
//! So the `SIGBUS` is unreachable: a truncated part is refused with the same named error it always
//! was, before a single byte past the file's end is touched. The `truncated-part-under-mmap` fault
//! test proves it.
//!
//! Every `unsafe` block here is in [docs/UNSAFE-INVENTORY.md](../../../docs/UNSAFE-INVENTORY.md).

use prism_types::error::{PrismError, Result};
use std::fs::File;

#[cfg(unix)]
mod ffi {
    use std::os::raw::{c_int, c_void};

    // The minimal mmap surface. Declared here rather than pulled from `libc`, because the charter
    // keeps the dependency tree to serde alone (docs/DECISIONS.md D-002). These signatures are the
    // stable POSIX ABI on the platforms PrismDB targets (linux, macOS), both LP64.
    extern "C" {
        pub fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }

    pub const PROT_READ: c_int = 0x1;
    pub const MAP_PRIVATE: c_int = 0x2;
    // mmap returns (void*)-1 on failure.
    pub fn map_failed() -> *mut c_void {
        usize::MAX as *mut c_void
    }
}

/// A read-only mapping of an entire file. Unmaps itself on drop.
pub struct Mmap {
    ptr: *mut u8,
    len: usize,
}

// The mapping is read-only over an immutable file and owns its region exclusively (each `Mmap`
// maps its own range and unmaps it on drop). No interior mutability, so sharing `&Mmap` across
// threads is sound.
unsafe impl Send for Mmap {}
unsafe impl Sync for Mmap {}

impl Mmap {
    /// Map `file` read-only for its entire current length.
    ///
    /// The length is read here, once, and *is* the mapped length — which is what makes a later
    /// out-of-range access a named error rather than a `SIGBUS` (see the module docs).
    #[cfg(unix)]
    pub fn open(file: &File) -> Result<Mmap> {
        use std::os::unix::io::AsRawFd;

        let len = file.metadata()?.len() as usize;
        if len == 0 {
            // mmap refuses a zero length; an empty file maps to an empty slice with no syscall.
            return Ok(Mmap {
                ptr: std::ptr::NonNull::dangling().as_ptr(),
                len: 0,
            });
        }

        // SAFETY: `fd` is a valid open file descriptor for the lifetime of this call (borrowed
        // from `file`). PROT_READ | MAP_PRIVATE requests a private read-only mapping; we never
        // write through `ptr`. `len` is the file's real length, so every mapped, in-range byte is
        // backed by the file. The returned pointer is checked against MAP_FAILED below. Ownership
        // of the mapping transfers to the returned `Mmap`, whose `Drop` unmaps exactly `len`.
        let ptr = unsafe {
            ffi::mmap(
                std::ptr::null_mut(),
                len,
                ffi::PROT_READ,
                ffi::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == ffi::map_failed() {
            return Err(PrismError::Io(std::io::Error::last_os_error().to_string()));
        }
        Ok(Mmap {
            ptr: ptr as *mut u8,
            len,
        })
    }

    #[cfg(not(unix))]
    pub fn open(_file: &File) -> Result<Mmap> {
        Err(PrismError::Invalid(
            "memory mapping is only implemented on unix".into(),
        ))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Borrow `[offset, offset+len)` of the file, bounds-checked.
    ///
    /// **This is the SIGBUS guard.** Every read of a mapped part goes through here, and a range
    /// that runs past the file's real length is refused with a named `Corrupt` error — the exact
    /// S1 truncation error, reached before any out-of-range byte is touched. `named` identifies the
    /// part/column/block so the message can point at what lied.
    pub fn slice(&self, offset: usize, len: usize, named: &dyn Fn() -> String) -> Result<&[u8]> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| PrismError::Corrupt(format!("{}: byte range overflows", named())))?;
        if end > self.len {
            return Err(PrismError::Corrupt(format!(
                "{} is truncated: needs bytes {offset}..{end}, but the mapped file is only {} bytes",
                named(),
                self.len
            )));
        }
        if len == 0 {
            return Ok(&[]);
        }
        // SAFETY: `offset..end` is within `[0, self.len)`, checked above, and `self.len` is the
        // mapped length, so every byte in this range is backed by the file (an in-range access
        // never touches an unbacked page, so it never SIGBUSes). The mapping is read-only and
        // outlives the returned slice, which borrows `self`.
        Ok(unsafe { std::slice::from_raw_parts(self.ptr.add(offset), len) })
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.len != 0 {
            // SAFETY: `ptr`/`len` are exactly what `mmap` returned and have not been unmapped
            // (Drop runs once). Unmapping a live read-only mapping is always sound.
            unsafe {
                ffi::munmap(self.ptr as *mut std::os::raw::c_void, self.len);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("prism-mmap-{}-{name}", std::process::id()))
    }

    #[test]
    fn a_mapping_reads_the_file_back() {
        let p = tmp("read");
        std::fs::write(&p, b"the immutable bytes").unwrap();
        let f = File::open(&p).unwrap();
        let m = Mmap::open(&f).unwrap();
        assert_eq!(m.slice(0, 3, &|| "x".into()).unwrap(), b"the");
        assert_eq!(m.slice(4, 9, &|| "x".into()).unwrap(), b"immutable");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reading_past_the_end_is_a_named_error_not_a_sigbus() {
        let p = tmp("short");
        std::fs::write(&p, b"only ten..").unwrap(); // 10 bytes
        let f = File::open(&p).unwrap();
        let m = Mmap::open(&f).unwrap();
        let err = m
            .slice(4, 100, &|| "part p1 column pq.codes block 3".into())
            .unwrap_err()
            .to_string();
        assert!(err.contains("truncated"), "{err}");
        assert!(
            err.contains("pq.codes"),
            "the error must name what lied: {err}"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn an_empty_file_maps_to_an_empty_slice() {
        let p = tmp("empty");
        File::create(&p).unwrap().flush().unwrap();
        let f = File::open(&p).unwrap();
        let m = Mmap::open(&f).unwrap();
        assert!(m.is_empty());
        assert_eq!(m.slice(0, 0, &|| "x".into()).unwrap(), b"");
        assert!(m.slice(0, 1, &|| "x".into()).is_err());
        let _ = std::fs::remove_file(&p);
    }
}
