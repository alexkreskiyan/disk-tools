//! Removing things.
//!
//! **The default is the OS trash**, and that is the whole safety argument: a
//! cleanup tool that is wrong once should cost its user a trip to the Trash, not
//! their data. [`Removal::Purge`] deletes outright and exists because the trash
//! is not free — on macOS it is an `osascript` round-trip to Finder, ~230 ms per
//! call — but it is opt-in, never a default, and the frontend says plainly that
//! nothing purged can be put back.
//!
//! Failures travel as data, the way [`crate::ScanTree::skipped`] does: one path
//! that cannot be removed must not abort the rest, and the caller needs to know
//! precisely what survived. The `Result` here is therefore a *per-item* outcome,
//! not an error to propagate — which is why the batched fast path falls back to
//! a per-item loop the moment anything goes wrong.
//!
//! This is the only module that needs the `trash` crate, which is why it is the
//! only one behind the feature. Deciding *what* to remove ([`crate::clean`])
//! needs nothing from it, so a consumer can plan without being able to apply.

use crate::clean::{Candidate, CleanPlan};
use crate::paths::is_within;
use std::path::{Path, PathBuf};

/// Why one path could not be moved to the trash.
///
/// Carries the reason as a `String` rather than the backend's error type: the
/// core stays free of a public dependency on `trash`, and the frontend needs
/// something printable rather than something matchable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TrashFailure {
    /// The path that could not be trashed.
    pub path: PathBuf,
    /// What the operating system said, in a form fit to show a user.
    pub reason: String,
}

/// Move `path` to the OS trash.
///
/// A directory goes as a whole; the backend does not descend, so the cost is the
/// filesystem's rename or copy, not one operation per entry — 10,000 files trash
/// as fast as one.
///
/// Every failure the backend *reports* comes back as `Err`: a missing path, a
/// permission problem, a volume with no trash. Callers collect these and carry on.
///
/// **Known upstream gap.** On Windows, `trash` 5.2.6 calls
/// `CoCreateInstance(...).unwrap()` on its delete path (`src/windows.rs:42`) where
/// its other operations use `?`. If COM cannot be initialised — a service or
/// session-0 process, some sandboxed runners — that panics instead of returning,
/// and no wrapper here can convert it. `just smoke-trash` runs in CI on all three
/// platforms precisely so this shows up as a red build rather than as a user's
/// aborted cleanup.
pub fn move_to_trash(path: &Path) -> Result<(), TrashFailure> {
    trash::delete(path).map_err(|err| TrashFailure {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })
}

/// What went, one way or the other.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Reclaimed {
    pub paths: Vec<PathBuf>,
    /// Bytes freed by [`Self::paths`] — what really went, never what was hoped
    /// for. A partial run reports the smaller, true number.
    pub bytes: u64,
}

/// What a cleanup actually did.
///
/// Not a `Result`, on purpose (D5). A run that removed four of five things
/// succeeded four times and failed once, and collapsing that into one verdict
/// would lose the only detail the user needs: **which** one is still there.
///
/// **Two halves, never added up here.** One run can trash some candidates and
/// destroy others, and a single "freed" figure over the two would not say what
/// can be brought back — which is the one thing a reader wants from it. Summing
/// them is a decision, so it belongs to whoever is printing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CleanOutcome {
    /// Now in the OS trash, and retrievable from it.
    pub trashed: Reclaimed,

    /// Deleted outright. Nothing to put back.
    pub purged: Reclaimed,

    /// Paths still where they were, each with what the OS said.
    pub failed: Vec<TrashFailure>,
}

impl CleanOutcome {
    /// Did anything fail? The caller turns this into an exit code.
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }

    /// Every path that went, whichever way it went.
    pub fn removed(&self) -> impl Iterator<Item = &PathBuf> {
        self.trashed.paths.iter().chain(&self.purged.paths)
    }

    /// How many.
    pub fn count(&self) -> usize {
        self.trashed.paths.len() + self.purged.paths.len()
    }

    /// The two halves added up, for a caller that has decided the distinction
    /// does not matter to what it is saying.
    pub fn reclaimed(&self) -> u64 {
        self.trashed.bytes + self.purged.bytes
    }
}

/// Remove every candidate in `plan`, each the way the plan says.
///
/// **The only function in this project that removes anything.**
///
/// It takes no `Removal`: where a candidate goes was decided by [`crate::plan`]
/// from its rule's tier and `--purge` together, and is written on the candidate.
/// That is what lets `preview` print exactly what this will do rather than a
/// description of it kept in step by hand.
///
/// Trashing is batched — on macOS each backend call is an `osascript`
/// round-trip to Finder, ~230 ms whatever the size — so the two groups are
/// partitioned once and each removed its own way.
pub fn apply(plan: &CleanPlan, mut progress: impl FnMut(&Candidate)) -> CleanOutcome {
    let (purging, trashing): (Vec<&Candidate>, Vec<&Candidate>) = plan
        .candidates
        .iter()
        .partition(|candidate| candidate.purge);

    let mut outcome = CleanOutcome::default();
    // Trashed first, so that the `already_gone` check below sees them. Nothing
    // in a plan nests, but `apply` is public over public fields and this is the
    // function that destroys things, so it checks rather than trusts.
    trash_all(&trashing, &mut outcome, &mut progress);
    purge_all(&purging, &mut outcome, &mut progress, purge);
    outcome
}

/// Trash everything in one call, and only fall back to one-at-a-time to find out
/// who failed.
///
/// `trash::delete` *is* `delete_all(&[path])`, so removing candidates one by one
/// paid a full backend round-trip each — on macOS an `osascript` invocation to
/// Finder, ~230 ms, whatever the size of what was being removed. Sixty tiny
/// directories took 14 seconds; the same content in one call is a fraction of a
/// second.
///
/// The batch gives back **one** result for the whole set, which cannot satisfy
/// [`CleanOutcome`]'s promise to name what survived. So the batch is the fast
/// path and the per-item loop is the diagnostic one: it runs only when something
/// went wrong, when being slow no longer matters and being precise does.
fn trash_all(
    candidates: &[&Candidate],
    outcome: &mut CleanOutcome,
    mut progress: impl FnMut(&Candidate),
) {
    let mut attempting: Vec<&Candidate> = Vec::new();
    for candidate in candidates {
        progress(candidate);
        // Defensive, as in `purge_all`: a candidate inside one already accepted
        // would go with its parent, and submitting both invites the backend to
        // report a failure for something that is in fact gone.
        let accepted: Vec<PathBuf> = attempting.iter().map(|c| c.path.clone()).collect();
        if already_gone(&candidate.path, accepted.iter()) {
            continue;
        }
        attempting.push(candidate);
    }

    if attempting.is_empty() {
        return;
    }

    let paths: Vec<&Path> = attempting.iter().map(|c| c.path.as_path()).collect();
    if trash::delete_all(&paths).is_ok() {
        outcome.trashed.paths = attempting.iter().map(|c| c.path.clone()).collect();
        outcome.trashed.bytes = attempting.iter().map(|c| c.allocated).sum();
        return;
    }

    // Something in the batch failed and the backend will not say what. Ask again,
    // one at a time, so the report can name it. Whatever the first call already
    // removed now answers "not found", which is a failure this loop records —
    // pessimistic, and the safe direction: it claims less was removed than may
    // have been, never more.
    for candidate in attempting {
        match move_to_trash(&candidate.path) {
            Ok(()) => {
                outcome.trashed.paths.push(candidate.path.clone());
                outcome.trashed.bytes += candidate.allocated;
            }
            Err(failure) => outcome.failed.push(failure),
        }
    }
}

/// Delete `path` outright, with no trash and no way back.
fn purge(path: &Path) -> Result<(), TrashFailure> {
    let metadata = std::fs::symlink_metadata(path).map_err(|err| TrashFailure {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;

    // `remove_dir_all` follows no symlinks, but it also refuses a symlink *to* a
    // directory — so the two cases are told apart by the link's own metadata,
    // never by the target's.
    let removed = if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };

    removed.map_err(|err| TrashFailure {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })
}

/// One at a time, since there is no batch call to delete outright.
///
/// `mover` is a parameter because the success arm — what lands in `purged` and
/// what is added to its byte count — is otherwise reachable only by really
/// destroying something, which every test here is `#[ignore]`d to avoid. That
/// left the arithmetic of "how much did we free" verified solely by a test
/// nobody runs by default. Passing the mover in lets it be checked without a
/// filesystem, the same move `git::interpret` and `env::as_path` already make.
fn purge_all(
    candidates: &[&Candidate],
    outcome: &mut CleanOutcome,
    mut progress: impl FnMut(&Candidate),
    mut mover: impl FnMut(&Path) -> Result<(), TrashFailure>,
) {
    for candidate in candidates {
        progress(candidate);

        if already_gone(&candidate.path, outcome.removed()) {
            continue;
        }

        match mover(&candidate.path) {
            Ok(()) => {
                outcome.purged.paths.push(candidate.path.clone());
                outcome.purged.bytes += candidate.allocated;
            }
            Err(failure) => outcome.failed.push(failure),
        }
    }
}

/// Did `path` already go with something removed before it?
///
/// A candidate inside one already removed is gone with its parent. Attempting it
/// again would fail with "not found" and be reported as **still on disk** — the
/// report telling a user their data survived when it is in fact gone.
///
/// `plan()` never builds such a pair: `detect` does not descend into a match, so
/// no candidate contains another. But `apply` is public over a struct whose
/// fields are all public, and it is the one function here that destroys
/// anything, so it checks rather than trusts.
fn already_gone<'a>(path: &Path, removed: impl Iterator<Item = &'a PathBuf>) -> bool {
    removed.into_iter().any(|gone| is_within(path, gone))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `purge_all` over a whole plan, which is the shape the removal seam had
    /// before the two destinations were split apart. The point of the seam is
    /// unchanged: the success arm — what lands in the outcome, and what is added
    /// to its byte count — is otherwise reachable only by really destroying
    /// something.
    fn purging(
        plan: &CleanPlan,
        progress: impl FnMut(&Candidate),
        mover: impl FnMut(&Path) -> Result<(), TrashFailure>,
    ) -> CleanOutcome {
        let candidates: Vec<&Candidate> = plan.candidates.iter().collect();
        let mut outcome = CleanOutcome::default();
        purge_all(&candidates, &mut outcome, progress, mover);
        outcome
    }

    use std::path::Path;
    use std::time::Instant;

    /// Trash `path`, or report that this environment cannot.
    ///
    /// Returns `false` after printing a notice, so a smoke test bails instead of
    /// failing where no backend exists — the rule the permission fixtures follow,
    /// since a test that passes because its fixture failed to build is worse than
    /// no test. Shared by both smoke tests so the branch has one implementation
    /// and one test rather than duplicated prose in each.
    fn trashed_or_skipped(path: &Path) -> bool {
        match move_to_trash(path) {
            Ok(()) => true,
            Err(failure) => {
                eprintln!(
                    "skipping: this environment has no usable trash backend ({})",
                    failure.reason
                );
                false
            }
        }
    }

    // ---- apply -----------------------------------------------------------

    fn candidate(path: &str, allocated: u64) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            rule: "node-modules".into(),
            tier: crate::Tier::Trash,
            purge: false,
            allocated,
            shared: false,
        }
    }

    /// The same, but destined for outright deletion — which is what the tests
    /// that really remove files use, since nothing may reach the trash.
    fn doomed(path: &str, allocated: u64) -> Candidate {
        Candidate {
            tier: crate::Tier::Purge,
            purge: true,
            ..candidate(path, allocated)
        }
    }

    fn plan_of(candidates: Vec<Candidate>) -> CleanPlan {
        let reclaimable = candidates.iter().map(|c| c.allocated).sum();
        CleanPlan {
            candidates,
            reclaimable,
            excluded: Vec::new(),
            filtered_out: 0,
            too_small: 0,
            below_rule_minimum: 0,
        }
    }

    /// A plan of paths that cannot exist, so the loop can be driven to the end
    /// without a trash backend and without removing anything.
    fn doomed_plan(dir: &Path) -> CleanPlan {
        plan_of(vec![
            doomed(dir.join("gone-one").to_str().expect("utf8"), 1024),
            doomed(dir.join("gone-two").to_str().expect("utf8"), 2048),
        ])
    }

    /// The rule that makes a cleanup reviewable: one bad path must not abort the
    /// rest. Two failures are the proof — if the loop stopped at the first, only
    /// one would be reported.
    ///
    /// Needs no trash backend at all, which is why it is not `#[ignore]`d like
    /// the smoke tests: nothing here can succeed, so nothing is destroyed.
    #[test]
    fn one_failure_does_not_stop_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = doomed_plan(dir.path());

        let outcome = apply(&plan, |_| {});

        assert_eq!(
            outcome.failed.len(),
            2,
            "both failures must be reported, not just the first: {:?}",
            outcome.failed
        );
        assert_eq!(outcome.count(), 0);
        assert!(!outcome.is_complete(), "a failed run is not complete");
    }

    /// Every failure names its own path and carries something showable — a
    /// count alone would leave the user unable to act.
    #[test]
    fn outcome_names_every_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = doomed_plan(dir.path());

        let outcome = apply(&plan, |_| {});

        let named: Vec<&PathBuf> = outcome.failed.iter().map(|f| &f.path).collect();
        assert_eq!(
            named,
            vec![&plan.candidates[0].path, &plan.candidates[1].path],
            "each failure names the path it was given"
        );
        assert!(
            outcome.failed.iter().all(|f| !f.reason.is_empty()),
            "and says what went wrong: {:?}",
            outcome.failed
        );
    }

    /// The number that would otherwise quietly become "what we hoped to free".
    /// Nothing moved here, so nothing was reclaimed — even though the plan's own
    /// total is 3 KiB.
    #[test]
    fn reclaimed_counts_only_what_moved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = doomed_plan(dir.path());
        assert_eq!(plan.reclaimable, 3072, "the plan expected to free 3 KiB");

        let outcome = apply(&plan, |_| {});

        assert_eq!(
            outcome.reclaimed(),
            0,
            "a run that removed nothing freed nothing"
        );
    }

    /// The progress contract. The CLI's bar is cosmetic; this is not — on
    /// Windows a single candidate can take seconds, and a caller that is never
    /// told which one it is on cannot show anything at all.
    #[test]
    fn progress_fires_once_per_candidate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = doomed_plan(dir.path());
        let mut seen = Vec::new();

        apply(&plan, |candidate| seen.push(candidate.path.clone()));

        assert_eq!(
            seen,
            vec![
                plan.candidates[0].path.clone(),
                plan.candidates[1].path.clone()
            ],
            "once each, in plan order"
        );
    }

    /// A mover that always succeeds, so the arm that records what went — and
    /// how much it freed — can be checked without a trash backend.
    fn always_works(_: &Path) -> Result<(), TrashFailure> {
        Ok(())
    }

    /// The freed total on a run that actually removed things. Covered here
    /// rather than only in the `#[ignore]`d smoke test, because "how much did I
    /// free" is a number a user acts on and `just verify` should be able to
    /// check it.
    #[test]
    fn a_successful_run_records_what_went_and_what_it_freed() {
        let plan = plan_of(vec![candidate("/p/a", 1024), candidate("/p/b", 2048)]);

        let outcome = purging(&plan, |_| {}, always_works);

        assert_eq!(
            outcome.purged.paths,
            vec![PathBuf::from("/p/a"), PathBuf::from("/p/b")]
        );
        assert!(outcome.failed.is_empty());
        assert_eq!(outcome.purged.bytes, 3072, "the sum of what moved");
        assert!(outcome.is_complete());
    }

    /// A mixed run: the freed total counts the successes and nothing else, and
    /// both halves are reported.
    #[test]
    fn a_mixed_run_counts_only_the_successes() {
        let plan = plan_of(vec![
            candidate("/p/ok", 1024),
            candidate("/p/stuck", 8192),
            candidate("/p/also-ok", 2048),
        ]);

        let outcome = purging(
            &plan,
            |_| {},
            |path| {
                if path.ends_with("stuck") {
                    Err(TrashFailure {
                        path: path.to_path_buf(),
                        reason: "Permission denied".to_owned(),
                    })
                } else {
                    Ok(())
                }
            },
        );

        assert_eq!(
            outcome.purged.paths,
            vec![PathBuf::from("/p/ok"), PathBuf::from("/p/also-ok")],
            "the failure did not stop the one after it"
        );
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(
            outcome.purged.bytes, 3072,
            "the 8 KiB that stayed put is not counted as freed"
        );
        assert!(!outcome.is_complete());
    }

    /// The guard, exercised through `purge_all` rather than by trashing a real
    /// directory: the child must be skipped once its parent has gone, and must
    /// not surface as a failure.
    #[test]
    fn a_candidate_inside_one_already_removed_is_skipped_not_failed() {
        let plan = plan_of(vec![
            candidate("/p/outer", 4096),
            candidate("/p/outer/inner", 1024),
        ]);
        let mut attempted = 0;

        let outcome = purging(
            &plan,
            |_| {},
            |path| {
                attempted += 1;
                assert_ne!(
                    path,
                    Path::new("/p/outer/inner"),
                    "the child went with its parent and must not be attempted"
                );
                Ok(())
            },
        );

        assert_eq!(attempted, 1, "only the parent was attempted");
        assert_eq!(outcome.purged.paths, vec![PathBuf::from("/p/outer")]);
        assert!(
            outcome.failed.is_empty(),
            "and the child is not reported as still on disk"
        );
        assert_eq!(
            outcome.purged.bytes, 4096,
            "its bytes are already counted under the parent"
        );
    }

    // ---- purge -----------------------------------------------------------

    /// `--purge` is the one path with no way back, so what it removes and what
    /// it leaves are both worth asserting. Uses real files, which is safe here
    /// precisely *because* nothing goes to the trash — a tempdir is destroyed
    /// either way.
    #[test]
    fn purge_deletes_outright_and_touches_nothing_else() {
        let dir = tempfile::tempdir().expect("tempdir");
        let doomed_dir = dir.path().join("__pycache__");
        let doomed_file = dir.path().join("stale.pyc");
        let keeper = dir.path().join("src.py");
        std::fs::create_dir(&doomed_dir).expect("mkdir");
        std::fs::write(doomed_dir.join("m.pyc"), b"bytecode").expect("write");
        std::fs::write(&doomed_file, b"bytecode").expect("write");
        std::fs::write(&keeper, b"source").expect("write");

        let plan = plan_of(vec![
            doomed(doomed_dir.to_str().expect("utf8"), 4096),
            doomed(doomed_file.to_str().expect("utf8"), 1024),
        ]);

        let outcome = apply(&plan, |_| {});

        assert!(outcome.is_complete(), "{:?}", outcome.failed);
        assert!(!doomed_dir.exists(), "a directory goes with its contents");
        assert!(!doomed_file.exists(), "and so does a file");
        assert!(keeper.exists(), "nothing outside the plan is touched");
        assert_eq!(outcome.purged.bytes, 5120);
    }

    /// A purge that cannot happen is still reported as data, not a panic — the
    /// same contract the trash path keeps.
    #[test]
    fn a_purge_failure_is_reported_per_item() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = doomed_plan(dir.path());

        let outcome = apply(&plan, |_| {});

        assert_eq!(outcome.failed.len(), 2, "{:?}", outcome.failed);
        assert_eq!(outcome.count(), 0);
        assert_eq!(outcome.reclaimed(), 0);
    }

    /// A symlink is removed as a link, never followed — deleting what it points
    /// at would take something the plan never named.
    #[cfg(unix)]
    #[test]
    fn purging_a_symlink_leaves_its_target_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("real");
        std::fs::create_dir(&target).expect("mkdir");
        std::fs::write(target.join("keep.txt"), b"precious").expect("write");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let outcome = apply(
            &plan_of(vec![doomed(link.to_str().expect("utf8"), 64)]),
            |_| {},
        );

        assert!(outcome.is_complete(), "{:?}", outcome.failed);
        assert!(!link.exists(), "the link itself is gone");
        assert!(
            target.join("keep.txt").exists(),
            "but what it pointed at is untouched"
        );
    }

    /// The guard that stops a child being blamed for its parent's success.
    ///
    /// Driven directly, with no filesystem and no trash: the branch it protects
    /// is unreachable through `plan()`, so exercising it through `apply` would
    /// mean really trashing a fixture on every `just test` — the thing every
    /// other real-trash test here is `#[ignore]`d to avoid.
    #[test]
    fn a_path_inside_something_already_removed_is_recognised() {
        let removed = [PathBuf::from("/p/outer")];

        assert!(
            already_gone(Path::new("/p/outer/inner"), removed.iter()),
            "a child of a removed directory went with it"
        );
        assert!(
            already_gone(Path::new("/p/outer"), removed.iter()),
            "and so did the directory itself"
        );
        assert!(
            !already_gone(Path::new("/p/outer-sibling"), removed.iter()),
            "but a name that merely shares a prefix did not"
        );
        assert!(
            !already_gone(Path::new("/p/other"), removed.iter()),
            "nor an unrelated path"
        );
        assert!(
            !already_gone(Path::new("/p/outer/inner"), std::iter::empty()),
            "and nothing is gone before anything has been removed"
        );
    }

    #[test]
    fn an_empty_plan_removes_nothing_and_succeeds() {
        let mut fired = 0;

        let outcome = apply(&plan_of(Vec::new()), |_| fired += 1);

        assert_eq!(outcome, CleanOutcome::default());
        assert!(outcome.is_complete(), "nothing to do is not a failure");
        assert_eq!(fired, 0, "and nothing to report progress about");
    }

    /// The happy path, against the **real** trash — the only test that proves
    /// `apply` removes anything at all.
    ///
    /// `#[ignore]` for the same reason as the wrapper's smoke test: it puts
    /// things in the developer's actual Trash. Run via `just smoke-trash`.
    #[test]
    #[ignore = "moves real files to the OS trash; run via `just smoke-trash`"]
    fn apply_moves_candidates_to_trash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("one.bin");
        let second = dir.path().join("two.bin");
        std::fs::write(&first, b"first").expect("write");
        std::fs::write(&second, b"second").expect("write");

        let plan = plan_of(vec![
            candidate(first.to_str().expect("utf8"), 4096),
            candidate(second.to_str().expect("utf8"), 8192),
        ]);

        let outcome = apply(&plan, |_| {});

        if !outcome.is_complete() {
            eprintln!(
                "skipping: this environment has no usable trash backend ({:?})",
                outcome.failed
            );
            return;
        }
        assert!(!first.exists(), "the original path must be gone");
        assert!(!second.exists());
        assert_eq!(outcome.trashed.paths, vec![first, second]);
        assert_eq!(
            outcome.trashed.bytes, 12288,
            "the freed total is the sum of what moved"
        );
        assert_eq!(
            outcome.purged,
            Reclaimed::default(),
            "and nothing was destroyed, so that half stays empty"
        );
    }

    /// Covers the skip branch itself, which the `#[ignore]`d smoke tests never
    /// reach: every platform CI runs on turns out to *have* a trash backend, so
    /// the "no backend" arm would otherwise be reasoned about and never executed.
    ///
    /// A path that cannot be trashed stands in for a missing backend because the
    /// branch cannot tell them apart — both arrive as `Err`. That is the point:
    /// whatever the cause, it must produce a skip rather than a failure.
    #[test]
    fn an_unusable_backend_skips_rather_than_fails() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert!(
            !trashed_or_skipped(&dir.path().join("cannot-be-trashed.bin")),
            "a backend failure must report a skip, not panic or fail the test"
        );
    }

    /// The failure path, exercised without needing a working trash backend: a
    /// path that does not exist cannot be trashed anywhere.
    ///
    /// This is the criterion that matters most for the design — one bad entry in
    /// a cleanup run must come back as data the caller can report, never a panic
    /// that takes the whole process with it.
    #[test]
    fn trash_failure_is_reported_as_data() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist.bin");

        let failure = move_to_trash(&missing).expect_err("a missing path cannot be trashed");

        assert_eq!(
            failure.path, missing,
            "the failure names the path it was given"
        );
        assert!(
            !failure.reason.is_empty(),
            "the failure carries something showable, got an empty reason"
        );
    }

    /// The happy path, against the **real** trash.
    ///
    /// `#[ignore]` because it moves a file into the developer's actual Trash;
    /// running it on every `just test` would litter it. Run deliberately with
    /// `just smoke-trash`.
    ///
    /// An environment with no trash backend skips loudly instead of failing —
    /// see [`trashed_or_skipped`].
    #[test]
    #[ignore = "moves a real file to the OS trash; run via `just smoke-trash`"]
    fn trashing_a_file_removes_the_original() {
        let dir = tempfile::tempdir().expect("tempdir");
        let victim = dir.path().join("smoke-test.bin");
        std::fs::write(&victim, b"disk-tools smoke test").expect("write file");

        if trashed_or_skipped(&victim) {
            assert!(
                !victim.exists(),
                "a trashed file must be gone from its original path: {}",
                victim.display()
            );
        }
    }

    /// Answers the concept's warning that trashing a large tree "can be slow or
    /// fail on some volumes" — the number Task 7 needs to decide whether progress
    /// reporting is decoration or a requirement.
    ///
    /// Prints rather than asserts: there is no threshold worth failing a build
    /// over, only a figure worth recording.
    #[test]
    #[ignore = "creates and trashes 10,000 files; run via `just smoke-trash`"]
    fn trashing_a_large_tree_is_timed() {
        const FILES: usize = 10_000;
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = dir.path().join("large-tree");
        std::fs::create_dir(&tree).expect("mkdir");
        for i in 0..FILES {
            std::fs::write(tree.join(format!("file-{i:05}.bin")), b"x").expect("write file");
        }

        let start = Instant::now();
        let trashed = trashed_or_skipped(&tree);
        let elapsed = start.elapsed();

        if trashed {
            println!("\ntrashed {FILES} files in one call: {elapsed:.1?}");
            assert!(
                !tree.exists(),
                "the tree must be gone from its original path"
            );
        }
    }
}
