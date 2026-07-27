//! What a subtree occupies, and what every directory inside it occupies.
//!
//! [`scan`](crate::scan) answers "what is big" and returns a whole
//! [`ScanTree`](crate::ScanTree) to do it: hardlink attribution, apparent sizes,
//! the entries it had to skip. The browser asks a smaller question — *how many
//! bytes* — and would throw all of that away. So this walks the same way and
//! keeps only the numbers.
//!
//! Three things make it usable from a screen rather than from a command:
//!
//! **It can be stopped, promptly.** The flag is read on entering every
//! directory *and* every few hundred entries within one, because a directory
//! with a hundred thousand files is a long time to be uninterruptible. A
//! cancellation that only discards the result would leave the work running,
//! competing for the same rayon pool as whatever replaced it — which is what a
//! user produces by walking quickly through a deep tree.
//!
//! **It reports as it goes**, so a figure can rise on screen instead of
//! appearing at the end. The callback runs on pool threads and may be called
//! concurrently.
//!
//! **It hands back everything it measured**, not just the one total it was asked
//! for. Walking a directory visits every directory beneath it, and throwing
//! those subtotals away means computing them again the moment the user steps
//! inside — the walk that just finished had the answer and dropped it. The cost
//! is a `PathBuf` and a `u64` per directory, which is the order
//! [`scan`](crate::scan) already pays for its tree.
//!
//! Unreadable entries contribute nothing and are not reported. `scan` returns
//! them as [`SkippedEntry`](crate::SkippedEntry) because its report has a place
//! to put them; a size does not.

use crate::size::allocated_size;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// How many directory entries pass between two reads of the cancel flag.
///
/// Small enough that a huge directory stays interruptible, large enough that the
/// atomic load is lost in the noise of a `stat` per entry.
const CANCEL_EVERY: usize = 512;

/// A total, what it was made of, and whether it is the whole answer.
///
/// `complete` is the point of the type: a cancelled walk still has a number, and
/// presenting that number as a size would be a lie that grows with the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measured {
    pub allocated: u64,
    pub complete: bool,

    /// Every directory walked, with its own subtree total — the root included.
    ///
    /// Empty when the walk was cancelled: a subtree that did not finish has no
    /// total, and neither has anything above it.
    pub directories: Vec<(PathBuf, u64)>,
}

/// Total allocated bytes under `root`, stoppable through `cancel`.
///
/// `progress` is called with the running total as it climbs, from whichever pool
/// thread got there — throttling, if any is wanted, belongs to the caller, since
/// this crate reads no clock.
///
/// Symlinks are not followed: a link's own metadata is what counts, exactly as
/// in [`scan`](crate::scan), or a loop would never terminate.
pub fn measure(root: &Path, cancel: &AtomicBool, progress: &(dyn Fn(u64) + Sync)) -> Measured {
    let running = AtomicU64::new(0);
    let walked = walk(root, cancel, &running, progress);

    Measured {
        allocated: walked.allocated,
        complete: !walked.stopped,
        // A partial breakdown would be a set of directories claiming totals they
        // do not have.
        directories: if walked.stopped {
            Vec::new()
        } else {
            walked.directories
        },
    }
}

/// One subtree's contribution.
struct Walked {
    allocated: u64,
    stopped: bool,
    directories: Vec<(PathBuf, u64)>,
}

fn walk(
    dir: &Path,
    cancel: &AtomicBool,
    running: &AtomicU64,
    progress: &(dyn Fn(u64) + Sync),
) -> Walked {
    let stopped = Walked {
        allocated: 0,
        stopped: true,
        directories: Vec::new(),
    };

    // Checked on entry to every directory, which is what makes a cancelled walk
    // stop rather than merely be ignored.
    if cancel.load(Ordering::Relaxed) {
        return stopped;
    }

    let Some((here, subdirs)) = level(dir, cancel) else {
        return Walked {
            allocated: 0,
            // An unreadable directory is a zero, not a cancellation: the answer
            // is complete, it just does not include what could not be read.
            stopped: false,
            directories: vec![(dir.to_path_buf(), 0)],
        };
    };
    if cancel.load(Ordering::Relaxed) {
        return stopped;
    }

    if here > 0 {
        // One add per directory rather than one per file: the callback exists to
        // move a number on screen, not to count.
        progress(running.fetch_add(here, Ordering::Relaxed) + here);
    }

    // Directory-level parallelism, as in `walk.rs` — and through the same global
    // pool. A pool per measurement would pile up threads with every directory
    // the user walks into.
    let nested: Vec<Walked> = subdirs
        .into_par_iter()
        .map(|subdir| walk(&subdir, cancel, running, progress))
        .collect();

    let mut walked = Walked {
        allocated: here,
        stopped: false,
        directories: Vec::new(),
    };
    for child in nested {
        walked.allocated += child.allocated;
        walked.stopped |= child.stopped;
        walked.directories.extend(child.directories);
    }
    walked
        .directories
        .push((dir.to_path_buf(), walked.allocated));
    walked
}

/// The files' bytes and the subdirectories of one directory, or `None` when it
/// cannot be read.
///
/// The cancel flag is read as it goes: a directory of a hundred thousand files
/// is a hundred thousand `stat` calls, and stopping only at the end of that is
/// not stopping.
fn level(dir: &Path, cancel: &AtomicBool) -> Option<(u64, Vec<PathBuf>)> {
    let listing = std::fs::read_dir(dir).ok()?;

    let mut here = 0u64;
    let mut subdirs = Vec::new();

    for (seen, entry) in listing.flatten().enumerate() {
        if seen % CANCEL_EVERY == 0 && cancel.load(Ordering::Relaxed) {
            return Some((here, Vec::new()));
        }

        // `file_type` arrives with the `readdir` result; asking metadata for the
        // same fact would be an extra stat per entry.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();

        if file_type.is_dir() {
            subdirs.push(path);
            continue;
        }
        if let Ok(metadata) = entry.metadata()
            && let Ok(bytes) = allocated_size(&path, &metadata)
        {
            here += bytes;
        }
    }

    Some((here, subdirs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Two levels, so cancellation has somewhere to happen and recursion has
    /// something to recurse into.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("top.bin"), vec![b'x'; 4096]).expect("write");
        std::fs::create_dir(dir.path().join("nested")).expect("mkdir");
        std::fs::write(dir.path().join("nested/inner.bin"), vec![b'x'; 8192]).expect("write");
        dir
    }

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn a_total_covers_the_whole_subtree() {
        let dir = fixture();

        let measured = measure(dir.path(), &never(), &|_| {});

        assert!(measured.complete);
        assert!(
            measured.allocated >= 4096 + 8192,
            "{} is less than the files it contains",
            measured.allocated
        );
    }

    #[test]
    fn an_empty_directory_measures_zero_and_is_complete() {
        let dir = tempfile::tempdir().expect("tempdir");

        let measured = measure(dir.path(), &never(), &|_| {});

        assert_eq!(measured.allocated, 0);
        assert!(measured.complete);
        assert_eq!(
            measured.directories,
            vec![(dir.path().to_path_buf(), 0)],
            "the root is measured even when it is empty"
        );
    }

    /// A directory that is not there is a zero, not a panic: the browser lists
    /// what `read_dir` said a moment ago, and it can be gone by now.
    #[test]
    fn a_vanished_directory_is_zero_rather_than_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");

        let measured = measure(&dir.path().join("gone"), &never(), &|_| {});

        assert_eq!(measured.allocated, 0);
        assert!(measured.complete, "nothing was cancelled");
    }

    /// The flag is honoured at the top, which is the cheapest proof that it is
    /// read at all.
    #[test]
    fn a_flag_already_set_stops_before_anything_is_counted() {
        let dir = fixture();
        let cancel = AtomicBool::new(true);

        let measured = measure(dir.path(), &cancel, &|_| {});

        assert_eq!(measured.allocated, 0);
        assert!(
            !measured.complete,
            "and it says the number is not the answer"
        );
    }

    /// Cancelling part-way returns what was counted, marked as partial. The
    /// number is not useless — it is just not a size.
    #[test]
    fn cancelling_part_way_returns_a_partial_total() {
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 0..40 {
            let sub = dir.path().join(format!("sub{n}"));
            std::fs::create_dir(&sub).expect("mkdir");
            std::fs::write(sub.join("f.bin"), vec![b'x'; 4096]).expect("write");
        }

        let cancel = AtomicBool::new(false);
        // Stop as soon as anything has been counted at all.
        let measured = measure(dir.path(), &cancel, &|_| {
            cancel.store(true, Ordering::Relaxed);
        });

        assert!(!measured.complete, "a stopped walk is never complete");
        assert!(
            measured.allocated < 40 * 4096,
            "it stopped, so it cannot have counted everything: {}",
            measured.allocated
        );
        assert!(
            measured.directories.is_empty(),
            "and no directory beneath it may claim a total either"
        );
    }

    /// The figure has to climb, or there is nothing to show while it works.
    #[test]
    fn progress_reports_a_rising_total() {
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 0..8 {
            let sub = dir.path().join(format!("sub{n}"));
            std::fs::create_dir(&sub).expect("mkdir");
            std::fs::write(sub.join("f.bin"), vec![b'x'; 4096]).expect("write");
        }

        let calls = AtomicUsize::new(0);
        let highest = AtomicU64::new(0);
        let measured = measure(dir.path(), &never(), &|running| {
            calls.fetch_add(1, Ordering::Relaxed);
            highest.fetch_max(running, Ordering::Relaxed);
        });

        assert!(calls.load(Ordering::Relaxed) >= 8, "one per directory");
        assert_eq!(
            highest.load(Ordering::Relaxed),
            measured.allocated,
            "the last figure reported is the one returned"
        );
    }

    /// An empty directory has nothing to report, and a callback firing with an
    /// unchanged total would make the screen flicker for no reason.
    #[test]
    fn nothing_to_count_reports_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("empty")).expect("mkdir");

        let calls = AtomicUsize::new(0);
        measure(dir.path(), &never(), &|_| {
            calls.fetch_add(1, Ordering::Relaxed);
        });

        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    /// The walk visits every directory beneath the root, so it may as well say
    /// what each of them came to — recomputing that the moment the user steps
    /// inside is the same work a second time.
    #[test]
    fn every_directory_walked_reports_its_own_total() {
        let dir = fixture();

        let measured = measure(dir.path(), &never(), &|_| {});

        let nested = measured
            .directories
            .iter()
            .find(|(path, _)| path.ends_with("nested"))
            .expect("the subdirectory is in the breakdown");
        assert!(nested.1 >= 8192, "with its own subtree total: {}", nested.1);

        let root = measured
            .directories
            .iter()
            .find(|(path, _)| path == dir.path())
            .expect("and so is the root");
        assert_eq!(root.1, measured.allocated);
    }

    /// Nesting is what makes this worth having: an inner total has to be the sum
    /// of what is under it, not of what sits beside it.
    #[test]
    fn totals_nest() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("a/b")).expect("mkdir");
        std::fs::write(dir.path().join("a/mid.bin"), vec![b'x'; 4096]).expect("write");
        std::fs::write(dir.path().join("a/b/deep.bin"), vec![b'x'; 8192]).expect("write");

        let measured = measure(dir.path(), &never(), &|_| {});
        let total = |suffix: &str| {
            measured
                .directories
                .iter()
                .find(|(path, _)| path.ends_with(suffix))
                .unwrap_or_else(|| panic!("{suffix} missing from {:?}", measured.directories))
                .1
        };

        assert!(total("a/b") >= 8192);
        assert!(total("a") >= total("a/b") + 4096);
        assert_eq!(total("a"), measured.allocated, "and the root is the whole");
    }

    /// A directory that cannot be read is a zero rather than a hole: the answer
    /// is complete, it just does not include what could not be seen.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_subdirectory_is_zero_and_does_not_stop_the_walk() {
        use std::os::unix::fs::PermissionsExt;

        let dir = fixture();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("mkdir");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
                .expect("restore");
            eprintln!("skipping: privileges ignore the locked directory");
            return;
        }

        let measured = measure(dir.path(), &never(), &|_| {});
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");

        assert!(measured.complete);
        assert!(
            measured.allocated >= 4096 + 8192,
            "the readable part is still counted"
        );
        assert!(
            measured
                .directories
                .iter()
                .any(|(path, bytes)| path.ends_with("locked") && *bytes == 0)
        );
    }

    /// Following one would count the target twice, and a loop would never end.
    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_directory_is_not_followed() {
        let dir = fixture();
        std::os::unix::fs::symlink(dir.path().join("nested"), dir.path().join("link"))
            .expect("symlink");

        let with_link = measure(dir.path(), &never(), &|_| {});
        std::fs::remove_file(dir.path().join("link")).expect("unlink");
        let without = measure(dir.path(), &never(), &|_| {});

        // The link itself occupies something, so this is not an equality — what
        // matters is that `nested` was not counted a second time through it.
        assert!(
            with_link.allocated < without.allocated + 8192,
            "{} against {}",
            with_link.allocated,
            without.allocated
        );
    }
}
