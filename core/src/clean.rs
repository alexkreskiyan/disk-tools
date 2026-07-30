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

use crate::detect::{DetectOptions, detect};
use crate::git;
use crate::paths::{is_within, normalize_lexically, under_root};
use crate::rules::{Rule, UserDirs};
use crate::tree::{ScanNode, ScanTree};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use crate::rules::Tier;

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

/// One thing the plan proposes to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Candidate {
    pub path: PathBuf,

    /// The name of the rule that claimed it. v0.2 carried a `Category` enum
    /// here; rules are open-ended, so an enum can no longer name them.
    pub rule: String,

    /// What the **rule** says to do with it, untouched by any flag.
    ///
    /// `--safe` and the frontend's refusal both read this, and they must: a
    /// `--purge` that rewrote it would cancel a confirmation it has nothing to
    /// do with. Where the candidate actually goes is [`Self::purge`].
    pub tier: Tier,

    /// This one is deleted outright rather than trashed.
    ///
    /// The rule's tier and `--purge` resolved together — the only new fact
    /// either produces, and the reason `apply` needs nothing but the plan.
    pub purge: bool,

    /// Attributed bytes — what removing this would free, before the question
    /// [`Self::shared`] asks.
    pub allocated: u64,

    /// The copy kept instead of this one, when this candidate is a duplicate.
    ///
    /// `None` for everything a rule claimed — the two sources of a candidate,
    /// told apart by the one fact that only one of them has.
    ///
    /// Carried on the candidate rather than in a table beside the plan so that
    /// [`CleanPlan::merge`] stays a concatenation: group indices would have to
    /// be renumbered per plan, and the second root is where that goes wrong.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    #[cfg_attr(feature = "serde", serde(default))]
    pub duplicate_of: Option<PathBuf>,

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
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
pub enum ExcludeReason {
    /// On the never-touch denylist. No flag overrides this.
    Denylisted,

    /// Build output whose project has uncommitted work — you may be mid-change,
    /// and it only regenerates identically from committed source.
    ///
    /// Settled by the rule's own `requires-clean-repo`; no flag overrides it.
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

    /// How many candidates `--safe` kept out; `0` when it was not in effect.
    ///
    /// Not an [`Excluded`], deliberately: that list is for the tool *refusing*
    /// something, and `--safe` is the user's own narrowing. But they still need
    /// to know something was there, and the alternative — planning a second time
    /// without the flag — costs a full extra pass of the git guard, which is by
    /// far the most expensive thing here (measured at ~23 ms per repository).
    /// A count is what was wanted; a count is what this is.
    pub filtered_out: usize,

    /// How many candidates fell below [`CleanOptions::min_size`] — the flag.
    ///
    /// Counted separately from `filtered_out` because the two are different
    /// answers to "why is it not here": one needs confirmation, the other is
    /// small. Merging them would let the report offer `--safe` as the remedy for
    /// something `--safe` had nothing to do with.
    pub too_small: usize,

    /// How many fell below the **rule's** own `min_size` instead.
    ///
    /// Split from `too_small` for exactly the reason `too_small` is split from
    /// `filtered_out`: the remedy differs. One is answered by lowering a flag,
    /// the other by editing the rule in the config file, and a report naming
    /// `--min-size` for a threshold the user never passed on the command line
    /// sends them to change something they never set.
    ///
    /// A candidate below **both** counts here, since the rule is the narrower
    /// statement and the one the user would have to go and find.
    pub below_rule_minimum: usize,
}

impl CleanPlan {
    /// Combine the plans of several disjoint roots into one.
    ///
    /// `clean` with no path walks a root per rule, and each walk produces its own
    /// plan. Merging the **plans** rather than the trees is what avoids inventing
    /// a synthetic node above them — one that would need a path and a size it
    /// does not have.
    ///
    /// Every field is additive, and safely so **only because the roots do not
    /// overlap**: [`crate::Rules::scan_roots`] drops any root that lies inside
    /// another, so no path can appear in two plans and no byte is summed twice.
    /// Handed overlapping roots this would double both.
    ///
    /// Candidates are re-sorted, since concatenating two sorted lists does not
    /// give a sorted one — and a plan whose order shifts between runs cannot be
    /// reviewed.
    pub fn merge(plans: Vec<CleanPlan>) -> CleanPlan {
        let mut merged = CleanPlan::default();

        for plan in plans {
            merged.candidates.extend(plan.candidates);
            merged.excluded.extend(plan.excluded);
            merged.reclaimable += plan.reclaimable;
            merged.filtered_out += plan.filtered_out;
            merged.too_small += plan.too_small;
            merged.below_rule_minimum += plan.below_rule_minimum;
        }

        merged
            .candidates
            .sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));
        merged
            .excluded
            .sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));
        merged
    }
}

/// Everything a cleanup needs to know.
#[derive(Debug, Clone, Default)]
pub struct CleanOptions {
    /// Which rules run, and what "now" is for the ones that ask about age.
    pub detect: DetectOptions,

    /// Where this user's directories are.
    ///
    /// Kept here rather than inside [`DetectOptions`] because the **denylist**
    /// is what still needs it — `~/Library/Application Support` and `%APPDATA%`
    /// are never-touch roots that only a known home can locate. Detection
    /// resolved its own roots when the rules were compiled and no longer asks.
    pub user_dirs: UserDirs,

    /// `--safe`: admit auto-tier candidates only.
    pub safe_only: bool,

    /// `--purge`: send the whole plan past the trash, whatever each rule says.
    ///
    /// Flag beats file, as everywhere. A `--purge` that honoured a rule's
    /// `trash` would be a flag that lies about its own name. It is read **after**
    /// `safe_only`, so it can never turn something that needed confirming into
    /// something that does not.
    pub purge_all: bool,

    /// `--min-size`: do not offer anything smaller than this.
    ///
    /// **Unlike the scan's flag of the same name, this narrows the plan itself**
    /// rather than only the printout. It has to: the cleanup report is not a
    /// view of a tree, it is the list of what `--apply` will remove, and a report
    /// showing two entries while the removal takes a hundred and fifty is the
    /// exact mismatch every other rule here exists to prevent.
    ///
    /// `reclaimable` therefore drops with it, which is correct — it is what
    /// *would* be freed, not what might have been.
    pub min_size: u64,
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
/// 4. each survivor takes the tier its rule declares;
/// 5. `--safe` keeps only auto-tier;
/// 6. candidates whose content is reachable from outside them are marked, so
///    the total can be read as the upper bound it is.
///
/// Steps 2 and 3 come before 5 because they are the tool *refusing*, which the
/// user needs told; step 5 is the user's own narrowing, which they already know
/// about.
pub fn plan(tree: &ScanTree, options: &CleanOptions) -> CleanPlan {
    let denied = denylist(&options.user_dirs);
    let groups = link_groups_by_path(tree);
    let nodes = nodes_by_path(tree);

    let mut candidates = Vec::new();
    let mut excluded = Vec::new();
    let mut filtered_out = 0;
    let mut too_small = 0;
    let mut below_rule_minimum = 0;
    // One `git status` per repository, not per candidate: a tree of sibling
    // Rust projects would otherwise spawn a process for each, and a workspace
    // whose members share a repository would ask the same question repeatedly.
    let mut repos: HashMap<PathBuf, git::RepoState> = HashMap::new();

    for detection in detect(tree, &options.detect) {
        // The rule that claimed it decides the three questions below. It is
        // always present — `detect` only ever names a rule it matched with — but
        // a missing one must not panic in a delete path, so it reads as the
        // cautious default: confirm tier, no threshold, no guard.
        let rule = options.detect.rules.get(&detection.rule);

        if is_denied(&detection.path, &denied) {
            excluded.push(Excluded {
                path: detection.path,
                reason: ExcludeReason::Denylisted,
            });
            continue;
        }

        if is_mid_change(&detection.path, rule, &mut repos) {
            excluded.push(Excluded {
                path: detection.path,
                reason: ExcludeReason::DirtyRepo,
            });
            continue;
        }

        // The user's own narrowings, all after the refusals above so that a
        // denied or guarded path is still reported as such.
        //
        // Both thresholds apply and the larger wins, but *which* one dropped it
        // is counted apart, because the remedy differs: lower the flag, or go
        // and edit the rule. The rule is checked first, since it is the narrower
        // statement and the harder one to find.
        let rule_minimum = rule.map_or(0, |rule| rule.min_size);
        if detection.allocated < rule_minimum {
            below_rule_minimum += 1;
            continue;
        }
        if detection.allocated < options.min_size {
            too_small += 1;
            continue;
        }

        let tier = rule.map_or(Tier::Confirm, |rule| rule.tier);
        // Not recorded in `excluded`, deliberately. That list answers "the tool
        // refused something you might have expected"; `--safe` is the user's own
        // narrowing, and putting the two in one list would show a protected
        // system directory beside something they asked to hide. The frontend
        // knows the flag was passed and can say so itself.
        //
        // **Asked of the rule's own tier**, before `--purge` is applied below.
        // `--safe` means "drop what needs confirming", and `purge` needs none —
        // it is a stronger claim of regenerability than `trash`, not a weaker
        // one.
        if options.safe_only && tier.needs_confirming() {
            filtered_out += 1;
            continue;
        }

        let shared = nodes
            .get(detection.path.as_path())
            .is_some_and(|node| holds_shared_content(node, &detection.path, &groups));

        candidates.push(Candidate {
            path: detection.path,
            rule: detection.rule,
            tier,
            // The rule's tier and the flag, resolved once and written down, so
            // that the plan says what will happen and `apply` needs no second
            // input to work it out.
            purge: tier == Tier::Purge || options.purge_all,
            duplicate_of: None,
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
        filtered_out,
        too_small,
        below_rule_minimum,
    }
}

/// The name every duplicate candidate carries where a rule-claimed one carries
/// the rule that claimed it.
///
/// Reserved in the config file, so a report naming it can only ever mean this.
pub const DUPLICATE_RULE: &str = "duplicate";

/// Decide which redundant copies may go, and what that would free.
///
/// The counterpart to [`plan`] for the other source of candidates, producing the
/// **same** [`CleanPlan`] — so `preview` and `clean` keep speaking about one
/// thing and `apply` needs no idea that duplicates exist.
///
/// What it keeps from [`plan`], because these are properties of removing
/// anything at all rather than of rules:
///
/// - the **denylist**, unconditional and reported;
/// - `--safe`, `--purge` and `--min-size`, counted the same way;
/// - `shared`, so a copy whose inode has another name is not sold as free bytes.
///
/// What it drops: the git guard, which asks whether build output would rebuild
/// identically from committed source — a question that says nothing about a
/// photograph. `below_rule_minimum` stays 0 for the same reason: there is no
/// rule here to have a minimum.
///
/// **Writes nothing**, like everything else in this module. The tree is needed
/// only to answer the `shared` question, which is about inodes rather than
/// content.
#[cfg(feature = "duplicates")]
pub fn plan_duplicates(
    tree: &ScanTree,
    found: &crate::duplicates::Duplicates,
    options: &CleanOptions,
) -> CleanPlan {
    let denied = denylist(&options.user_dirs);
    let groups = link_groups_by_path(tree);
    let nodes = nodes_by_path(tree);

    let mut candidates = Vec::new();
    let mut excluded = Vec::new();
    let mut filtered_out = 0;
    let mut too_small = 0;

    for group in &found.groups {
        for copy in &group.copies {
            if is_denied(&copy.path, &denied) {
                excluded.push(Excluded {
                    path: copy.path.clone(),
                    reason: ExcludeReason::Denylisted,
                });
                continue;
            }

            if copy.allocated < options.min_size {
                too_small += 1;
                continue;
            }

            // Every duplicate is confirm-tier: the concept's own rule, that a
            // duplicate is never removed without being looked at. So `--safe`,
            // which means "drop what needs confirming", drops all of them —
            // and it is asked here, before `--purge`, exactly as in `plan`.
            if options.safe_only {
                filtered_out += 1;
                continue;
            }

            let shared = nodes
                .get(copy.path.as_path())
                .is_some_and(|node| holds_shared_content(node, &copy.path, &groups));

            candidates.push(Candidate {
                path: copy.path.clone(),
                rule: DUPLICATE_RULE.to_string(),
                tier: Tier::Confirm,
                // `--purge` decides the destination and nothing else; the tier
                // above stays what it is, so the confirmation it demands cannot
                // be cancelled by a flag about where files go.
                purge: options.purge_all,
                duplicate_of: Some(group.keeper.clone()),
                allocated: copy.allocated,
                shared,
            });
        }
    }

    candidates.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));
    excluded.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));

    // Each candidate is one file and no file is in two groups, so nothing here
    // nests and no byte is summed twice — the property `plan` gets from `detect`
    // never descending into a match.
    let reclaimable = candidates.iter().map(|c| c.allocated).sum();

    CleanPlan {
        candidates,
        reclaimable,
        excluded,
        filtered_out,
        too_small,
        below_rule_minimum: 0,
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
/// Scoped to the rules that ask for it — `rust-target` is the only built-in one.
/// "You may be mid-change" is a statement about a project, and a cache does not
/// belong to one; applying the guard everywhere would refuse to clean `~/.cache`
/// because some unrelated repository above it was dirty.
///
/// v0.2 answered this from the category enum. With open-ended rules there is no
/// enum to ask, so each rule states it — which also means a user writing a rule
/// for their own build output can have the same protection.
///
/// A rule that does not ask short-circuits **before** the filesystem is touched,
/// so the common case costs nothing rather than merely ignoring the answer.
/// There is no flag that overrides this: `--allow-dirty` went in v0.5, because
/// the guard belongs to the rule that wants it and a global switch would be a
/// coarser duplicate.
///
/// `repos` memoises the verdict per repository, since several candidates often
/// share one.
fn is_mid_change(
    path: &Path,
    rule: Option<&Rule>,
    repos: &mut HashMap<PathBuf, git::RepoState>,
) -> bool {
    if !rule.is_some_and(|rule| rule.requires_clean_repo) {
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
    use crate::rules::{Rules, age_rule, builtin_rules};
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

    /// The built-in rules against a `UserDirs` — the whole of what a `clean` run
    /// gets before any config is read.
    fn with(dirs: UserDirs, rules: Vec<Rule>) -> CleanOptions {
        CleanOptions {
            detect: DetectOptions {
                rules: Rules::new(rules, &dirs).expect("compile"),
                now: now(),
            },
            user_dirs: dirs,
            ..CleanOptions::default()
        }
    }

    fn opts() -> CleanOptions {
        with(UserDirs::default(), builtin_rules())
    }

    /// Options with a home, which is what makes the user-scoped rules — both the
    /// `user-caches` rule and the denylist's `Application Support` — live.
    fn with_home(home: &str) -> CleanOptions {
        with(
            UserDirs {
                home: Some(PathBuf::from(home)),
                ..UserDirs::default()
            },
            builtin_rules(),
        )
    }

    /// The built-ins plus the age rule, appended last as `--older-than` does.
    fn aging(older_than: Duration) -> CleanOptions {
        let mut rules = builtin_rules();
        rules.push(age_rule(older_than));
        with(UserDirs::default(), rules)
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
        let options = with(
            UserDirs {
                app_data: Some(PathBuf::from("/users/me/AppData/Roaming")),
                ..UserDirs::default()
            },
            builtin_rules(),
        );

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

    // ---- the three tiers -------------------------------------------------

    /// A rule set with one rule at each tier, over the same three names.
    fn tiered() -> CleanOptions {
        let rules = Rules::new(
            vec![
                Rule {
                    name: "destroyed".into(),
                    includes: vec!["**/node_modules/".into()],
                    tier: Tier::Purge,
                    ..Rule::default()
                },
                Rule {
                    name: "recovered".into(),
                    includes: vec!["**/target/".into()],
                    tier: Tier::Trash,
                    ..Rule::default()
                },
                Rule {
                    name: "asked".into(),
                    includes: vec!["**/notes/".into()],
                    tier: Tier::Confirm,
                    ..Rule::default()
                },
            ],
            &UserDirs::default(),
        )
        .expect("compiles");

        CleanOptions {
            detect: DetectOptions {
                rules,
                now: SystemTime::UNIX_EPOCH,
            },
            ..opts()
        }
    }

    fn three_tiers() -> ScanTree {
        tree(dir(
            "/p",
            vec![
                dir("/p/node_modules", vec![]),
                dir("/p/target", vec![]),
                dir("/p/notes", vec![]),
            ],
        ))
    }

    /// Where a candidate goes is decided here, so that `apply` needs nothing
    /// but the plan and `preview` can print exactly what will happen.
    #[test]
    fn the_plan_says_where_each_candidate_goes() {
        let plan = plan(&three_tiers(), &tiered());
        let fate = |rule: &str| {
            plan.candidates
                .iter()
                .find(|c| c.rule == rule)
                .unwrap_or_else(|| panic!("{rule} is in the plan"))
                .purge
        };

        assert!(
            fate("destroyed"),
            "a purge-tier rule destroys its candidates"
        );
        assert!(!fate("recovered"));
        assert!(!fate("asked"));
    }

    /// `--safe` drops what needs confirming, which is the whole of what it
    /// means. Purge is a **stronger** claim of regenerability than trash, not a
    /// weaker one, so it survives.
    #[test]
    fn safe_keeps_purge_and_trash_and_drops_confirm() {
        let plan = plan(
            &three_tiers(),
            &CleanOptions {
                safe_only: true,
                ..tiered()
            },
        );

        let rules: Vec<&str> = plan.candidates.iter().map(|c| c.rule.as_str()).collect();
        assert_eq!(rules, ["destroyed", "recovered"]);
        assert_eq!(plan.filtered_out, 1);
    }

    /// The flag says "all of them", and beats every rule's tier — a `--purge`
    /// that honoured a `trash` would be a flag that lies about its own name.
    #[test]
    fn the_purge_flag_overrides_every_tier() {
        let plan = plan(
            &three_tiers(),
            &CleanOptions {
                purge_all: true,
                ..tiered()
            },
        );

        assert!(
            plan.candidates.iter().all(|c| c.purge),
            "{:?}",
            plan.candidates
        );
    }

    /// The trap the two fields exist to avoid. `--purge` decides a destination;
    /// it must not turn something that needed confirming into something that
    /// does not, or a single flag would cancel a confirmation it has nothing to
    /// do with.
    #[test]
    fn the_purge_flag_never_cancels_a_confirmation() {
        let plan = plan(
            &three_tiers(),
            &CleanOptions {
                purge_all: true,
                ..tiered()
            },
        );
        let asked = plan
            .candidates
            .iter()
            .find(|c| c.rule == "asked")
            .expect("still in the plan");

        assert!(asked.purge, "it is destroyed, as asked");
        assert!(
            asked.tier.needs_confirming(),
            "but it still needs confirming, so `clean` will still refuse"
        );
    }

    /// And `--safe` with `--purge` together: the confirm-tier candidate is gone
    /// from the plan rather than destroyed by it.
    #[test]
    fn safe_still_drops_confirm_under_the_purge_flag() {
        let plan = plan(
            &three_tiers(),
            &CleanOptions {
                safe_only: true,
                purge_all: true,
                ..tiered()
            },
        );

        assert!(
            !plan.candidates.iter().any(|c| c.rule == "asked"),
            "{:?}",
            plan.candidates
        );
        assert!(plan.candidates.iter().all(|c| c.purge));
    }

    /// No rule, no tier: the cautious answer, and it is not destroyed.
    #[test]
    fn an_unclaimed_candidate_asks_and_is_not_destroyed() {
        let plan = plan(&three_tiers(), &tiered());

        assert!(
            plan.candidates
                .iter()
                .all(|c| c.purge == (c.rule == "destroyed")),
            "only the rule that said so: {:?}",
            plan.candidates
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

    /// The guard belongs to the rule that wants it. `--allow-dirty` went in
    /// v0.5 because a global switch would have been a coarser duplicate of a
    /// setting that was already there — and this is what turning it off looks
    /// like now.
    #[test]
    fn a_rule_that_does_not_ask_is_not_guarded() {
        let Some(handle) = repo_fixture(false) else {
            return;
        };
        let root = handle.path();
        let unguarded = Rules::new(
            vec![Rule {
                name: "rust-target".into(),
                includes: vec!["**/target/".into()],
                requires_sibling: vec!["Cargo.toml".into()],
                requires_clean_repo: false,
                tier: Tier::Trash,
                ..Rule::default()
            }],
            &UserDirs::default(),
        )
        .expect("compiles");
        let options = CleanOptions {
            detect: DetectOptions {
                rules: unguarded,
                now: SystemTime::UNIX_EPOCH,
            },
            ..opts()
        };

        let plan = plan(&repo_tree(root), &options);

        assert!(
            paths(&plan).contains(&root.join("target").as_path()),
            "the rule did not ask to be guarded: {:?}",
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
    fn the_safe_list_rules_are_auto_tier() {
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
            plan.candidates.iter().all(|c| c.tier == Tier::Trash),
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
        assert_eq!(plan.candidates[0].rule, "old");
    }

    /// v0.2 demoted anything named `venv`/`.venv` to confirm tier whatever
    /// claimed it. v0.3 deletes that list: the rule sets the tier, full stop.
    ///
    /// Under the built-in rules nothing changes, and this test is the evidence —
    /// no safe-list rule matches a `venv/`, so the only way one reaches a plan is
    /// the age rule, which is confirm anyway. The difference appears only if a
    /// user writes an `auto` rule that matches one, which is their call to make.
    #[test]
    fn virtualenvs_are_still_confirm_tier_under_the_builtin_rules() {
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

    /// The tier comes from the rule and from nowhere else — no name, no path and
    /// no category can override it. That is the whole of D2, and it cuts both
    /// ways: it is also how a user grants themselves removal without asking.
    #[test]
    fn the_tier_is_whatever_the_rule_declared() {
        let options = with(
            UserDirs::default(),
            vec![Rule {
                name: "mine".into(),
                includes: vec!["**/venv/".into()],
                tier: Tier::Trash,
                ..Rule::default()
            }],
        );

        let plan = plan(&tree(dir("/p", vec![dir("/p/venv", vec![])])), &options);

        assert_eq!(paths(&plan), vec![Path::new("/p/venv")]);
        assert_eq!(
            plan.candidates[0].tier,
            Tier::Trash,
            "the user said auto, so it is auto"
        );
    }

    /// A rule's own threshold narrows the plan exactly as `--min-size` does, and
    /// the two compose: whichever is larger wins.
    #[test]
    fn a_rules_min_size_narrows_the_plan() {
        let options = with(
            UserDirs::default(),
            vec![Rule {
                name: "big-only".into(),
                includes: vec!["**/node_modules/".into()],
                min_size: 1_048_576,
                tier: Tier::Trash,
                ..Rule::default()
            }],
        );

        let plan = plan(
            &tree(dir(
                "/p",
                vec![
                    dir(
                        "/p/big",
                        vec![dir(
                            "/p/big/node_modules",
                            vec![file("/p/big/node_modules/x.bin", 2_000_000)],
                        )],
                    ),
                    dir("/p/small", vec![dir("/p/small/node_modules", vec![])]),
                ],
            )),
            &options,
        );

        assert_eq!(paths(&plan), vec![Path::new("/p/big/node_modules")]);
        assert_eq!(
            plan.below_rule_minimum, 1,
            "counted against the rule's threshold, since that is what dropped it"
        );
        assert_eq!(
            plan.too_small, 0,
            "and not against the flag, which was never passed"
        );
    }

    /// The two thresholds are counted apart because their remedies differ: one
    /// is a flag on this command line, the other a line in a config file the
    /// user has to go and find. A report naming `--min-size` for a rule's
    /// threshold sends them to change something they never set.
    #[test]
    fn the_two_size_thresholds_are_counted_separately() {
        let options = CleanOptions {
            min_size: 1_048_576,
            ..with(
                UserDirs::default(),
                vec![
                    Rule {
                        name: "strict".into(),
                        includes: vec!["**/__pycache__/".into()],
                        min_size: 4_194_304,
                        tier: Tier::Trash,
                        ..Rule::default()
                    },
                    Rule {
                        name: "loose".into(),
                        includes: vec!["**/node_modules/".into()],
                        tier: Tier::Trash,
                        ..Rule::default()
                    },
                ],
            )
        };

        let plan = plan(
            &tree(dir(
                "/p",
                vec![
                    // Over the flag, under its own rule's threshold.
                    dir(
                        "/p/a",
                        vec![dir(
                            "/p/a/__pycache__",
                            vec![file("/p/a/__pycache__/x.bin", 2_000_000)],
                        )],
                    ),
                    // Under the flag; its rule sets no threshold of its own.
                    dir("/p/b", vec![dir("/p/b/node_modules", vec![])]),
                ],
            )),
            &options,
        );

        assert!(plan.candidates.is_empty(), "{:?}", paths(&plan));
        assert_eq!(plan.below_rule_minimum, 1, "the pycache, by its own rule");
        assert_eq!(plan.too_small, 1, "the node_modules, by the flag");
    }

    /// A candidate under both is counted once, against the rule — the narrower
    /// statement, and the one the user would have to hunt for.
    #[test]
    fn below_both_thresholds_counts_against_the_rule() {
        let options = CleanOptions {
            min_size: 1_048_576,
            ..with(
                UserDirs::default(),
                vec![Rule {
                    name: "strict".into(),
                    includes: vec!["**/node_modules/".into()],
                    min_size: 4_194_304,
                    tier: Tier::Trash,
                    ..Rule::default()
                }],
            )
        };

        let plan = plan(
            &tree(dir("/p", vec![dir("/p/node_modules", vec![])])),
            &options,
        );

        assert_eq!(plan.below_rule_minimum, 1);
        assert_eq!(plan.too_small, 0, "counted once, not twice");
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

    /// The count comes back with the plan, from the one pass that made it.
    ///
    /// It used to be obtained by planning a second time without the flag and
    /// subtracting — which cost a full extra run of the git guard (~23 ms per
    /// repository, measured), making `--safe` the slowest of the three modes
    /// despite being the cautious one, and which subtracted two independently
    /// measured numbers that could disagree.
    #[test]
    fn the_plan_reports_how_many_safe_kept_out() {
        let fixture = tree(dir(
            "/p",
            vec![
                dir("/p/node_modules", vec![]),
                aged(file("/p/one.bin", 4096), 900 * DAY),
                aged(file("/p/two.bin", 4096), 900 * DAY),
            ],
        ));

        let everything = plan(&fixture, &aging(90 * DAY));
        assert_eq!(everything.candidates.len(), 3);
        assert_eq!(
            everything.filtered_out, 0,
            "without --safe nothing is filtered"
        );

        let safe = plan(
            &fixture,
            &CleanOptions {
                safe_only: true,
                ..aging(90 * DAY)
            },
        );

        assert_eq!(paths(&safe), vec![Path::new("/p/node_modules")]);
        assert_eq!(
            safe.filtered_out, 2,
            "and the two confirm-tier ones are counted, not silently gone"
        );
    }

    /// The flag exists because a real run produced 151 candidates of which 150
    /// were tiny `__pycache__` directories inside one virtualenv — a report a
    /// user is meant to read before deleting, 99% of which said nothing.
    ///
    /// Unlike the scan's flag of the same name, this narrows **the plan**, not
    /// the printout: what is shown is what `--apply` removes, and the total
    /// moves with it.
    #[test]
    fn min_size_narrows_the_plan_and_the_total() {
        let fixture = tree(dir(
            "/p",
            vec![
                dir(
                    "/p/big",
                    vec![dir(
                        "/p/big/node_modules",
                        vec![file("/p/big/node_modules/lib.bin", 2_000_000)],
                    )],
                ),
                dir("/p/small", vec![dir("/p/small/__pycache__", vec![])]),
            ],
        ));

        let everything = plan(&fixture, &opts());
        assert_eq!(everything.candidates.len(), 2);
        assert_eq!(everything.too_small, 0, "no threshold, nothing below it");

        let filtered = plan(
            &fixture,
            &CleanOptions {
                min_size: 1_048_576,
                ..opts()
            },
        );

        assert_eq!(paths(&filtered), vec![Path::new("/p/big/node_modules")]);
        assert_eq!(filtered.too_small, 1, "and the one dropped is counted");
        assert_eq!(
            filtered.reclaimable,
            filtered.candidates.iter().map(|c| c.allocated).sum::<u64>(),
            "the total is what would actually be removed, not what might have been"
        );
        assert!(filtered.reclaimable < everything.reclaimable);
    }

    /// The two narrowings are counted apart, because the remedy differs: one is
    /// answered by dropping `--safe`, the other by lowering `--min-size`.
    #[test]
    fn the_two_user_filters_are_counted_separately() {
        let fixture = tree(dir(
            "/p",
            vec![
                dir(
                    "/p/node_modules",
                    vec![file("/p/node_modules/x.bin", 2_000_000)],
                ),
                aged(file("/p/ancient.bin", 2_000_000), 900 * DAY),
                dir("/p/tiny", vec![dir("/p/tiny/__pycache__", vec![])]),
            ],
        ));

        let plan = plan(
            &fixture,
            &CleanOptions {
                safe_only: true,
                min_size: 1_048_576,
                ..aging(90 * DAY)
            },
        );

        assert_eq!(paths(&plan), vec![Path::new("/p/node_modules")]);
        assert_eq!(plan.filtered_out, 1, "the aged one needed confirmation");
        assert_eq!(plan.too_small, 1, "the small one was simply small");
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

    // ---- merging the plans of several roots ------------------------------

    /// `clean` with no path plans one root at a time, and the report has to read
    /// as one plan. Concatenating two sorted lists does not give a sorted one,
    /// and an order that shifts between runs cannot be reviewed.
    #[test]
    fn merge_orders_the_combined_candidates_by_path() {
        let first = plan(
            &tree(dir("/z", vec![dir("/z/node_modules", vec![])])),
            &opts(),
        );
        let second = plan(
            &tree(dir("/a", vec![dir("/a/node_modules", vec![])])),
            &opts(),
        );

        let merged = CleanPlan::merge(vec![first, second]);

        assert_eq!(
            paths(&merged),
            vec![Path::new("/a/node_modules"), Path::new("/z/node_modules")],
            "the later root's candidate sorts ahead of the earlier one's"
        );
    }

    #[test]
    fn merge_sums_the_total_and_every_counter() {
        let fixture = |root: &str| {
            let nm = format!("{root}/node_modules");
            let small = format!("{root}/tiny");
            let pyc = format!("{small}/__pycache__");
            tree(dir(
                root,
                vec![
                    dir(&nm, vec![file(&format!("{nm}/x.bin"), 2_000_000)]),
                    dir(&small, vec![dir(&pyc, vec![])]),
                ],
            ))
        };
        let options = CleanOptions {
            min_size: 1_048_576,
            ..opts()
        };

        let merged = CleanPlan::merge(vec![
            plan(&fixture("/a"), &options),
            plan(&fixture("/b"), &options),
        ]);

        assert_eq!(merged.candidates.len(), 2);
        assert_eq!(
            merged.reclaimable,
            merged.candidates.iter().map(|c| c.allocated).sum::<u64>()
        );
        assert_eq!(merged.too_small, 2, "one from each root");
    }

    #[test]
    fn merging_nothing_yields_an_empty_plan() {
        let merged = CleanPlan::merge(Vec::new());

        assert_eq!(merged, CleanPlan::default());
    }

    /// The refusals survive the merge too. A denylisted path dropped here would
    /// be the tool protecting something and then not saying so.
    #[test]
    fn merge_keeps_every_refusal() {
        let denied = plan(
            &tree(dir("/Windows", vec![dir("/Windows/node_modules", vec![])])),
            &opts(),
        );
        let ordinary = plan(
            &tree(dir("/p", vec![dir("/p/node_modules", vec![])])),
            &opts(),
        );

        let merged = CleanPlan::merge(vec![ordinary, denied]);

        assert_eq!(paths(&merged), vec![Path::new("/p/node_modules")]);
        assert_eq!(
            merged.excluded,
            vec![Excluded {
                path: PathBuf::from("/Windows/node_modules"),
                reason: ExcludeReason::Denylisted,
            }]
        );
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

    // ---- duplicates ------------------------------------------------------
    //
    // Hand-built `Duplicates` values: the pass that produces them is tested in
    // its own module, and what belongs here is only what the plan adds to it.

    #[cfg(feature = "duplicates")]
    mod duplicates {
        use super::*;
        use crate::duplicates::{DuplicateGroup, Duplicates};

        fn copy(path: &str, allocated: u64) -> crate::duplicates::Copy {
            crate::duplicates::Copy {
                path: PathBuf::from(path),
                allocated,
            }
        }

        fn group(keeper: &str, copies: Vec<crate::duplicates::Copy>) -> DuplicateGroup {
            DuplicateGroup {
                apparent: 4096,
                keeper: PathBuf::from(keeper),
                keeper_fell_back: false,
                reclaimable: copies.iter().map(|c| c.allocated).sum(),
                copies,
            }
        }

        fn found(groups: Vec<DuplicateGroup>) -> Duplicates {
            Duplicates {
                groups,
                ..Duplicates::default()
            }
        }

        /// A tree holding every path named, so the `shared` lookup has nodes to
        /// find — which is all the plan needs a tree for.
        fn tree_of(paths: &[&str]) -> ScanTree {
            tree(dir("/p", paths.iter().map(|p| file(p, 4096)).collect()))
        }

        #[test]
        fn every_copy_but_the_keeper_becomes_a_candidate() {
            let tree = tree_of(&["/p/a.bin", "/p/b.bin", "/p/c.bin"]);
            let found = found(vec![group(
                "/p/a.bin",
                vec![copy("/p/b.bin", 4096), copy("/p/c.bin", 4096)],
            )]);

            let plan = plan_duplicates(&tree, &found, &opts());

            assert_eq!(plan.candidates.len(), 2);
            assert!(
                plan.candidates
                    .iter()
                    .all(|c| c.path != Path::new("/p/a.bin")),
                "the keeper is never in the plan"
            );
            assert_eq!(plan.reclaimable, 8192);
            for candidate in &plan.candidates {
                assert_eq!(
                    candidate.tier,
                    Tier::Confirm,
                    "a duplicate is always looked at"
                );
                assert_eq!(candidate.duplicate_of, Some(PathBuf::from("/p/a.bin")));
                assert_eq!(candidate.rule, DUPLICATE_RULE);
                assert!(!candidate.purge, "the trash, unless asked otherwise");
            }
        }

        #[test]
        fn a_denylisted_copy_is_refused_and_reported() {
            let denied = "/home/me/Library/Application Support/thing.bin";
            let tree = tree_of(&["/p/a.bin", denied]);
            let found = found(vec![group("/p/a.bin", vec![copy(denied, 4096)])]);

            let plan = plan_duplicates(&tree, &found, &with_home("/home/me"));

            assert!(plan.candidates.is_empty(), "{:?}", plan.candidates);
            assert_eq!(
                plan.excluded,
                vec![Excluded {
                    path: PathBuf::from(denied),
                    reason: ExcludeReason::Denylisted,
                }]
            );
            assert_eq!(plan.reclaimable, 0, "a refusal frees nothing");
        }

        /// `--purge` moves the destination and nothing else. Rewriting the tier
        /// would let a flag about where files go cancel the confirmation that
        /// keeps a duplicate from being removed unseen.
        #[test]
        fn purge_changes_the_destination_not_the_tier() {
            let tree = tree_of(&["/p/a.bin", "/p/b.bin"]);
            let found = found(vec![group("/p/a.bin", vec![copy("/p/b.bin", 4096)])]);
            let options = CleanOptions {
                purge_all: true,
                ..opts()
            };

            let plan = plan_duplicates(&tree, &found, &options);

            assert!(plan.candidates[0].purge);
            assert_eq!(plan.candidates[0].tier, Tier::Confirm);
        }

        /// `--safe` admits only what needs no confirming, and no duplicate does.
        #[test]
        fn safe_plans_no_duplicates_at_all() {
            let tree = tree_of(&["/p/a.bin", "/p/b.bin", "/p/c.bin"]);
            let found = found(vec![group(
                "/p/a.bin",
                vec![copy("/p/b.bin", 4096), copy("/p/c.bin", 4096)],
            )]);
            let options = CleanOptions {
                safe_only: true,
                ..opts()
            };

            let plan = plan_duplicates(&tree, &found, &options);

            assert!(plan.candidates.is_empty());
            assert_eq!(plan.filtered_out, 2, "counted, so the report can say so");
            assert_eq!(plan.reclaimable, 0);
        }

        #[test]
        fn min_size_is_counted_apart_from_everything_else() {
            let tree = tree_of(&["/p/a.bin", "/p/b.bin"]);
            let found = found(vec![group("/p/a.bin", vec![copy("/p/b.bin", 4096)])]);
            let options = CleanOptions {
                min_size: 8192,
                ..opts()
            };

            let plan = plan_duplicates(&tree, &found, &options);

            assert!(plan.candidates.is_empty());
            assert_eq!(plan.too_small, 1);
            assert_eq!(
                plan.below_rule_minimum, 0,
                "there is no rule here to have a minimum of its own"
            );
        }

        /// Removing a name of a hardlinked inode frees nothing while another
        /// name survives — so its bytes are an upper bound, and the plan says so.
        #[test]
        fn a_copy_whose_inode_has_another_name_is_shared() {
            let mut tree = tree_of(&["/p/a.bin", "/p/b.bin", "/p/elsewhere.bin"]);
            tree.link_groups = vec![vec![
                PathBuf::from("/p/b.bin"),
                PathBuf::from("/p/elsewhere.bin"),
            ]];
            let found = found(vec![group("/p/a.bin", vec![copy("/p/b.bin", 4096)])]);

            let plan = plan_duplicates(&tree, &found, &opts());

            assert!(plan.candidates[0].shared);
        }

        #[test]
        fn an_ordinary_copy_is_not_shared() {
            let tree = tree_of(&["/p/a.bin", "/p/b.bin"]);
            let found = found(vec![group("/p/a.bin", vec![copy("/p/b.bin", 4096)])]);

            assert!(!plan_duplicates(&tree, &found, &opts()).candidates[0].shared);
        }

        /// Merging is what a multi-root run does, and the keeper has to survive
        /// it — the reason it rides on the candidate rather than in a table.
        #[test]
        fn merging_keeps_each_candidate_pointing_at_its_keeper() {
            let one = plan_duplicates(
                &tree_of(&["/p/a.bin", "/p/b.bin"]),
                &found(vec![group("/p/a.bin", vec![copy("/p/b.bin", 4096)])]),
                &opts(),
            );
            let two = plan_duplicates(
                &tree_of(&["/q/x.bin", "/q/y.bin"]),
                &found(vec![group("/q/x.bin", vec![copy("/q/y.bin", 2048)])]),
                &opts(),
            );

            let merged = CleanPlan::merge(vec![one, two]);

            assert_eq!(merged.reclaimable, 6144);
            assert_eq!(
                merged
                    .candidates
                    .iter()
                    .map(|c| c.duplicate_of.clone())
                    .collect::<Vec<_>>(),
                vec![
                    Some(PathBuf::from("/p/a.bin")),
                    Some(PathBuf::from("/q/x.bin")),
                ]
            );
        }

        /// The plan reads a tree and a set of groups and touches nothing — the
        /// same promise `plan` makes, and the one that lets every rule above be
        /// tested without a filesystem.
        #[test]
        fn plan_duplicates_writes_nothing() {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            let keeper = root.join("a.bin");
            let copy_path = root.join("b.bin");
            std::fs::write(&keeper, b"x").expect("write");
            std::fs::write(&copy_path, b"x").expect("write");

            let tree = crate::scan(&crate::ScanOptions {
                root: root.to_path_buf(),
                ..crate::ScanOptions::default()
            });
            let found = found(vec![DuplicateGroup {
                apparent: 1,
                keeper: keeper.clone(),
                keeper_fell_back: false,
                reclaimable: 1,
                copies: vec![crate::duplicates::Copy {
                    path: copy_path.clone(),
                    allocated: 1,
                }],
            }]);

            let plan = plan_duplicates(&tree, &found, &opts());

            assert_eq!(plan.candidates.len(), 1);
            assert!(
                keeper.exists() && copy_path.exists(),
                "both files still there"
            );
        }
    }
}
