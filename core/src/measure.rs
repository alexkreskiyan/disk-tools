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
//! **It reports every directory as that directory finishes**, not once at the
//! end. Walking a directory visits every directory beneath it, so its walk
//! already contains all of theirs — and a caller that waits for the whole thing
//! before learning any of it will start those inner walks itself and do the same
//! work twice. Reporting as it goes is what makes the outer walk *subsume* the
//! inner ones instead of racing them.
//!
//! A subtotal reported this way is final. It survives the walk above it being
//! cancelled: that subtree did finish, whatever happened to its neighbours.
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

/// A total, and whether it is the whole answer.
///
/// `complete` is the point of the type: a cancelled walk still has a number, and
/// presenting that number as a size would be a lie that grows with the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    pub allocated: u64,
    pub complete: bool,
}

/// One directory, done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finished<'a> {
    /// The directory whose subtree has just been added up.
    pub path: &'a Path,
    /// What that subtree comes to. Final — nothing will be added to it.
    pub allocated: u64,
    /// Bytes counted anywhere in this walk so far, for a figure that climbs.
    pub running_total: u64,
}

/// Total allocated bytes under `root`, stoppable through `cancel`.
///
/// `finished` is called once for every directory whose subtree has been added
/// up, including `root` itself, from whichever pool thread got there.
/// Throttling, if any is wanted, belongs to the caller, since this crate reads
/// no clock.
///
/// Symlinks are not followed: a link's own metadata is what counts, exactly as
/// in [`scan`](crate::scan), or a loop would never terminate.
pub fn measure(
    root: &Path,
    cancel: &AtomicBool,
    finished: &(dyn Fn(Finished<'_>) + Sync),
) -> Measured {
    let running = AtomicU64::new(0);
    let walked = walk(root, cancel, &running, finished);

    Measured {
        allocated: walked.allocated,
        complete: !walked.stopped,
    }
}

/// One subtree's contribution.
struct Walked {
    allocated: u64,
    stopped: bool,
}

fn walk(
    dir: &Path,
    cancel: &AtomicBool,
    running: &AtomicU64,
    finished: &(dyn Fn(Finished<'_>) + Sync),
) -> Walked {
    // Checked on entry to every directory, which is what makes a cancelled walk
    // stop rather than merely be ignored.
    if cancel.load(Ordering::Relaxed) {
        return Walked {
            allocated: 0,
            stopped: true,
        };
    }

    let Some((here, subdirs)) = level(dir, cancel) else {
        // An unreadable directory is a zero, not a cancellation: the answer is
        // complete, it just does not include what could not be read.
        finished(Finished {
            path: dir,
            allocated: 0,
            running_total: running.load(Ordering::Relaxed),
        });
        return Walked {
            allocated: 0,
            stopped: false,
        };
    };

    // One add per directory rather than one per file: the running figure exists
    // to move a number on screen, not to count.
    let mut allocated = here;
    if here > 0 {
        running.fetch_add(here, Ordering::Relaxed);
    }

    // Directory-level parallelism, as in `walk.rs` — and through the same global
    // pool. A pool per measurement would pile up threads with every directory
    // the user walks into.
    let nested: Vec<Walked> = subdirs
        .into_par_iter()
        .map(|subdir| walk(&subdir, cancel, running, finished))
        .collect();

    let mut stopped = false;
    for child in nested {
        allocated += child.allocated;
        stopped |= child.stopped;
    }

    // Only a subtree that finished has a total. One cut short must not be
    // reported, or a partial figure would be cached as final.
    if !stopped {
        finished(Finished {
            path: dir,
            allocated,
            running_total: running.load(Ordering::Relaxed),
        });
    }

    Walked { allocated, stopped }
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
    use std::sync::Mutex;
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

    /// Collect what the walk reports, so a test can ask what it was told.
    #[derive(Default)]
    struct Reported {
        directories: Mutex<Vec<(PathBuf, u64)>>,
    }

    impl Reported {
        fn record(&self) -> impl Fn(Finished<'_>) + Sync {
            move |done: Finished<'_>| {
                self.directories
                    .lock()
                    .expect("no panics")
                    .push((done.path.to_path_buf(), done.allocated));
            }
        }

        fn total_for(&self, suffix: &str) -> Option<u64> {
            self.directories
                .lock()
                .expect("no panics")
                .iter()
                .find(|(path, _)| path.ends_with(suffix))
                .map(|(_, bytes)| *bytes)
        }

        fn count(&self) -> usize {
            self.directories.lock().expect("no panics").len()
        }
    }

    fn quietly(root: &Path, cancel: &AtomicBool) -> Measured {
        measure(root, cancel, &|_| {})
    }

    #[test]
    fn a_total_covers_the_whole_subtree() {
        let dir = fixture();

        let measured = quietly(dir.path(), &never());

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
        let reported = Reported::default();

        let measured = measure(dir.path(), &never(), &reported.record());

        assert_eq!(
            measured,
            Measured {
                allocated: 0,
                complete: true
            }
        );
        assert_eq!(
            reported.count(),
            1,
            "the root is reported even when it is empty"
        );
    }

    /// A directory that is not there is a zero, not a panic: the browser lists
    /// what `read_dir` said a moment ago, and it can be gone by now.
    #[test]
    fn a_vanished_directory_is_zero_rather_than_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");

        let measured = quietly(&dir.path().join("gone"), &never());

        assert_eq!(measured.allocated, 0);
        assert!(measured.complete, "nothing was cancelled");
    }

    /// The flag is honoured at the top, which is the cheapest proof that it is
    /// read at all.
    #[test]
    fn a_flag_already_set_stops_before_anything_is_counted() {
        let dir = fixture();
        let reported = Reported::default();

        let measured = measure(dir.path(), &AtomicBool::new(true), &reported.record());

        assert_eq!(measured.allocated, 0);
        assert!(
            !measured.complete,
            "and it says the number is not the answer"
        );
        assert_eq!(reported.count(), 0, "nothing may claim a total");
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

    /// A subtree that finished is a fact about that subtree. Cutting the walk
    /// short above it does not make it less true — and throwing it away is how
    /// the browser ended up measuring the same directory twice.
    #[test]
    fn a_subtree_that_finished_keeps_its_total_when_the_walk_is_cut_short() {
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 0..40 {
            let sub = dir.path().join(format!("sub{n}"));
            std::fs::create_dir(&sub).expect("mkdir");
            std::fs::write(sub.join("f.bin"), vec![b'x'; 4096]).expect("write");
        }

        let cancel = AtomicBool::new(false);
        let reported = Reported::default();
        let record = reported.record();
        let measured = measure(dir.path(), &cancel, &|done| {
            record(done);
            cancel.store(true, Ordering::Relaxed);
        });

        assert!(!measured.complete);
        assert!(reported.count() >= 1, "what did finish was still reported");
        assert!(
            !reported
                .directories
                .lock()
                .expect("no panics")
                .iter()
                .any(|(path, _)| path == dir.path()),
            "but the root did not finish, so it is not among them"
        );
    }

    /// The figure has to climb, or there is nothing to show while it works.
    #[test]
    fn the_running_total_climbs_to_the_answer() {
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 0..8 {
            let sub = dir.path().join(format!("sub{n}"));
            std::fs::create_dir(&sub).expect("mkdir");
            std::fs::write(sub.join("f.bin"), vec![b'x'; 4096]).expect("write");
        }

        let calls = AtomicUsize::new(0);
        let highest = AtomicU64::new(0);
        let measured = measure(dir.path(), &never(), &|done| {
            calls.fetch_add(1, Ordering::Relaxed);
            highest.fetch_max(done.running_total, Ordering::Relaxed);
        });

        assert!(calls.load(Ordering::Relaxed) >= 9, "one per directory");
        assert_eq!(
            highest.load(Ordering::Relaxed),
            measured.allocated,
            "the last figure reported is the one returned"
        );
    }

    /// The walk visits every directory beneath the root and says what each came
    /// to — as it goes, so a caller watching one of them need not walk it again.
    #[test]
    fn every_directory_is_reported_with_its_own_total() {
        let dir = fixture();
        let reported = Reported::default();

        let measured = measure(dir.path(), &never(), &reported.record());

        assert!(
            reported
                .total_for("nested")
                .is_some_and(|bytes| bytes >= 8192),
            "{:?}",
            reported.directories.lock().expect("no panics")
        );
        assert_eq!(
            reported
                .directories
                .lock()
                .expect("no panics")
                .iter()
                .find(|(path, _)| path == dir.path())
                .map(|(_, bytes)| *bytes),
            Some(measured.allocated),
            "the root last, with the whole"
        );
    }

    /// Nesting is what makes this worth having: an inner total has to be the sum
    /// of what is under it, not of what sits beside it.
    #[test]
    fn totals_nest() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("a/b")).expect("mkdir");
        std::fs::write(dir.path().join("a/mid.bin"), vec![b'x'; 4096]).expect("write");
        std::fs::write(dir.path().join("a/b/deep.bin"), vec![b'x'; 8192]).expect("write");

        let reported = Reported::default();
        let measured = measure(dir.path(), &never(), &reported.record());
        let total = |suffix: &str| reported.total_for(suffix).expect(suffix);

        assert!(total("a/b") >= 8192);
        assert!(total("a") >= total("a/b") + 4096);
        assert_eq!(total("a"), measured.allocated, "and the root is the whole");
    }

    /// The order is the whole point: a caller watching `a/b` learns its total
    /// while the walk of `a` is still going, and so never starts a walk of its
    /// own. Guaranteed by construction — a directory reports after its children
    /// — rather than by being quick, so this is not a race.
    #[test]
    fn a_directory_is_reported_before_the_one_above_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("a/b/c")).expect("mkdir");
        std::fs::write(dir.path().join("a/b/c/f.bin"), vec![b'x'; 4096]).expect("write");

        let reported = Reported::default();
        measure(dir.path(), &never(), &reported.record());

        let order = reported.directories.lock().expect("no panics");
        let at = |suffix: &str| {
            order
                .iter()
                .position(|(path, _)| path.ends_with(suffix))
                .unwrap_or_else(|| panic!("{suffix} missing from {order:?}"))
        };

        assert!(at("a/b/c") < at("a/b"));
        assert!(at("a/b") < at("a"));
        // By path, not by `ends_with("")` — that is true of every path, and an
        // assertion written with it passes without checking anything.
        assert_eq!(
            order.iter().position(|(path, _)| path == dir.path()),
            Some(order.len() - 1),
            "and the root is last of all"
        );
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

        let reported = Reported::default();
        let measured = measure(dir.path(), &never(), &reported.record());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");

        assert!(measured.complete);
        assert!(
            measured.allocated >= 4096 + 8192,
            "the readable part is still counted"
        );
        assert_eq!(reported.total_for("locked"), Some(0));
    }

    /// An empty directory has nothing to add, and a running figure that did not
    /// move would make the screen flicker for no reason.
    #[test]
    fn a_directory_with_nothing_in_it_adds_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("empty")).expect("mkdir");

        let reported = Reported::default();
        let measured = measure(dir.path(), &never(), &reported.record());

        assert_eq!(measured.allocated, 0);
        assert_eq!(reported.total_for("empty"), Some(0));
    }

    /// Following one would count the target twice, and a loop would never end.
    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_directory_is_not_followed() {
        let dir = fixture();
        std::os::unix::fs::symlink(dir.path().join("nested"), dir.path().join("link"))
            .expect("symlink");

        let with_link = quietly(dir.path(), &never());
        std::fs::remove_file(dir.path().join("link")).expect("unlink");
        let without = quietly(dir.path(), &never());

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
