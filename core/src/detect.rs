//! Deciding what *looks* disposable.
//!
//! The rules themselves live in [`crate::rules`]; this is the one pass that
//! applies them. It runs over the finished [`ScanTree`], not during the walk: a
//! node's siblings are exactly what marker-aware detection needs ("is there a
//! `Cargo.toml` beside this `target/`?"), and the tree already groups them. The
//! hot parallel walk stays untouched.
//!
//! Everything here is a **pure function of its inputs**. No filesystem, no
//! clock, no environment — the current time is passed in and the user's
//! directories were resolved before the rules were compiled, the way
//! [`crate::ScanOptions`] already promises. A rule that consulted the
//! environment could not be tested with a temporary directory standing in for a
//! home, and for code whose output is later fed to a delete operation that is not
//! a trade worth making.
//!
//! Nothing here removes, ranks or excludes anything. It reports what matched;
//! the denylist, the tiers and the totals belong to the cleanup engine.

use crate::rules::Facts;
use crate::rules::{Rules, UserDirs};
use crate::tree::{ScanNode, ScanTree};
use globset::Candidate;
use std::path::PathBuf;
use std::time::SystemTime;

/// Everything the rules need, all of it explicit.
#[derive(Debug, Clone)]
pub struct DetectOptions {
    /// The compiled rule list. Its **order is its precedence**.
    pub rules: Rules,

    /// What "now" is, for any rule carrying an `older_than`.
    ///
    /// Not an `Option` and not a call to [`SystemTime::now`]. v0.2 paired the
    /// threshold with the clock in one `Age` struct so that a threshold without
    /// an instant to measure from — armed-looking and matching nothing — was
    /// unrepresentable. With thresholds now living inside individual rules that
    /// pairing is gone, and a mandatory clock is what restores the guarantee.
    pub now: SystemTime,
}

impl Default for DetectOptions {
    /// The built-in rules and no home.
    ///
    /// Matches what a `clean` run gets before any config is read: the three
    /// unrooted safe-list rules apply, and the two cache rules — which need to
    /// know where the user lives — are dropped.
    fn default() -> Self {
        DetectOptions {
            rules: Rules::builtin(&UserDirs::default()),
            now: SystemTime::UNIX_EPOCH,
        }
    }
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

    /// The name of the rule that claimed it — which is how the plan later finds
    /// its tier, its `min_size` and whether it wants a clean repository.
    pub rule: String,

    pub allocated: u64,
}

/// Find everything the rules claim.
///
/// One depth-first pass. At each node the rules are tried in list order and the
/// first whose every predicate holds takes it.
///
/// **A match is never descended into.** Once `node_modules/` is claimed, its
/// 40,000 children are not 40,000 further candidates: the subtree is one thing
/// to delete, and reporting its contents separately would be both useless and
/// dangerous. That is what lets the caller sum candidate sizes without counting
/// the same bytes twice.
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
    // A directory under no rule's territory: neither it nor anything beneath it
    // can match, so the whole subtree goes unvisited. One comparison here
    // replaces a glob match on every file below.
    if node.is_dir && options.rules.prunes(&node.path) {
        return;
    }

    if let Some(rule) = claim(node, siblings, options) {
        found.push(Detection {
            path: node.path.clone(),
            rule,
            allocated: node.allocated,
        });
        return; // the subtree is this one candidate
    }

    for child in &node.children {
        visit(child, &node.children, options, found);
    }
}

/// Which rule, if any, claims this node.
///
/// The candidate is built **once** and reused across every pattern — it
/// precomputes the basename and, on Windows, the lowercased form, which is
/// precisely the work that must not be repeated per rule.
///
/// A rule that matches by glob but fails a later predicate does not abandon the
/// node: the next-lowest rule is tried. Otherwise disabling one rule's marker
/// requirement would silently shadow every rule beneath it.
fn claim(node: &ScanNode, siblings: &[ScanNode], options: &DetectOptions) -> Option<String> {
    let candidate = Candidate::new(node.path.as_path());
    // The predicates live on `Rules` so that `Rules::state` — the colour the TUI
    // paints — is decided by the same code. A directory shown as junk and a
    // directory offered for removal must not be different sets.
    let facts = Facts {
        is_dir: node.is_dir,
        modified: node.modified,
        now: options.now,
        has_sibling: &|name| has_sibling(siblings, name),
    };

    for index in options.rules.matching(&candidate, node.is_dir) {
        if options.rules.excluded(index, &candidate) {
            continue;
        }
        if !options.rules.predicates_hold(index, &facts) {
            continue;
        }

        return Some(options.rules.rule_at(index).name.clone());
    }

    None
}

fn has_sibling(siblings: &[ScanNode], name: &str) -> bool {
    siblings
        .iter()
        .any(|sibling| sibling.path.file_name().is_some_and(|n| n == name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{Rule, Tier, age_rule, builtin_rules};
    use std::path::Path;
    use std::time::Duration;

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

    fn compiled(rules: Vec<Rule>, dirs: &UserDirs) -> DetectOptions {
        DetectOptions {
            rules: Rules::new(rules, dirs).expect("compile"),
            now: now(),
        }
    }

    /// Every built-in rule, the age rule off — the default a first run gets.
    fn opts() -> DetectOptions {
        DetectOptions {
            now: now(),
            ..DetectOptions::default()
        }
    }

    fn with_dirs(dirs: UserDirs) -> DetectOptions {
        compiled(builtin_rules(), &dirs)
    }

    fn home(path: &str) -> UserDirs {
        UserDirs {
            home: Some(PathBuf::from(path)),
            ..UserDirs::default()
        }
    }

    /// The built-ins plus the age rule, appended last as `--older-than` does.
    fn aging(older_than: Duration) -> DetectOptions {
        let mut rules = builtin_rules();
        rules.push(age_rule(older_than));
        compiled(rules, &UserDirs::default())
    }

    /// Just the paths, for asserting on a whole result at once.
    fn paths(found: &[Detection]) -> Vec<&Path> {
        found.iter().map(|m| m.path.as_path()).collect()
    }

    fn rules_of(found: &[Detection]) -> Vec<&str> {
        found.iter().map(|m| m.rule.as_str()).collect()
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
        assert_eq!(rules_of(&found), vec!["rust-target"]);
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

    /// The scan root has no containing listing, so there are no siblings to find
    /// a manifest among. Conservative and correct.
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

        assert_eq!(rules_of(&found), vec!["node-modules"]);
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
        assert_eq!(rules_of(&found), vec!["pycache", "pycache"]);
    }

    /// The tilde distinction, which is the entire safety of this category:
    /// `~/Library/Caches` is regenerable user data, `/Library/Caches` is the
    /// system's and is denied outright.
    #[test]
    fn user_caches_are_scoped_to_home() {
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
            &with_dirs(home("/home/me")),
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
        let found = detect(
            &tree(dir(
                r"c:\users\me\appdata\local",
                vec![dir(r"c:\users\me\appdata\local\temp", vec![])],
            )),
            &with_dirs(UserDirs {
                local_app_data: Some(PathBuf::from(r"C:\Users\Me\AppData\Local")),
                ..UserDirs::default()
            }),
        );

        assert_eq!(rules_of(&found), vec!["windows-temp"]);
    }

    /// The other half of that decision. Off Windows the comparison stays exact:
    /// on a case-sensitive volume `.cache` and `.Cache` are two directories, and
    /// treating them as one would put a directory the user never named up for
    /// deletion.
    #[cfg(not(windows))]
    #[test]
    fn case_is_significant_off_windows() {
        let found = detect(
            &tree(dir("/home/me", vec![dir("/home/me/.Cache", vec![])])),
            &with_dirs(home("/home/me")),
        );

        assert!(
            found.is_empty(),
            "`.Cache` is not `.cache` where the filesystem says so: {found:?}"
        );
    }

    /// `%LOCALAPPDATA%\Temp` is its own rule rather than part of `user-caches`:
    /// the two are anchored to different roots, and one rule cannot have two.
    #[cfg(windows)]
    #[test]
    fn windows_temp_is_a_user_cache() {
        let found = detect(
            &tree(dir(
                r"C:\Users\me\AppData\Local",
                vec![dir(r"C:\Users\me\AppData\Local\Temp", vec![])],
            )),
            &with_dirs(UserDirs {
                local_app_data: Some(PathBuf::from(r"C:\Users\me\AppData\Local")),
                ..UserDirs::default()
            }),
        );

        assert_eq!(rules_of(&found), vec!["windows-temp"]);
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
    fn a_disabled_rule_matches_nothing() {
        let rules = builtin_rules()
            .into_iter()
            .map(|rule| Rule {
                enabled: rule.name != "node-modules",
                ..rule
            })
            .collect();

        let found = detect(
            &tree(dir(
                "/p",
                vec![
                    dir("/p/node_modules", vec![]),
                    dir("/p/__pycache__", vec![]),
                ],
            )),
            &compiled(rules, &UserDirs::default()),
        );

        assert_eq!(
            paths(&found),
            vec![Path::new("/p/__pycache__")],
            "disabling one rule must leave the others alone"
        );
    }

    /// The age rule is off unless asked for, and the reason its criterion is the
    /// *absence* of output: an always-on age rule would bury the safe-list
    /// candidates that are the point of the feature under confirm-tier noise on
    /// the very first run.
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
        let found = detect(
            &tree(dir(
                "/p",
                vec![
                    aged(file("/p/stale.bin"), 91 * DAY),
                    aged(file("/p/fresh.bin"), 89 * DAY),
                ],
            )),
            &aging(90 * DAY),
        );

        assert_eq!(paths(&found), vec![Path::new("/p/stale.bin")]);
        assert_eq!(rules_of(&found), vec!["old"]);
    }

    /// "Older **or exactly equal**". Exercised with a file one second either side
    /// as well, so the assertion pins the boundary rather than merely the
    /// direction.
    #[test]
    fn threshold_boundary_is_inclusive() {
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
            &aging(90 * DAY),
        );

        assert_eq!(
            paths(&found),
            vec![Path::new("/p/exactly.bin"), Path::new("/p/just-older.bin")],
            "the file exactly at the threshold matches; the one a second younger does not"
        );
    }

    /// A directory is judged on its own mtime. Here the directory is fresh and
    /// its contents ancient: the directory does not match, because its timestamp
    /// says something inside it changed recently — and the pass descends.
    #[test]
    fn directory_age_is_its_own() {
        let found = detect(
            &tree(dir(
                "/p",
                vec![dir(
                    "/p/recent",
                    vec![aged(file("/p/recent/ancient.bin"), 900 * DAY)],
                )],
            )),
            &aging(90 * DAY),
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
        let mut unknown = file("/p/mystery.bin");
        unknown.modified = None;

        let found = detect(&tree(dir("/p", vec![unknown])), &aging(DAY));

        assert!(found.is_empty(), "{found:?}");
    }

    /// The nesting rule applied to age. An old directory full of old files is
    /// one candidate: reporting the children too would let the caller's total
    /// count the same bytes more than once, since a directory's `allocated`
    /// already covers its subtree.
    #[test]
    fn an_old_directory_does_not_also_report_its_old_children() {
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
            &aging(90 * DAY),
        );

        assert_eq!(paths(&found), vec![Path::new("/p/stale")]);
        assert_eq!(
            found[0].allocated,
            4096 + 4096,
            "the match carries the whole subtree's bytes"
        );
    }

    /// List order is precedence, and this is what it buys: the age rule is
    /// appended last, so a junk rule that also matches names the candidate — and
    /// therefore decides its tier, auto rather than confirm.
    #[test]
    fn a_junk_match_beats_the_age_rule() {
        let found = detect(
            &tree(dir(
                "/p",
                vec![
                    file("/p/Cargo.toml"),
                    aged(dir("/p/target", vec![]), 900 * DAY),
                ],
            )),
            &aging(90 * DAY),
        );

        assert_eq!(rules_of(&found), vec!["rust-target"]);
    }

    /// Disabling a rule must not silently reclassify its directories as `old` by
    /// accident — but it must not shield them either. With its own rule off, an
    /// ancient `node_modules` is merely old, and its tier changes with it.
    #[test]
    fn a_disabled_rule_can_still_be_claimed_by_age() {
        let mut rules: Vec<Rule> = builtin_rules()
            .into_iter()
            .map(|rule| Rule {
                enabled: false,
                ..rule
            })
            .collect();
        rules.push(age_rule(90 * DAY));

        let found = detect(
            &tree(dir(
                "/p",
                vec![aged(dir("/p/node_modules", vec![]), 900 * DAY)],
            )),
            &compiled(rules, &UserDirs::default()),
        );

        assert_eq!(rules_of(&found), vec!["old"]);
    }

    /// A rule that matches by glob but fails a later predicate must hand the node
    /// on rather than swallow it. Otherwise `rust-target`'s missing manifest
    /// would shadow every rule written beneath it.
    #[test]
    fn a_failed_predicate_falls_through_to_the_next_rule() {
        let rules = vec![
            Rule {
                name: "needs-marker".into(),
                includes: vec!["**/target/".into()],
                requires_sibling: vec!["Cargo.toml".into()],
                tier: Tier::Auto,
                ..Rule::default()
            },
            Rule {
                name: "catch-all".into(),
                includes: vec!["**/target/".into()],
                ..Rule::default()
            },
        ];

        let found = detect(
            &tree(dir("/p", vec![dir("/p/target", vec![])])),
            &compiled(rules, &UserDirs::default()),
        );

        assert_eq!(rules_of(&found), vec!["catch-all"]);
    }

    /// A rule's own exclusion, likewise: it declines the node, and the next rule
    /// gets its turn.
    #[test]
    fn an_exclusion_hands_the_node_to_the_next_rule() {
        let rules = vec![
            Rule {
                name: "narrow".into(),
                includes: vec!["**/node_modules/".into()],
                excludes: vec!["**/vendor/**".into()],
                tier: Tier::Auto,
                ..Rule::default()
            },
            Rule {
                name: "wide".into(),
                includes: vec!["**/node_modules/".into()],
                ..Rule::default()
            },
        ];
        let options = compiled(rules, &UserDirs::default());

        let inside = detect(
            &tree(dir(
                "/p/vendor",
                vec![dir("/p/vendor/node_modules", vec![])],
            )),
            &options,
        );
        assert_eq!(rules_of(&inside), vec!["wide"]);

        let outside = detect(
            &tree(dir("/p/app", vec![dir("/p/app/node_modules", vec![])])),
            &options,
        );
        assert_eq!(rules_of(&outside), vec!["narrow"]);
    }

    /// A rule below its own `min_size` is still claimed here and still not
    /// descended into. Dropping the match instead would send the pass inside a
    /// `__pycache__`, turning one skipped candidate into a hundred tiny ones —
    /// which is why the threshold belongs to `plan` and not to this pass.
    #[test]
    fn min_size_does_not_block_the_match() {
        let rules = vec![Rule {
            name: "small".into(),
            includes: vec!["**/__pycache__/".into()],
            min_size: 1_048_576,
            tier: Tier::Auto,
            ..Rule::default()
        }];

        let found = detect(
            &tree(dir(
                "/p",
                vec![dir("/p/__pycache__", vec![file("/p/__pycache__/a.pyc")])],
            )),
            &compiled(rules, &UserDirs::default()),
        );

        assert_eq!(
            paths(&found),
            vec![Path::new("/p/__pycache__")],
            "claimed whole, and its contents never offered separately"
        );
    }

    /// Pruning must never cost a candidate. A rule rooted deep in the tree still
    /// has to be reached, so every directory on the way to it is walked.
    #[test]
    fn a_rooted_rule_is_still_reached_through_unrelated_directories() {
        let rules = vec![Rule {
            name: "scoped".into(),
            root: Some("/home/me/Projects".into()),
            includes: vec!["**/node_modules/".into()],
            tier: Tier::Auto,
            ..Rule::default()
        }];

        let found = detect(
            &tree(dir(
                "/",
                vec![dir(
                    "/home",
                    vec![dir(
                        "/home/me",
                        vec![
                            dir(
                                "/home/me/Projects",
                                vec![dir("/home/me/Projects/node_modules", vec![])],
                            ),
                            dir(
                                "/home/me/Downloads",
                                vec![dir("/home/me/Downloads/node_modules", vec![])],
                            ),
                        ],
                    )],
                )],
            )),
            &compiled(rules, &UserDirs::default()),
        );

        assert_eq!(
            paths(&found),
            vec![Path::new("/home/me/Projects/node_modules")],
            "reached through /home and /home/me; the sibling tree is out of scope"
        );
    }

    #[test]
    fn an_empty_tree_yields_nothing() {
        let found = detect(&tree(dir("/p", vec![])), &opts());

        assert!(found.is_empty(), "{found:?}");
    }
}
