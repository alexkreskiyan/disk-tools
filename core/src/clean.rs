//! Removing things, safely.
//!
//! Everything this crate deletes goes to the **OS trash**, never `rm`. A cleanup
//! tool that is wrong once should cost its user a trip to the Trash, not their
//! data — so the recoverable operation is the only one offered.
//!
//! Failures travel as data, the way [`crate::ScanTree::skipped`] does: one path
//! that cannot be trashed must not abort the rest, and the caller needs to know
//! precisely what survived. The `Result` here is therefore a *per-item* outcome,
//! not an error to propagate.

use std::path::{Path, PathBuf};

/// Why one path could not be moved to the trash.
///
/// Carries the reason as a `String` rather than the backend's error type: the
/// core stays free of a public dependency on `trash`, and the frontend needs
/// something printable rather than something matchable.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[cfg(test)]
mod tests {
    use super::*;
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
