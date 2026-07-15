//! Per-file size measurement.
//!
//! Two numbers per file, and the gap between them is the point of this tool:
//! `apparent` is what `ls` shows, `allocated` is what you'd actually get back by
//! deleting it. They diverge for sparse and compressed files.

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

/// `st_blocks` counts 512-byte units by POSIX definition, whatever the
/// filesystem's real block size is — this is deliberately not `blksize()`,
/// which is the preferred I/O chunk and a common mix-up.
#[cfg(unix)]
fn allocated(_path: &Path, metadata: &Metadata) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;

    Ok(metadata.blocks() * 512)
}

/// Windows keeps allocated size out of `Metadata` entirely, so this is the one
/// place the crate reaches for FFI — hence the scoped `allow` against the
/// crate-wide `deny(unsafe_code)`.
#[cfg(windows)]
#[allow(unsafe_code)]
fn allocated(path: &Path, _metadata: &Metadata) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;

    /// Not exported by `windows-sys`.
    const INVALID_FILE_SIZE: u32 = u32::MAX;

    // The path goes to the API exactly as the walk produced it. Canonicalizing
    // first would rewrite it to `\\?\` form, which other tools reject and which
    // fails outright on some volumes (RAM disks, network drives, Docker mounts).
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

    /// A normal file occupies whole blocks — this fails loudly if `allocated`
    /// ever degrades into returning the logical length.
    #[test]
    fn allocated_is_block_multiple() {
        let (_dir, path) = file_with("small.bin", b"0123456789");

        let sizes = measure_path(&path);

        assert_eq!(sizes.apparent, 10);
        assert_eq!(
            sizes.allocated % 512,
            0,
            "allocated should be a whole number of 512-byte units, got {}",
            sizes.allocated
        );
        assert!(
            sizes.allocated >= sizes.apparent,
            "a 10-byte non-compressed file should occupy at least its length, got allocated={}",
            sizes.allocated
        );
    }
}
