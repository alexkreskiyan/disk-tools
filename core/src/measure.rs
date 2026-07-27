//! What one subtree occupies, without building a tree for it.
//!
//! [`scan`](crate::scan) answers "what is big" and returns a whole
//! [`ScanTree`](crate::ScanTree) to do it. The browser asks a smaller question —
//! *how many bytes is this one directory* — for every subdirectory on screen,
//! and would throw the tree away. So this walks the same way and keeps only a
//! running total.
//!
//! Two things make it usable from a screen rather than from a command:
//!
//! **It can be stopped.** The flag is read on entering every directory, not once
//! at the top. A cancellation that only discards the result would leave the work
//! running, competing for the same rayon pool as whatever replaced it — which is
//! precisely what a user gets when they walk quickly through a deep tree.
//!
//! **It reports as it goes**, so a figure can rise on screen instead of
//! appearing at the end. The callback runs on pool threads and may be called
//! concurrently.
//!
//! Unreadable entries contribute nothing and are not reported. `scan` returns
//! them as [`SkippedEntry`](crate::SkippedEntry) because its report has a place
//! to put them; a size does not.

use crate::size::allocated_size;
use rayon::prelude::*;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// A total, and whether it is the whole answer.
///
/// `complete` is the point of the type: a cancelled walk still has a number, and
/// presenting that number as a size would be a lie that grows with the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    pub allocated: u64,
    pub complete: bool,
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
    let total = AtomicU64::new(0);
    let stopped = walk(root, cancel, &total, progress);

    Measured {
        allocated: total.load(Ordering::Relaxed),
        complete: !stopped,
    }
}

/// Returns whether it stopped early.
fn walk(
    dir: &Path,
    cancel: &AtomicBool,
    total: &AtomicU64,
    progress: &(dyn Fn(u64) + Sync),
) -> bool {
    // Checked on entry to every directory, which is what makes a cancelled walk
    // stop rather than merely be ignored.
    if cancel.load(Ordering::Relaxed) {
        return true;
    }

    let Ok(listing) = std::fs::read_dir(dir) else {
        return false;
    };

    let mut here = 0u64;
    let mut subdirs = Vec::new();

    for entry in listing.flatten() {
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

    if here > 0 {
        // One add per directory rather than one per file: the callback exists to
        // move a number on screen, not to count.
        progress(total.fetch_add(here, Ordering::Relaxed) + here);
    }

    // Directory-level parallelism, as in `walk.rs` — and through the same global
    // pool. A pool per measurement would pile up threads with every directory
    // the user walks into.
    subdirs
        .into_par_iter()
        .map(|subdir| walk(&subdir, cancel, total, progress))
        .reduce(|| false, |a, b| a || b)
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

        assert_eq!(
            measure(dir.path(), &never(), &|_| {}),
            Measured {
                allocated: 0,
                complete: true
            }
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
