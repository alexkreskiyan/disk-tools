//! Asking git one question: is there uncommitted work here?
//!
//! Build output is regenerable only if the source that produces it is committed.
//! So before removing a `target/`, [`crate::clean`] asks this module whether the
//! project owning it is mid-change, and declines if so.
//!
//! **By shelling out, deliberately** (D6). `gix` is large and `git2` needs
//! libgit2, which is a lot of dependency for one boolean — and this is the only
//! place in the crate that wants it.
//!
//! Everything unknown resolves to [`RepoState::Dirty`]. A missing binary, a
//! repository too broken to read, a failure nobody has thought of yet: the tool
//! does not delete what it cannot vouch for.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// What git says about a repository.
///
/// "No repository" is not a variant: [`enclosing_repo`] already says that by
/// returning `None`, and having two ways to express it invites the two to
/// disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepoState {
    /// Tracked, and everything is committed.
    Clean,
    /// Uncommitted work — or a repository we could not read, which is treated
    /// the same way on purpose.
    Dirty,
}

/// The nearest ancestor holding a `.git`, `path` itself included.
///
/// Nearest wins, which is what §8.2.3 asks for with nested repositories: a
/// submodule's own state is the one that matters for its build output, not its
/// parent's.
///
/// Three details, each of which would otherwise leave a repository unguarded:
///
/// - **The path is anchored first.** `Path::ancestors` on a relative path stops
///   at `""`, so `disk-tools clean target` run from inside a repository would
///   never reach the `.git` above the working directory. [`std::path::absolute`]
///   fixes that lexically — it consults the current directory but touches no
///   filesystem and resolves no symlink, which `canonicalize()` (banned
///   project-wide) would.
/// - **`symlink_metadata`, not `exists()`.** `exists()` follows links and
///   reports `false` for a broken one, so a `.git` symlink whose target is gone
///   would read as "no repository here" — and the walk would continue past a
///   repository that is, if anything, *less* trustworthy than usual.
/// - **Any `.git` entry counts, file or directory.** In a worktree or submodule
///   it is a file pointing elsewhere.
pub(crate) fn enclosing_repo(path: &Path) -> Option<PathBuf> {
    let anchored = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());

    anchored
        .ancestors()
        .find(|ancestor| ancestor.join(".git").symlink_metadata().is_ok())
        .map(Path::to_path_buf)
}

/// Is there uncommitted work in `repo`?
pub(crate) fn state(repo: &Path) -> RepoState {
    let output = Command::new("git")
        // These override `-C` entirely, so an inherited one — from a git hook,
        // an IDE integration, a wrapper script — would have git answer about a
        // completely different repository. A `Clean` verdict about the wrong
        // project is exactly the failure this guard exists to prevent.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        // `-C` rather than `current_dir`, so the child's working directory is
        // set by git itself and this process's own is never involved.
        .arg("-C")
        .arg(repo)
        // `--untracked-files` is spelled out because `status.showUntrackedFiles
        // = no` in the user's config would otherwise hide brand-new files —
        // uncommitted work that produces no output at all, which reads as clean.
        // `normal` restores the default rather than `all`, which would enumerate
        // every file inside an untracked directory for no extra answer.
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .output();

    interpret(output)
}

/// Turn the outcome of running git into a verdict.
///
/// Only one shape means clean: the command ran, succeeded, and printed nothing.
/// Everything else is [`RepoState::Dirty`] — and that is the whole safety
/// argument, so it is a single fall-through arm rather than a list of cases
/// somebody could forget to extend:
///
/// - **`Err`** — no `git` on `PATH`, or it could not be executed. §8.2.3 step 3.
/// - **Non-zero exit** — a corrupt repository, or one git refuses to read.
/// - **Any output at all** — `--porcelain` prints one line per change, so a
///   non-empty stdout *is* the uncommitted work.
fn interpret(result: io::Result<Output>) -> RepoState {
    match result {
        Ok(output) if output.status.success() && output.stdout.is_empty() => RepoState::Clean,
        _ => RepoState::Dirty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The state of the repository enclosing `path`, if any — the two-step the
    /// guard performs, as one call.
    fn state_for(path: &Path) -> Option<RepoState> {
        enclosing_repo(path).map(|repo| state(&repo))
    }

    /// Build a repository, or report that this environment has no git.
    ///
    /// A test that passes because its fixture failed to build is worse than no
    /// test — the rule the `chmod 000` and hardlink fixtures already follow. So
    /// **only a missing binary is a reason to skip.** Anything else — an
    /// unwritable `$HOME`, a sandbox refusing to spawn, a full disk — is an
    /// environment fault, and swallowing it would leave every git test here
    /// silently asserting nothing while still counting as a pass.
    fn init_repo(root: &Path) -> bool {
        match Command::new("git").arg("init").arg(root).output() {
            Ok(output) if output.status.success() => true,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                eprintln!("skipping: no git binary in this environment");
                false
            }
            other => panic!("git init failed, and not because git is missing: {other:?}"),
        }
    }

    /// Commit everything, so `--porcelain` has nothing left to report.
    ///
    /// Identity is passed with `-c` rather than written to the repository: the
    /// test must not depend on the developer's global git config, and must not
    /// modify it either.
    fn commit_all(root: &Path) {
        let add = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["add", "."])
            .output()
            .expect("git add");
        assert!(add.status.success(), "git add failed: {add:?}");

        let commit = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=disk-tools test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--message",
                "fixture",
            ])
            .output()
            .expect("git commit");
        assert!(commit.status.success(), "git commit failed: {commit:?}");
    }

    #[test]
    fn a_path_outside_any_repository_has_no_state() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert_eq!(state_for(dir.path()), None);
    }

    #[test]
    fn uncommitted_work_is_dirty() {
        let dir = tempfile::tempdir().expect("tempdir");
        if !init_repo(dir.path()) {
            return;
        }
        fs::write(dir.path().join("src.rs"), b"fn main() {}").expect("write");

        assert_eq!(
            state_for(&dir.path().join("target")),
            Some(RepoState::Dirty)
        );
    }

    #[test]
    fn a_fully_committed_repository_is_clean() {
        let dir = tempfile::tempdir().expect("tempdir");
        if !init_repo(dir.path()) {
            return;
        }
        fs::write(dir.path().join("src.rs"), b"fn main() {}").expect("write");
        commit_all(dir.path());

        assert_eq!(
            state_for(&dir.path().join("target")),
            Some(RepoState::Clean)
        );
    }

    /// §8.2.3's nested-repository rule. The inner repository is clean and the
    /// outer one is not, so only picking the *nearest* `.git` gives `Clean` —
    /// picking the outermost, or the first found walking down, would not.
    ///
    /// **This test is the only thing enforcing that.** Nearest-versus-outermost
    /// is a choice of iteration order, which mutation testing has no operator
    /// for; a green mutation score says nothing about it. Do not delete this on
    /// the assumption that coverage has it.
    #[test]
    fn the_nearest_repository_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outer = dir.path();
        let inner = outer.join("inner");
        fs::create_dir(&inner).expect("mkdir");
        if !init_repo(outer) || !init_repo(&inner) {
            return;
        }
        // Dirty the outer repository only.
        fs::write(outer.join("loose.txt"), b"uncommitted").expect("write");
        fs::write(inner.join("src.rs"), b"fn main() {}").expect("write");
        commit_all(&inner);

        assert_eq!(state_for(&inner.join("target")), Some(RepoState::Clean));
        assert_eq!(state_for(&outer.join("target")), Some(RepoState::Dirty));
    }

    /// A `.git` **file** — what a worktree or submodule has instead of a
    /// directory. Checking `is_dir()` would miss it and leave the build output
    /// of every submodule unguarded.
    ///
    /// Like the nested-repository test above, this guards a *method choice*
    /// (`symlink_metadata` versus `is_dir` versus `exists`) that no mutation
    /// operator expresses. The test is the whole safety net.
    #[test]
    fn a_git_file_counts_as_a_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join(".git"),
            b"gitdir: /elsewhere/.git/worktrees/x",
        )
        .expect("write .git file");

        assert_eq!(
            state_for(&dir.path().join("target")),
            Some(RepoState::Dirty),
            "a .git file is a repository, and an unreadable one is dirty"
        );
    }

    /// A `.git` symlink whose target is gone. `exists()` follows links and calls
    /// this "no repository", walking on past a repository that is, if anything,
    /// less trustworthy than a healthy one — the wrong direction to be wrong in.
    #[cfg(unix)]
    #[test]
    fn a_broken_git_symlink_still_counts_as_a_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join(".git"))
            .expect("symlink");

        assert!(
            !dir.path().join(".git").exists(),
            "the fixture must really be broken, or this proves nothing"
        );
        assert_eq!(
            enclosing_repo(&dir.path().join("target")),
            Some(dir.path().to_path_buf()),
            "a broken .git is still a .git"
        );
    }

    /// `Path::ancestors` on a relative path stops at `""`, so without anchoring
    /// the walk could never reach a `.git` above the working directory —
    /// `disk-tools clean target` from inside a repository would go unguarded.
    #[test]
    fn a_relative_path_is_anchored_before_the_walk() {
        let found = enclosing_repo(Path::new("target"));

        // Whatever the answer, it must be an absolute path: a relative one means
        // the walk stopped at the empty component instead of climbing.
        if let Some(repo) = found {
            assert!(repo.is_absolute(), "anchored before walking, got {repo:?}");
        }
    }

    /// §8.2.3 step 3, driven at the seam. `std::env::set_var` is `unsafe` in
    /// edition 2024 and this crate denies `unsafe`, so `PATH` cannot be
    /// manipulated from inside the test — but a missing binary *is* exactly the
    /// error `Command::output` returns, so the real branch is exercised.
    #[test]
    fn a_missing_git_binary_is_treated_as_dirty() {
        let missing = io::Error::from(io::ErrorKind::NotFound);

        assert_eq!(
            interpret(Err(missing)),
            RepoState::Dirty,
            "when the tool cannot know, it does not delete"
        );
    }

    /// A repository git itself refuses to read exits non-zero. Trusting only the
    /// exit code, or only the empty stdout, would call that clean.
    #[test]
    fn a_repository_git_cannot_read_is_dirty() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A `.git` that is neither a valid directory nor a valid pointer file.
        fs::write(dir.path().join(".git"), b"not a git link").expect("write");

        let state = state_for(&dir.path().join("target"));

        assert_eq!(
            state,
            Some(RepoState::Dirty),
            "an unreadable repository must never read as clean"
        );
    }
}
