//! The dry-run report — what a cleanup *would* do.
//!
//! This is the safety story's user-facing half. A dry run is the default, so
//! this text is what someone reads before deciding to type `--apply`, and every
//! wording choice below exists because getting it wrong would mislead them into
//! a deletion.
//!
//! Shaped like [`super::skipped`] — a flat list and a summary — rather than
//! [`super::tree`]: bars and percentages answer "what is big", and this answers
//! "what would go".

use super::tree::format_size;
use disk_tools_core::{
    Candidate, Category, CleanOutcome, CleanPlan, ExcludeReason, Excluded, Tier,
};
use std::fmt::Write;

/// Why the plan is being shown.
///
/// The same list serves two moments, and the closing line must not. Printed
/// before an `--apply`, "nothing was removed" is about to become false — which
/// would make the last thing a user reads before a deletion the one sentence in
/// the report that is a lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// A dry run: this is the whole output, and nothing will happen.
    DryRun,
    /// A preview: the removal follows immediately.
    AboutToApply,
}

/// Render the plan for a human.
///
/// `hidden_by_safe` is how many candidates `--safe` kept out, or `None` when the
/// flag was not used. The core does not record them — that is deliberate, since
/// `--safe` is the user's own narrowing rather than a refusal — so the count is
/// passed in by the caller, which plans a second time to obtain it.
pub fn render_clean(plan: &CleanPlan, hidden_by_safe: Option<usize>, intent: Intent) -> String {
    let mut out = String::new();

    if plan.candidates.is_empty() {
        // Finding nothing is an ordinary outcome, not a failure, and the report
        // should not make it read like one.
        let _ = writeln!(out, "Nothing to clean.");
    } else {
        write_candidates(&plan.candidates, &mut out);
        let _ = writeln!(out);
        write_total(plan, &mut out);
    }

    if !plan.excluded.is_empty() {
        let _ = writeln!(out);
        write_excluded(&plan.excluded, &mut out);
    }

    if let Some(hidden) = hidden_by_safe.filter(|&n| n > 0) {
        let noun = if hidden == 1 {
            "candidate"
        } else {
            "candidates"
        };
        let _ = writeln!(
            out,
            "\n{hidden} more {noun} need confirmation; --safe is hiding them."
        );
    }

    if !plan.candidates.is_empty() && intent == Intent::DryRun {
        let _ = writeln!(out, "\nDry run — nothing was removed. Re-run with --apply.");
    }

    out
}

/// Report what a cleanup actually did.
///
/// The concept's rule is that there is no partial **silent** delete. A partial
/// delete is acceptable; leaving the user to guess which half happened is not.
/// So a failure names its path and what the OS said, and the closing line states
/// plainly that something is still there.
pub fn render_outcome(outcome: &CleanOutcome, shared_removed: bool, purged: bool) -> String {
    let mut out = String::new();
    let attempted = outcome.removed.len() + outcome.failed.len();

    if outcome.removed.is_empty() {
        let _ = writeln!(out, "Removed nothing.");
    } else {
        // "At most" whenever something removed held content reachable from
        // outside it — the same hedge the dry run's total carries, because the
        // figure has exactly the same softness. Saying it carefully in the
        // preview and flatly in the outcome would be one run disagreeing with
        // itself, and the flat version is the wrong one.
        let freed = if shared_removed {
            format!("Freed at most {}", format_size(outcome.reclaimed))
        } else {
            format!("Freed {}", format_size(outcome.reclaimed))
        };
        let _ = writeln!(
            out,
            "Removed {} of {attempted}. {freed}.",
            outcome.removed.len()
        );
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
    if !outcome.removed.is_empty() && !purged {
        let removed = outcome.removed.len();
        let (noun, verb) = if removed == 1 {
            ("candidate", "is")
        } else {
            ("candidates", "are")
        };
        let _ = writeln!(
            out,
            "The other {removed} {noun} {verb} in the trash and can be put back."
        );
    }

    out
}

fn write_candidates(candidates: &[Candidate], out: &mut String) {
    // Widths from the data rather than fixed, so the columns stay tight when
    // only one category is present.
    let category_width = candidates
        .iter()
        .map(|c| category(c.category).len())
        .max()
        .unwrap_or(0);
    let tier_width = candidates
        .iter()
        .map(|c| tier(c.tier).len())
        .max()
        .unwrap_or(0);

    for candidate in candidates {
        // Writing to a `String` is infallible.
        let _ = writeln!(
            out,
            "{size:>8}  {category:<cw$}  {tier:<tw$}  {path}{shared}",
            size = format_size(candidate.allocated),
            category = category(candidate.category),
            tier = tier(candidate.tier),
            path = candidate.path.display(),
            shared = if candidate.shared { "  (shared)" } else { "" },
            cw = category_width,
            tw = tier_width,
        );
    }
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
/// were: nothing overrides the denylist, while `--allow-dirty` exists precisely
/// for the other. Rendering them alike would send someone reaching for a flag
/// that cannot help.
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
        ExcludeReason::DirtyRepo => "uncommitted changes; --allow-dirty to include",
    }
}

/// The name the concept and the README use for each category, so the report,
/// the docs and any future config key all say the same word.
fn category(category: Category) -> &'static str {
    match category {
        Category::RustTarget => "rust-target",
        Category::NodeModules => "node-modules",
        Category::Pycache => "pycache",
        Category::UserCaches => "user-caches",
        Category::Old => "old",
    }
}

fn tier(tier: Tier) -> &'static str {
    match tier {
        Tier::Auto => "auto",
        Tier::Confirm => "confirm",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn candidate(path: &str, category: Category, tier: Tier, allocated: u64) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            category,
            tier,
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
        }
    }

    #[test]
    fn dry_run_lists_candidates_with_a_total() {
        let report = render_clean(
            &plan(vec![
                candidate(
                    "/p/node_modules",
                    Category::NodeModules,
                    Tier::Auto,
                    1_048_576,
                ),
                candidate("/p/old.bin", Category::Old, Tier::Confirm, 1_048_576),
            ]),
            None,
            Intent::DryRun,
        );

        // Path, category and tier for each, then the total.
        assert!(report.contains("/p/node_modules"), "{report}");
        assert!(report.contains("node-modules"), "{report}");
        assert!(report.contains("auto"), "{report}");
        assert!(report.contains("/p/old.bin"), "{report}");
        assert!(report.contains("old"), "{report}");
        assert!(report.contains("confirm"), "{report}");
        assert!(report.contains("Reclaimable: 2.0M"), "{report}");
        assert!(
            report.contains("Dry run") && report.contains("--apply"),
            "a dry run must say it removed nothing and how to proceed: {report}"
        );
    }

    /// The soft-total wording. "May free less" is the claim the marker supports;
    /// "will not be freed" would be a different, stronger and wrong one.
    #[test]
    fn shared_candidate_is_flagged_and_total_labelled_upper_bound() {
        let mut shared = candidate("/p/node_modules", Category::NodeModules, Tier::Auto, 2048);
        shared.shared = true;

        let report = render_clean(&plan(vec![shared]), None, Intent::DryRun);

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
        let report = render_clean(
            &plan(vec![candidate(
                "/p/node_modules",
                Category::NodeModules,
                Tier::Auto,
                2048,
            )]),
            None,
            Intent::DryRun,
        );

        assert!(report.contains("Reclaimable: 2.0K"), "{report}");
        assert!(
            !report.contains("at most"),
            "with nothing shared the figure is exact: {report}"
        );
    }

    #[test]
    fn empty_plan_renders_a_plain_message() {
        let report = render_clean(&plan(Vec::new()), None, Intent::DryRun);

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

        let report = render_clean(&empty, None, Intent::DryRun);

        assert!(report.contains("Nothing to clean."), "{report}");
        assert!(report.contains("/Windows/target"), "{report}");
    }

    /// The distinction that decides whether the user reaches for a flag.
    #[test]
    fn a_denylisted_exclusion_does_not_suggest_allow_dirty() {
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

        let report = render_clean(&with_both, None, Intent::DryRun);
        let denied = report
            .lines()
            .find(|line| line.contains("/Windows/target"))
            .expect("the denylisted line is present");
        let dirty = report
            .lines()
            .find(|line| line.contains("/repo/target"))
            .expect("the dirty-repo line is present");

        assert!(
            !denied.contains("--allow-dirty"),
            "nothing overrides the denylist, so the line must not offer a flag: {denied}"
        );
        assert!(
            denied.contains("no flag removes this"),
            "and it should say so: {denied}"
        );
        assert!(
            dirty.contains("--allow-dirty"),
            "the guard is exactly what that flag is for: {dirty}"
        );
    }

    /// `--safe` hides confirm-tier candidates without recording them in the
    /// plan. The report is where the user learns there was something there.
    #[test]
    fn the_safe_filter_reports_how_many_it_hid() {
        let report = render_clean(
            &plan(vec![candidate(
                "/p/node_modules",
                Category::NodeModules,
                Tier::Auto,
                2048,
            )]),
            Some(3),
            Intent::DryRun,
        );

        assert!(
            report.contains("3 more candidates need confirmation"),
            "{report}"
        );
        assert!(report.contains("--safe"), "{report}");
    }

    #[test]
    fn nothing_hidden_says_nothing() {
        let report = render_clean(
            &plan(vec![candidate(
                "/p/nm",
                Category::NodeModules,
                Tier::Auto,
                1,
            )]),
            Some(0),
            Intent::DryRun,
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
        let mut shared = candidate("/p/nm", Category::NodeModules, Tier::Auto, 2048);
        shared.shared = true;
        let report = render_clean(&plan(vec![shared]), Some(1), Intent::DryRun);

        assert!(report.contains("1 candidate shares"), "{report}");
        assert!(report.contains("1 more candidate need"), "{report}");
    }

    #[test]
    fn every_category_and_tier_has_a_label() {
        for c in [
            Category::RustTarget,
            Category::NodeModules,
            Category::Pycache,
            Category::UserCaches,
            Category::Old,
        ] {
            assert!(!category(c).is_empty(), "{c:?}");
        }
        assert_eq!(tier(Tier::Auto), "auto");
        assert_eq!(tier(Tier::Confirm), "confirm");
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::*;
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
            removed: vec![PathBuf::from("/p/node_modules")],
            failed: Vec::new(),
            reclaimed: 2048,
        };

        let report = render_outcome(&outcome, false, false);

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
            removed: Vec::new(),
            failed: vec![failure("/p/one"), failure("/p/two")],
            reclaimed: 0,
        };

        let report = render_outcome(&outcome, false, false);

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
            removed: vec![PathBuf::from("/p/gone")],
            failed: vec![failure("/p/stuck")],
            reclaimed: 1024,
        };

        let report = render_outcome(&outcome, false, false);

        assert!(report.contains("1 candidate still on disk"), "{report}");
        assert!(
            report.contains("The other 1 candidate is in the trash"),
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
            removed: vec![PathBuf::from("/p/node_modules")],
            failed: Vec::new(),
            reclaimed: 4096,
        };

        assert!(
            render_outcome(&outcome, true, false).contains("Freed at most 4.0K"),
            "{}",
            render_outcome(&outcome, true, false)
        );
        assert!(
            render_outcome(&outcome, false, false).contains("Freed 4.0K"),
            "and without sharing the figure is exact"
        );
    }

    #[test]
    fn counts_are_singular_where_they_should_be() {
        let outcome = CleanOutcome {
            removed: Vec::new(),
            failed: vec![failure("/p/one")],
            reclaimed: 0,
        };

        assert!(
            render_outcome(&outcome, false, false).contains("1 candidate still on disk"),
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
                category: Category::NodeModules,
                tier: Tier::Auto,
                allocated: 2048,
                shared: false,
            }],
            reclaimable: 2048,
            excluded: Vec::new(),
            filtered_out: 0,
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
        let report = render_clean(&one_candidate(), None, Intent::AboutToApply);

        assert!(
            report.contains("/p/node_modules"),
            "the preview still lists what is about to go: {report}"
        );
        assert!(
            !report.contains("Dry run"),
            "but must not say a dry run happened, because one is not: {report}"
        );
        assert!(
            !report.contains("nothing was removed"),
            "nor that nothing was removed, moments before removing it: {report}"
        );
    }

    /// And the dry run still says so — the guard must not have silenced both.
    #[test]
    fn a_dry_run_still_says_it_removed_nothing() {
        let report = render_clean(&one_candidate(), None, Intent::DryRun);

        assert!(
            report.contains("Dry run — nothing was removed. Re-run with --apply."),
            "{report}"
        );
    }
}

#[cfg(test)]
mod purge_tests {
    use super::*;
    use disk_tools_core::TrashFailure;
    use std::path::PathBuf;

    fn partial() -> CleanOutcome {
        CleanOutcome {
            removed: vec![PathBuf::from("/p/gone")],
            failed: vec![TrashFailure {
                path: PathBuf::from("/p/stuck"),
                reason: "Permission denied".to_owned(),
            }],
            reclaimed: 1024,
        }
    }

    /// The recoverability line is the whole difference between the two modes,
    /// and after `--purge` it would be a plain lie: there is nothing in the
    /// trash to put back.
    #[test]
    fn a_purged_run_never_claims_anything_can_be_put_back() {
        let report = render_outcome(&partial(), false, true);

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
        let report = render_outcome(&partial(), false, false);

        assert!(
            report.contains("in the trash and can be put back"),
            "{report}"
        );
    }
}
