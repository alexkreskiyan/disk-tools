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

use crate::rules::{Facts, Rules, State};
use crate::size::allocated_size;
use rayon::prelude::*;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

/// How many directory entries pass between two reads of the cancel flag.
///
/// Small enough that a huge directory stays interruptible, large enough that the
/// atomic load is lost in the noise of a `stat` per entry.
const CANCEL_EVERY: usize = 512;

/// What the rules make of what is being counted.
///
/// The bytes and the claim come out of **one** walk because they are the same
/// walk: every entry is already being stated, and the alternative is a second
/// pass over the same tree to answer a question the first pass had all the facts
/// for.
pub struct Claim<'a> {
    pub rules: &'a Rules,
    /// Supplied, never read from a clock — this crate consults none.
    pub now: SystemTime,
    /// `root` already lies inside something a rule claims.
    ///
    /// Then everything beneath it goes with it, no pattern below needs testing,
    /// and its reclaimable figure is its whole size. Without this a walk of a
    /// `node_modules` would report nothing reclaimable, because nothing *inside*
    /// a `node_modules` matches `**/node_modules/`.
    pub claimed: bool,
}

/// A total, and whether it is the whole answer.
///
/// `complete` is the point of the type: a cancelled walk still has a number, and
/// presenting that number as a size would be a lie that grows with the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    pub allocated: u64,
    /// Of those bytes, what the rules claim — what `clean` would offer to
    /// remove from this subtree.
    pub reclaimable: u64,
    pub complete: bool,
}

/// One directory, done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finished<'a> {
    /// The directory whose subtree has just been added up.
    pub path: &'a Path,
    /// What that subtree comes to. Final — nothing will be added to it.
    pub allocated: u64,
    /// What the rules claim within it. Final in the same way.
    pub reclaimable: u64,
    /// Bytes counted anywhere in this walk so far, for a figure that climbs.
    pub running_total: u64,
}

/// Total allocated bytes under `root` and what the rules claim of them,
/// stoppable through `cancel`.
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
    claim: &Claim<'_>,
    cancel: &AtomicBool,
    finished: &(dyn Fn(Finished<'_>) + Sync),
) -> Measured {
    let running = AtomicU64::new(0);
    let walked = walk(root, claim.claimed, claim, cancel, &running, finished);

    Measured {
        allocated: walked.allocated,
        reclaimable: walked.reclaimable,
        complete: !walked.stopped,
    }
}

/// One subtree's contribution.
struct Walked {
    allocated: u64,
    reclaimable: u64,
    stopped: bool,
}

fn walk(
    dir: &Path,
    claimed: bool,
    claim: &Claim<'_>,
    cancel: &AtomicBool,
    running: &AtomicU64,
    finished: &(dyn Fn(Finished<'_>) + Sync),
) -> Walked {
    // Checked on entry to every directory, which is what makes a cancelled walk
    // stop rather than merely be ignored.
    if cancel.load(Ordering::Relaxed) {
        return Walked {
            allocated: 0,
            reclaimable: 0,
            stopped: true,
        };
    }

    // One comparison per directory in place of a glob match per entry beneath
    // it. Inside something already claimed there is nothing left to decide, and
    // outside every rule's root there is nothing that could be decided.
    let testing = !claimed && !claim.rules.prunes(dir);

    let Some(level) = level(dir, testing.then_some(claim), cancel) else {
        // An unreadable directory is a zero, not a cancellation: the answer is
        // complete, it just does not include what could not be read.
        finished(Finished {
            path: dir,
            allocated: 0,
            reclaimable: 0,
            running_total: running.load(Ordering::Relaxed),
        });
        return Walked {
            allocated: 0,
            reclaimable: 0,
            stopped: false,
        };
    };

    // One add per directory rather than one per file: the running figure exists
    // to move a number on screen, not to count.
    let mut allocated = level.allocated;
    let mut reclaimable = level.reclaimable;
    if level.allocated > 0 {
        running.fetch_add(level.allocated, Ordering::Relaxed);
    }

    // Directory-level parallelism, as in `walk.rs` — and through the same global
    // pool. A pool per measurement would pile up threads with every directory
    // the user walks into.
    let nested: Vec<Walked> = level
        .subdirs
        .into_par_iter()
        .map(|(subdir, sub_claimed)| {
            walk(
                &subdir,
                claimed || sub_claimed,
                claim,
                cancel,
                running,
                finished,
            )
        })
        .collect();

    // A directory whose own listing was cut short is as incomplete as one whose
    // children were: its figure is partial either way, and caching a partial
    // figure as a total is the one thing this must never do.
    let mut stopped = level.stopped;
    for child in nested {
        allocated += child.allocated;
        reclaimable += child.reclaimable;
        stopped |= child.stopped;
    }

    // Everything inside something a rule claims goes with it, whatever the
    // patterns happen to say about the individual entries.
    if claimed {
        reclaimable = allocated;
    }

    // Only a subtree that finished has a total. One cut short must not be
    // reported, or a partial figure would be cached as final.
    if !stopped {
        finished(Finished {
            path: dir,
            allocated,
            reclaimable,
            running_total: running.load(Ordering::Relaxed),
        });
    }

    Walked {
        allocated,
        reclaimable,
        stopped,
    }
}

/// What one directory holds directly, and where the walk goes next.
struct Level {
    /// Bytes in the files immediately here.
    allocated: u64,
    /// Of those, what the rules claim.
    reclaimable: u64,
    /// Each subdirectory, and whether a rule claims it outright.
    subdirs: Vec<(PathBuf, bool)>,
    /// The listing was cut short by the cancel flag.
    stopped: bool,
}

/// One directory's own contents, or `None` when it cannot be read.
///
/// `claim` is `Some` only where a rule could reach: asking otherwise would cost
/// a glob match per entry for an answer that is already known to be no.
///
/// The cancel flag is read as it goes: a directory of a hundred thousand files
/// is a hundred thousand `stat` calls, and stopping only at the end of that is
/// not stopping.
fn level(dir: &Path, claim: Option<&Claim<'_>>, cancel: &AtomicBool) -> Option<Level> {
    let listing = std::fs::read_dir(dir).ok()?;

    // The whole listing is read before any rule is asked, because
    // `requires_sibling` is a question about the listing rather than about the
    // entry — a `target/` is build output only while the `Cargo.toml` beside it
    // is there, and the entry after it may be that manifest.
    let mut names: Vec<OsString> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut allocated = 0u64;

    for (seen, entry) in listing.flatten().enumerate() {
        if seen % CANCEL_EVERY == 0 && cancel.load(Ordering::Relaxed) {
            return Some(Level {
                allocated,
                reclaimable: 0,
                subdirs: Vec::new(),
                stopped: true,
            });
        }

        // `file_type` arrives with the `readdir` result; asking metadata for the
        // same fact would be an extra stat per entry.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let is_dir = file_type.is_dir();

        // A file is stated for its bytes; a directory only when a rule might ask
        // how old it is.
        let metadata = if is_dir && claim.is_none() {
            None
        } else {
            entry.metadata().ok()
        };
        let bytes = match &metadata {
            Some(metadata) if !is_dir => allocated_size(&path, metadata).unwrap_or(0),
            _ => 0,
        };
        allocated += bytes;

        if claim.is_some() {
            names.push(entry.file_name());
        }
        rows.push(Row {
            path,
            is_dir,
            bytes,
            modified: metadata.and_then(|metadata| metadata.modified().ok()),
        });
    }

    let Some(claim) = claim else {
        return Some(Level {
            allocated,
            reclaimable: 0,
            subdirs: rows
                .into_iter()
                .filter(|row| row.is_dir)
                .map(|row| (row.path, false))
                .collect(),
            stopped: false,
        });
    };

    let any_sibling =
        |wanted: &dyn Fn(&OsStr) -> bool| names.iter().any(|beside| wanted(beside.as_os_str()));
    let mut reclaimable = 0u64;
    let mut subdirs = Vec::new();
    for row in rows {
        // The same question `detect` asks, answered the same way — so a figure
        // on screen and a candidate in a plan cannot disagree.
        let claimed = claim.rules.state(
            &row.path,
            &Facts {
                is_dir: row.is_dir,
                modified: row.modified,
                now: claim.now,
                any_sibling: &any_sibling,
            },
        ) == State::Included;

        if row.is_dir {
            subdirs.push((row.path, claimed));
        } else if claimed {
            reclaimable += row.bytes;
        }
    }

    Some(Level {
        allocated,
        reclaimable,
        subdirs,
        stopped: false,
    })
}

/// One entry, held until the whole listing is known.
struct Row {
    path: PathBuf,
    is_dir: bool,
    bytes: u64,
    modified: Option<SystemTime>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{Part, Rule, UserDirs};
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

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_750_000_000)
    }

    /// No rule reaches anywhere: the bytes are the whole question.
    fn blind() -> Rules {
        Rules::default()
    }

    fn claim(rules: &Rules) -> Claim<'_> {
        Claim {
            rules,
            now: now(),
            claimed: false,
        }
    }

    /// Collect what the walk reports, so a test can ask what it was told.
    #[derive(Default)]
    struct Reported {
        directories: Mutex<Vec<(PathBuf, u64, u64)>>,
    }

    impl Reported {
        fn record(&self) -> impl Fn(Finished<'_>) + Sync {
            move |done: Finished<'_>| {
                self.directories.lock().expect("no panics").push((
                    done.path.to_path_buf(),
                    done.allocated,
                    done.reclaimable,
                ));
            }
        }

        fn total_for(&self, suffix: &str) -> Option<u64> {
            self.directories
                .lock()
                .expect("no panics")
                .iter()
                .find(|(path, ..)| path.ends_with(suffix))
                .map(|(_, bytes, _)| *bytes)
        }

        fn claimed_for(&self, suffix: &str) -> Option<u64> {
            self.directories
                .lock()
                .expect("no panics")
                .iter()
                .find(|(path, ..)| path.ends_with(suffix))
                .map(|(.., claimed)| *claimed)
        }

        fn count(&self) -> usize {
            self.directories.lock().expect("no panics").len()
        }
    }

    fn quietly(root: &Path, cancel: &AtomicBool) -> Measured {
        observed(root, cancel, &|_| {})
    }

    /// A walk no rule reaches, which is what every test about bytes wants.
    fn observed(
        root: &Path,
        cancel: &AtomicBool,
        finished: &(dyn Fn(Finished<'_>) + Sync),
    ) -> Measured {
        let rules = blind();
        measure(root, &claim(&rules), cancel, finished)
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

        let measured = observed(dir.path(), &never(), &reported.record());

        assert_eq!(
            measured,
            Measured {
                allocated: 0,
                reclaimable: 0,
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

        let measured = observed(dir.path(), &AtomicBool::new(true), &reported.record());

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
        let measured = observed(dir.path(), &cancel, &|_| {
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
        let measured = observed(dir.path(), &cancel, &|done| {
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
                .any(|(path, ..)| path == dir.path()),
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
        let measured = observed(dir.path(), &never(), &|done| {
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

        let measured = observed(dir.path(), &never(), &reported.record());

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
                .find(|(path, ..)| path == dir.path())
                .map(|(_, bytes, _)| *bytes),
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
        let measured = observed(dir.path(), &never(), &reported.record());
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
        observed(dir.path(), &never(), &reported.record());

        let order = reported.directories.lock().expect("no panics");
        let at = |suffix: &str| {
            order
                .iter()
                .position(|(path, ..)| path.ends_with(suffix))
                .unwrap_or_else(|| panic!("{suffix} missing from {order:?}"))
        };

        assert!(at("a/b/c") < at("a/b"));
        assert!(at("a/b") < at("a"));
        // By path, not by `ends_with("")` — that is true of every path, and an
        // assertion written with it passes without checking anything.
        assert_eq!(
            order.iter().position(|(path, ..)| path == dir.path()),
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
        let measured = observed(dir.path(), &never(), &reported.record());
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
        let measured = observed(dir.path(), &never(), &reported.record());

        assert_eq!(measured.allocated, 0);
        assert_eq!(reported.total_for("empty"), Some(0));
    }

    // ---- what the rules claim --------------------------------------------

    /// A rule set, and a tree with one thing in it that the rule claims.
    fn junk() -> (tempfile::TempDir, Rules) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("keep.bin"), vec![b'x'; 4096]).expect("write");
        let modules = dir.path().join("app/node_modules");
        std::fs::create_dir_all(modules.join("left")).expect("mkdir");
        std::fs::write(modules.join("left/f.bin"), vec![b'x'; 8192]).expect("write");
        std::fs::write(modules.join("g.bin"), vec![b'x'; 8192]).expect("write");

        let rules = Rules::new(
            vec![Rule {
                name: "node-modules".into(),
                parts: vec![Part {
                    includes: vec!["**/node_modules/".into()],
                    ..Part::default()
                }],
                ..Rule::default()
            }],
            &UserDirs::default(),
        )
        .expect("compiles");
        (dir, rules)
    }

    /// The whole point of counting the claim during the walk: the row for a
    /// directory says how much of it `clean` would take.
    #[test]
    fn what_a_rule_claims_is_counted_as_the_walk_goes() {
        let (dir, rules) = junk();
        let reported = Reported::default();

        let measured = measure(dir.path(), &claim(&rules), &never(), &reported.record());

        assert!(measured.allocated >= 4096 + 16384);
        assert_eq!(
            measured.reclaimable,
            reported.total_for("node_modules").expect("reported"),
            "exactly the claimed subtree, and none of `keep.bin`"
        );
        assert!(measured.reclaimable >= 16384);
        assert!(
            measured.reclaimable < measured.allocated,
            "the file beside it is not junk"
        );
    }

    /// Inside a claim there is nothing left to decide, so the figure is the
    /// whole subtree — including the parts no pattern would match on their own.
    #[test]
    fn everything_inside_a_claim_goes_with_it() {
        let (dir, rules) = junk();
        let reported = Reported::default();

        measure(dir.path(), &claim(&rules), &never(), &reported.record());

        for inside in ["node_modules", "node_modules/left"] {
            assert_eq!(
                reported.claimed_for(inside),
                reported.total_for(inside),
                "{inside} is claimed in full"
            );
        }
    }

    /// A walk started *at* something claimed — the browser sizing a row it has
    /// already coloured as junk. Nothing inside a `node_modules` matches
    /// `**/node_modules/`, so without being told, this would report zero.
    #[test]
    fn a_walk_that_starts_inside_a_claim_reports_all_of_it() {
        let (dir, rules) = junk();
        let modules = dir.path().join("app/node_modules");

        let measured = measure(
            &modules,
            &Claim {
                rules: &rules,
                now: now(),
                claimed: true,
            },
            &never(),
            &|_| {},
        );

        assert_eq!(measured.reclaimable, measured.allocated);
        assert!(measured.allocated >= 16384);
    }

    /// A claim nested inside another must not be added twice — the figure is
    /// what removing the outer one frees, not what removing both would.
    #[test]
    fn a_claim_inside_a_claim_is_counted_once() {
        let (dir, rules) = junk();
        let inner = dir.path().join("app/node_modules/left/node_modules");
        std::fs::create_dir_all(&inner).expect("mkdir");
        std::fs::write(inner.join("f.bin"), vec![b'x'; 8192]).expect("write");
        let reported = Reported::default();

        let measured = measure(dir.path(), &claim(&rules), &never(), &reported.record());

        assert_eq!(
            measured.reclaimable,
            reported.total_for("app/node_modules").expect("reported"),
            "the outer claim, once"
        );
    }

    /// A rule that reaches a file claims that file and nothing around it.
    #[test]
    fn a_claimed_file_counts_only_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("stale.pyc"), vec![b'x'; 4096]).expect("write");
        std::fs::write(dir.path().join("live.py"), vec![b'x'; 4096]).expect("write");
        let rules = Rules::new(
            vec![Rule {
                name: "pycache".into(),
                parts: vec![Part {
                    includes: vec!["**/*.pyc".into()],
                    ..Part::default()
                }],
                ..Rule::default()
            }],
            &UserDirs::default(),
        )
        .expect("compiles");

        let measured = measure(dir.path(), &claim(&rules), &never(), &|_| {});

        assert!(measured.reclaimable >= 4096);
        assert!(measured.reclaimable < measured.allocated);
    }

    /// No rules is not a special case, and it is what the pruning path takes.
    #[test]
    fn with_no_rules_nothing_is_reclaimable() {
        let (dir, _) = junk();

        let measured = quietly(dir.path(), &never());

        assert_eq!(measured.reclaimable, 0);
        assert!(measured.allocated > 0);
    }

    /// A listing the flag cut short is partial, and a partial figure must never
    /// be reported as a directory's total — it would be cached as one.
    #[test]
    fn a_listing_cut_short_says_so_rather_than_looking_finished() {
        let dir = fixture();

        let level = level(dir.path(), None, &AtomicBool::new(true)).expect("readable");

        assert!(level.stopped);
        assert!(level.subdirs.is_empty());
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
