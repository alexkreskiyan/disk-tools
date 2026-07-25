//! Parallel directory traversal.
//!
//! Produces a *flat* list of entries rather than a tree: hardlink attribution
//! (Task 4) groups by identity across the whole scan, so it has to settle before
//! anything gets summed. The tree is built afterwards, from these entries.

use crate::ScanOptions;
use crate::size;
use crate::tree::{SkipReason, SkippedEntry};
use rayon::prelude::*;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// What identifies a file on disk, so two paths pointing at the same bytes can
/// be recognised as one. Consumed by the dedup pass.
///
/// `(st_dev, st_ino)` on Unix; `(volume serial, file id)` on Windows, where both
/// halves come from the directory handle in [`crate::windows_dir`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FileId {
    pub device: u64,
    pub inode: u64,
}

/// What a directory listing can say about one entry that `Metadata` cannot.
///
/// Windows-only in practice: `Metadata` there carries neither the allocated size
/// nor a file identity, and both are available for a whole directory from one
/// handle. Unix needs none of this — `st_blocks` and `(dev, ino)` are already in
/// the `Metadata` the walk has in hand.
///
/// `modified` rides along because it is in the same struct the listing already
/// fills — free where the other two are the point. There is deliberately **no**
/// link count here: `FILE_ID_BOTH_DIR_INFO` carries none, and obtaining one
/// would mean a handle per file, the very cost this type exists to avoid.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EntryFacts {
    pub allocated: u64,
    pub apparent: u64,
    pub id: FileId,
    pub modified: Option<SystemTime>,
}

/// Everything a single directory listing revealed, keyed by file name.
type DirFacts = HashMap<OsString, EntryFacts>;

/// Read the per-entry facts for `dir` in one pass.
///
/// Empty on Unix: the walk already holds richer `Metadata` per entry, so there
/// is nothing to add and nothing to pay for (`HashMap::new` does not allocate).
#[cfg(windows)]
fn dir_facts(dir: &Path) -> DirFacts {
    crate::windows_dir::facts(dir)
}

#[cfg(not(windows))]
fn dir_facts(_dir: &Path) -> DirFacts {
    DirFacts::new()
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
    /// mtime, `None` when the platform gave none.
    pub modified: Option<SystemTime>,
    /// How many names this inode has in total. `None` for directories and on
    /// every platform that cannot say cheaply — see [`link_count`].
    pub links: Option<u32>,
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

    // The root has no containing listing to consult, so it takes the per-file
    // path; it is one entry, and a directory's own size is a single block.
    let mut walked = match entry_for(root, &metadata, None) {
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

    // One directory handle answers, for every entry at once, what `Metadata`
    // can't say on Windows: the allocated size and the file identity. Empty on
    // Unix, where the per-entry metadata already carries both.
    let facts = dir_facts(dir);

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

        // A mount point's inode belongs to the mounted filesystem, so a
        // differing device means we've reached the boundary: don't record it and
        // don't descend, the way `du -x` behaves.
        if file_type.is_dir()
            && options.one_file_system
            && crosses_filesystem(&metadata, root_device)
        {
            continue;
        }

        match entry_for(&path, &metadata, facts.get(&entry.file_name())) {
            Ok(entry) => {
                // Descend only into a directory we could actually record. If its
                // own measurement fails it becomes the single skip below;
                // recursing anyway would orphan the measured subtree in
                // aggregation — its parent would be missing from the tree — and
                // silently drop those bytes.
                if entry.is_dir {
                    subdirs.push(path);
                }
                walked.entries.push(entry);
            }
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

/// Build the entry, preferring what the directory listing already told us.
///
/// `facts` is `Some` only on Windows, and only for entries the listing covered —
/// a file created between the listing and this call falls back to the per-file
/// path, which is also the only path Unix ever takes.
fn entry_for(
    path: &Path,
    metadata: &Metadata,
    facts: Option<&EntryFacts>,
) -> io::Result<WalkEntry> {
    let is_dir = metadata.is_dir();
    let (allocated, apparent, id, modified) = match facts {
        // The listing already answered all four, so nothing here calls the OS
        // again — the reason `modified` is collected in this task rather than a
        // later one.
        Some(facts) => (
            facts.allocated,
            facts.apparent,
            Some(facts.id),
            facts.modified,
        ),
        None => {
            let sizes = size::measure(path, metadata)?;
            (
                sizes.allocated,
                sizes.apparent,
                file_id(metadata),
                metadata.modified().ok(),
            )
        }
    };

    Ok(WalkEntry {
        path: path.to_path_buf(),
        allocated,
        apparent,
        is_dir,
        // Directories can't be hardlinked, so an identity would never be used.
        id: if is_dir { None } else { id },
        modified,
        // A directory's `nlink` counts its subdirectories, not names for it —
        // a different quantity entirely, and one that would make "this content
        // is shared" fire on every directory in the tree.
        links: if is_dir { None } else { link_count(metadata) },
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

/// `Metadata` alone cannot identify a file off Unix, and the reason is
/// structural: `file_index` is unstable, and std hardcodes it to `None` unless
/// the metadata came from an open handle, which a directory listing never is.
/// The only way through `Metadata` would be an `open()` per file — precisely the
/// syscall this walk is built to avoid.
///
/// This is why Windows takes its identity from the directory handle instead
/// ([`crate::windows_dir`]); the path through here is the fallback for an entry
/// the listing missed, and such an entry simply goes undeduplicated.
#[cfg(not(unix))]
fn file_id(_metadata: &Metadata) -> Option<FileId> {
    None
}

/// `st_nlink` is in the metadata the walk already fetched — free.
///
/// `try_from` rather than `as`: a count that does not fit is nonsense we would
/// rather report as "unknown" than as a wrapped-around number that later reads
/// as a hardlink group.
#[cfg(unix)]
fn link_count(metadata: &Metadata) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;

    u32::try_from(metadata.nlink()).ok()
}

/// Windows has no cheap link count, so callers get `None` and must not read
/// that as "one".
///
/// `Metadata::number_of_links` is `None` for the same reason `file_index` is —
/// it needs an open handle. Nor can [`crate::windows_dir`] help:
/// `FILE_ID_BOTH_DIR_INFO` carries no link count at all, so unlike the allocated
/// size and the file id there is nothing to lift out of the directory listing.
///
/// The consequence is bounded rather than total. `ScanTree::link_groups` still
/// shows content shared *within* a scan on both platforms; only sharing with
/// something outside the scanned tree is invisible here.
#[cfg(not(unix))]
fn link_count(_metadata: &Metadata) -> Option<u32> {
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

    /// The directory listing supplies a real `AllocationSize`, so a file whose
    /// length is not a whole number of clusters must report **more** than it
    /// holds. This is precisely what `GetCompressedFileSize` could not see: it
    /// returns the logical length for an uncompressed file, which is why
    /// `size::allocated` — still the fallback — reports 5,000 here.
    ///
    /// 5,000 bytes is deliberately past the ~1 KiB MFT record: a file small
    /// enough to be *resident* has no cluster of its own and NTFS reports
    /// `AllocationSize` 0 for it, which is honest but useless as a signal.
    #[cfg(windows)]
    #[test]
    fn windows_allocated_comes_from_the_directory_listing() {
        const LOGICAL: usize = 5000;
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("chunky.bin"), LOGICAL);

        let walked = walk(&opts(dir.path()));
        let file = walked
            .entries
            .iter()
            .find(|e| e.path.ends_with("chunky.bin"))
            .expect("the file was walked");

        assert_eq!(file.apparent, LOGICAL as u64);
        assert!(
            file.allocated > file.apparent,
            "a non-resident {LOGICAL}-byte file occupies whole clusters, so more \
             than its length — got allocated={}",
            file.allocated
        );
        assert_eq!(
            file.allocated % 512,
            0,
            "AllocationSize is a multiple of the cluster size, got {}",
            file.allocated
        );
    }

    /// Windows entries now carry an identity — `(volume serial, file id)` from
    /// the same listing — so the dedup pass can recognise two names for one
    /// file. Before this, `file_id` returned `None` there and hardlinks were
    /// counted once per link.
    #[cfg(windows)]
    #[test]
    fn windows_hardlinks_share_an_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = dir.path().join("original.bin");
        write(&original, 5000);
        let link = dir.path().join("link.bin");
        if fs::hard_link(&original, &link).is_err() {
            eprintln!("skipping: this filesystem does not support hard links");
            return;
        }

        let walked = walk(&opts(dir.path()));
        let ids: Vec<Option<FileId>> = walked
            .entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.id)
            .collect();

        assert_eq!(ids.len(), 2, "both names are walked");
        assert!(ids[0].is_some(), "Windows entries now carry an identity");
        assert_eq!(ids[0], ids[1], "two links to one file share an identity");
    }

    /// The mtime the walk reports must be the file's own, not "roughly now" —
    /// compared against an independent `symlink_metadata` of the same path, so a
    /// bug that stamped every entry with the scan time would fail here.
    #[test]
    fn modified_matches_the_files_mtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("f.bin");
        write(&path, 10);
        let expected = fs::symlink_metadata(&path)
            .expect("stat")
            .modified()
            .expect("this platform records mtime");

        let walked = walk(&opts(dir.path()));
        let file = walked
            .entries
            .iter()
            .find(|e| e.path == path)
            .expect("the file was walked");

        assert_eq!(file.modified, Some(expected));
    }

    /// Do two readings of one path's mtime describe the same instant?
    ///
    /// Exact on Unix. On Windows a **directory's** timestamp is written back to
    /// its parent's listing lazily, so a value read from that listing can trail
    /// a direct `stat` of the same directory by milliseconds — the same field
    /// seen at two removes, not a discrepancy in what we record. Files are
    /// unaffected: their entry is flushed when the handle closes, which is why
    /// [`modified_matches_the_files_mtime`] stays exact.
    fn same_instant(scanned: SystemTime, stated: SystemTime) -> bool {
        if !cfg!(windows) {
            return scanned == stated;
        }
        let drift = scanned
            .duration_since(stated)
            .or_else(|_| stated.duration_since(scanned))
            .expect("one of the two orderings holds");
        drift < std::time::Duration::from_secs(1)
    }

    /// A directory carries its **own** mtime, never its children's — the signal
    /// the age rule is built on (a directory's mtime moves when its entries
    /// change, which is exactly the "still in use" evidence wanted).
    #[test]
    fn a_directory_carries_its_own_mtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).expect("mkdir");
        write(&sub.join("inner.bin"), 10);
        let expected = fs::symlink_metadata(&sub)
            .expect("stat")
            .modified()
            .expect("this platform records mtime");

        let walked = walk(&opts(dir.path()));
        let node = walked
            .entries
            .iter()
            .find(|e| e.path == sub)
            .expect("the directory was walked");
        let modified = node
            .modified
            .expect("the platform records a directory mtime");

        assert!(
            same_instant(modified, expected),
            "the directory's own mtime, got {modified:?} against {expected:?}"
        );
    }

    /// No filesystem this runs on will hand back an entry without a timestamp,
    /// so the branch is driven directly rather than faked with a fixture that
    /// cannot exist. The rule it protects: an unknown mtime is `None`, never a
    /// substituted "now" that would make the age rule judge a file it knows
    /// nothing about.
    #[test]
    fn missing_timestamp_yields_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("f.bin");
        write(&path, 10);
        let metadata = fs::symlink_metadata(&path).expect("stat");

        let facts = EntryFacts {
            allocated: 4096,
            apparent: 10,
            id: FileId {
                device: 1,
                inode: 2,
            },
            modified: None,
        };

        let entry = entry_for(&path, &metadata, Some(&facts)).expect("build entry");

        assert_eq!(
            entry.modified, None,
            "an entry the OS gave no timestamp for must stay unknown"
        );
    }

    /// `links` is the count of names for the inode, so a hardlinked file reports
    /// 2 from **both** of its paths.
    #[cfg(unix)]
    #[test]
    fn hardlinked_file_reports_two_links() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = dir.path().join("original.bin");
        let link = dir.path().join("link.bin");
        write(&original, 4096);
        fs::hard_link(&original, &link).expect("hard_link");
        let lone = dir.path().join("lone.bin");
        write(&lone, 4096);

        let walked = walk(&opts(dir.path()));
        let links_of = |path: &Path| {
            walked
                .entries
                .iter()
                .find(|e| e.path == path)
                .unwrap_or_else(|| panic!("entry for {path:?}"))
                .links
        };

        assert_eq!(links_of(&original), Some(2));
        assert_eq!(links_of(&link), Some(2));
        // A file with one name must not be swept up by the same signal.
        assert_eq!(links_of(&lone), Some(1));
    }

    /// Directories are excluded deliberately: `st_nlink` on a directory counts
    /// its subdirectories, which has nothing to do with shared content and would
    /// make every directory in a tree look hardlinked.
    #[cfg(unix)]
    #[test]
    fn directories_report_no_link_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("sub/deeper")).expect("mkdir");

        let walked = walk(&opts(dir.path()));

        for entry in walked.entries.iter().filter(|e| e.is_dir) {
            assert_eq!(
                entry.links, None,
                "{:?} is a directory and must carry no link count",
                entry.path
            );
        }
    }

    /// The documented Windows gap (D10): no link count, because getting one
    /// would cost a handle per file. This pins it as a decision rather than
    /// letting a future change quietly reintroduce that cost.
    #[cfg(windows)]
    #[test]
    fn windows_reports_no_link_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = dir.path().join("original.bin");
        write(&original, 5000);
        if fs::hard_link(&original, dir.path().join("link.bin")).is_err() {
            eprintln!("skipping: this filesystem does not support hard links");
            return;
        }

        let walked = walk(&opts(dir.path()));

        for entry in &walked.entries {
            assert_eq!(
                entry.links, None,
                "{:?}: Windows must not report a link count",
                entry.path
            );
        }
    }

    /// On Windows the timestamp comes out of the directory listing's
    /// `LastWriteTime`, not from a second call per file. Checked against the
    /// same value `Metadata` reports, to a one-second tolerance: both describe
    /// the same instant, but they travel through different conversions.
    #[cfg(windows)]
    #[test]
    fn windows_modified_comes_from_the_listing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("f.bin");
        write(&path, 5000);
        let expected = fs::symlink_metadata(&path)
            .expect("stat")
            .modified()
            .expect("mtime");

        let walked = walk(&opts(dir.path()));
        let file = walked
            .entries
            .iter()
            .find(|e| e.path == path)
            .expect("the file was walked");
        let modified = file.modified.expect("the listing carries LastWriteTime");

        assert!(
            same_instant(modified, expected),
            "listing mtime {modified:?} and metadata mtime {expected:?} must agree"
        );
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
