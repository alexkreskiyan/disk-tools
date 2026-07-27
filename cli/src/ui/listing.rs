//! One directory, read as cheaply as the platform allows.
//!
//! The TUI does its own listing rather than going through
//! [`disk_tools_core::scan`]: it needs a creation time, which `ScanNode` does
//! not carry, and it needs the answer for *one* directory rather than a whole
//! tree. Directory totals arrive separately and on demand (v0.4 Task 3).
//!
//! The file sizes here are **allocated**, from the core, so that the same file
//! reads the same in `scan` and in `ui`. `metadata.len()` would have been the
//! apparent size — a different number, and one that would sit incoherently
//! beside the directory totals the next task computes.

use disk_tools_core::{State, allocated_size};
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::time::SystemTime;

/// One row on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: OsString,
    pub is_dir: bool,

    /// Allocated bytes for a file. `None` for a directory until it is measured,
    /// and `None` for a file whose size could not be read — the two are the same
    /// statement here: not known yet.
    pub size: Option<u64>,

    pub modified: Option<SystemTime>,

    /// `None` where the platform records no birth time.
    ///
    /// Linux reports it only through `statx`, and only on filesystems that keep
    /// it; `Metadata::created()` returns `Unsupported` otherwise. That is not an
    /// error worth refusing to list a directory over — it is one column with
    /// nothing in it, and the sort deals with that.
    pub created: Option<SystemTime>,

    /// What the configured rules say about this path.
    ///
    /// Filled in by the browser, which has the rules; `list` is about one
    /// directory and knows nothing of them.
    pub state: State,

    /// A size is being computed for this row right now.
    ///
    /// Distinct from `size.is_none()`: a directory part-way through a walk has a
    /// number, and it is climbing. Everything that reads a size has to know the
    /// difference — the spinner shows it, the share-of-listing arithmetic
    /// excludes it, and a partial figure never becomes a total.
    pub measuring: bool,
}

/// Read `dir`, one metadata call per entry.
///
/// An entry that cannot be stated is still listed, with what is known: a
/// permission problem on one file is no reason to hide the other ninety-nine.
/// Failure to read the **directory** is an error, since then there is nothing to
/// show at all.
pub fn list(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        // `file_type` from the directory handle costs nothing extra; the
        // metadata call after it is the one per-entry syscall.
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        let metadata = entry.metadata().ok();

        entries.push(Entry {
            name: entry.file_name(),
            is_dir,
            size: metadata
                .as_ref()
                .filter(|_| !is_dir)
                .and_then(|metadata| allocated_size(&path, metadata).ok()),
            modified: metadata.as_ref().and_then(|m| m.modified().ok()),
            created: metadata.as_ref().and_then(|m| m.created().ok()),
            // Both set by the browser: one directory listed on its own has no
            // rules to consult and starts no work.
            state: State::Untracked,
            measuring: false,
        });
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.bin"), vec![b'x'; 4096]).expect("write");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        dir
    }

    fn find<'a>(entries: &'a [Entry], name: &str) -> &'a Entry {
        entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("{name} missing from {entries:?}"))
    }

    /// A file's size is known from its listing; a directory's is not, and saying
    /// `Some(0)` would be a claim rather than an absence.
    #[test]
    fn a_file_has_a_size_and_a_directory_does_not() {
        let dir = fixture();

        let entries = list(dir.path()).expect("list");

        assert_eq!(entries.len(), 2);
        assert!(
            find(&entries, "a.bin")
                .size
                .is_some_and(|size| size >= 4096),
            "allocated is at least the content length"
        );
        assert_eq!(find(&entries, "sub").size, None);
        assert!(find(&entries, "sub").is_dir);
        assert!(!find(&entries, "a.bin").is_dir);
    }

    /// Allocated, not apparent — the number `scan` would print for the same
    /// file. A sparse or compressed file makes the two differ; a 4 KiB one only
    /// proves they are read from the same place.
    #[test]
    fn the_size_is_the_one_scan_reports() {
        let dir = fixture();
        let path = dir.path().join("a.bin");
        let metadata = std::fs::metadata(&path).expect("metadata");

        let entries = list(dir.path()).expect("list");

        assert_eq!(
            find(&entries, "a.bin").size,
            Some(allocated_size(&path, &metadata).expect("allocated"))
        );
    }

    #[test]
    fn an_empty_directory_lists_nothing_and_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert_eq!(list(dir.path()).expect("list"), Vec::new());
    }

    /// There is nothing to show, so this one *is* an error — unlike a single
    /// unreadable entry inside a directory that can otherwise be read.
    #[test]
    fn a_directory_that_is_not_there_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");

        let err = list(&dir.path().join("nope")).expect_err("must fail");

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    /// Timestamps are read where the platform has them. `created` is allowed to
    /// be `None` — Linux without `statx` birth times is the case — so this
    /// asserts what every platform must manage and states the other as a fact
    /// about the run rather than a requirement.
    #[test]
    fn timestamps_are_read_where_the_platform_keeps_them() {
        let dir = fixture();

        let entries = list(dir.path()).expect("list");

        assert!(
            entries.iter().all(|entry| entry.modified.is_some()),
            "mtime is universal"
        );
        if entries.iter().all(|entry| entry.created.is_none()) {
            eprintln!("note: this platform records no birth times");
        }
    }
}
