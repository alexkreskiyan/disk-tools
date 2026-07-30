//! The duplicate plan, as a report.
//!
//! Every other report in this tool is a list of things to remove. This one is a
//! list of **groups**, each of which keeps one file — so the unit of the report
//! is not a candidate, and the one thing it must never be ambiguous about is
//! which path in a group is the one that stays.
//!
//! It renders from the [`CleanPlan`], not from the pass that produced it. That
//! matters: a copy the denylist refused is in the pass's groups and not in the
//! plan, and a report drawn from the former would offer to remove something
//! `clean` would then refuse. What the plan says is what happens.

use super::clean::write_notices;
use super::{age, tree::format_size};
use crate::args::{Intent, Report, Sort};
use disk_tools_core::{Candidate, CleanPlan, Keep, Kept};
use std::fmt::Write;
use std::time::SystemTime;

/// One group as the report sees it: the copy that stays, and what goes with it.
struct Group<'a> {
    kept: &'a Kept,
    copies: Vec<&'a Candidate>,
    /// What removing every copy here would free.
    reclaimable: u64,
    /// One copy's own size — the same for all of them, give or take a
    /// filesystem, and what makes "×3" mean something.
    each: u64,
}

/// Render a plan whose candidates are redundant copies.
///
/// `keep` is the rule that was in force, needed for one sentence: a group whose
/// keeper had to be settled some other way has to say **which** other way, and
/// that only reads as an answer beside the rule that was asked for.
pub fn render_dup(
    plan: &CleanPlan,
    hidden_by_safe: Option<usize>,
    intent: Intent,
    report: Report,
    keep: Keep,
    now: SystemTime,
) -> String {
    let mut out = String::new();
    let groups = group(&plan.candidates, report.sort);

    if groups.is_empty() {
        // Two knobs explain almost every empty run, and naming them is the
        // difference between "there are none" and "you did not look there".
        let _ = writeln!(
            out,
            "No duplicates found.\n\n\
             Files below --min-size are never compared, and anything inside a directory \
             your rules claim — a node_modules, a target — is skipped whole."
        );
        write_notices(plan, hidden_by_safe, intent, &mut out);
        return out;
    }

    // Said once, at the top. Every line below shows a size next to a path that
    // is *not* being removed, and a reader who has to work that out from the
    // columns will get it wrong exactly once.
    let _ = writeln!(
        out,
        "Each group keeps one copy. The size is what removing the others frees.\n"
    );

    if report.depth == 0 {
        write_groups(&groups, &mut out);
    } else {
        write_copies(&groups, keep, now, &mut out);
    }

    let _ = writeln!(out);
    write_total(plan, &groups, &mut out);
    write_degraded(&groups, keep, &mut out);
    write_notices(plan, hidden_by_safe, intent, &mut out);

    out
}

/// Gather the plan's candidates back into the groups they came from.
///
/// Keyed on the keeper's path, which every candidate of a group carries. That is
/// what lets the plan stay a flat, mergeable list and still be shown as groups.
fn group<'a>(candidates: &'a [Candidate], sort: Sort) -> Vec<Group<'a>> {
    let mut groups: Vec<Group<'a>> = Vec::new();

    for candidate in candidates {
        // A rule-claimed candidate has no keeper and cannot appear here. The two
        // sources are never mixed in one plan, so this is a guard rather than a
        // case.
        let Some(kept) = &candidate.duplicate_of else {
            continue;
        };
        match groups.iter_mut().find(|g| g.kept.path == kept.path) {
            Some(group) => {
                group.reclaimable += candidate.allocated;
                group.copies.push(candidate);
            }
            None => groups.push(Group {
                kept,
                reclaimable: candidate.allocated,
                each: candidate.allocated,
                copies: vec![candidate],
            }),
        }
    }

    for group in &mut groups {
        group.copies.sort_by(|a, b| a.path.cmp(&b.path));
    }
    match sort {
        Sort::Name => groups.sort_by(|a, b| a.kept.path.cmp(&b.kept.path)),
        // By keeper path where the sizes tie, as everywhere: a report whose
        // order changes between two runs over one unchanged disk cannot be
        // diffed.
        Sort::Size => groups.sort_by(|a, b| {
            b.reclaimable
                .cmp(&a.reclaimable)
                .then(a.kept.path.cmp(&b.kept.path))
        }),
    }
    groups
}

/// One line per group — the default, because a duplicate search returns
/// hundreds and the question asked of it is which of them are worth acting on.
fn write_groups(groups: &[Group<'_>], out: &mut String) {
    let count_width = groups
        .iter()
        .map(|g| (g.copies.len() + 1).to_string().len())
        .max()
        .unwrap_or(1);

    for group in groups {
        let _ = writeln!(
            out,
            "{size:>8}  ×{count:<cw$}  keeps {path}{degraded}",
            size = format_size(group.reclaimable),
            count = group.copies.len() + 1,
            path = group.kept.path.display(),
            degraded = if group.kept.fell_back { "  (*)" } else { "" },
            cw = count_width,
        );
    }
}

/// Every path, said plainly: one `keep`, the rest `remove`.
fn write_copies(groups: &[Group<'_>], keep: Keep, now: SystemTime, out: &mut String) {
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
            let _ = writeln!(out);
        }
        let _ = writeln!(
            out,
            "{size:>8}  ×{count}  {each} each",
            size = format_size(group.reclaimable),
            count = group.copies.len() + 1,
            each = format_size(group.each),
        );
        let _ = writeln!(
            out,
            "  keep    {path}{basis}",
            path = group.kept.path.display(),
            basis = basis(group.kept, keep, now),
        );
        for copy in &group.copies {
            let _ = writeln!(
                out,
                "  remove  {path}{shared}",
                path = copy.path.display(),
                shared = if copy.shared { "  (shared)" } else { "" },
            );
        }
    }
}

/// Why *this* copy is the one that stays.
///
/// The keeper rule was changed once already because its basis was invisible: on
/// live data the byte-first path kept `IMG (1).jpg` over `IMG.jpg`, and nothing
/// in the report said what it had gone on. A rule you cannot check is one you
/// can only trust.
fn basis(kept: &Kept, keep: Keep, now: SystemTime) -> String {
    let Some(date) = kept.date else {
        return match keep {
            // Nothing was read, and nothing is claimed.
            Keep::First => String::new(),
            _ => "   (no dates; kept the first path)".to_owned(),
        };
    };

    let word = match (keep, kept.fell_back) {
        (Keep::OldestCreated | Keep::NewestCreated, false) => "created",
        (Keep::OldestModified | Keep::NewestModified, false) => "modified",
        // Degraded: the label has to name the date that actually decided, or the
        // report claims a creation time no file in the group had.
        (Keep::OldestCreated | Keep::NewestCreated, true) => "modified, no created",
        (Keep::OldestModified | Keep::NewestModified, true) => "created, no modified",
        (Keep::First, _) => return String::new(),
    };
    format!("   ({word} {})", age(now, Some(date)))
}

fn write_total(plan: &CleanPlan, groups: &[Group<'_>], out: &mut String) {
    let copies: usize = groups.iter().map(|g| g.copies.len()).sum();
    let shared = plan.candidates.iter().filter(|c| c.shared).count();
    let total = format_size(plan.reclaimable);
    let (group_noun, copy_noun) = (noun(groups.len(), "group"), noun(copies, "copy"));

    if shared == 0 {
        let _ = writeln!(
            out,
            "Reclaimable: {total} — {} {copy_noun} in {} {group_noun}.",
            copies,
            groups.len()
        );
        return;
    }

    // The same hedge the rule report carries, for the same reason: a copy whose
    // inode has another name frees nothing while that name survives.
    let _ = writeln!(
        out,
        "Reclaimable: at most {total} — {} {copy_noun} in {} {group_noun}, of which {shared} \
         {} content with something outside the group, so removing {} may free less.",
        copies,
        groups.len(),
        if shared == 1 { "shares" } else { "share" },
        if shared == 1 { "it" } else { "them" },
    );
}

/// Say how many keepers were not chosen the way that was asked.
///
/// Never silent. "Kept the oldest" and "kept whatever had a date" are different
/// claims, and a report that made the second while printing the first would be
/// wrong in precisely the way the fallback exists to avoid.
fn write_degraded(groups: &[Group<'_>], keep: Keep, out: &mut String) {
    let degraded = groups.iter().filter(|g| g.kept.fell_back).count();
    if degraded == 0 {
        return;
    }
    let missing = match keep {
        Keep::OldestCreated | Keep::NewestCreated => "creation time",
        Keep::OldestModified | Keep::NewestModified => "modification time",
        // `First` reads no date, so it cannot degrade and this cannot be hit.
        Keep::First => return,
    };
    let _ = writeln!(
        out,
        "\n(*) {degraded} {} no {missing} at all; {} keeper was settled by the other date, \
         or failing that by the path.",
        if degraded == 1 {
            "group has"
        } else {
            "groups have"
        },
        if degraded == 1 { "its" } else { "their" },
    );
}

/// "1 group" / "2 groups", and "1 copy" / "2 copies".
fn noun(count: usize, singular: &'static str) -> String {
    if count == 1 {
        return singular.to_owned();
    }
    match singular {
        "copy" => "copies".to_owned(),
        other => format!("{other}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_tools_core::Tier;
    use std::path::PathBuf;
    use std::time::Duration;

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_750_000_000)
    }

    fn days(n: u64) -> Option<SystemTime> {
        Some(now() - Duration::from_secs(n * 24 * 60 * 60))
    }

    fn kept(path: &str, date: Option<SystemTime>, fell_back: bool) -> Kept {
        Kept {
            path: PathBuf::from(path),
            date,
            fell_back,
        }
    }

    fn copy(path: &str, allocated: u64, kept: &Kept) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            rule: "duplicate".into(),
            tier: Tier::Confirm,
            purge: false,
            duplicate_of: Some(kept.clone()),
            allocated,
            shared: false,
        }
    }

    fn plan_of(candidates: Vec<Candidate>) -> CleanPlan {
        CleanPlan {
            reclaimable: candidates.iter().map(|c| c.allocated).sum(),
            candidates,
            ..CleanPlan::default()
        }
    }

    fn a_plan() -> CleanPlan {
        let one = kept("/p/keep-one.bin", days(400), false);
        let two = kept("/p/keep-two.bin", days(10), false);
        plan_of(vec![
            copy("/p/a-copy.bin", 4096, &one),
            copy("/p/z-copy.bin", 4096, &one),
            copy("/p/small.bin", 1024, &two),
        ])
    }

    fn report(depth: usize, sort: Sort) -> Report {
        Report {
            depth,
            sort,
            json: false,
        }
    }

    fn rendered(plan: &CleanPlan, depth: usize, keep: Keep) -> String {
        render_dup(
            plan,
            None,
            Intent::Preview,
            report(depth, Sort::Size),
            keep,
            now(),
        )
    }

    #[test]
    fn a_group_is_one_line_at_depth_zero() {
        let shown = rendered(&a_plan(), 0, Keep::OldestCreated);

        assert!(
            shown.contains("×3  keeps /p/keep-one.bin"),
            "two copies and the one that stays make three: {shown}"
        );
        assert!(shown.contains("×2  keeps /p/keep-two.bin"), "{shown}");
        assert!(
            !shown.contains("  remove  "),
            "the paths themselves belong to -d 1: {shown}"
        );
    }

    /// The one thing this report may not leave to inference.
    #[test]
    fn the_report_says_which_path_is_the_one_that_stays() {
        let shown = rendered(&a_plan(), 0, Keep::OldestCreated);
        assert!(shown.contains("Each group keeps one copy"), "{shown}");
    }

    #[test]
    fn depth_one_names_every_path_as_keep_or_remove() {
        let shown = rendered(&a_plan(), 1, Keep::OldestCreated);

        assert!(shown.contains("  keep    /p/keep-one.bin"), "{shown}");
        assert!(shown.contains("  remove  /p/a-copy.bin"), "{shown}");
        assert!(shown.contains("  remove  /p/z-copy.bin"), "{shown}");
    }

    /// The keeper rule was changed once because nothing showed what it had gone
    /// on. Whatever else the report drops, it does not drop this.
    #[test]
    fn the_keeper_shows_the_date_that_decided_it() {
        let shown = rendered(&a_plan(), 1, Keep::OldestCreated);
        assert!(shown.contains("(created 1y)"), "{shown}");

        let by_mtime = rendered(&a_plan(), 1, Keep::NewestModified);
        assert!(by_mtime.contains("(modified 1y)"), "{by_mtime}");
    }

    /// A rule that reads no date claims none.
    #[test]
    fn keep_first_shows_no_basis() {
        let one = kept("/p/keeper.bin", None, false);
        let plan = plan_of(vec![copy("/p/copy.bin", 4096, &one)]);

        let shown = rendered(&plan, 1, Keep::First);
        assert!(shown.contains("keep    /p/keeper.bin\n"), "{shown}");
    }

    /// Degraded, the label must name the date that actually decided — claiming
    /// a creation time no file had is the misleading the fallback exists to
    /// prevent.
    #[test]
    fn a_degraded_group_names_the_date_it_fell_back_to() {
        let one = kept("/p/keeper.bin", days(30), true);
        let plan = plan_of(vec![copy("/p/copy.bin", 4096, &one)]);

        let shown = rendered(&plan, 1, Keep::OldestCreated);
        assert!(shown.contains("(modified, no created 1mo)"), "{shown}");
        assert!(
            shown.contains("1 group has no creation time"),
            "and the run says how many: {shown}"
        );
    }

    #[test]
    fn a_degraded_group_is_marked_at_depth_zero_too() {
        let one = kept("/p/keeper.bin", days(30), true);
        let plan = plan_of(vec![copy("/p/copy.bin", 4096, &one)]);

        let shown = rendered(&plan, 0, Keep::OldestCreated);
        assert!(shown.contains("(*)"), "{shown}");
        assert!(
            shown.contains("(*) 1 group has no creation time"),
            "{shown}"
        );
    }

    #[test]
    fn nothing_degraded_says_nothing() {
        let shown = rendered(&a_plan(), 0, Keep::OldestCreated);
        assert!(!shown.contains("(*)"), "{shown}");
    }

    #[test]
    fn groups_are_ordered_by_what_they_free_then_by_keeper() {
        let shown = rendered(&a_plan(), 0, Keep::OldestCreated);
        let first = shown.find("keep-one").expect("group one");
        let second = shown.find("keep-two").expect("group two");
        assert!(first < second, "the bigger reclaim comes first: {shown}");

        let by_name = render_dup(
            &a_plan(),
            None,
            Intent::Preview,
            report(0, Sort::Name),
            Keep::OldestCreated,
            now(),
        );
        assert!(
            by_name.find("keep-one").expect("group one")
                < by_name.find("keep-two").expect("group two"),
            "{by_name}"
        );
    }

    #[test]
    fn the_total_counts_copies_and_groups() {
        let shown = rendered(&a_plan(), 0, Keep::OldestCreated);
        assert!(
            shown.contains("Reclaimable: 9.0K — 3 copies in 2 groups."),
            "{shown}"
        );
    }

    #[test]
    fn a_shared_copy_softens_the_total_and_is_marked() {
        let one = kept("/p/keeper.bin", days(1), false);
        let mut candidate = copy("/p/copy.bin", 4096, &one);
        candidate.shared = true;
        let plan = plan_of(vec![candidate]);

        let shown = rendered(&plan, 1, Keep::OldestCreated);
        assert!(shown.contains("at most"), "{shown}");
        assert!(shown.contains("(shared)"), "{shown}");
    }

    /// "There are none" and "you did not look there" are different statements,
    /// and only one of them is true after a run with a 1 MiB floor.
    #[test]
    fn an_empty_plan_names_what_would_explain_it() {
        let shown = rendered(&CleanPlan::default(), 0, Keep::OldestCreated);

        assert!(shown.contains("No duplicates found."), "{shown}");
        assert!(shown.contains("--min-size"), "{shown}");
        assert!(shown.contains("node_modules"), "{shown}");
    }

    /// The shared tail: this report is acted on the same way the other one is.
    #[test]
    fn the_closing_line_still_names_the_other_verb() {
        let shown = rendered(&a_plan(), 0, Keep::OldestCreated);
        assert!(shown.contains("`clean`"), "{shown}");

        let removing = render_dup(
            &a_plan(),
            None,
            Intent::Removing,
            report(0, Sort::Size),
            Keep::OldestCreated,
            now(),
        );
        assert!(
            !removing.contains("nothing was removed"),
            "the removal is about to happen: {removing}"
        );
    }

    /// Which report is chosen is settled by the mode, not guessed from the
    /// plan — but every candidate still carries its keeper, and that is what
    /// the grouping reads.
    #[test]
    fn every_candidate_carries_the_copy_kept_instead_of_it() {
        let plan = a_plan();
        assert!(plan.candidates.iter().all(|c| c.duplicate_of.is_some()));
        assert_eq!(
            plan.candidates[0]
                .duplicate_of
                .as_ref()
                .expect("keeper")
                .path,
            PathBuf::from("/p/keep-one.bin")
        );
    }
}
