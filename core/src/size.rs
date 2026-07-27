//! Per-file size measurement.
//!
//! Two numbers per file, and the gap between them is the point of this tool:
//! `apparent` is what `ls` shows, `allocated` is what you'd actually get back by
//! deleting it. They diverge for sparse and compressed files.
//!
//! **On Windows this module is the fallback, not the main path.** The walk gets
//! both numbers from the directory listing (see [`crate::windows_dir`]), which
//! is the only place a real `AllocationSize` is available; what remains here is
//! used when the listing did not cover an entry. See [`allocated`] for why the
//! path-based API cannot answer the question on its own.

use std::fs::Metadata;
use std::io;
use std::path::Path;

/// Both size readings for one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sizes {
    /// Bytes the file occupies on disk.
    pub allocated: u64,
    /// Logical length — the file's content size.
    pub apparent: u64,
}

/// Measure one regular file.
///
/// Takes the `metadata` the caller already fetched rather than looking it up
/// again: the walk needs exactly one metadata call per real file, and this is
/// where it gets spent.
///
/// The `io::Result` exists for Windows, where reading allocated size means a
/// syscall that can fail. On Unix this is arithmetic over metadata already in
/// hand and cannot fail — so callers should not expect the `Err` arm to be
/// reachable there.
pub(crate) fn measure(path: &Path, metadata: &Metadata) -> io::Result<Sizes> {
    Ok(Sizes {
        allocated: allocated(path, metadata)?,
        apparent: metadata.len(),
    })
}

/// What one file occupies on disk.
///
/// Public because the TUI lists directories itself and must report the **same**
/// number `scan` does. `metadata.len()` is the apparent size, and showing that
/// in one verb and this in another would make the same file read two ways —
/// worse once directory totals arrive, since those are allocated and would sit
/// in the same column.
pub fn allocated_size(path: &Path, metadata: &Metadata) -> io::Result<u64> {
    allocated(path, metadata)
}

/// `st_blocks` counts 512-byte units by POSIX definition, whatever the
/// filesystem's real block size is — this is deliberately not `blksize()`,
/// which is the preferred I/O chunk and a common mix-up.
#[cfg(unix)]
fn allocated(_path: &Path, metadata: &Metadata) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;

    Ok(metadata.blocks() * 512)
}

/// Give a drive-letter path past `MAX_PATH` the `\\?\` verbatim prefix Win32
/// needs, and leave everything else untouched.
///
/// `std::fs` does this internally, but `GetCompressedFileSizeW` is raw FFI and
/// gets whatever we hand it — so this is the one spot that has to prefix by hand.
/// The conversion is driven by length, not by retrying after a failure: a length
/// is a fact, whereas "which error means too long" is a guess. `std::path::absolute`
/// produces the absolute form `\\?\` requires without touching the filesystem —
/// unlike `canonicalize`, which this project never calls and which would rewrite
/// short paths too, breaking on RAM disks / network drives / Docker mounts.
///
/// Only `C:\`-style drive paths are converted. A UNC path (`\\server\share`)
/// needs the distinct `\\?\UNC\server\share` form, not a naive `\\?\` prefix, so
/// rather than get that subtly wrong it is left alone — a long UNC path may still
/// hit the limit and skip, a documented v0.1 gap for network shares.
#[cfg(windows)]
fn verbatim_if_long(path: &Path) -> std::borrow::Cow<'_, Path> {
    use std::borrow::Cow;
    use std::path::{Component, Prefix};

    // Under MAX_PATH (260), leaving margin so a short path stays short.
    const THRESHOLD: usize = 248;

    if path.as_os_str().len() <= THRESHOLD {
        return Cow::Borrowed(path);
    }
    // Convert drive-letter paths only; anything else (already-verbatim, UNC,
    // device namespace) is handed back untouched.
    let is_plain_drive = matches!(
        path.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
    );
    if !is_plain_drive {
        return Cow::Borrowed(path);
    }
    match std::path::absolute(path) {
        Ok(absolute) => {
            let mut verbatim = std::ffi::OsString::from(r"\\?\");
            verbatim.push(absolute.as_os_str());
            Cow::Owned(std::path::PathBuf::from(verbatim))
        }
        // Nothing to convert to — hand back the original and let the FFI raise a
        // real error, which the caller turns into a skip.
        Err(_) => Cow::Borrowed(path),
    }
}

/// Windows keeps allocated size out of `Metadata` entirely, so this is the one
/// place the crate reaches for FFI — hence the scoped `allow` against the
/// crate-wide `deny(unsafe_code)`.
///
/// **This is a fallback.** `GetCompressedFileSize` returns the true on-disk size
/// only for compressed and sparse files; for an ordinary file it is documented
/// to return "the actual file size, the same as the value returned by a call to
/// `GetFileSize`", so a 1-byte file reports 1 byte here where Unix reports a
/// whole block.
///
/// The real figure is `AllocationSize`, which [`crate::windows_dir`] now reads
/// for every entry of a directory at once. This path survives for the entries
/// that listing misses — a file created between the listing and the walk
/// reaching it — where an approximate size beats none.
#[cfg(windows)]
#[allow(unsafe_code)]
fn allocated(path: &Path, _metadata: &Metadata) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;

    /// Not exported by `windows-sys`.
    const INVALID_FILE_SIZE: u32 = u32::MAX;

    // Prefix long paths (but never canonicalize) — see `verbatim_if_long`.
    let path = verbatim_if_long(path);
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);

    let mut high: u32 = 0;
    // SAFETY: `wide` is NUL-terminated and outlives the call; `high` is a live,
    // writable `u32`. The callee only reads the string and writes `high`.
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };

    // INVALID_FILE_SIZE is also a legitimate low dword for files whose size ends
    // in 0xFFFFFFFF, so it only signals failure when the thread's error code is
    // actually set.
    if low == INVALID_FILE_SIZE {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(0) {
            return Err(err);
        }
    }

    Ok(u64::from(high) << 32 | u64::from(low))
}

/// Platforms with no concept of allocated size report the logical length, so
/// `allocated == apparent` there and sparse files look full-size.
#[cfg(not(any(unix, windows)))]
fn allocated(_path: &Path, metadata: &Metadata) -> io::Result<u64> {
    Ok(metadata.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    /// Write `contents` to `name` inside a fresh temp dir, returning both so the
    /// dir outlives the measurement (dropping it deletes the file).
    fn file_with(name: &str, contents: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join(name);
        let mut file = fs::File::create(&path).expect("create file");
        file.write_all(contents).expect("write contents");
        file.sync_all().expect("sync");
        (dir, path)
    }

    fn measure_path(path: &std::path::Path) -> Sizes {
        let metadata = fs::symlink_metadata(path).expect("metadata");
        measure(path, &metadata).expect("measure")
    }

    #[test]
    fn apparent_equals_content_length() {
        let (_dir, path) = file_with("known.bin", &vec![b'x'; 1234]);

        let sizes = measure_path(&path);

        assert_eq!(sizes.apparent, 1234);
    }

    /// A sparse file must report fewer bytes on disk than its logical length.
    ///
    /// The fixture uses `set_len` (ftruncate), *not* seek-then-write: APFS
    /// allocates the whole range for seek+write, so that idiom produces a
    /// non-sparse file and this test would fail on macOS for the wrong reason.
    /// Writing one byte first keeps `allocated` non-zero, so an empty file
    /// can't make the assertion pass by accident.
    #[cfg(unix)]
    #[test]
    fn sparse_file_allocated_less_than_apparent() {
        const LOGICAL: u64 = 4 * 1024 * 1024;

        let (_dir, path) = file_with("sparse.bin", b"x");
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("reopen for truncate");
        file.set_len(LOGICAL).expect("set_len");
        file.sync_all().expect("sync");

        let sizes = measure_path(&path);

        assert_eq!(sizes.apparent, LOGICAL);
        assert!(
            sizes.allocated < sizes.apparent,
            "expected sparse file to occupy less than its length, got allocated={} apparent={}",
            sizes.allocated,
            sizes.apparent
        );
        assert!(
            sizes.allocated > 0,
            "fixture should have one written byte on disk, so allocated must be non-zero"
        );
    }

    /// An empty file has nothing on disk to account for. Guards the boundary
    /// where `allocated >= apparent` collapses to `0 >= 0`.
    #[test]
    fn empty_file_occupies_nothing() {
        let (_dir, path) = file_with("empty.bin", b"");

        let sizes = measure_path(&path);

        assert_eq!(sizes.apparent, 0);
        assert_eq!(sizes.allocated, 0);
    }

    /// Short paths reach the FFI untouched — no needless rewriting.
    #[cfg(windows)]
    #[test]
    fn short_path_is_not_prefixed() {
        let path = std::path::Path::new(r"C:\short\enough");

        assert_eq!(verbatim_if_long(path).as_ref(), path);
    }

    /// A path past MAX_PATH gets the `\\?\` prefix, so `GetCompressedFileSizeW`
    /// never hits the length limit.
    #[cfg(windows)]
    #[test]
    fn long_path_gets_verbatim_prefix() {
        let long = format!(r"C:\{}", "a".repeat(300));
        let path = std::path::Path::new(&long);

        let converted = verbatim_if_long(path);

        assert!(
            converted.as_os_str().to_string_lossy().starts_with(r"\\?\"),
            "a >MAX_PATH path must reach the FFI in verbatim form, got {converted:?}"
        );
    }

    /// Prefixing an already-verbatim path would corrupt it.
    #[cfg(windows)]
    #[test]
    fn verbatim_path_is_not_prefixed_twice() {
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        let path = std::path::Path::new(&long);

        let converted = verbatim_if_long(path);

        assert_eq!(converted.as_ref(), path);
        assert!(
            !converted
                .as_os_str()
                .to_string_lossy()
                .starts_with(r"\\?\\\?\")
        );
    }

    /// A UNC path needs `\\?\UNC\...`, not a naive `\\?\` prefix, so a long one
    /// is left untouched rather than corrupted (documented v0.1 gap).
    #[cfg(windows)]
    #[test]
    fn long_unc_path_is_left_untouched() {
        let long = format!(r"\\server\share\{}", "a".repeat(300));
        let path = std::path::Path::new(&long);

        assert_eq!(verbatim_if_long(path).as_ref(), path);
    }

    /// A normal file occupies whole blocks — this fails loudly if `allocated`
    /// ever degrades into returning the logical length.
    ///
    /// The block-multiple invariant is **Unix's alone**, and not by accident:
    /// `GetCompressedFileSize` is documented to return "the actual file size,
    /// the same as the value returned by a call to `GetFileSize`" unless the
    /// file is compressed or sparse. Windows therefore reports 10 here, and the
    /// weaker assertion below is all that can be checked — see
    /// `ordinary_windows_file_reports_allocated_equal_to_apparent`.
    #[test]
    fn allocated_is_block_multiple() {
        let (_dir, path) = file_with("small.bin", b"0123456789");

        let sizes = measure_path(&path);

        assert_eq!(sizes.apparent, 10);
        #[cfg(unix)]
        assert!(
            sizes.allocated.is_multiple_of(512),
            "allocated should be a whole number of 512-byte units, got {}",
            sizes.allocated
        );
        #[cfg(windows)]
        assert!(
            sizes.allocated.is_multiple_of(512) || sizes.allocated == sizes.apparent,
            "allocated should be whole 512-byte units or — as Windows documents \
             for an uncompressed file — the logical length, got {}",
            sizes.allocated
        );
        assert!(
            sizes.allocated >= sizes.apparent,
            "a 10-byte non-compressed file should occupy at least its length, got allocated={}",
            sizes.allocated
        );
    }

    /// Pins the documented Windows contract, so the gap it describes cannot be
    /// mistaken for a bug later.
    ///
    /// `GetCompressedFileSize` "retrieves the actual number of bytes of disk
    /// storage used" only for compressed and sparse files; for everything else
    /// the docs are explicit that "the value obtained is the actual file size,
    /// the same as the value returned by a call to `GetFileSize`". So on
    /// Windows an ordinary file's `allocated` equals its `apparent` — slack
    /// space up to the cluster boundary is invisible, unlike Unix's
    /// `blocks * 512`.
    ///
    /// This is *not* a guard against `allocated` degrading into
    /// `metadata.len()` — no such guard is possible without a sparse or
    /// compressed fixture, which needs `DeviceIoControl` FFI. See the README's
    /// limitations.
    #[cfg(windows)]
    #[test]
    fn ordinary_windows_file_reports_allocated_equal_to_apparent() {
        const LOGICAL: usize = 5000; // not a whole number of clusters
        let (_dir, path) = file_with("ordinary.bin", &vec![b'x'; LOGICAL]);

        let sizes = measure_path(&path);

        assert_eq!(sizes.apparent, LOGICAL as u64);
        assert_eq!(
            sizes.allocated, sizes.apparent,
            "GetCompressedFileSize returns the logical size for an uncompressed, \
             non-sparse file — if this ever rounds up to a cluster, the README's \
             Windows limitation is stale and should be revisited"
        );
    }
}
