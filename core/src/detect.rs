//! Deciding what *looks* disposable.
//!
//! Rules run over the finished [`ScanTree`], not during the walk: a node's
//! siblings are exactly what marker-aware detection needs ("is there a
//! `Cargo.toml` beside this `target/`?"), and the tree already groups them.
//! The hot parallel walk stays untouched.
//!
//! Everything here is a **pure function of its inputs**. No filesystem, no
//! clock, no environment — the current time and the user's directories are
//! passed in, the way [`crate::ScanOptions`] already promises. A rule that
//! consulted the environment could not be tested with a temporary directory
//! standing in for a home, and for code whose output is later fed to a delete
//! operation that is not a trade worth making.
//!
//! Nothing here removes, ranks or excludes anything. It reports what matched;
//! the denylist, the tiers and the totals belong to the cleanup engine.

use crate::tree::{ScanNode, ScanTree};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Why a path was picked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Category {
    /// A Cargo build directory, recognised by the manifest beside it.
    RustTarget,
    /// An npm-style dependency directory.
    NodeModules,
    /// Compiled Python bytecode — a `__pycache__` directory or a loose `.pyc`.
    Pycache,
    /// One of *this user's* cache directories. Never their system counterparts.
    UserCaches,
    /// Untouched for at least the requested duration.
    Old,
}

/// Which safe-list categories are live.
///
/// All of them by default: v0.2 ships built-in defaults and needs no config to
/// clean (D8). Config arrives in v0.3 and will only ever *disable* entries here
/// — it never invents a new deletion rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CategorySet {
    pub rust_target: bool,
    pub node_modules: bool,
    pub pycache: bool,
    pub user_caches: bool,
}

impl Default for CategorySet {
    fn default() -> Self {
        CategorySet {
            rust_target: true,
            node_modules: true,
            pycache: true,
            user_caches: true,
        }
    }
}

impl CategorySet {
    /// Nothing enabled — the starting point for "only this one category".
    pub fn none() -> Self {
        CategorySet {
            rust_target: false,
            node_modules: false,
            pycache: false,
            user_caches: false,
        }
    }

    fn enables(&self, category: Category) -> bool {
        match category {
            Category::RustTarget => self.rust_target,
            Category::NodeModules => self.node_modules,
            Category::Pycache => self.pycache,
            Category::UserCaches => self.user_caches,
            // Gated by the age rule being armed at all, not by this set.
            Category::Old => true,
        }
    }
}

/// Where this user's own directories are.
///
/// Supplied by the frontend, never discovered here — the core consults no
/// environment. Both fields are `Option` because a frontend may genuinely not
/// know: a `None` home matches **nothing**, which is the safe direction. It is
/// never treated as "match any home".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserDirs {
    /// `$HOME` / `%USERPROFILE%`.
    pub home: Option<PathBuf>,
    /// `%LOCALAPPDATA%` on Windows. `None` elsewhere.
    pub local_app_data: Option<PathBuf>,
}

impl UserDirs {
    /// The cache directories that belong to this user.
    ///
    /// Built from the roots above rather than accepted as a ready-made list:
    /// *which* directories count as user caches is knowledge about the category,
    /// so it belongs beside the category. The frontend only has to answer "where
    /// is home".
    ///
    /// The tilde matters and is the whole point — `~/Library/Caches` is a
    /// candidate, `/Library/Caches` is on the denylist (§8.3). Deriving these
    /// from a known home is what keeps the two apart.
    fn cache_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(home) = &self.home {
            roots.push(home.join(".cache"));
            roots.push(home.join("Library").join("Caches"));
        }
        if let Some(local) = &self.local_app_data {
            roots.push(local.join("Temp"));
        }
        roots
    }
}

/// The age rule's two inputs, kept together because neither means anything
/// alone.
///
/// A threshold with no instant to measure it from silently matches nothing,
/// which is the worst way for a deletion rule to be wrong — it looks armed and
/// is not. Pairing them makes that state unrepresentable rather than merely
/// documented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Age {
    /// Untouched for at least this long.
    pub older_than: Duration,

    /// What "now" is. An input rather than a call to [`SystemTime::now`], so a
    /// boundary test does not depend on the clock.
    pub now: SystemTime,
}

/// Everything the rules need, all of it explicit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetectOptions {
    /// Which safe-list categories to apply.
    pub categories: CategorySet,

    /// The age rule. **`None` means it does not run at all** (D9) — not "zero",
    /// not "everything". That is the default, and deliberately so: an always-on
    /// age rule would bury the safe-list candidates that are the point of the
    /// feature under confirm-tier noise on the very first run.
    pub age: Option<Age>,

    /// Where this user's caches live.
    pub user_dirs: UserDirs,
}

/// One thing a rule claimed.
///
/// `allocated` is carried because the traversal is holding the node anyway —
/// the alternative is the cleanup engine finding each path in the tree a second
/// time. It is the node's full subtree total, since the whole subtree is what a
/// match stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub path: PathBuf,
    pub category: Category,
    pub allocated: u64,
}

/// Find everything the enabled rules claim.
///
/// One depth-first pass. At each node the junk rules are tried first, then the
/// age rule — §8.2.2 only applies the latter to nodes "not already claimed".
///
/// **A match is never descended into.** Once `node_modules/` is claimed, its
/// 40,000 children are not 40,000 further candidates: the subtree is one thing
/// to delete, and reporting its contents separately would be both useless and
/// dangerous. The same holds for an age match, so no candidate ever nests inside
/// another — which is what lets the caller sum their sizes without counting the
/// same bytes twice.
///
/// Order follows the tree, whose children are already sorted by path, so two
/// runs over the same tree return the same list.
pub fn detect(tree: &ScanTree, options: &DetectOptions) -> Vec<Detection> {
    let mut found = Vec::new();
    // The root has no containing directory, so it has no siblings to consult.
    visit(&tree.root, &[], options, &mut found);
    found
}

fn visit(
    node: &ScanNode,
    siblings: &[ScanNode],
    options: &DetectOptions,
    found: &mut Vec<Detection>,
) {
    if let Some(category) = claim(node, siblings, options) {
        found.push(Detection {
            path: node.path.clone(),
            category,
            allocated: node.allocated,
        });
        return; // the subtree is this one candidate
    }

    for child in &node.children {
        visit(child, &node.children, options, found);
    }
}

/// Which rule, if any, claims this node.
fn claim(node: &ScanNode, siblings: &[ScanNode], options: &DetectOptions) -> Option<Category> {
    junk(node, siblings, options)
        .filter(|category| options.categories.enables(*category))
        .or_else(|| old(node, options))
}

/// The safe-list rules (§8.2.1).
fn junk(node: &ScanNode, siblings: &[ScanNode], options: &DetectOptions) -> Option<Category> {
    // A cache root is matched by its whole path, so it is checked before the
    // name-based rules and needs no marker.
    if options
        .user_dirs
        .cache_roots()
        .iter()
        .any(|root| same_path(&node.path, root))
    {
        return Some(Category::UserCaches);
    }

    let name = node.path.file_name()?;

    if node.is_dir {
        if name == "target" && has_sibling(siblings, "Cargo.toml") {
            return Some(Category::RustTarget);
        }
        if name == "node_modules" {
            return Some(Category::NodeModules);
        }
        if name == "__pycache__" {
            return Some(Category::Pycache);
        }
        return None;
    }

    // A loose `.pyc` outside any `__pycache__`; one inside is covered by the
    // directory match, which is never descended into.
    if node.path.extension().is_some_and(|ext| ext == "pyc") {
        return Some(Category::Pycache);
    }

    None
}

/// The age rule (§8.2.2).
///
/// Opt-in: with no threshold it claims nothing at all. A directory is judged on
/// its **own** mtime, which is what the model carries — a directory's timestamp
/// moves when its entries change, and that is precisely the "still in use"
/// evidence wanted.
fn old(node: &ScanNode, options: &DetectOptions) -> Option<Category> {
    let age = options.age?;
    // Absence of evidence is not evidence of age.
    let modified = node.modified?;
    let threshold = age.now.checked_sub(age.older_than)?;

    // "Older or exactly equal" — the boundary is inclusive.
    (modified <= threshold).then_some(Category::Old)
}

/// Do these two paths name the same directory?
///
/// Compared component-wise, so `Path`'s own normalisation of separators and `.`
/// components applies — and never through `canonicalize()`, which this project
/// does not call.
///
/// **ASCII-case-insensitive on Windows**, where the filesystem is: a home
/// resolved as `C:\Users\Me\AppData\Local` must still match a scan that walked
/// `c:\users\me\appdata\local`, or the cache root goes unrecognised. Folding is
/// ASCII-only rather than NTFS's full Unicode upcase tables, so a non-ASCII
/// name differing in case still misses — the same as before this existed, so
/// strictly an improvement and never a new false match.
#[cfg(windows)]
fn same_path(a: &Path, b: &Path) -> bool {
    let mut left = a.components();
    let mut right = b.components();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(l), Some(r)) if l.as_os_str().eq_ignore_ascii_case(r.as_os_str()) => {}
            _ => return false,
        }
    }
}

/// Exact elsewhere, deliberately.
///
/// macOS is usually case-insensitive but APFS can be formatted either way, and
/// Linux is case-sensitive outright. Folding case on a case-sensitive volume
/// would make two genuinely different directories compare equal — and this
/// comparison decides whether something becomes a deletion candidate, so the
/// wrong direction to err in is obvious.
#[cfg(not(windows))]
fn same_path(a: &Path, b: &Path) -> bool {
    a == b
}

fn has_sibling(siblings: &[ScanNode], name: &str) -> bool {
    siblings
        .iter()
        .any(|sibling| sibling.path.file_name().is_some_and(|n| n == name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// A fixed "now" far enough from the epoch that subtracting a threshold
    /// cannot underflow.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_750_000_000)
    }

    fn file(path: &str) -> ScanNode {
        ScanNode {
            path: PathBuf::from(path),
            allocated: 4096,
            apparent: 4000,
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
            apparent: 4000,
            is_dir: true,
            modified: Some(now()),
            links: None,
            children,
        }
    }

    /// Set a node's mtime to `age` before [`now`].
    fn aged(mut node: ScanNode, age: Duration) -> ScanNode {
        node.modified = Some(now() - age);
        node
    }

    fn tree(root: ScanNode) -> ScanTree {
        ScanTree {
            root,
            skipped: Vec::new(),
            link_groups: Vec::new(),
        }
    }

    /// Every category on, the age rule off — the default a first run gets.
    fn opts() -> DetectOptions {
        DetectOptions::default()
    }

    /// The age rule armed against the fixed [`now`].
    fn aging(older_than: Duration) -> DetectOptions {
        DetectOptions {
            age: Some(Age {
                older_than,
                now: now(),
            }),
            ..DetectOptions::default()
        }
    }

    /// Just the paths, for asserting on a whole result at once.
    fn paths(found: &[Detection]) -> Vec<&Path> {
        found.iter().map(|m| m.path.as_path()).collect()
    }

    fn categories(found: &[Detection]) -> Vec<Category> {
        found.iter().map(|m| m.category).collect()
    }

    #[test]
    fn rust_target_requires_a_sibling_manifest() {
        let found = detect(
            &tree(dir(
                "/p",
                vec![file("/p/Cargo.toml"), dir("/p/target", vec![])],
            )),
            &opts(),
        );

        assert_eq!(paths(&found), vec![Path::new("/p/target")]);
        assert_eq!(categories(&found), vec![Category::RustTarget]);
    }

    /// The marker is the whole rule: `target/` is an ordinary directory name,
    /// and deleting someone's `target/` because it shared a name with a build
    /// directory is exactly the mistake this guards.
    #[test]
    fn bare_target_directory_is_not_junk() {
        let found = detect(
            &tree(dir("/p", vec![dir("/p/target", vec![file("/p/target/a")])])),
            &opts(),
        );

        assert!(found.is_empty(), "no manifest, no match: {found:?}");
    }

    /// §8.2.1's stated edge case: the scan root has no containing listing, so
    /// there are no siblings to find a manifest among. Conservative and correct.
    #[test]
    fn target_at_the_scan_root_does_not_match() {
        let found = detect(&tree(dir("/p/target", vec![file("/p/target/a")])), &opts());

        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn node_modules_matches_without_a_marker() {
        let found = detect(
            &tree(dir("/p", vec![dir("/p/node_modules", vec![])])),
            &opts(),
        );

        assert_eq!(categories(&found), vec![Category::NodeModules]);
    }

    #[test]
    fn pycache_matches_directory_and_loose_pyc() {
        let found = detect(
            &tree(dir(
                "/p",
                vec![
                    dir("/p/__pycache__", vec![]),
                    file("/p/stale.pyc"),
                    file("/p/keep.py"),
                ],
            )),
            &opts(),
        );

        assert_eq!(
            paths(&found),
            vec![Path::new("/p/__pycache__"), Path::new("/p/stale.pyc")],
            "the source file must not be touched"
        );
        assert_eq!(
            categories(&found),
            vec![Category::Pycache, Category::Pycache]
        );
    }

    /// The tilde distinction, which is the entire safety of this category:
    /// `~/Library/Caches` is regenerable user data, `/Library/Caches` is the
    /// system's and is denied outright (§8.3).
    #[test]
    fn user_caches_are_scoped_to_home() {
        let options = DetectOptions {
            user_dirs: UserDirs {
                home: Some(PathBuf::from("/home/me")),
                ..UserDirs::default()
            },
            ..opts()
        };

        let found = detect(
            &tree(dir(
                "/",
                vec![
                    dir(
                        "/home/me",
                        vec![
                            dir("/home/me/.cache", vec![]),
                            dir(
                                "/home/me/Library",
                                vec![dir("/home/me/Library/Caches", vec![])],
                            ),
                        ],
                    ),
                    dir("/Library", vec![dir("/Library/Caches", vec![])]),
                ],
            )),
            &options,
        );

        assert_eq!(
            paths(&found),
            vec![
                Path::new("/home/me/.cache"),
                Path::new("/home/me/Library/Caches")
            ],
            "only the user's own caches, never the system's"
        );
    }

    /// A frontend that cannot find a home must get *no* cache matches — an
    /// unknown home is never treated as "any home".
    #[test]
    fn an_unknown_home_matches_no_caches() {
        let found = detect(
            &tree(dir(
                "/",
                vec![dir("/home/me", vec![dir("/home/me/.cache", vec![])])],
            )),
            &opts(),
        );

        assert!(found.is_empty(), "{found:?}");
    }

    /// The scan and the resolved home can reach the same directory spelled
    /// differently — one from `%LOCALAPPDATA%`, the other from walking the disk.
    /// On Windows that must still match, or the cache root is silently missed.
    #[cfg(windows)]
    #[test]
    fn a_windows_cache_root_matches_whatever_its_case() {
        let options = DetectOptions {
            user_dirs: UserDirs {
                local_app_data: Some(PathBuf::from(r"C:\Users\Me\AppData\Local")),
                ..UserDirs::default()
            },
            ..opts()
        };

        let found = detect(
            &tree(dir(
                r"c:\users\me\appdata\local",
                vec![dir(r"c:\users\me\appdata\local\temp", vec![])],
            )),
            &options,
        );

        assert_eq!(categories(&found), vec![Category::UserCaches]);
    }

    /// The other half of that decision. Off Windows the comparison stays exact:
    /// on a case-sensitive volume `.cache` and `.Cache` are two directories, and
    /// treating them as one would put a directory the user never named up for
    /// deletion.
    #[cfg(not(windows))]
    #[test]
    fn case_is_significant_off_windows() {
        let options = DetectOptions {
            user_dirs: UserDirs {
                home: Some(PathBuf::from("/home/me")),
                ..UserDirs::default()
            },
            ..opts()
        };

        let found = detect(
            &tree(dir("/home/me", vec![dir("/home/me/.Cache", vec![])])),
            &options,
        );

        assert!(
            found.is_empty(),
            "`.Cache` is not `.cache` where the filesystem says so: {found:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_temp_is_a_user_cache() {
        let options = DetectOptions {
            user_dirs: UserDirs {
                local_app_data: Some(PathBuf::from(r"C:\Users\me\AppData\Local")),
                ..UserDirs::default()
            },
            ..opts()
        };

        let found = detect(
            &tree(dir(
                r"C:\Users\me\AppData\Local",
                vec![dir(r"C:\Users\me\AppData\Local\Temp", vec![])],
            )),
            &options,
        );

        assert_eq!(categories(&found), vec![Category::UserCaches]);
    }

    /// The rule that keeps a candidate list readable — and a deletion
    /// reviewable. A `node_modules` with 40,000 entries is one thing to remove,
    /// not 40,000 things to read past.
    #[test]
    fn children_of_a_match_are_not_reported_separately() {
        let found = detect(
            &tree(dir(
                "/p",
                vec![dir(
                    "/p/node_modules",
                    vec![
                        file("/p/node_modules/a.pyc"),
                        dir("/p/node_modules/__pycache__", vec![]),
                    ],
                )],
            )),
            &opts(),
        );

        assert_eq!(
            paths(&found),
            vec![Path::new("/p/node_modules")],
            "the subtree is one candidate, whatever else it contains"
        );
    }

    #[test]
    fn a_disabled_category_matches_nothing() {
        let options = DetectOptions {
            categories: CategorySet {
                node_modules: false,
                ..CategorySet::default()
            },
            ..opts()
        };

        let found = detect(
            &tree(dir(
                "/p",
                vec![
                    dir("/p/node_modules", vec![]),
                    dir("/p/__pycache__", vec![]),
                ],
            )),
            &options,
        );

        assert_eq!(
            paths(&found),
            vec![Path::new("/p/__pycache__")],
            "disabling one category must leave the others alone"
        );
    }

    /// D9's default, and the reason its criterion is the *absence* of output: an
    /// always-on age rule would bury the safe-list candidates that are the point
    /// of the feature under confirm-tier noise on the very first run.
    #[test]
    fn age_rule_is_off_without_the_flag() {
        let found = detect(
            &tree(dir("/p", vec![aged(file("/p/ancient.bin"), 10_000 * DAY)])),
            &opts(),
        );

        assert!(
            found.is_empty(),
            "no --older-than means no age candidates at all: {found:?}"
        );
    }

    #[test]
    fn old_file_past_threshold_matches() {
        let options = aging(90 * DAY);

        let found = detect(
            &tree(dir(
                "/p",
                vec![
                    aged(file("/p/stale.bin"), 91 * DAY),
                    aged(file("/p/fresh.bin"), 89 * DAY),
                ],
            )),
            &options,
        );

        assert_eq!(paths(&found), vec![Path::new("/p/stale.bin")]);
        assert_eq!(categories(&found), vec![Category::Old]);
    }

    /// "Older **or exactly equal**" (§8.2.2 step 2). Exercised with a file one
    /// second either side as well, so the assertion pins the boundary rather
    /// than merely the direction.
    #[test]
    fn threshold_boundary_is_inclusive() {
        let options = aging(90 * DAY);

        let found = detect(
            &tree(dir(
                "/p",
                vec![
                    aged(file("/p/exactly.bin"), 90 * DAY),
                    aged(
                        file("/p/just-younger.bin"),
                        90 * DAY - Duration::from_secs(1),
                    ),
                    aged(file("/p/just-older.bin"), 90 * DAY + Duration::from_secs(1)),
                ],
            )),
            &options,
        );

        assert_eq!(
            paths(&found),
            vec![Path::new("/p/exactly.bin"), Path::new("/p/just-older.bin")],
            "the file exactly at the threshold matches; the one a second younger does not"
        );
    }

    /// A directory is judged on its own mtime. Here the directory is fresh and
    /// its contents ancient: nothing matches, because the directory's timestamp
    /// says something inside it changed recently.
    #[test]
    fn directory_age_is_its_own() {
        let options = aging(90 * DAY);

        let found = detect(
            &tree(dir(
                "/p",
                vec![dir(
                    "/p/recent",
                    vec![aged(file("/p/recent/ancient.bin"), 900 * DAY)],
                )],
            )),
            &options,
        );

        assert_eq!(
            paths(&found),
            vec![Path::new("/p/recent/ancient.bin")],
            "the fresh directory is not old, but the stale file inside it is"
        );
    }

    /// An entry whose timestamp the platform never recorded must not be guessed
    /// at — every entry on a filesystem that reports none would otherwise become
    /// a deletion candidate the moment `--older-than` was passed.
    #[test]
    fn unknown_timestamp_never_matches() {
        let options = aging(DAY);
        let mut unknown = file("/p/mystery.bin");
        unknown.modified = None;

        let found = detect(&tree(dir("/p", vec![unknown])), &options);

        assert!(found.is_empty(), "{found:?}");
    }

    /// The nesting rule applied to age. An old directory full of old files is
    /// one candidate: reporting the children too would let the caller's total
    /// count the same bytes more than once, since a directory's `allocated`
    /// already covers its subtree.
    #[test]
    fn an_old_directory_does_not_also_report_its_old_children() {
        let options = aging(90 * DAY);

        let found = detect(
            &tree(dir(
                "/p",
                vec![aged(
                    dir(
                        "/p/stale",
                        vec![aged(file("/p/stale/inner.bin"), 900 * DAY)],
                    ),
                    900 * DAY,
                )],
            )),
            &options,
        );

        assert_eq!(paths(&found), vec![Path::new("/p/stale")]);
        assert_eq!(
            found[0].allocated,
            4096 + 4096,
            "the match carries the whole subtree's bytes"
        );
    }

    /// A junk match wins over the age rule on the same node, so the category a
    /// candidate reports is the specific one — which is what decides its tier in
    /// the next task, auto rather than confirm.
    #[test]
    fn a_junk_match_beats_the_age_rule() {
        let options = aging(90 * DAY);

        let found = detect(
            &tree(dir(
                "/p",
                vec![
                    file("/p/Cargo.toml"),
                    aged(dir("/p/target", vec![]), 900 * DAY),
                ],
            )),
            &options,
        );

        assert_eq!(categories(&found), vec![Category::RustTarget]);
    }

    /// Disabling a category must not silently reclassify its directories as
    /// `Old` — that would turn a category toggle into a tier change.
    #[test]
    fn a_disabled_category_can_still_be_claimed_by_age() {
        let options = DetectOptions {
            categories: CategorySet::none(),
            ..aging(90 * DAY)
        };

        let found = detect(
            &tree(dir(
                "/p",
                vec![aged(dir("/p/node_modules", vec![]), 900 * DAY)],
            )),
            &options,
        );

        assert_eq!(
            categories(&found),
            vec![Category::Old],
            "with its own category off, an ancient node_modules is merely old"
        );
    }

    #[test]
    fn an_empty_tree_yields_nothing() {
        let found = detect(&tree(dir("/p", vec![])), &opts());

        assert!(found.is_empty(), "{found:?}");
    }
}
