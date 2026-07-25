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
use disk_tools_core::{Candidate, Category, CleanPlan, ExcludeReason, Excluded, Tier};
use std::fmt::Write;

/// Render the plan for a human.
///
/// `hidden_by_safe` is how many candidates `--safe` kept out, or `None` when the
/// flag was not used. The core does not record them — that is deliberate, since
/// `--safe` is the user's own narrowing rather than a refusal — so the count is
/// passed in by the caller, which plans a second time to obtain it.
pub fn render_clean(plan: &CleanPlan, hidden_by_safe: Option<usize>) -> String {
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

    if !plan.candidates.is_empty() {
        let _ = writeln!(out, "\nDry run — nothing was removed. Re-run with --apply.");
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

        let report = render_clean(&plan(vec![shared]), None);

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
        );

        assert!(report.contains("Reclaimable: 2.0K"), "{report}");
        assert!(
            !report.contains("at most"),
            "with nothing shared the figure is exact: {report}"
        );
    }

    #[test]
    fn empty_plan_renders_a_plain_message() {
        let report = render_clean(&plan(Vec::new()), None);

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

        let report = render_clean(&empty, None);

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

        let report = render_clean(&with_both, None);
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
        let report = render_clean(&plan(vec![shared]), Some(1));

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
