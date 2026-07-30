//! The plan, as a report — what a cleanup would do, or is about to.
//!
//! This is the safety story's user-facing half. `preview` prints this and stops,
//! so the text is what someone reads before deciding to run `clean`, and every
//! wording choice below exists because getting it wrong would mislead them into
//! a deletion.
//!
//! Shaped like [`super::skipped`] — a flat list and a summary — rather than
//! [`super::tree`]: bars and percentages answer "what is big", and this answers
//! "what would go".
//!
//! The same list serves both verbs, and the **closing line must not**. Printed
//! by `clean`, "nothing was removed" is about to become false — which would make
//! the last thing a user reads before a deletion the one sentence in the report
//! that is a lie. That is what [`crate::args::Intent`] is for.

use super::tree::format_size;
use crate::args::{Intent, Report, Sort};
use disk_tools_core::{
    Candidate, CleanOutcome, CleanPlan, ExcludeReason, Excluded, ScanNode, ScanTree, Tier,
};
use std::fmt::Write;
use std::path::Path;

/// Render the plan for a human.
///
/// `hidden_by_safe` is how many candidates `--safe` kept out, or `None` when the
/// flag was not used. The core does not record them — that is deliberate, since
/// `--safe` is the user's own narrowing rather than a refusal — so the count is
/// passed in by the caller, which plans a second time to obtain it.
pub fn render_clean(
    plan: &CleanPlan,
    hidden_by_safe: Option<usize>,
    intent: Intent,
    report: Report,
    inside: &[ScanTree],
) -> String {
    let mut out = String::new();

    if plan.candidates.is_empty() {
        // Finding nothing is an ordinary outcome, not a failure, and the report
        // should not make it read like one.
        let _ = writeln!(out, "Nothing to clean.");
    } else {
        if report.depth == 0 {
            write_rules(&plan.candidates, report.sort, &mut out);
        } else {
            write_candidates(
                &plan.candidates,
                report.sort,
                report.depth,
                inside,
                &mut out,
            );
        }
        let _ = writeln!(out);
        write_total(plan, &mut out);
    }

    if !plan.excluded.is_empty() {
        let _ = writeln!(out);
        write_excluded(&plan.excluded, &mut out);
    }

    if let Some(hidden) = hidden_by_safe.filter(|&n| n > 0) {
        // The verb and the pronoun have to agree with the noun, not just the
        // noun with the count — "1 more candidate need confirmation … hiding
        // them" reads as though more than one were being withheld, which is the
        // one thing this line exists to say precisely.
        let (noun, verb, them) = if hidden == 1 {
            ("candidate", "needs", "it")
        } else {
            ("candidates", "need", "them")
        };
        let _ = writeln!(
            out,
            "\n{hidden} more {noun} {verb} confirmation; --safe is hiding {them}."
        );
    }

    // Three separate lines, and deliberately so: each names the thing a user
    // would have to change to see what is missing. Offering `--safe` as the
    // answer to "where did the rest go" when the answer is `--min-size` sends
    // them at the wrong flag — and naming `--min-size` when the threshold came
    // from a rule sends them to change something they never set.
    if plan.too_small > 0 {
        let _ = writeln!(
            out,
            "\n{} more {} below --min-size.",
            plan.too_small,
            are(plan.too_small)
        );
    }

    if plan.below_rule_minimum > 0 {
        let _ = writeln!(
            out,
            "\n{} more {} below their rule's own min-size; edit the rule to see {}.",
            plan.below_rule_minimum,
            are(plan.below_rule_minimum),
            if plan.below_rule_minimum == 1 {
                "it"
            } else {
                "them"
            }
        );
    }

    if !plan.candidates.is_empty() && intent == Intent::Preview {
        // Naming the verb rather than a flag: the way this report is acted on is
        // to retype the line that produced it, and the only thing that changes
        // is the first word.
        let _ = writeln!(
            out,
            "\nPreview — nothing was removed. The same line with `clean` removes it."
        );
    }

    out
}

/// Report what a cleanup actually did.
///
/// The concept's rule is that there is no partial **silent** delete. A partial
/// delete is acceptable; leaving the user to guess which half happened is not.
/// So a failure names its path and what the OS said, and the closing line states
/// plainly that something is still there.
pub fn render_outcome(outcome: &CleanOutcome, shared_removed: bool) -> String {
    let mut out = String::new();
    let attempted = outcome.count() + outcome.failed.len();

    if outcome.count() == 0 {
        let _ = writeln!(out, "Removed nothing.");
    } else {
        // "At most" whenever something removed held content reachable from
        // outside it — the same hedge the dry run's total carries, because the
        // figure has exactly the same softness. Saying it carefully in the
        // preview and flatly in the outcome would be one run disagreeing with
        // itself, and the flat version is the wrong one.
        let freed = if shared_removed {
            format!("Freed at most {}", format_size(outcome.reclaimed()))
        } else {
            format!("Freed {}", format_size(outcome.reclaimed()))
        };
        let _ = writeln!(out, "Removed {} of {attempted}. {freed}.", outcome.count());

        // The two halves, whenever the run was mixed. A single "freed" figure
        // over both does not say what can be brought back, which is the one
        // thing a reader wants from it — and the split is the whole reason the
        // core refuses to add them up itself.
        if !outcome.trashed.paths.is_empty() && !outcome.purged.paths.is_empty() {
            let _ = writeln!(
                out,
                "  {} in the trash, recoverable; {} destroyed, not.",
                format_size(outcome.trashed.bytes),
                format_size(outcome.purged.bytes)
            );
        }
    }

    if outcome.failed.is_empty() {
        return out;
    }

    let _ = writeln!(out, "\nNot removed:");
    for failure in &outcome.failed {
        let _ = writeln!(out, "  {} — {}", failure.path.display(), failure.reason);
    }

    let count = outcome.failed.len();
    let noun = if count == 1 {
        "candidate"
    } else {
        "candidates"
    };
    let _ = writeln!(out, "\n{count} {noun} still on disk.");

    // The recoverability claim names what it covers, and appears only when
    // something was actually removed. Printed unconditionally it is a plain
    // falsehood after a run that trashed nothing — and "everything above",
    // sitting directly under the failure list, reads as promising exactly the
    // items that are *not* in the trash. This is the sentence a user reads to
    // find out whether their work survived.
    //
    // Read straight off the outcome rather than from a flag: the claim covers
    // exactly the paths that went to the trash, and after v0.5 one run can send
    // some there and destroy the rest. A sentence derived from `--purge` would
    // have described the invocation, not the paths it is standing under.
    if !outcome.trashed.paths.is_empty() {
        let trashed = outcome.trashed.paths.len();
        let (noun, verb) = if trashed == 1 {
            ("candidate", "is")
        } else {
            ("candidates", "are")
        };
        let _ = writeln!(
            out,
            "{trashed} {noun} {verb} in the trash and can be put back."
        );
    }

    out
}

/// "candidate is" / "candidates are" — the verb has to agree with the noun, not
/// only the noun with the count.
fn are(count: usize) -> &'static str {
    if count == 1 {
        "candidate is"
    } else {
        "candidates are"
    }
}

/// One line per rule: which of them is doing this, and to how much.
///
/// The default, because "what would this take" is asked of the whole run before
/// it is asked of any one path — and because a rule set that has grown a
/// mistake shows it here in three lines rather than in nine hundred.
///
/// Every candidate of a rule carries that rule's tier, so a group has exactly
/// one and can state it.
fn write_rules(candidates: &[Candidate], sort: Sort, out: &mut String) {
    let mut groups: Vec<Group> = Vec::new();
    for candidate in candidates {
        match groups.iter_mut().find(|g| g.rule == candidate.rule) {
            Some(group) => {
                group.count += 1;
                group.allocated += candidate.allocated;
            }
            None => groups.push(Group {
                rule: &candidate.rule,
                tier: candidate.tier,
                purge: candidate.purge,
                count: 1,
                allocated: candidate.allocated,
            }),
        }
    }

    match sort {
        Sort::Name => groups.sort_by(|a, b| a.rule.cmp(b.rule)),
        // By path where the sizes tie, as everywhere: a report whose order
        // changes between two runs over one unchanged disk cannot be diffed.
        Sort::Size => groups.sort_by(|a, b| b.allocated.cmp(&a.allocated).then(a.rule.cmp(b.rule))),
    }

    let rule_width = groups
        .iter()
        .map(|g| g.rule.chars().count())
        .max()
        .unwrap_or(0);
    let count_width = groups
        .iter()
        .map(|g| g.count.to_string().len())
        .max()
        .unwrap_or(0);

    for group in groups {
        let _ = writeln!(
            out,
            "{size:>8}  {rule:<rw$}  {count:>cw$} {noun:<10}  {tier}",
            size = format_size(group.allocated),
            rule = group.rule,
            count = group.count,
            // Singular where it is one, because "1 candidates" is the kind of
            // thing that makes a reader doubt the number beside it.
            noun = if group.count == 1 {
                "candidate"
            } else {
                "candidates"
            },
            tier = fate(group.tier, group.purge),
            rw = rule_width,
            cw = count_width,
        );
    }
}

/// One rule's share of the plan.
struct Group<'a> {
    rule: &'a str,
    tier: Tier,
    /// Every candidate of a rule shares its destination, so a group has one.
    purge: bool,
    count: usize,
    allocated: u64,
}

fn write_candidates(
    candidates: &[Candidate],
    sort: Sort,
    depth: usize,
    inside: &[ScanTree],
    out: &mut String,
) {
    // Borrowed and sorted here rather than in the plan. `CleanPlan.candidates`
    // is ordered by path so that two runs over one tree produce identical
    // plans, and that is worth more than saving this sort.
    let mut candidates: Vec<&Candidate> = candidates.iter().collect();
    match sort {
        Sort::Name => candidates.sort_by(|a, b| a.path.cmp(&b.path)),
        Sort::Size => {
            candidates.sort_by(|a, b| b.allocated.cmp(&a.allocated).then(a.path.cmp(&b.path)));
        }
    }

    // Widths from the data rather than fixed, so the columns stay tight when
    // only one rule is present. Rule names are user-supplied in v0.3, so this
    // is no longer a bounded set of five known strings.
    let rule_width = candidates
        .iter()
        .map(|c| c.rule.chars().count())
        .max()
        .unwrap_or(0);
    let tier_width = candidates
        .iter()
        .map(|c| fate(c.tier, c.purge).len())
        .max()
        .unwrap_or(0);

    for candidate in candidates {
        // Writing to a `String` is infallible.
        let _ = writeln!(
            out,
            "{size:>8}  {rule:<rw$}  {tier:<tw$}  {path}{shared}",
            size = format_size(candidate.allocated),
            rule = candidate.rule,
            tier = fate(candidate.tier, candidate.purge),
            path = candidate.path.display(),
            shared = if candidate.shared { "  (shared)" } else { "" },
            rw = rule_width,
            tw = tier_width,
        );

        // Level 1 is the candidate; everything past it is inside one. A
        // candidate is removed **whole**, so this changes no decision — it
        // answers "why is this four gigabytes", which is worth one flag value
        // and no more.
        if depth > 1
            && let Some(node) = find(inside, &candidate.path)
        {
            write_inside(node, depth - 1, 1, sort, out);
        }
    }
}

/// One candidate's contents, `levels` deep.
fn write_inside(node: &ScanNode, levels: usize, indent: usize, sort: Sort, out: &mut String) {
    if levels == 0 {
        return;
    }

    let mut children: Vec<&ScanNode> = node.children.iter().collect();
    match sort {
        Sort::Name => children.sort_by(|a, b| a.path.cmp(&b.path)),
        Sort::Size => {
            children.sort_by(|a, b| b.allocated.cmp(&a.allocated).then(a.path.cmp(&b.path)));
        }
    }

    for child in children {
        let name = child
            .path
            .file_name()
            .unwrap_or(child.path.as_os_str())
            .to_string_lossy();
        let _ = writeln!(
            out,
            "{size:>8}  {blank:indent$}{name}{mark}",
            size = format_size(child.allocated),
            blank = "",
            indent = indent * 2,
            mark = if child.is_dir { "/" } else { "" },
        );
        write_inside(child, levels - 1, indent + 1, sort, out);
    }
}

/// The node a candidate stands for, in whichever tree it came from.
///
/// The trees are handed in beside the plan rather than folded into it: a
/// `CleanPlan` that owned subtrees would be a plan carrying the whole scan
/// result — 1.4 GB on a real home — and `--json` would then have to decide
/// whether to serialise it. `None` when the caller did not keep them, which is
/// every run below `-d 2`.
fn find<'a>(trees: &'a [ScanTree], path: &Path) -> Option<&'a ScanNode> {
    trees.iter().find_map(|tree| descend(&tree.root, path))
}

fn descend<'a>(node: &'a ScanNode, path: &Path) -> Option<&'a ScanNode> {
    if node.path == path {
        return Some(node);
    }
    // Component-wise, so `/p/app-old` is not mistaken for something under
    // `/p/app` — the same comparison the denylist is careful about.
    if !path.starts_with(&node.path) {
        return None;
    }
    node.children.iter().find_map(|child| descend(child, path))
}

/// The total, labelled honestly.
///
/// A `shared` candidate holds content reachable from outside it, so removing it
/// frees less than its size — which makes the sum an **upper bound**. The
/// wording says "may free less", never "will not be freed": the marker means the
/// figure is soft, not that the bytes are unreachable.
fn write_total(plan: &CleanPlan, out: &mut String) {
    let shared = plan.candidates.iter().filter(|c| c.shared).count();
    let total = format_size(plan.reclaimable);

    if shared == 0 {
        let _ = writeln!(out, "Reclaimable: {total}");
        return;
    }

    let subject = if shared == 1 {
        "1 candidate shares".to_owned()
    } else {
        format!("{shared} candidates share")
    };
    let _ = writeln!(
        out,
        "Reclaimable: at most {total} — {subject} content with something outside it, \
         so removing them may free less."
    );
}

/// What was refused, and whether anything can be done about it.
///
/// The two reasons are **not** interchangeable and must not read as if they
/// were: nothing overrides the denylist, while the git guard is a setting the
/// user chose and can unchoose. Rendering them alike would send someone
/// reaching for something that cannot help.
fn write_excluded(excluded: &[Excluded], out: &mut String) {
    let _ = writeln!(out, "Not touched:");
    for entry in excluded {
        let _ = writeln!(
            out,
            "  {} — {}",
            entry.path.display(),
            excuse(&entry.reason)
        );
    }
}

fn excuse(reason: &ExcludeReason) -> &'static str {
    match reason {
        ExcludeReason::Denylisted => "protected; no flag removes this",
        // Names the rule key, not a flag: `--allow-dirty` was removed in v0.5
        // because the guard belongs to the rule that wants it. A message
        // offering a flag that no longer parses is worse than none.
        ExcludeReason::DirtyRepo => {
            "uncommitted changes; set requires-clean-repo = false on the rule to include"
        }
    }
}

/// What will happen to this one, in a word.
///
/// The tier's word, except where `--purge` has overruled it. The column answers
/// "what does `clean` do with this", which is what the three tier names were
/// renamed to answer — so a candidate about to be destroyed says `purge` here
/// whether that came from its rule or from the flag.
///
/// `tier` itself is left alone on the candidate: `--safe` and the confirm-tier
/// refusal read it, and a flag that rewrote it would cancel a confirmation it
/// has nothing to do with.
fn fate(tier: Tier, purge: bool) -> &'static str {
    match tier {
        _ if purge => "purge",
        Tier::Purge => "purge",
        Tier::Trash => "trash",
        Tier::Confirm => "confirm",
    }
}

/// The candidate listing — depth 1 — which is what most of the tests below are
/// about. Depth 0 groups by rule and has tests of its own.
#[cfg(test)]
fn listed() -> Report {
    Report {
        depth: 1,
        sort: Sort::Name,
        json: false,
    }
}

/// The renderer with nothing to unfold, which is every level up to and
/// including the listing.
///
/// The trees are only consulted past depth 1, so a test about the listing, the
/// totals or the footers has nothing to hand over — and passing an empty slice
/// at twenty call sites would say the same thing twenty times.
#[cfg(test)]
fn rendered(
    plan: &CleanPlan,
    hidden_by_safe: Option<usize>,
    intent: Intent,
    report: Report,
) -> String {
    render_clean(plan, hidden_by_safe, intent, report, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn candidate(path: &str, rule: &str, tier: Tier, allocated: u64) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            rule: rule.into(),
            tier,
            purge: tier == Tier::Purge,
            duplicate_of: None,
            allocated,
            shared: false,
        }
    }

    fn plan(candidates: Vec<Candidate>) -> CleanPlan {
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

    /// Two rules, three candidates, sizes that make every ordering visible.
    fn spread() -> CleanPlan {
        plan(vec![
            candidate("/p/a/node_modules", "node-modules", Tier::Trash, 1_048_576),
            candidate("/p/b/node_modules", "node-modules", Tier::Trash, 2_097_152),
            candidate("/p/target", "rust-target", Tier::Trash, 4_194_304),
        ])
    }

    fn at(depth: usize, sort: Sort) -> Report {
        Report {
            depth,
            sort,
            json: false,
        }
    }

    /// The default. "What would this take" is asked of the whole run before it
    /// is asked of any one path.
    #[test]
    fn depth_zero_is_one_line_per_rule() {
        let report = rendered(&spread(), None, Intent::Preview, at(0, Sort::Name));

        let rows: Vec<&str> = report.lines().take_while(|line| !line.is_empty()).collect();
        assert_eq!(rows.len(), 2, "one per rule, not per candidate: {report}");
        assert!(rows[0].contains("node-modules"), "{report}");
        assert!(
            rows[0].contains("3.0M") && rows[0].contains("2 candidates"),
            "the group carries its total and its count: {report}"
        );
        assert!(
            rows[1].contains("rust-target") && rows[1].contains("1 candidate  "),
            "and one candidate is singular: {report}"
        );
        assert!(
            !report.contains("/p/a/node_modules"),
            "no paths at this level: {report}"
        );
    }

    /// The level and the order are display only, so the figure they add up to
    /// cannot move with them.
    #[test]
    fn the_total_is_the_same_at_every_level_and_order() {
        let totals: Vec<String> = [
            at(0, Sort::Name),
            at(0, Sort::Size),
            at(1, Sort::Name),
            at(1, Sort::Size),
        ]
        .into_iter()
        .map(|report| {
            rendered(&spread(), None, Intent::Preview, report)
                .lines()
                .find(|line| line.starts_with("Reclaimable:"))
                .expect("a total")
                .to_owned()
        })
        .collect();

        assert_eq!(totals[0], "Reclaimable: 7.0M");
        assert!(totals.iter().all(|total| *total == totals[0]), "{totals:?}");
    }

    /// And the groups add up to the candidates, which is the arithmetic a
    /// reader does in their head between the two levels.
    #[test]
    fn the_groups_sum_to_what_the_candidates_come_to() {
        let grouped = rendered(&spread(), None, Intent::Preview, at(0, Sort::Name));
        let sizes = |report: &str| -> Vec<String> {
            report
                .lines()
                .take_while(|line| !line.is_empty())
                .map(|line| line.split_whitespace().next().expect("a size").to_owned())
                .collect()
        };

        assert_eq!(sizes(&grouped), ["3.0M", "4.0M"]);
    }

    #[test]
    fn sorting_by_size_puts_the_biggest_first_at_both_levels() {
        let grouped = rendered(&spread(), None, Intent::Preview, at(0, Sort::Size));
        let listing = rendered(&spread(), None, Intent::Preview, at(1, Sort::Size));
        let first = |report: &str| report.lines().next().expect("a row").to_owned();

        assert!(first(&grouped).contains("rust-target"), "{grouped}");
        assert!(first(&listing).contains("/p/target"), "{listing}");
    }

    /// Size is not a unique key — a rule over a large tree yields hundreds of
    /// identical ones — so without a tiebreak the same disk state prints in a
    /// different order each run and two reports cannot be diffed.
    #[test]
    fn equal_sizes_are_ordered_by_path() {
        let tied = plan(vec![
            candidate("/p/zulu", "r", Tier::Trash, 4096),
            candidate("/p/alpha", "r", Tier::Trash, 4096),
            candidate("/p/mike", "r", Tier::Trash, 4096),
        ]);

        let report = rendered(&tied, None, Intent::Preview, at(1, Sort::Size));

        let paths: Vec<&str> = report
            .lines()
            .take_while(|line| !line.is_empty())
            .filter_map(|line| line.split_whitespace().last())
            .collect();
        assert_eq!(paths, ["/p/alpha", "/p/mike", "/p/zulu"]);
    }

    /// A depth past the candidates is not an error. Levels beyond 1 unfold
    /// inside a candidate, which is a later task; until then they list.
    #[test]
    fn a_depth_beyond_the_plan_still_lists() {
        for depth in [1, 2, 9] {
            let report = rendered(&spread(), None, Intent::Preview, at(depth, Sort::Name));
            assert!(report.contains("/p/target"), "depth {depth}: {report}");
        }
    }

    /// An empty plan says so once, at any level — never as an empty grouping.
    #[test]
    fn nothing_to_clean_reads_the_same_at_every_level() {
        for depth in [0, 1, 2] {
            let report = rendered(
                &plan(Vec::new()),
                None,
                Intent::Preview,
                at(depth, Sort::Name),
            );
            assert_eq!(report, "Nothing to clean.\n", "depth {depth}");
        }
    }

    // ---- inside a candidate ----------------------------------------------

    fn branch(path: &str, allocated: u64, children: Vec<ScanNode>) -> ScanNode {
        ScanNode {
            path: PathBuf::from(path),
            allocated,
            apparent: allocated,
            is_dir: true,
            modified: None,
            links: None,
            children,
        }
    }

    fn leaf(path: &str, allocated: u64) -> ScanNode {
        ScanNode {
            is_dir: false,
            ..branch(path, allocated, Vec::new())
        }
    }

    /// One candidate, `/p/target`, with two levels under it.
    fn with_contents() -> (CleanPlan, Vec<ScanTree>) {
        let tree = ScanTree {
            root: branch(
                "/p",
                4_194_304,
                vec![branch(
                    "/p/target",
                    4_194_304,
                    vec![
                        branch(
                            "/p/target/debug",
                            3_145_728,
                            vec![branch("/p/target/debug/deps", 2_097_152, Vec::new())],
                        ),
                        branch("/p/target/release", 1_044_480, Vec::new()),
                        leaf("/p/target/.rustc_info.json", 4096),
                    ],
                )],
            ),
            skipped: Vec::new(),
            link_groups: Vec::new(),
        };
        let plan = plan(vec![candidate(
            "/p/target",
            "rust-target",
            Tier::Trash,
            4_194_304,
        )]);
        (plan, vec![tree])
    }

    #[test]
    fn depth_two_lists_a_candidates_own_children() {
        let (plan, trees) = with_contents();

        let report = render_clean(&plan, None, Intent::Preview, at(2, Sort::Name), &trees);

        assert!(
            report.contains("/p/target"),
            "the candidate itself: {report}"
        );
        for child in ["debug/", "release/", ".rustc_info.json"] {
            assert!(report.contains(child), "{child} missing from {report}");
        }
        assert!(
            !report.contains("deps/"),
            "but not the level below that: {report}"
        );
    }

    #[test]
    fn each_further_level_goes_one_deeper() {
        let (plan, trees) = with_contents();

        let report = render_clean(&plan, None, Intent::Preview, at(3, Sort::Name), &trees);

        assert!(report.contains("deps/"), "{report}");
    }

    /// A depth past the leaves is not an error — it is simply where the tree
    /// runs out.
    #[test]
    fn a_depth_past_the_leaves_stops_there() {
        let (plan, trees) = with_contents();

        let deep = render_clean(&plan, None, Intent::Preview, at(9, Sort::Name), &trees);
        let exact = render_clean(&plan, None, Intent::Preview, at(4, Sort::Name), &trees);

        assert_eq!(deep, exact, "there was nothing further to show");
    }

    /// Depth is display only, and a candidate is removed whole — so no level
    /// may change what the run would free.
    #[test]
    fn unfolding_a_candidate_changes_no_total() {
        let (plan, trees) = with_contents();

        let totals: Vec<String> = (0..5)
            .map(|depth| {
                render_clean(&plan, None, Intent::Preview, at(depth, Sort::Name), &trees)
                    .lines()
                    .find(|line| line.starts_with("Reclaimable:"))
                    .expect("a total")
                    .to_owned()
            })
            .collect();

        assert_eq!(totals[0], "Reclaimable: 4.0M");
        assert!(totals.iter().all(|total| *total == totals[0]), "{totals:?}");
    }

    #[test]
    fn children_follow_the_same_order_as_their_parents() {
        let (plan, trees) = with_contents();

        let report = render_clean(&plan, None, Intent::Preview, at(2, Sort::Size), &trees);

        let children: Vec<&str> = report
            .lines()
            .filter(|line| !line.contains("/p/target "))
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter(|name| {
                name.starts_with("debug") || name.starts_with("release") || name.starts_with('.')
            })
            .collect();
        assert_eq!(children, ["debug/", "release/", ".rustc_info.json"]);
    }

    /// Below `-d 2` the caller keeps no trees, because a tree costs ~630 bytes
    /// per entry and the flag was not passed. The renderer must be content with
    /// that rather than treat it as a failure.
    #[test]
    fn with_no_trees_the_candidate_line_stands_alone() {
        let (plan, _) = with_contents();

        let report = render_clean(&plan, None, Intent::Preview, at(2, Sort::Name), &[]);

        assert!(report.contains("/p/target"), "{report}");
        assert!(!report.contains("debug/"), "{report}");
    }

    #[test]
    fn dry_run_lists_candidates_with_a_total() {
        let report = rendered(
            &plan(vec![
                candidate("/p/node_modules", "node-modules", Tier::Trash, 1_048_576),
                candidate("/p/old.bin", "old", Tier::Confirm, 1_048_576),
            ]),
            None,
            Intent::Preview,
            listed(),
        );

        // Path, rule and tier for each, then the total.
        assert!(report.contains("/p/node_modules"), "{report}");
        assert!(report.contains("node-modules"), "{report}");
        assert!(report.contains("trash"), "{report}");
        assert!(report.contains("/p/old.bin"), "{report}");
        assert!(report.contains("old"), "{report}");
        assert!(report.contains("confirm"), "{report}");
        assert!(report.contains("Reclaimable: 2.0M"), "{report}");
        assert!(
            report.contains("Preview") && report.contains("`clean`"),
            "a preview must say it removed nothing and name the verb that does: {report}"
        );
    }

    /// The soft-total wording. "May free less" is the claim the marker supports;
    /// "will not be freed" would be a different, stronger and wrong one.
    #[test]
    fn shared_candidate_is_flagged_and_total_labelled_upper_bound() {
        let mut shared = candidate("/p/node_modules", "node-modules", Tier::Trash, 2048);
        shared.shared = true;

        let report = rendered(&plan(vec![shared]), None, Intent::Preview, listed());

        assert!(
            report.contains("(shared)"),
            "the candidate is flagged: {report}"
        );
        assert!(
            report.contains("at most") && report.contains("may free less"),
            "the total must read as an upper bound: {report}"
        );
        assert!(
            !report.contains("will not be freed"),
            "the marker means the figure is soft, not that the bytes are unreachable: {report}"
        );
    }

    #[test]
    fn an_unshared_plan_states_the_total_plainly() {
        let report = rendered(
            &plan(vec![candidate(
                "/p/node_modules",
                "node-modules",
                Tier::Trash,
                2048,
            )]),
            None,
            Intent::Preview,
            listed(),
        );

        assert!(report.contains("Reclaimable: 2.0K"), "{report}");
        assert!(
            !report.contains("at most"),
            "with nothing shared the figure is exact: {report}"
        );
    }

    #[test]
    fn empty_plan_renders_a_plain_message() {
        let report = rendered(&plan(Vec::new()), None, Intent::Preview, listed());

        assert_eq!(report, "Nothing to clean.\n");
    }

    /// Even with no candidates, a refusal is worth reporting — otherwise a user
    /// whose whole scan was denylisted sees only "nothing to clean".
    #[test]
    fn an_empty_plan_still_reports_what_was_refused() {
        let mut empty = plan(Vec::new());
        empty.excluded = vec![Excluded {
            path: PathBuf::from("/Windows/target"),
            reason: ExcludeReason::Denylisted,
        }];

        let report = rendered(&empty, None, Intent::Preview, listed());

        assert!(report.contains("Nothing to clean."), "{report}");
        assert!(report.contains("/Windows/target"), "{report}");
    }

    /// The distinction that decides whether the user reaches for a flag.
    #[test]
    fn a_denylisted_exclusion_offers_no_remedy_and_a_dirty_one_does() {
        let mut with_both = plan(Vec::new());
        with_both.excluded = vec![
            Excluded {
                path: PathBuf::from("/Windows/target"),
                reason: ExcludeReason::Denylisted,
            },
            Excluded {
                path: PathBuf::from("/repo/target"),
                reason: ExcludeReason::DirtyRepo,
            },
        ];

        let report = rendered(&with_both, None, Intent::Preview, listed());
        let denied = report
            .lines()
            .find(|line| line.contains("/Windows/target"))
            .expect("the denylisted line is present");
        let dirty = report
            .lines()
            .find(|line| line.contains("/repo/target"))
            .expect("the dirty-repo line is present");

        assert!(
            !denied.contains("requires-clean-repo"),
            "nothing overrides the denylist, so the line must offer nothing: {denied}"
        );
        assert!(
            denied.contains("no flag removes this"),
            "and it should say so: {denied}"
        );
        assert!(
            dirty.contains("requires-clean-repo"),
            "the guard is a rule setting, and the line has to name it — `--allow-dirty` \
             went in v0.5 and a message offering it would not even parse: {dirty}"
        );
    }

    /// `--safe` hides confirm-tier candidates without recording them in the
    /// plan. The report is where the user learns there was something there.
    #[test]
    fn the_safe_filter_reports_how_many_it_hid() {
        let report = rendered(
            &plan(vec![candidate(
                "/p/node_modules",
                "node-modules",
                Tier::Trash,
                2048,
            )]),
            Some(3),
            Intent::Preview,
            listed(),
        );

        assert!(
            report.contains("3 more candidates need confirmation"),
            "{report}"
        );
        assert!(report.contains("--safe"), "{report}");
    }

    #[test]
    fn nothing_hidden_says_nothing() {
        let report = rendered(
            &plan(vec![candidate("/p/nm", "node-modules", Tier::Trash, 1)]),
            Some(0),
            Intent::Preview,
            listed(),
        );

        assert!(
            !report.contains("need confirmation"),
            "an empty hidden count must not produce a line: {report}"
        );
    }

    /// One candidate reads "1 candidate", not "1 candidates" — the report is
    /// prose a person reads under some pressure.
    #[test]
    fn counts_are_singular_where_they_should_be() {
        let mut shared = candidate("/p/nm", "node-modules", Tier::Trash, 2048);
        shared.shared = true;
        let report = rendered(&plan(vec![shared]), Some(1), Intent::Preview, listed());

        assert!(report.contains("1 candidate shares"), "{report}");
        assert!(
            report.contains("1 more candidate needs confirmation; --safe is hiding it."),
            "the verb and the pronoun agree with the noun, not only with the count:\n{report}"
        );
    }

    /// The column answers "what does `clean` do with this", which is what the
    /// three names were chosen to answer — so `--purge` overruling a tier shows
    /// there, and the tier itself is left alone on the candidate for `--safe`
    /// and the refusal to read.
    #[test]
    fn the_column_says_what_will_happen() {
        assert_eq!(fate(Tier::Purge, true), "purge");
        assert_eq!(fate(Tier::Trash, false), "trash");
        assert_eq!(fate(Tier::Confirm, false), "confirm");

        for tier in [Tier::Purge, Tier::Trash, Tier::Confirm] {
            assert_eq!(
                fate(tier, true),
                "purge",
                "{tier:?} under --purge is destroyed like everything else"
            );
        }
    }

    /// A user's rule name is not from a fixed set, so the column has to size
    /// itself from the data rather than from a known-longest label.
    #[test]
    fn the_rule_column_fits_the_longest_name_present() {
        let plan = plan(vec![
            candidate("/p/a", "nm", Tier::Trash, 1024),
            candidate("/p/b", "a-very-long-user-rule-name", Tier::Trash, 1024),
        ]);

        let report = rendered(&plan, None, Intent::Preview, listed());

        assert!(
            report.contains("nm                          trash"),
            "the short name must be padded to the long one:\n{report}"
        );
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::*;
    use disk_tools_core::Reclaimed;
    use disk_tools_core::TrashFailure;
    use std::path::PathBuf;

    fn failure(path: &str) -> TrashFailure {
        TrashFailure {
            path: PathBuf::from(path),
            reason: "Permission denied".to_owned(),
        }
    }

    #[test]
    fn a_complete_run_states_what_it_freed() {
        let outcome = CleanOutcome {
            trashed: Reclaimed {
                paths: vec![PathBuf::from("/p/node_modules")],
                bytes: 2048,
            },
            purged: Reclaimed::default(),
            failed: Vec::new(),
        };

        let report = render_outcome(&outcome, false);

        assert!(report.contains("Removed 1 of 1"), "{report}");
        assert!(report.contains("Freed 2.0K"), "{report}");
        assert!(
            !report.contains("still on disk"),
            "nothing failed, so nothing is left: {report}"
        );
    }

    /// The sentence a user reads to find out whether their work survived. After
    /// a run that removed nothing it must not claim anything is recoverable —
    /// there is nothing in the trash to put back.
    #[test]
    fn a_run_that_removed_nothing_promises_nothing() {
        let outcome = CleanOutcome {
            trashed: Reclaimed {
                paths: Vec::new(),
                bytes: 0,
            },
            purged: Reclaimed::default(),
            failed: vec![failure("/p/one"), failure("/p/two")],
        };

        let report = render_outcome(&outcome, false);

        assert!(report.contains("Removed nothing."), "{report}");
        assert!(report.contains("2 candidates still on disk"), "{report}");
        assert!(
            !report.contains("in the trash"),
            "nothing was trashed, so nothing may be described as recoverable: {report}"
        );
    }

    /// And when something *was* removed, the claim has to name what it covers —
    /// it sits directly under the failure list, where "everything above" would
    /// read as promising exactly the items that are still on disk.
    #[test]
    fn a_partial_run_says_which_items_are_recoverable() {
        let outcome = CleanOutcome {
            trashed: Reclaimed {
                paths: vec![PathBuf::from("/p/gone")],
                bytes: 1024,
            },
            purged: Reclaimed::default(),
            failed: vec![failure("/p/stuck")],
        };

        let report = render_outcome(&outcome, false);

        assert!(report.contains("1 candidate still on disk"), "{report}");
        assert!(
            report.contains("1 candidate is in the trash"),
            "the recoverable set is named, not implied: {report}"
        );
        assert!(
            !report.contains("Everything above"),
            "which is exactly the phrasing that would have been wrong: {report}"
        );
    }

    /// The freed figure carries the same hedge the dry-run total did. Stating it
    /// carefully before and flatly after would be one run disagreeing with
    /// itself, and the flat version is the wrong one.
    #[test]
    fn a_shared_removal_reports_the_freed_total_as_an_upper_bound() {
        let outcome = CleanOutcome {
            trashed: Reclaimed {
                paths: vec![PathBuf::from("/p/node_modules")],
                bytes: 4096,
            },
            purged: Reclaimed::default(),
            failed: Vec::new(),
        };

        assert!(
            render_outcome(&outcome, true).contains("Freed at most 4.0K"),
            "{}",
            render_outcome(&outcome, true)
        );
        assert!(
            render_outcome(&outcome, false).contains("Freed 4.0K"),
            "and without sharing the figure is exact"
        );
    }

    #[test]
    fn counts_are_singular_where_they_should_be() {
        let outcome = CleanOutcome {
            trashed: Reclaimed {
                paths: Vec::new(),
                bytes: 0,
            },
            purged: Reclaimed::default(),
            failed: vec![failure("/p/one")],
        };

        assert!(
            render_outcome(&outcome, false).contains("1 candidate still on disk"),
            "not `1 candidates`"
        );
    }
}

#[cfg(test)]
mod intent_tests {
    use super::*;
    use std::path::PathBuf;

    fn one_candidate() -> CleanPlan {
        CleanPlan {
            candidates: vec![Candidate {
                path: PathBuf::from("/p/node_modules"),
                rule: "node-modules".into(),
                tier: Tier::Trash,
                purge: false,
                duplicate_of: None,
                allocated: 2048,
                shared: false,
            }],
            reclaimable: 2048,
            excluded: Vec::new(),
            filtered_out: 0,
            too_small: 0,
            below_rule_minimum: 0,
        }
    }

    /// The regression this enum exists to prevent, and the reason it is worth an
    /// enum rather than a `bool`.
    ///
    /// Shown before an `--apply`, "nothing was removed" is about to become
    /// false — making the last sentence a user reads before a deletion the one
    /// line in the report that is a lie. It was written that way first, and only
    /// caught by running the binary, because every test called `render_clean`
    /// directly and none knew about the `--apply` path.
    #[test]
    fn a_preview_does_not_claim_nothing_was_removed() {
        let report = rendered(&one_candidate(), None, Intent::Removing, listed());

        assert!(
            report.contains("/p/node_modules"),
            "the preview still lists what is about to go: {report}"
        );
        assert!(
            !report.contains("Preview"),
            "but must not call itself a preview, because it is not: {report}"
        );
        assert!(
            !report.contains("nothing was removed"),
            "nor that nothing was removed, moments before removing it: {report}"
        );
    }

    /// And the preview still says so — the guard must not have silenced both.
    #[test]
    fn a_preview_still_says_it_removed_nothing() {
        let report = rendered(&one_candidate(), None, Intent::Preview, listed());

        assert!(
            report
                .contains("Preview — nothing was removed. The same line with `clean` removes it."),
            "{report}"
        );
    }
}

#[cfg(test)]
mod purge_tests {
    use super::*;
    use disk_tools_core::Reclaimed;
    use disk_tools_core::TrashFailure;
    use std::path::PathBuf;

    fn partial() -> CleanOutcome {
        CleanOutcome {
            trashed: Reclaimed {
                paths: vec![PathBuf::from("/p/gone")],
                bytes: 1024,
            },
            purged: Reclaimed::default(),
            failed: vec![TrashFailure {
                path: PathBuf::from("/p/stuck"),
                reason: "Permission denied".to_owned(),
            }],
        }
    }

    /// The recoverability line is read off the outcome, not off a flag: after a
    /// run that destroyed everything it would be a plain lie, since there is
    /// nothing in the trash to put back.
    #[test]
    fn a_purged_run_never_claims_anything_can_be_put_back() {
        let purged = CleanOutcome {
            purged: partial().trashed,
            trashed: Reclaimed::default(),
            failed: partial().failed,
        };

        let report = render_outcome(&purged, false);

        assert!(report.contains("1 candidate still on disk"), "{report}");
        assert!(
            !report.contains("trash") && !report.contains("put back"),
            "nothing was trashed, so nothing may be described as recoverable: {report}"
        );
    }

    /// And the trashing run still says it — the guard must not have silenced
    /// both.
    #[test]
    fn a_trashed_run_still_says_what_can_be_put_back() {
        let report = render_outcome(&partial(), false);

        assert!(
            report.contains("in the trash and can be put back"),
            "{report}"
        );
    }
}

#[cfg(test)]
mod min_size_tests {
    use super::*;
    use std::path::PathBuf;

    /// The line has to name its own remedy. Folding it into the `--safe` message
    /// would point the user at a flag that has nothing to do with why the entry
    /// is missing.
    #[test]
    fn candidates_below_the_threshold_are_counted_with_their_own_reason() {
        let mut plan = CleanPlan {
            candidates: vec![Candidate {
                path: PathBuf::from("/p/node_modules"),
                rule: "node-modules".into(),
                tier: Tier::Trash,
                purge: false,
                duplicate_of: None,
                allocated: 2_000_000,
                shared: false,
            }],
            reclaimable: 2_000_000,
            excluded: Vec::new(),
            filtered_out: 0,
            too_small: 150,
            below_rule_minimum: 0,
        };

        let report = rendered(&plan, None, Intent::Preview, listed());

        assert!(
            report.contains("150 more candidates are below --min-size"),
            "{report}"
        );
        assert!(
            !report.contains("--safe"),
            "the remedy is the size flag, not the tier one: {report}"
        );

        // And with all three narrowings in effect, all three are stated.
        plan.filtered_out = 3;
        plan.below_rule_minimum = 7;
        let all = rendered(&plan, Some(3), Intent::Preview, listed());
        assert!(all.contains("3 more candidates need confirmation"), "{all}");
        assert!(
            all.contains("150 more candidates are below --min-size"),
            "{all}"
        );
        assert!(
            all.contains("7 more candidates are below their rule's own min-size"),
            "{all}"
        );
    }

    /// The two size thresholds are not the same statement, and they do not have
    /// the same remedy: one is a flag on this command line, the other is a line
    /// in a file the user has to go and find. Naming `--min-size` for a rule's
    /// threshold sends them to change something they never set.
    #[test]
    fn a_rules_own_threshold_is_reported_apart_from_the_flag() {
        let plan = CleanPlan {
            candidates: Vec::new(),
            reclaimable: 0,
            excluded: Vec::new(),
            filtered_out: 0,
            too_small: 0,
            below_rule_minimum: 2,
        };

        let report = rendered(&plan, None, Intent::Preview, listed());

        assert!(
            report.contains("2 more candidates are below their rule's own min-size"),
            "{report}"
        );
        assert!(
            report.contains("edit the rule"),
            "and it must say where to go: {report}"
        );
        assert!(
            !report.contains("--min-size"),
            "naming a flag the user never passed is the bug this splits: {report}"
        );
    }

    #[test]
    fn one_below_a_rules_threshold_reads_singular() {
        let plan = CleanPlan {
            candidates: Vec::new(),
            reclaimable: 0,
            excluded: Vec::new(),
            filtered_out: 0,
            too_small: 0,
            below_rule_minimum: 1,
        };

        let report = rendered(&plan, None, Intent::Preview, listed());

        assert!(
            report.contains("1 more candidate is below their rule's own min-size"),
            "{report}"
        );
        assert!(
            report.contains("edit the rule to see it."),
            "the pronoun agrees too: {report}"
        );
    }

    #[test]
    fn nothing_below_the_threshold_says_nothing() {
        let plan = CleanPlan {
            candidates: Vec::new(),
            reclaimable: 0,
            excluded: Vec::new(),
            filtered_out: 0,
            too_small: 0,
            below_rule_minimum: 0,
        };

        assert!(!rendered(&plan, None, Intent::Preview, listed(),).contains("--min-size"));
    }

    #[test]
    fn one_below_the_threshold_reads_singular() {
        let plan = CleanPlan {
            candidates: Vec::new(),
            reclaimable: 0,
            excluded: Vec::new(),
            filtered_out: 0,
            too_small: 1,
            below_rule_minimum: 0,
        };

        assert!(
            rendered(&plan, None, Intent::Preview, listed(),).contains("1 more candidate is below"),
            "not `1 more candidates are`"
        );
    }
}
