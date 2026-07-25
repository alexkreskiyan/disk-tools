//! Deciding what may be removed, and what removing it would free.
//!
//! **[`plan`] writes nothing.** It reads a [`ScanTree`] and returns a decision,
//! which is what makes almost every rule below testable against a hand-built
//! tree: a denylist bug should cost a failing test, not a directory. Removal
//! lives in [`crate::trash`], the only module that needs the `trash` crate — so
//! planning is available even to a consumer who cannot apply.
//!
//! One rule *reads*: the git guard has to look for a `.git` and ask git about
//! it ([`crate::git`]). Nothing else here touches the filesystem, and nothing
//! here touches it for writing.
//!
//! All four of v0.2's safety mechanisms are decided here — the never-touch
//! denylist, the two tiers, `--safe`, and that guard.

use crate::detect::{Category, DetectOptions, UserDirs, detect};
use crate::git;
use crate::paths::{is_within, normalize_lexically, under_root};
use crate::tree::{ScanNode, ScanTree};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Directories that are never candidates, whatever matched them, named
/// relative to the filesystem root.
///
/// Root-relative rather than absolute so that `Windows` protects a system
/// installed on `D:` exactly as it protects one on `C:`; a hardcoded
/// `C:\Windows` would quietly cover only the common case.
///
/// **Not gated per platform**, deliberately. Over-denying costs a user one
/// directory they could have cleaned; under-denying costs them a system
/// directory. Given that asymmetry the whole list applies everywhere, which also
/// means every entry is exercised on every CI runner rather than only on the one
/// platform that has it.
const DENIED_ROOTS: &[&[&str]] = &[
    &["System"],
    // No tilde: this is the *system* cache. `~/Library/Caches` is a candidate
    // (§8.3), and telling the two apart is the whole point of deriving the
    // user's roots from a known home.
    &["Library", "Caches"],
    &["Windows"],
    &["Program Files"],
    &["Program Files (x86)"],
];

/// Directory names that may never be auto-tier, however they were matched.
///
/// Recreating a virtualenv is less deterministic than a lockfile install, so it
/// is the user's call every time (concept §4).
const CONFIRM_ONLY_NAMES: &[&str] = &["venv", ".venv"];

/// How eligible a candidate is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Tier {
    /// Regenerable, so removable without per-item confirmation.
    Auto,
    /// Needs the user to say yes to this specific path.
    Confirm,
}

/// One thing the plan proposes to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Candidate {
    pub path: PathBuf,
    pub category: Category,
    pub tier: Tier,

    /// Attributed bytes — what removing this would free, before the question
    /// [`Self::shared`] asks.
    pub allocated: u64,

    /// This holds content reachable from outside it, so `allocated` is an upper
    /// bound rather than a promise.
    ///
    /// Exact on Unix. On Windows only sharing *within the scan* is visible, so
    /// the absence of this marker there is **not** evidence of unshared content
    /// (D10) — a report must not imply otherwise.
    pub shared: bool,
}

/// Why something that matched is not in the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ExcludeReason {
    /// On the never-touch denylist. No flag overrides this.
    Denylisted,

    /// Build output whose project has uncommitted work — you may be mid-change,
    /// and it only regenerates identically from committed source.
    /// `--allow-dirty` overrides this one.
    DirtyRepo,
}

/// Something a rule matched and the plan then refused.
///
/// Recorded rather than dropped: a user who expected a directory to appear needs
/// to know why it did not.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Excluded {
    pub path: PathBuf,
    pub reason: ExcludeReason,
}

/// What a cleanup would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CleanPlan {
    /// Sorted by path, so two runs over one tree produce identical plans.
    pub candidates: Vec<Candidate>,

    /// The sum of the candidates' `allocated`. An **upper bound** whenever any
    /// candidate is `shared`.
    pub reclaimable: u64,

    pub excluded: Vec<Excluded>,
}

/// Everything a cleanup needs to know.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanOptions {
    /// Which rules run, and against what — categories, the age rule, and where
    /// this user's directories are.
    pub detect: DetectOptions,

    /// `--safe`: admit auto-tier candidates only.
    pub safe_only: bool,

    /// `--allow-dirty`: relax the git guard. **Never** the denylist.
    pub allow_dirty: bool,
}

/// Decide what could be removed, and what that would free.
///
/// **Writes nothing.** It reads a [`ScanTree`] and returns a decision, which is
/// what makes almost every rule below testable against a hand-built tree — a
/// denylist bug should cost a failing test, not a directory. The one exception
/// is the git guard, which has to look at the disk to answer at all; it still
/// only reads.
///
/// The order is the safety model:
///
/// 1. the rules propose ([`crate::detect`]);
/// 2. the **denylist** removes, unconditionally — no flag overrides it, and what
///    it removes is reported rather than dropped;
/// 3. the **git guard** removes build output whose project is mid-change,
///    likewise reported; `--allow-dirty` relaxes this one and nothing else;
/// 4. each survivor gets a tier, virtualenvs demoted whatever their category;
/// 5. `--safe` keeps only auto-tier;
/// 6. candidates whose content is reachable from outside them are marked, so
///    the total can be read as the upper bound it is.
///
/// Steps 2 and 3 come before 5 because they are the tool *refusing*, which the
/// user needs told; step 5 is the user's own narrowing, which they already know
/// about.
pub fn plan(tree: &ScanTree, options: &CleanOptions) -> CleanPlan {
    let denied = denylist(&options.detect.user_dirs);
    let groups = link_groups_by_path(tree);
    let nodes = nodes_by_path(tree);

    let mut candidates = Vec::new();
    let mut excluded = Vec::new();
    // One `git status` per repository, not per candidate: a tree of sibling
    // Rust projects would otherwise spawn a process for each, and a workspace
    // whose members share a repository would ask the same question repeatedly.
    let mut repos: HashMap<PathBuf, git::RepoState> = HashMap::new();

    for detection in detect(tree, &options.detect) {
        if is_denied(&detection.path, &denied) {
            excluded.push(Excluded {
                path: detection.path,
                reason: ExcludeReason::Denylisted,
            });
            continue;
        }

        if is_mid_change(&detection.path, detection.category, options, &mut repos) {
            excluded.push(Excluded {
                path: detection.path,
                reason: ExcludeReason::DirtyRepo,
            });
            continue;
        }

        let tier = tier_for(&detection.path, detection.category);
        // Not recorded in `excluded`, deliberately. That list answers "the tool
        // refused something you might have expected"; `--safe` is the user's own
        // narrowing, and putting the two in one list would show a protected
        // system directory beside something they asked to hide. The frontend
        // knows the flag was passed and can say so itself.
        if options.safe_only && tier != Tier::Auto {
            continue;
        }

        let shared = nodes
            .get(detection.path.as_path())
            .is_some_and(|node| holds_shared_content(node, &detection.path, &groups));

        candidates.push(Candidate {
            path: detection.path,
            category: detection.category,
            tier,
            allocated: detection.allocated,
            shared,
        });
    }

    // The walk is parallel and its order is not guaranteed; a plan that lists
    // the same paths in a different order every run cannot be reviewed.
    candidates.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));
    excluded.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));

    // No candidate nests inside another (`detect` never descends into a match),
    // so summing counts no byte twice.
    let reclaimable = candidates.iter().map(|c| c.allocated).sum();

    CleanPlan {
        candidates,
        reclaimable,
        excluded,
    }
}

/// The never-touch roots, absolute ones resolved against this user's directories.
fn denylist(user_dirs: &UserDirs) -> Vec<PathBuf> {
    let mut denied = Vec::new();
    if let Some(home) = &user_dirs.home {
        denied.push(home.join("Library").join("Application Support"));
    }
    if let Some(app_data) = &user_dirs.app_data {
        denied.push(app_data.clone());
    }
    denied
}

/// Is this path protected?
///
/// Checked in **both** its raw and its lexically-resolved form, because either
/// alone leaks. `Path` cannot resolve `..`, so `/home/me/../../System` would
/// slip past a literal comparison; and resolving *instead of* comparing would
/// lose `/System/../Users/x`, which matches `System` literally and stops
/// matching once resolved. Taking the union means the denylist only ever grows,
/// which is the only direction it may move.
fn is_denied(path: &Path, absolute: &[PathBuf]) -> bool {
    matches_denylist(path, absolute) || matches_denylist(&normalize_lexically(path), absolute)
}

fn matches_denylist(path: &Path, absolute: &[PathBuf]) -> bool {
    DENIED_ROOTS.iter().any(|root| under_root(path, root))
        || absolute.iter().any(|root| is_within(path, root))
}

/// Is this build output whose project has uncommitted work?
///
/// Scoped to build-output categories (§8.2.3 step 1) — `rust-target` is the only
/// one in v0.2. "You may be mid-change" is a statement about a project, and a
/// cache does not belong to one; applying the guard there would refuse to clean
/// `~/.cache` because some unrelated repository above it was dirty.
///
/// `--allow-dirty` short-circuits **before** the filesystem is touched, so the
/// override costs nothing rather than merely ignoring the answer.
///
/// `repos` memoises the verdict per repository, since several candidates often
/// share one.
fn is_mid_change(
    path: &Path,
    category: Category,
    options: &CleanOptions,
    repos: &mut HashMap<PathBuf, git::RepoState>,
) -> bool {
    if options.allow_dirty || !is_build_output(category) {
        return false;
    }

    // No repository above it, so there is no "mid-change" to be in.
    let Some(repo) = git::enclosing_repo(path) else {
        return false;
    };

    let state = *repos
        .entry(repo.clone())
        .or_insert_with(|| git::state(&repo));

    state == git::RepoState::Dirty
}

/// Which categories are output a build regenerates from source.
fn is_build_output(category: Category) -> bool {
    match category {
        Category::RustTarget => true,
        // `node_modules` is restored by a lockfile install rather than a build,
        // and the rest are caches or age matches. None of them are produced from
        // the working tree, so a dirty repository says nothing about them.
        Category::NodeModules | Category::Pycache | Category::UserCaches | Category::Old => false,
    }
}

/// Auto for the safe-list categories, confirm for anything judged by age — and
/// confirm for a virtualenv whatever its category said.
///
/// The demotion changes nothing today: no category matches a `venv/`, so the
/// only way one reaches a plan is the age rule, which is already confirm. It
/// exists so that a category added later cannot silently make virtualenvs
/// removable without asking.
fn tier_for(path: &Path, category: Category) -> Tier {
    if path
        .file_name()
        .is_some_and(|name| CONFIRM_ONLY_NAMES.iter().any(|deny| name == *deny))
    {
        return Tier::Confirm;
    }

    match category {
        Category::Old => Tier::Confirm,
        Category::RustTarget | Category::NodeModules | Category::Pycache | Category::UserCaches => {
            Tier::Auto
        }
    }
}

/// Every path that shares an inode with another path in this scan, mapped to
/// the group it belongs to.
fn link_groups_by_path(tree: &ScanTree) -> HashMap<&Path, &[PathBuf]> {
    let mut by_path = HashMap::new();
    for group in &tree.link_groups {
        for path in group {
            by_path.insert(path.as_path(), group.as_slice());
        }
    }
    by_path
}

fn nodes_by_path(tree: &ScanTree) -> HashMap<&Path, &ScanNode> {
    let mut by_path = HashMap::new();
    collect_nodes(&tree.root, &mut by_path);
    by_path
}

fn collect_nodes<'a>(node: &'a ScanNode, out: &mut HashMap<&'a Path, &'a ScanNode>) {
    out.insert(node.path.as_path(), node);
    for child in &node.children {
        collect_nodes(child, out);
    }
}

/// Would deleting `candidate` actually free its bytes, or is its content
/// reachable from somewhere else?
///
/// Two signals answer that between them (D10), and a file trips either:
///
/// 1. **A partner inside this scan but outside the candidate.** Exact, and
///    available on both platforms, since `FileId` exists on Windows too.
/// 2. **`links` greater than the group holding it** — a name for the same inode
///    that the scan never saw. A file in no group counts as a group of one, so a
///    twin outside the scanned tree is caught here. Unix only: `links` is `None`
///    on Windows, and `None` means *unknown*, never `1`.
fn holds_shared_content(
    node: &ScanNode,
    candidate: &Path,
    groups: &HashMap<&Path, &[PathBuf]>,
) -> bool {
    if !node.is_dir {
        let group = groups.get(node.path.as_path()).copied().unwrap_or_default();
        // A file in no group still owns its single name.
        let group_size = group.len().max(1);

        if group.iter().any(|partner| !is_within(partner, candidate)) {
            return true;
        }
        if node.links.is_some_and(|links| links as usize > group_size) {
            return true;
        }
    }

    node.children
        .iter()
        .any(|child| holds_shared_content(child, candidate, groups))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Age;
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    // ---- fixtures --------------------------------------------------------
    //
    // Hand-built trees throughout: `plan` is pure, so none of these rules need
    // a filesystem to be exercised — which is the point of the split. The one
    // test that does touch disk is `plan_writes_nothing`, and it does so to
    // prove the opposite.

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_750_000_000)
    }

    fn file(path: &str, allocated: u64) -> ScanNode {
        ScanNode {
            path: PathBuf::from(path),
            allocated,
            apparent: allocated,
            is_dir: false,
            modified: Some(now()),
            links: Some(1),
            children: Vec::new(),
        }
    }

    fn dir(path: &str, children: Vec<ScanNode>) -> ScanNode {
        ScanNode {
            path: PathBuf::from(path),
            allocated: 4096 + children.iter().map(|c| c.allocated).sum::<u64>(),
            apparent: 4096,
            is_dir: true,
            modified: Some(now()),
            links: None,
            children,
        }
    }

    fn tree(root: ScanNode) -> ScanTree {
        ScanTree {
            root,
            skipped: Vec::new(),
            link_groups: Vec::new(),
        }
    }

    fn opts() -> CleanOptions {
        CleanOptions::default()
    }

    /// Options with a home, which is what makes the user-scoped rules — both the
    /// `user-caches` category and the denylist's `Application Support` — live.
    fn with_home(home: &str) -> CleanOptions {
        CleanOptions {
            detect: DetectOptions {
                user_dirs: UserDirs {
                    home: Some(PathBuf::from(home)),
                    ..UserDirs::default()
                },
                ..DetectOptions::default()
            },
            ..CleanOptions::default()
        }
    }

    fn aging(older_than: Duration) -> CleanOptions {
        CleanOptions {
            detect: DetectOptions {
                age: Some(Age {
                    older_than,
                    now: now(),
                }),
                ..DetectOptions::default()
            },
            ..CleanOptions::default()
        }
    }

    fn aged(mut node: ScanNode, age: Duration) -> ScanNode {
        node.modified = Some(now() - age);
        node
    }

    fn paths(plan: &CleanPlan) -> Vec<&Path> {
        plan.candidates.iter().map(|c| c.path.as_path()).collect()
    }

    /// A `node_modules` holding one file, ready to be given hardlink partners.
    fn candidate_with_a_file(file_path: &str, links: Option<u32>) -> ScanNode {
        let mut inner = file(file_path, 8192);
        inner.links = links;
        dir("/p/node_modules", vec![inner])
    }

    // ---- the denylist ----------------------------------------------------

    /// One case per entry. This is the mechanism with no override — `--safe`
    /// narrows the plan, `--allow-dirty` relaxes the git guard, and neither
    /// touches this — so each entry gets its own assertion rather than a
    /// representative sample.
    #[test]
    fn denylisted_paths_are_never_candidates() {
        let cases = [
            "/System/node_modules",
            "/Library/Caches/node_modules",
            "/Windows/node_modules",
            "/Program Files/node_modules",
            "/Program Files (x86)/node_modules",
            "/home/me/Library/Application Support/node_modules",
        ];

        for case in cases {
            let parent = PathBuf::from(case);
            let parent = parent.parent().expect("has a parent").to_owned();
            let plan = plan(
                &tree(dir(
                    parent.to_str().expect("utf8"),
                    vec![dir(case, vec![file(&format!("{case}/big.bin"), 1_000_000)])],
                )),
                &with_home("/home/me"),
            );

            assert!(
                plan.candidates.is_empty(),
                "{case} must never be a candidate, got {:?}",
                paths(&plan)
            );
            assert_eq!(plan.reclaimable, 0, "{case} must contribute nothing");
        }
    }

    /// Windows' roaming profile is a denylist root in its own right — it is not
    /// derivable from `%LOCALAPPDATA%`, so a `UserDirs` that omits it leaves the
    /// entry unenforced.
    #[test]
    fn the_roaming_profile_is_denied_when_known() {
        let options = CleanOptions {
            detect: DetectOptions {
                user_dirs: UserDirs {
                    app_data: Some(PathBuf::from("/users/me/AppData/Roaming")),
                    ..UserDirs::default()
                },
                ..DetectOptions::default()
            },
            ..CleanOptions::default()
        };

        let plan = plan(
            &tree(dir(
                "/users/me/AppData/Roaming",
                vec![dir("/users/me/AppData/Roaming/node_modules", vec![])],
            )),
            &options,
        );

        assert!(plan.candidates.is_empty(), "{:?}", paths(&plan));
    }

    /// Nothing is dropped in silence: a user who expected a directory in the
    /// plan has to be able to see why it is absent.
    #[test]
    fn a_denylisted_path_is_reported_with_its_reason() {
        let plan = plan(
            &tree(dir("/Windows", vec![dir("/Windows/node_modules", vec![])])),
            &opts(),
        );

        assert_eq!(
            plan.excluded,
            vec![Excluded {
                path: PathBuf::from("/Windows/node_modules"),
                reason: ExcludeReason::Denylisted,
            }]
        );
    }

    /// The tilde distinction under load. `~/Library/Caches` is itself a
    /// candidate; the identically-named system directory is not, and a
    /// `node_modules` sitting inside it — which *does* match a rule — is denied
    /// rather than cleaned.
    #[test]
    fn the_denylist_beats_a_safe_list_match() {
        let plan = plan(
            &tree(dir(
                "/",
                vec![
                    dir(
                        "/Library",
                        vec![dir(
                            "/Library/Caches",
                            vec![dir("/Library/Caches/node_modules", vec![])],
                        )],
                    ),
                    dir(
                        "/home/me",
                        vec![dir(
                            "/home/me/Library",
                            vec![dir("/home/me/Library/Caches", vec![])],
                        )],
                    ),
                ],
            )),
            &with_home("/home/me"),
        );

        assert_eq!(
            paths(&plan),
            vec![Path::new("/home/me/Library/Caches")],
            "the user's cache is a candidate; the system's contents are denied"
        );
        assert_eq!(
            plan.excluded,
            vec![Excluded {
                path: PathBuf::from("/Library/Caches/node_modules"),
                reason: ExcludeReason::Denylisted,
            }],
            "and the denial is reported rather than silent"
        );
    }

    /// `Path` does not resolve `..`, and the scan carries whatever the user
    /// typed: `disk-tools clean /home/me/../../System` would reach every node
    /// under `/System` with the traversal still in its path. Compared
    /// literally, the denylist would never fire.
    #[test]
    fn a_parent_traversal_cannot_escape_the_denylist() {
        let plan = plan(
            &tree(dir(
                "/home/me/../../System",
                vec![dir("/home/me/../../System/node_modules", vec![])],
            )),
            &opts(),
        );

        assert!(
            plan.candidates.is_empty(),
            "a traversal into a protected root must still be denied, got {:?}",
            paths(&plan)
        );
        assert_eq!(plan.excluded.len(), 1, "and reported");
    }

    /// The other direction: a path that matches an entry *literally* must stay
    /// denied even though resolving it walks back out. Checking only the
    /// resolved form would lose this one, which is why both are checked.
    #[test]
    fn resolving_a_path_can_never_lift_a_denial() {
        assert!(
            is_denied(Path::new("/System/../Users/me/node_modules"), &[]),
            "the raw form is under a denied root, so the denial stands"
        );
    }

    // ---- the git guard ---------------------------------------------------
    //
    // Driven against **real** repositories: a guard verified against a stubbed
    // git tells you the stub works. The fixtures skip loudly where git is
    // absent, following the `chmod 000` precedent — and every CI runner has git,
    // so these really run rather than quietly passing.

    /// Build a repository under a temp dir, with `Cargo.toml` and `target/` so
    /// the `rust-target` rule has something to match. Returns `None` where this
    /// environment has no usable git.
    fn repo_fixture(committed: bool) -> Option<tempfile::TempDir> {
        let handle = tempfile::tempdir().expect("tempdir");
        let root = handle.path();

        let init = Command::new("git").arg("init").arg(root).output();
        match init {
            Ok(output) if output.status.success() => {}
            _ => {
                eprintln!("skipping: no usable git in this environment");
                return None;
            }
        }

        std::fs::write(root.join("Cargo.toml"), b"[package]\nname = \"x\"\n").expect("write");
        std::fs::create_dir(root.join("target")).expect("mkdir");
        std::fs::create_dir(root.join("node_modules")).expect("mkdir");

        if committed {
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

        Some(handle)
    }

    /// A tree shaped like the fixture on disk, so `plan` sees paths that really
    /// exist and the guard can consult them.
    fn repo_tree(root: &Path) -> ScanTree {
        let as_str = |p: PathBuf| p.to_str().expect("utf8 temp path").to_owned();
        tree(dir(
            &as_str(root.to_path_buf()),
            vec![
                file(&as_str(root.join("Cargo.toml")), 100),
                dir(&as_str(root.join("node_modules")), vec![]),
                dir(&as_str(root.join("target")), vec![]),
            ],
        ))
    }

    #[test]
    fn dirty_repo_excludes_the_build_directory_with_a_reason() {
        let Some(handle) = repo_fixture(false) else {
            return;
        };
        let root = handle.path();

        let plan = plan(&repo_tree(root), &opts());

        assert!(
            !paths(&plan).contains(&root.join("target").as_path()),
            "build output of a mid-change project must not be a candidate: {:?}",
            paths(&plan)
        );
        assert_eq!(
            plan.excluded,
            vec![Excluded {
                path: root.join("target"),
                reason: ExcludeReason::DirtyRepo,
            }],
            "and the user must be told why"
        );
    }

    #[test]
    fn clean_repo_keeps_the_candidate() {
        let Some(handle) = repo_fixture(true) else {
            return;
        };
        let root = handle.path();

        let plan = plan(&repo_tree(root), &opts());

        assert!(
            paths(&plan).contains(&root.join("target").as_path()),
            "everything is committed, so the build output regenerates: {:?}",
            paths(&plan)
        );
        assert!(plan.excluded.is_empty(), "{:?}", plan.excluded);
    }

    #[test]
    fn allow_dirty_disables_the_guard() {
        let Some(handle) = repo_fixture(false) else {
            return;
        };
        let root = handle.path();
        let options = CleanOptions {
            allow_dirty: true,
            ..opts()
        };

        let plan = plan(&repo_tree(root), &options);

        assert!(
            paths(&plan).contains(&root.join("target").as_path()),
            "the user overrode the guard: {:?}",
            paths(&plan)
        );
        assert!(plan.excluded.is_empty(), "{:?}", plan.excluded);
    }

    /// §8.2.3 step 1. `node_modules` is restored by a lockfile install, not
    /// rebuilt from the working tree, so a dirty repository says nothing about
    /// it — guarding it would refuse cleanups for an unrelated reason.
    #[test]
    fn only_build_output_is_guarded() {
        let Some(handle) = repo_fixture(false) else {
            return;
        };
        let root = handle.path();

        let plan = plan(&repo_tree(root), &opts());

        assert_eq!(
            paths(&plan),
            vec![root.join("node_modules").as_path()],
            "the dependency directory is unaffected by the repository's state"
        );
    }

    /// A candidate with no repository above it at all: the guard has nothing to
    /// consult and must not invent an answer.
    #[test]
    fn path_outside_any_repo_is_unaffected() {
        let handle = tempfile::tempdir().expect("tempdir");
        let root = handle.path();
        std::fs::write(root.join("Cargo.toml"), b"[package]").expect("write");
        std::fs::create_dir(root.join("target")).expect("mkdir");
        std::fs::create_dir(root.join("node_modules")).expect("mkdir");

        let plan = plan(&repo_tree(root), &opts());

        assert!(
            paths(&plan).contains(&root.join("target").as_path()),
            "no repository, no guard: {:?}",
            paths(&plan)
        );
        assert!(plan.excluded.is_empty());
    }

    /// The ordering test, in the shape that caught the `--safe` reordering in
    /// Task 4: the denylist must run first, so its reason is the one reported.
    /// Reversed, a denylisted path in a dirty repository would be blamed on the
    /// repository — and `--allow-dirty` would then appear to be the way past it.
    #[test]
    fn the_denylist_is_reported_ahead_of_the_guard() {
        let plan = plan(
            &tree(dir(
                "/Windows",
                vec![
                    file("/Windows/Cargo.toml", 100),
                    dir("/Windows/target", vec![]),
                ],
            )),
            &opts(),
        );

        assert_eq!(
            plan.excluded,
            vec![Excluded {
                path: PathBuf::from("/Windows/target"),
                reason: ExcludeReason::Denylisted,
            }],
            "a protected path is denylisted, whatever git would have said"
        );
    }

    // ---- tiers -----------------------------------------------------------

    #[test]
    fn safe_list_categories_are_auto_tier() {
        let plan = plan(
            &tree(dir(
                "/p",
                vec![
                    file("/p/Cargo.toml", 100),
                    dir("/p/__pycache__", vec![]),
                    dir("/p/node_modules", vec![]),
                    dir("/p/target", vec![]),
                ],
            )),
            &opts(),
        );

        assert_eq!(plan.candidates.len(), 3, "{:?}", paths(&plan));
        assert!(
            plan.candidates.iter().all(|c| c.tier == Tier::Auto),
            "regenerable output needs no per-item confirmation: {:?}",
            plan.candidates
        );
    }

    #[test]
    fn old_matches_are_confirm_tier() {
        let plan = plan(
            &tree(dir(
                "/p",
                vec![aged(file("/p/ancient.bin", 4096), 900 * DAY)],
            )),
            &aging(90 * DAY),
        );

        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].tier, Tier::Confirm);
        assert_eq!(plan.candidates[0].category, Category::Old);
    }

    /// Recreating a virtualenv is less deterministic than a lockfile install, so
    /// it is the user's call every time — whatever rule found it.
    #[test]
    fn virtualenvs_are_confirm_tier() {
        for name in ["venv", ".venv"] {
            let path = format!("/p/{name}");
            let plan = plan(
                &tree(dir("/p", vec![aged(dir(&path, vec![]), 900 * DAY)])),
                &aging(90 * DAY),
            );

            assert_eq!(plan.candidates.len(), 1, "{name}: {:?}", paths(&plan));
            assert_eq!(plan.candidates[0].tier, Tier::Confirm, "{name}");
        }
    }

    /// The demotion must beat the category, not merely coincide with it — this
    /// drives the branch with a category that would otherwise be auto-tier.
    #[test]
    fn a_virtualenv_is_demoted_even_when_its_category_is_auto() {
        assert_eq!(
            tier_for(Path::new("/p/venv"), Category::NodeModules),
            Tier::Confirm,
            "the name wins over a category that would otherwise be auto"
        );
        assert_eq!(
            tier_for(Path::new("/p/node_modules"), Category::NodeModules),
            Tier::Auto,
            "and an ordinary auto-tier match is unaffected"
        );
    }

    #[test]
    fn safe_flag_admits_only_auto_tier() {
        let fixture = tree(dir(
            "/p",
            vec![
                dir("/p/node_modules", vec![]),
                aged(file("/p/ancient.bin", 4096), 900 * DAY),
            ],
        ));
        let mut options = aging(90 * DAY);

        let both = plan(&fixture, &options);
        assert_eq!(both.candidates.len(), 2, "{:?}", paths(&both));

        options.safe_only = true;
        let safe = plan(&fixture, &options);

        assert_eq!(paths(&safe), vec![Path::new("/p/node_modules")]);
        assert_eq!(
            safe.reclaimable, 4096,
            "the excluded confirm-tier bytes leave the total too"
        );
    }

    /// The denylist runs **before** `--safe`, and this is the only combination
    /// that can tell: a confirm-tier match inside a protected root. Every other
    /// case looks identical whichever order the two run in — with `--safe` off
    /// the filter is a no-op, and an auto-tier match passes the filter anyway.
    ///
    /// Get the order wrong and the path is dropped by the filter before the
    /// denylist ever sees it, so it vanishes from `excluded` too — the tool
    /// silently declining to mention that it protected something.
    #[test]
    fn the_denylist_beats_the_safe_filter() {
        let options = CleanOptions {
            safe_only: true,
            ..aging(90 * DAY)
        };

        let plan = plan(
            &tree(dir(
                "/Library/Caches",
                vec![aged(file("/Library/Caches/ancient.bin", 4096), 900 * DAY)],
            )),
            &options,
        );

        assert!(plan.candidates.is_empty(), "{:?}", paths(&plan));
        assert_eq!(
            plan.excluded,
            vec![Excluded {
                path: PathBuf::from("/Library/Caches/ancient.bin"),
                reason: ExcludeReason::Denylisted,
            }],
            "the denial must be reported even when --safe would also have dropped it"
        );
    }

    /// `--safe` is a **silent** filter, on purpose. `excluded` answers "the tool
    /// refused something you might have expected"; this is the user's own
    /// narrowing, and listing the two together would put a protected system
    /// directory beside something they asked to hide.
    #[test]
    fn the_safe_filter_records_nothing() {
        let options = CleanOptions {
            safe_only: true,
            ..aging(90 * DAY)
        };

        let plan = plan(
            &tree(dir(
                "/p",
                vec![aged(file("/p/ancient.bin", 4096), 900 * DAY)],
            )),
            &options,
        );

        assert!(plan.candidates.is_empty());
        assert!(
            plan.excluded.is_empty(),
            "a user-requested filter is not a refusal, got {:?}",
            plan.excluded
        );
    }

    // ---- totals ----------------------------------------------------------

    #[test]
    fn reclaimable_is_the_sum_of_candidate_allocated() {
        let plan = plan(
            &tree(dir(
                "/p",
                vec![
                    dir(
                        "/p/a",
                        vec![dir(
                            "/p/a/node_modules",
                            vec![file("/p/a/node_modules/x.bin", 100_000)],
                        )],
                    ),
                    dir(
                        "/p/b",
                        vec![dir(
                            "/p/b/node_modules",
                            vec![file("/p/b/node_modules/y.bin", 200_000)],
                        )],
                    ),
                ],
            )),
            &opts(),
        );

        assert_eq!(plan.candidates.len(), 2);
        assert_eq!(
            plan.reclaimable,
            plan.candidates.iter().map(|c| c.allocated).sum::<u64>()
        );
        // Each candidate's own subtree total, so the sum is 2×(dir + file).
        assert_eq!(plan.reclaimable, (4096 + 100_000) + (4096 + 200_000));
    }

    #[test]
    fn empty_tree_yields_empty_plan() {
        let plan = plan(&tree(dir("/p", vec![])), &opts());

        assert!(plan.candidates.is_empty());
        assert!(plan.excluded.is_empty());
        assert_eq!(plan.reclaimable, 0);
    }

    #[test]
    fn plan_is_deterministic_across_runs() {
        let fixture = tree(dir(
            "/p",
            vec![
                dir("/p/z", vec![dir("/p/z/node_modules", vec![])]),
                dir("/p/a", vec![dir("/p/a/node_modules", vec![])]),
                dir("/p/m", vec![dir("/p/m/__pycache__", vec![])]),
            ],
        ));

        let first = plan(&fixture, &opts());
        assert_eq!(first, plan(&fixture, &opts()));

        let mut sorted = first.candidates.clone();
        sorted.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));
        assert_eq!(first.candidates, sorted, "candidates are ordered by path");
    }

    // ---- the shared marker -----------------------------------------------

    #[test]
    fn unshared_candidate_is_not_marked() {
        let plan = plan(
            &tree(dir(
                "/p",
                vec![candidate_with_a_file("/p/node_modules/lib.bin", Some(1))],
            )),
            &opts(),
        );

        assert_eq!(plan.candidates.len(), 1);
        assert!(
            !plan.candidates[0].shared,
            "content with one name is not shared, and the total is exact"
        );
    }

    /// Case 2 of §8.3 — the one that over-reports. The keeper is inside the
    /// candidate but another name for the same inode lives outside it, so
    /// deleting the candidate frees nothing.
    #[test]
    fn partner_outside_the_candidate_marks_it_shared() {
        let mut fixture = tree(dir(
            "/p",
            vec![
                candidate_with_a_file("/p/node_modules/lib.bin", Some(2)),
                file("/p/keep/lib.bin", 0),
            ],
        ));
        fixture.link_groups = vec![vec![
            PathBuf::from("/p/keep/lib.bin"),
            PathBuf::from("/p/node_modules/lib.bin"),
        ]];

        let plan = plan(&fixture, &opts());

        assert_eq!(plan.candidates.len(), 1);
        assert!(
            plan.candidates[0].shared,
            "a partner outside the candidate makes the total an upper bound"
        );
    }

    /// The counterweight: without it the marker could simply fire on anything
    /// hardlinked, and a `node_modules` full of internal links would be reported
    /// as freeing nothing when it frees everything.
    #[test]
    fn hardlinks_entirely_inside_the_candidate_are_not_marked() {
        let mut first = file("/p/node_modules/a.bin", 8192);
        first.links = Some(2);
        let mut second = file("/p/node_modules/b.bin", 0);
        second.links = Some(2);

        let mut fixture = tree(dir("/p", vec![dir("/p/node_modules", vec![first, second])]));
        fixture.link_groups = vec![vec![
            PathBuf::from("/p/node_modules/a.bin"),
            PathBuf::from("/p/node_modules/b.bin"),
        ]];

        let plan = plan(&fixture, &opts());

        assert_eq!(plan.candidates.len(), 1);
        assert!(
            !plan.candidates[0].shared,
            "both names go with the candidate, so the bytes really are freed"
        );
    }

    /// The signal that catches a package manager hardlinking into a store
    /// outside the scanned tree — the case that matters most in practice. The
    /// scan sees one name; `links` says there are two.
    #[cfg(unix)]
    #[test]
    fn links_exceeding_the_group_marks_an_out_of_scan_partner() {
        let plan = plan(
            &tree(dir(
                "/p",
                vec![candidate_with_a_file("/p/node_modules/lib.bin", Some(2))],
            )),
            &opts(),
        );

        assert_eq!(plan.candidates.len(), 1);
        assert!(
            plan.candidates[0].shared,
            "two names but one in the scan means the other is outside it"
        );
    }

    /// The honest gap (D10). `links` is `None` on Windows, and `None` means
    /// *unknown*, never `1` — but unknown cannot be reported as shared either,
    /// so an out-of-scan partner goes unseen there. A test pins it so nobody
    /// later reads the absent marker as a guarantee.
    #[test]
    fn an_absent_link_count_does_not_mark() {
        let plan = plan(
            &tree(dir(
                "/p",
                vec![candidate_with_a_file("/p/node_modules/lib.bin", None)],
            )),
            &opts(),
        );

        assert_eq!(plan.candidates.len(), 1);
        assert!(
            !plan.candidates[0].shared,
            "the gap is in the signal, not a claim about the content"
        );
    }

    /// Sharing can sit anywhere under a candidate, not just at its top level —
    /// the marker walks the whole subtree.
    #[test]
    fn a_partner_deep_inside_the_candidate_is_still_found() {
        let mut deep = file("/p/node_modules/pkg/dist/lib.bin", 8192);
        deep.links = Some(2);

        let mut fixture = tree(dir(
            "/p",
            vec![dir(
                "/p/node_modules",
                vec![dir(
                    "/p/node_modules/pkg",
                    vec![dir("/p/node_modules/pkg/dist", vec![deep])],
                )],
            )],
        ));
        fixture.link_groups = vec![vec![
            PathBuf::from("/elsewhere/lib.bin"),
            PathBuf::from("/p/node_modules/pkg/dist/lib.bin"),
        ]];

        let plan = plan(&fixture, &opts());

        assert!(plan.candidates[0].shared);
    }

    // ---- purity ----------------------------------------------------------

    /// The strongest statement available that the default is safe: a real
    /// fixture, listed before and after, unchanged.
    #[test]
    fn plan_writes_nothing() {
        let dir_handle = tempfile::tempdir().expect("tempdir");
        let root = dir_handle.path();
        std::fs::create_dir(root.join("node_modules")).expect("mkdir");
        std::fs::write(root.join("node_modules/lib.bin"), b"payload").expect("write");

        let before = snapshot(root);
        let scanned = crate::scan(&crate::ScanOptions {
            root: root.to_path_buf(),
            ..crate::ScanOptions::default()
        });

        let plan = plan(&scanned, &opts());

        assert!(
            !plan.candidates.is_empty(),
            "the fixture must match, or this proves nothing"
        );
        assert_eq!(
            before,
            snapshot(root),
            "planning must not create, remove or alter anything"
        );
    }

    /// Every path under `root`, with its length — enough to catch a creation,
    /// a deletion or a truncation.
    fn snapshot(root: &Path) -> Vec<(PathBuf, u64)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            let entries = std::fs::read_dir(&path).expect("read_dir");
            for entry in entries {
                let entry = entry.expect("entry");
                let metadata = entry.metadata().expect("metadata");
                if metadata.is_dir() {
                    stack.push(entry.path());
                }
                out.push((entry.path(), metadata.len()));
            }
        }
        out.sort();
        out
    }
}
