//! Parallel directory traversal.
//!
//! Produces a *flat* list of entries rather than a tree: hardlink attribution
//! (Task 4) groups by identity across the whole scan, so it has to settle before
//! anything gets summed. The tree is built afterwards, from these entries.

use crate::ScanOptions;
use crate::size;
use crate::tree::{SkipReason, SkippedEntry};
use rayon::prelude::*;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};

/// What identifies a file on disk, so two paths pointing at the same bytes can
/// be recognised as one. Consumed by the dedup pass.
///
/// Unix-only for now — `(st_dev, st_ino)`. Windows cannot supply an equivalent
/// from a directory listing; see [`file_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FileId {
    device: u64,
    inode: u64,
}

/// One thing the walk found, before dedup attribution or aggregation.
#[derive(Debug, Clone)]
pub(crate) struct WalkEntry {
    pub path: PathBuf,
    pub allocated: u64,
    pub apparent: u64,
    pub is_dir: bool,
    /// `None` for directories, and on Windows when the OS declines to say.
    pub id: Option<FileId>,
}

/// The walk's whole output: what it measured, and what it couldn't.
#[derive(Debug, Default)]
pub(crate) struct Walked {
    pub entries: Vec<WalkEntry>,
    pub skipped: Vec<SkippedEntry>,
}

impl Walked {
    fn absorb(&mut self, other: Walked) {
        self.entries.extend(other.entries);
        self.skipped.extend(other.skipped);
    }

    fn skip(path: &Path, err: &io::Error) -> Self {
        Walked {
            entries: Vec::new(),
            skipped: vec![SkippedEntry {
                path: path.to_path_buf(),
                reason: skip_reason(err),
            }],
        }
    }

    fn record_skip(&mut self, path: PathBuf, err: &io::Error) {
        self.skipped.push(SkippedEntry {
            path,
            reason: skip_reason(err),
        });
    }
}

/// Walk `options.root`, measuring everything under it.
///
/// Never fails: an unreadable root is one skipped entry, not an error.
pub(crate) fn walk(options: &ScanOptions) -> Walked {
    let root = options.root.as_path();

    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(err) => return Walked::skip(root, &err),
    };

    let mut walked = match entry_for(root, &metadata) {
        Ok(entry) => Walked {
            entries: vec![entry],
            skipped: Vec::new(),
        },
        Err(err) => return Walked::skip(root, &err),
    };

    if metadata.is_dir() {
        let root_device = device_of(&metadata);
        walked.absorb(walk_dir(root, root_device, options));
    }

    walked
}

fn walk_dir(dir: &Path, root_device: Option<u64>, options: &ScanOptions) -> Walked {
    // `std::fs::read_dir` applies the `\\?\` long-path prefix itself, so this
    // needs no help — the only place that bypasses std, and so the only place
    // that must prefix by hand, is the raw FFI in `size`.
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(err) => return Walked::skip(dir, &err),
    };

    let mut walked = Walked::default();
    let mut subdirs = Vec::new();

    for entry in listing {
        let entry = match entry {
            Ok(entry) => entry,
            // The directory is readable but one of its entries isn't; the rest
            // of the listing is still worth having.
            Err(err) => {
                walked.record_skip(dir.to_path_buf(), &err);
                continue;
            }
        };
        let path = entry.path();

        // `file_type` comes back with the `readdir` result — free. Asking for
        // `metadata` to learn the same thing would cost an extra `stat` per
        // entry, which is the single most expensive mistake this walk can make.
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                walked.record_skip(path, &err);
                continue;
            }
        };

        // Does not traverse symlinks, so this is the link's own metadata.
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                walked.record_skip(path, &err);
                continue;
            }
        };

        if file_type.is_dir() {
            // A mount point's inode belongs to the mounted filesystem, so a
            // differing device means we've reached the boundary: don't record
            // it and don't descend, the way `du -x` behaves.
            if options.one_file_system && crosses_filesystem(&metadata, root_device) {
                continue;
            }
            subdirs.push(path.clone());
        }

        match entry_for(&path, &metadata) {
            Ok(entry) => walked.entries.push(entry),
            Err(err) => walked.record_skip(path, &err),
        }
    }

    // Directory-level parallelism: each subtree is measured independently and
    // hands its results back, so no lock is shared across threads.
    let nested: Vec<Walked> = subdirs
        .into_par_iter()
        .map(|subdir| walk_dir(&subdir, root_device, options))
        .collect();
    for child in nested {
        walked.absorb(child);
    }

    walked
}

fn entry_for(path: &Path, metadata: &Metadata) -> io::Result<WalkEntry> {
    let sizes = size::measure(path, metadata)?;
    let is_dir = metadata.is_dir();

    Ok(WalkEntry {
        path: path.to_path_buf(),
        allocated: sizes.allocated,
        apparent: sizes.apparent,
        is_dir,
        // Directories can't be hardlinked, so an identity would never be used.
        id: if is_dir { None } else { file_id(metadata) },
    })
}

fn skip_reason(err: &io::Error) -> SkipReason {
    match err.kind() {
        io::ErrorKind::PermissionDenied => SkipReason::PermissionDenied,
        io::ErrorKind::NotFound => SkipReason::NotFound,
        _ => SkipReason::Other(err.to_string()),
    }
}

/// Would descending into `metadata`'s directory leave the root's filesystem?
///
/// When either side is unknown we say no: `--one-file-system` becomes a no-op
/// rather than a wrong guess.
fn crosses_filesystem(metadata: &Metadata, root_device: Option<u64>) -> bool {
    match (device_of(metadata), root_device) {
        (Some(device), Some(root)) => device != root,
        _ => false,
    }
}

/// `st_dev` rides along with the metadata we already fetched — free.
#[cfg(unix)]
fn device_of(metadata: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;

    Some(metadata.dev())
}

/// Windows can't answer this from a directory listing. `volume_serial_number`
/// is unstable (`windows_by_handle`), and even on nightly std hardcodes it to
/// `None` in `From<WIN32_FIND_DATAW>` — the very path `DirEntry::metadata()`
/// takes. A real answer needs an open handle per directory, i.e. FFI and a
/// second `unsafe` exemption.
///
/// So `--one-file-system` is a documented Unix-only flag rather than a
/// half-right guess at drive letters, which would miss volume mount points
/// anyway.
#[cfg(not(unix))]
fn device_of(_metadata: &Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
fn file_id(metadata: &Metadata) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;

    Some(FileId {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// Windows hardlinks go undeduplicated for now, and the reason is structural.
///
/// `file_index` is unstable, and — more decisively — std hardcodes it to `None`
/// unless the metadata came from an open handle, which a directory listing
/// never is. `number_of_links` is `None` for the same reason, so we can't even
/// cheaply spot *which* files have links worth checking. The only way through is
/// an `open()` per file: precisely the syscall this walk is built to avoid.
///
/// Task 4 can close the gap cheaply if it wants: hardlinks share a size, so it
/// need only open handles for files whose sizes collide — a small fraction, and
/// the same trick `fclones` uses.
#[cfg(not(unix))]
fn file_id(_metadata: &Metadata) -> Option<FileId> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScanOptions;
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn opts(root: &Path) -> ScanOptions {
        ScanOptions {
            root: root.to_path_buf(),
            ..ScanOptions::default()
        }
    }

    fn write(path: &Path, bytes: usize) {
        fs::write(path, vec![b'x'; bytes]).expect("write file");
    }

    fn file_paths(walked: &Walked) -> Vec<&PathBuf> {
        walked
            .entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| &e.path)
            .collect()
    }

    #[test]
    fn visits_each_file_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir_all(root.join("a/b")).expect("mkdir");
        fs::create_dir(root.join("c")).expect("mkdir");
        write(&root.join("top.bin"), 10);
        write(&root.join("a/one.bin"), 20);
        write(&root.join("a/b/two.bin"), 30);
        write(&root.join("c/three.bin"), 40);

        let walked = walk(&opts(root));

        let files = file_paths(&walked);
        let unique: HashSet<_> = files.iter().collect();
        assert_eq!(
            files.len(),
            unique.len(),
            "every file must be visited exactly once, got {files:?}"
        );

        let mut got: Vec<_> = files
            .iter()
            .map(|p| p.strip_prefix(root).expect("under root").to_owned())
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                PathBuf::from("a/b/two.bin"),
                PathBuf::from("a/one.bin"),
                PathBuf::from("c/three.bin"),
                PathBuf::from("top.bin"),
            ]
        );

        // Directories carry their own size too — `du` counts them, and on ext4
        // they really do occupy a block.
        let mut dirs: Vec<_> = walked
            .entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| e.path.strip_prefix(root).expect("under root").to_owned())
            .collect();
        dirs.sort();
        assert_eq!(
            dirs,
            vec![
                PathBuf::from(""), // the root itself
                PathBuf::from("a"),
                PathBuf::from("a/b"),
                PathBuf::from("c"),
            ],
            "directories must be recorded exactly once each, root included"
        );

        // Exact count, so double-recording can't hide behind a set membership check.
        assert_eq!(walked.entries.len(), 8, "4 files + 4 dirs");

        // The identity key Task 4 dedups by. Directories can't be hardlinked, so
        // they carry none. On Windows files carry none either — the OS won't say
        // without an open handle (see `file_id`).
        for entry in &walked.entries {
            if entry.is_dir {
                assert!(
                    entry.id.is_none(),
                    "{:?} is a dir, so has no id",
                    entry.path
                );
            } else if cfg!(unix) {
                assert!(entry.id.is_some(), "{:?} should carry an id", entry.path);
            }
        }

        assert!(walked.skipped.is_empty(), "nothing should be skipped");
    }

    /// A `tempdir` is one filesystem, so the flag must be invisible there.
    /// Guards against the boundary check firing on the flag alone.
    #[test]
    fn one_file_system_keeps_everything_within_a_single_filesystem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("sub")).expect("mkdir");
        write(&root.join("sub/inner.bin"), 10);
        write(&root.join("outer.bin"), 20);

        let walked = walk(&ScanOptions {
            root: root.to_path_buf(),
            one_file_system: true,
            ..ScanOptions::default()
        });

        let mut got: Vec<_> = file_paths(&walked)
            .iter()
            .map(|p| p.strip_prefix(root).expect("under root").to_owned())
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![PathBuf::from("outer.bin"), PathBuf::from("sub/inner.bin")],
            "one filesystem means the flag prunes nothing"
        );
    }

    /// The boundary decision is a pure function of two device ids, so it can be
    /// exercised without a second filesystem — unlike the `#[ignore]` test below,
    /// which checks the real thing end to end.
    #[cfg(unix)]
    #[test]
    fn crosses_filesystem_compares_devices() {
        let dir = tempfile::tempdir().expect("tempdir");
        let metadata = fs::symlink_metadata(dir.path()).expect("stat");
        let own = device_of(&metadata);

        assert!(
            !crosses_filesystem(&metadata, own),
            "the same device is not a crossing"
        );
        assert!(
            crosses_filesystem(&metadata, Some(u64::MAX)),
            "a different device is a crossing"
        );
        assert!(
            !crosses_filesystem(&metadata, None),
            "an unknown root device must not be guessed at"
        );
    }

    #[cfg(unix)]
    #[test]
    fn device_of_reports_the_filesystem_stat_reports() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("f.bin");
        write(&path, 1);
        let metadata = fs::symlink_metadata(&path).expect("stat");

        assert_eq!(device_of(&metadata), Some(metadata.dev()));
        // Same tree, same filesystem — a constant would break this too.
        let dir_metadata = fs::symlink_metadata(dir.path()).expect("stat");
        assert_eq!(device_of(&metadata), device_of(&dir_metadata));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_dir_collected_as_skipped() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let locked = root.join("locked");
        fs::create_dir(&locked).expect("mkdir");
        write(&locked.join("hidden.bin"), 10);
        write(&root.join("visible.bin"), 20);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");

        // `chmod 000` doesn't stop root, and a test that passes because the
        // fixture failed is worse than no test. Bail out loudly instead.
        if fs::read_dir(&locked).is_ok() {
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("restore");
            eprintln!("skipping: running with privileges that ignore chmod 000");
            return;
        }

        let walked = walk(&opts(root));

        // Restore before any assertion can unwind, or TempDir::drop can't clean up.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("restore");

        assert!(
            walked
                .skipped
                .iter()
                .any(|s| s.path == locked && s.reason == SkipReason::PermissionDenied),
            "unreadable dir should be skipped with a permission reason, got {:?}",
            walked.skipped
        );

        let files = file_paths(&walked);
        assert!(
            files.iter().any(|p| p.ends_with("visible.bin")),
            "the walk must keep going past the unreadable dir"
        );
    }

    /// A directory can be listable (`r`) yet deny `stat` on its entries (no `x`),
    /// so `read_dir` succeeds but each `metadata()` fails. That drives the
    /// per-entry skip path, distinct from the whole-directory failure above.
    #[cfg(unix)]
    #[test]
    fn per_entry_metadata_error_is_recorded_as_skip() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let noexec = root.join("noexec");
        fs::create_dir(&noexec).expect("mkdir");
        write(&noexec.join("f.bin"), 10);
        // r-- : the name lists, but lstat of an entry needs the directory's x bit.
        fs::set_permissions(&noexec, fs::Permissions::from_mode(0o444)).expect("chmod");

        // As with the chmod-000 test, privileges that ignore the missing bit
        // would pass this for the wrong reason — bail loudly instead.
        if fs::symlink_metadata(noexec.join("f.bin")).is_ok() {
            fs::set_permissions(&noexec, fs::Permissions::from_mode(0o755)).expect("restore");
            eprintln!("skipping: privileges ignore the missing x bit");
            return;
        }

        let walked = walk(&opts(root));
        fs::set_permissions(&noexec, fs::Permissions::from_mode(0o755)).expect("restore");

        assert!(
            walked
                .skipped
                .iter()
                .any(|s| s.reason == SkipReason::PermissionDenied && s.path.ends_with("f.bin")),
            "a per-entry metadata failure must land in skipped, got {:?}",
            walked.skipped
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_not_followed_by_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("target")).expect("mkdir");
        write(&root.join("target/inside.bin"), 10);
        fs::create_dir(root.join("here")).expect("mkdir");
        std::os::unix::fs::symlink(root.join("target"), root.join("here/link")).expect("symlink");

        let walked = walk(&opts(root));

        assert!(
            walked
                .entries
                .iter()
                .any(|e| e.path == root.join("here/link")),
            "the symlink itself is recorded as an entry"
        );
        assert!(
            !walked
                .entries
                .iter()
                .any(|e| e.path.starts_with(root.join("here/link"))
                    && e.path != root.join("here/link")),
            "nothing may be reached *through* the symlink, got {:?}",
            walked.entries.iter().map(|e| &e.path).collect::<Vec<_>>()
        );
        // The real directory is still walked by its own path.
        assert!(
            walked
                .entries
                .iter()
                .any(|e| e.path == root.join("target/inside.bin"))
        );
    }

    #[test]
    fn nonexistent_root_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope");

        let walked = walk(&opts(&missing));

        assert!(walked.entries.is_empty());
        assert_eq!(walked.skipped.len(), 1);
        assert_eq!(walked.skipped[0].path, missing);
        assert_eq!(walked.skipped[0].reason, SkipReason::NotFound);
    }

    #[test]
    fn error_kind_maps_to_skip_reason() {
        use std::io::{Error, ErrorKind};

        assert_eq!(
            skip_reason(&Error::from(ErrorKind::PermissionDenied)),
            SkipReason::PermissionDenied
        );
        assert_eq!(
            skip_reason(&Error::from(ErrorKind::NotFound)),
            SkipReason::NotFound
        );
        match skip_reason(&Error::other("disk on fire")) {
            SkipReason::Other(msg) => assert!(msg.contains("disk on fire")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    /// Needs a real mount point, so it can't run in CI. Run by hand:
    /// `cargo test -p disk-tools-core -- --ignored`
    #[cfg(unix)]
    #[test]
    #[ignore = "needs a real mount point"]
    fn one_file_system_stops_at_boundary() {
        // /Volumes on macOS, /mnt or /media on Linux: pick a root that has a
        // different filesystem mounted underneath it.
        let root = Path::new("/Volumes");
        let options = ScanOptions {
            root: root.to_path_buf(),
            one_file_system: true,
            ..ScanOptions::default()
        };

        let walked = walk(&options);

        let root_dev = device_of(&fs::symlink_metadata(root).expect("stat /Volumes"));
        for entry in &walked.entries {
            if let Ok(meta) = fs::symlink_metadata(&entry.path) {
                assert_eq!(
                    device_of(&meta),
                    root_dev,
                    "{} is on another filesystem — traversal crossed the boundary",
                    entry.path.display()
                );
            }
        }
    }
}
