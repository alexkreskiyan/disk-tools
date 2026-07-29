//! `disk-tools` — the CLI frontend.
//!
//! Parses arguments into a [`disk_tools_core::ScanOptions`], scans, and prints
//! either a size-sorted tree report or JSON. A spinner and the skipped-entries
//! summary go to stderr, keeping stdout clean for pipes.

mod args;
mod config;
mod env;
mod render;
mod ui;

use args::{Args, Environment, Intent, Mode, Report, validate_root};
use clap::Parser;
use disk_tools_core::{
    CleanOptions, CleanOutcome, CleanPlan, ScanOptions, ScanTree, SkippedEntry, apply, plan, scan,
};
use indicatif::ProgressBar;
use render::clean::{render_clean, render_outcome};
use render::json::render_json;
use render::skipped::render_skipped;
use render::tree::{RenderOptions, render_tree};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

fn main() -> ExitCode {
    let args = Args::parse();
    let verbose = args.verbose;

    // The core reads no clock, no environment and no config file, so all three
    // are resolved here and handed over. The clock once, at the top, so every
    // rule in one run sees the same "now".
    let user_dirs = env::user_dirs();
    let xdg = env::xdg_config_home();
    let config_path = config::locate(args.config.as_deref(), &user_dirs, xdg.clone());

    // Before anything is scanned: a config that cannot be understood means the
    // rules are unknown, and the rules decide what may be deleted.
    //
    // Skipped for the `config` verb, which writes the file rather than obeying
    // it. Reading first would make `--config <new path> config init` fail on the
    // absence of exactly the file it was asked to create.
    let config = if matches!(args.command, args::Command::Config { .. }) {
        config::Config::default()
    } else {
        match config::load(args.config.as_deref(), &user_dirs, xdg) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("disk-tools: config: {err}");
                return ExitCode::from(2);
            }
        }
    };
    for warning in &config.warnings {
        eprintln!("disk-tools: config: unknown key `{warning}` (ignored)");
    }

    let mode = match args.resolve(Environment {
        now: SystemTime::now(),
        user_dirs,
        config,
        config_path,
    }) {
        Ok(mode) => mode,
        Err(err) => {
            eprintln!("disk-tools: config: {err}");
            return ExitCode::from(2);
        }
    };

    match mode {
        Mode::Scan {
            options,
            number,
            json,
        } => run_scan(options, number, json, verbose),
        Mode::Clean {
            roots,
            confirm_tier_allowed,
            roots_from_rules,
            clean,
            intent,
            report,
        } => run_clean(
            &roots,
            roots_from_rules,
            *clean,
            intent,
            confirm_tier_allowed,
            report,
            verbose,
        ),
        Mode::ConfigInit { target, force } => run_config_init(&target, force),
        Mode::Ui {
            root,
            rules,
            reload,
            now,
        } => run_ui(&root, *rules, *reload, now),
    }
}

/// Write the default configuration where this platform keeps one.
///
/// The path goes to **stdout**: it is the result of the command, and the obvious
/// next thing a user does with it is open it.
fn run_config_init(target: &std::path::Path, force: bool) -> ExitCode {
    match config::init(target, force) {
        Ok(()) => {
            println!("{}", target.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("disk-tools: config: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Open the browser, having first refused every reason not to.
///
/// Both checks happen **before** the alternate screen: a message printed inside
/// it is erased the moment the screen is left, so the user would see a program
/// that flickered and exited saying nothing.
fn run_ui(
    root: &std::path::Path,
    rules: disk_tools_core::Rules,
    reload: args::Reload,
    now: std::time::SystemTime,
) -> ExitCode {
    if let Err(refusal) = ui::check(root, ui::stdout_is_terminal()) {
        eprintln!("disk-tools: {refusal}");
        return ExitCode::from(2);
    }
    match ui::run(root, rules, reload, now) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("disk-tools: ui: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_scan(options: ScanOptions, number: Option<usize>, json: bool, verbose: bool) -> ExitCode {
    // Display-only knobs, built here because terminal width is the frontend's
    // business and the core neither has it nor wants it.
    let render_options = RenderOptions {
        number,
        depth: options.depth,
        min_size: options.min_size,
        apparent: options.apparent,
        width: terminal_width(),
    };

    let Some(tree) = scan_or_report(&options, "Scanning…") else {
        return ExitCode::from(2);
    };

    let report = if json {
        match render_json(&tree) {
            Ok(payload) => payload + "\n",
            Err(err) => {
                eprintln!("disk-tools: cannot encode JSON: {err}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        render_tree(&tree, &render_options)
    };

    emit(&report, &tree.skipped, verbose)
}

fn run_clean(
    roots: &[PathBuf],
    roots_from_rules: bool,
    clean: CleanOptions,
    intent: Intent,
    confirm_tier_allowed: bool,
    report: Report,
    verbose: bool,
) -> ExitCode {
    if roots.is_empty() {
        // Not an error and not an empty plan. "Nothing to clean" would be a
        // claim about the disk; this is a statement about the configuration, and
        // the two remedies are different things to go and do.
        eprintln!(
            "disk-tools: no rule names a directory to clean.\n\
             Pass a path, or give a rule a `root` other than \"*\"."
        );
        return ExitCode::SUCCESS;
    }

    if roots_from_rules {
        // Announced only when the rules chose them. The default config roots
        // `user-caches` at the home directory, so a bare `disk-tools clean`
        // walks all of it, and no one should have to guess why it is taking a
        // minute — least of all now that the walk ends in a removal.
        let listed: Vec<String> = roots
            .iter()
            .map(|root| root.display().to_string())
            .collect();
        eprintln!("disk-tools: examining {}", listed.join(", "));
    } else {
        // A path the user **named** and that is not there is a typo, and an
        // empty report would hide it. A root that came from a rule is only a
        // description, which may have gone stale — that one is a skip, since a
        // single missing directory is no reason to leave the others uncleaned.
        for root in roots {
            if let Err(problem) = validate_root(root) {
                eprintln!("disk-tools: {problem}");
                return ExitCode::from(2);
            }
        }
    }

    // The git guard runs a `git status` per repository, so a tree of many
    // projects spends real time after the walk finishes.
    let mut plans = Vec::with_capacity(roots.len());
    let mut skipped = Vec::new();
    for root in roots {
        let options = ScanOptions {
            root: root.clone(),
            ..ScanOptions::default()
        };
        // No `validate_root` here. A path the user *named* is checked before
        // this function is reached; a root that came from a rule is a
        // description that may have gone stale, and one missing directory is no
        // reason to leave the others uncleaned. `scan` reports it as a skip.
        let tree = scan_with_spinner(&options, "Scanning…");
        plans.push(plan(&tree, &clean));
        skipped.extend(tree.skipped);
    }

    let planned = CleanPlan::merge(plans);

    if intent == Intent::Removing {
        return remove(&planned, confirm_tier_allowed, report, &skipped, verbose);
    }

    // The count of what `--safe` hid comes back with the plan. It used to come
    // from planning a second time without the flag, which cost a full extra pass
    // of the git guard — measured at ~23 ms per repository, so `--safe` was the
    // slowest mode of the three despite being the cautious one. It also meant
    // subtracting two independently-measured numbers, which could disagree.
    let hidden = clean.safe_only.then_some(planned.filtered_out);

    emit(
        &render_clean(&planned, hidden, Intent::Preview, report),
        &skipped,
        verbose,
    )
}

/// The one path in this program that deletes anything.
///
/// The plan is printed **before** it is carried out, so the last thing a user
/// sees before the removal is the list of what is about to go — the same report
/// a dry run would have given them.
fn remove(
    planned: &CleanPlan,
    confirm_tier_allowed: bool,
    report: Report,
    skipped: &[SkippedEntry],
    verbose: bool,
) -> ExitCode {
    if planned.candidates.is_empty() {
        return emit(
            &render_clean(planned, None, Intent::Preview, report),
            skipped,
            verbose,
        );
    }

    let confirm = planned
        .candidates
        .iter()
        .filter(|c| c.tier.needs_confirming())
        .count();

    // Decided **before** anything is printed. The plan below goes out with an
    // intent, and `AboutToApply`'s closing line promises a removal — printing
    // that and then declining would make the last thing a user reads before the
    // outcome the one sentence in the report that is false. That is the defect
    // `Intent` was introduced for.
    //
    // `--safe` needs no case of its own: it keeps confirm-tier candidates out of
    // the plan, so the count is zero and there is nothing to refuse.
    if confirm > 0 && !confirm_tier_allowed {
        let code = emit(
            &render_clean(planned, None, Intent::Preview, report),
            skipped,
            verbose,
        );
        let (noun, verb) = if confirm == 1 {
            ("candidate", "is")
        } else {
            ("candidates", "are")
        };
        eprintln!(
            "\n{confirm} {noun} {verb} not regenerable, and nothing was removed.\n\
             Add --safe to take only the regenerable ones, or --yes to take all of them."
        );
        // The flags were well formed; the invitation was incomplete. That is a
        // usage answer, not an operation that failed.
        return if code == ExitCode::SUCCESS {
            ExitCode::from(2)
        } else {
            code
        };
    }

    // To stderr: this is context for the operation, not the report. It also
    // keeps stdout to the outcome alone for anything reading it.
    eprint!("{}", render_clean(planned, None, Intent::Removing, report));
    if confirm > 0 {
        // Reached only with `--yes`, or with `require-confirmation` turned off.
        // Saying the number out loud is what keeps either from being a blind yes.
        // The verb agrees with the count, as everywhere else in this report:
        // "1 of these are" is the kind of slip that makes a reader doubt the
        // number beside it, and this one is counting deletions.
        let verb = if confirm == 1 { "is" } else { "are" };
        eprintln!("{confirm} of these {verb} not regenerable — removing anyway, as asked.");
    }

    // The last word before something becomes unrecoverable, and counted from
    // the plan rather than from a flag: after v0.5 a rule can carry `purge` on
    // its own, so a run that destroys things need never have been asked to.
    let destroying = planned.candidates.iter().filter(|c| c.purge).count();
    if destroying > 0 {
        let (noun, verb) = if destroying == 1 {
            ("candidate", "is")
        } else {
            ("candidates", "are")
        };
        eprintln!(
            "{destroying} {noun} {verb} being deleted outright — NOT to the trash, \
             and cannot be put back."
        );
    }

    // A spinner, not a bar. Trashing is **one** batched call: a bar would fill
    // instantly as the candidates were submitted and then sit still for the
    // whole operation, which is worse than not drawing one. `--purge` is
    // per-item and could show a bar, but two different progress shapes for one
    // command would be its own confusion.
    let spinner = ProgressBar::new_spinner();
    spinner.set_message(format!("Removing {} items…", planned.candidates.len()));
    spinner.enable_steady_tick(Duration::from_millis(100));
    let outcome = apply(planned, |_| {});
    spinner.finish_and_clear();

    let code = emit(
        &render_outcome(&outcome, shared_was_removed(planned, &outcome)),
        skipped,
        verbose,
    );
    if !outcome.is_complete() {
        // A partial removal is not a success, whatever else went right.
        return ExitCode::FAILURE;
    }
    code
}

/// Scan, with a spinner, or report why the root is unusable.
///
/// The check belongs to a path the **user named**: one that does not exist is a
/// typo, and an empty report would hide it. A root that came from a rule gets
/// [`scan_with_spinner`] instead, and a missing one there is a skip.
fn scan_or_report(options: &ScanOptions, message: &'static str) -> Option<ScanTree> {
    if let Err(problem) = validate_root(&options.root) {
        eprintln!("disk-tools: {problem}");
        return None;
    }

    Some(scan_with_spinner(options, message))
}

/// Scan, with a spinner, whatever the root turns out to be.
fn scan_with_spinner(options: &ScanOptions, message: &'static str) -> ScanTree {
    // indicatif draws to stderr and hides itself when stderr isn't a tty, so a
    // piped run shows no spinner and stdout stays clean.
    let spinner = ProgressBar::new_spinner();
    spinner.set_message(message);
    spinner.enable_steady_tick(Duration::from_millis(100));
    let tree = scan(options);
    spinner.finish_and_clear();

    tree
}

/// Report to stdout, skips to stderr.
///
/// Takes the skips rather than the tree they came from: `clean` with no path
/// walks a root per rule and has several trees, one report, and one combined
/// list of what it could not read.
fn emit(report: &str, skipped: &[SkippedEntry], verbose: bool) -> ExitCode {
    if let Err(err) = write_report(report) {
        // `disk-tools <path> | head` closes the pipe after a few lines. For a
        // Unix filter that is a normal end of output, not a failure — stop
        // quietly rather than reporting an error nobody can act on.
        if err.kind() == io::ErrorKind::BrokenPipe {
            return ExitCode::SUCCESS;
        }
        eprintln!("disk-tools: cannot write report: {err}");
        return ExitCode::FAILURE;
    }

    // Always to stderr — visible on a terminal, out of a stdout pipe (so `--json`
    // stdout stays valid JSON). Errors are dropped: stderr closing (`2>&1 | head`)
    // must not turn a successful scan into a failure, and there is nowhere left
    // to report it anyway.
    if let Some(summary) = render_skipped(skipped, verbose) {
        let _ = write!(io::stderr(), "{summary}");
    }
    ExitCode::SUCCESS
}

/// Does the freed figure need the same hedge the dry run's total carried?
///
/// A candidate that shares content with something outside it did not free
/// everything its size claimed — but only if it actually went. Both halves
/// matter and neither is testable through the binary: the sentence they control
/// appears only after a **partial** failure, and a partial trash failure cannot
/// be provoked on macOS (v0.2 measured that). Pulled out here so the decision can
/// be driven directly, the way `already_gone` was for the same reason.
fn shared_was_removed(planned: &CleanPlan, outcome: &CleanOutcome) -> bool {
    planned
        .candidates
        .iter()
        .any(|candidate| candidate.shared && outcome.removed().any(|gone| *gone == candidate.path))
}

/// Write the finished report to stdout in one buffered pass.
///
/// Deliberately not `print!`: those macros **panic** when the reader goes away,
/// which is exactly what `| head` does, and they write through a `LineWriter`
/// that flushes once per newline — one syscall per tree line, 105,000 of them on
/// a large scan. A `BufWriter` hands the error back instead and writes in 8 KiB
/// chunks. The explicit `flush` matters: `BufWriter`'s `Drop` swallows errors.
fn write_report(report: &str) -> io::Result<()> {
    let mut out = BufWriter::new(io::stdout().lock());
    out.write_all(report.as_bytes())?;
    out.flush()
}

/// Terminal width, or a fixed fallback when stdout is not a tty (piped output).
fn terminal_width() -> usize {
    terminal_size::terminal_size()
        .map(|(width, _)| width.0 as usize)
        .unwrap_or(80)
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_tools_core::{Candidate, Reclaimed, Tier, TrashFailure};

    fn candidate(path: &str, shared: bool) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            rule: "node-modules".into(),
            tier: Tier::Trash,
            purge: false,
            allocated: 4096,
            shared,
        }
    }

    fn plan_of(candidates: Vec<Candidate>) -> CleanPlan {
        CleanPlan {
            reclaimable: candidates.iter().map(|c| c.allocated).sum(),
            candidates,
            ..CleanPlan::default()
        }
    }

    fn outcome(removed: &[&str], failed: &[&str]) -> CleanOutcome {
        CleanOutcome {
            trashed: Reclaimed {
                paths: removed.iter().map(PathBuf::from).collect(),
                bytes: 0,
            },
            purged: Reclaimed::default(),
            failed: failed
                .iter()
                .map(|path| TrashFailure {
                    path: PathBuf::from(path),
                    reason: "denied".into(),
                })
                .collect(),
        }
    }

    /// Both halves of the predicate, driven apart.
    ///
    /// Mutation testing found neither was covered: the sentence they control
    /// appears only after a partial failure, and a partial *trash* failure cannot
    /// be provoked on macOS. Replacing the `&&` with `||` left every test passing.
    #[test]
    fn the_hedge_needs_a_shared_candidate_that_actually_went() {
        let shared = plan_of(vec![candidate("/p/a", true)]);
        let plain = plan_of(vec![candidate("/p/a", false)]);

        assert!(
            shared_was_removed(&shared, &outcome(&["/p/a"], &[])),
            "shared and removed: the freed figure is an upper bound"
        );
        assert!(
            !shared_was_removed(&shared, &outcome(&[], &["/p/a"])),
            "shared but it stayed — nothing it shares was freed either way"
        );
        assert!(
            !shared_was_removed(&plain, &outcome(&["/p/a"], &[])),
            "removed but unshared: the figure is exact, and hedging it would be \
             the report doubting a number it knows"
        );
    }

    /// One shared candidate among many is enough — the total covers them all.
    #[test]
    fn one_shared_removal_among_several_is_enough() {
        let plan = plan_of(vec![
            candidate("/p/a", false),
            candidate("/p/b", true),
            candidate("/p/c", false),
        ]);

        assert!(shared_was_removed(
            &plan,
            &outcome(&["/p/a", "/p/b", "/p/c"], &[])
        ));
        assert!(
            !shared_was_removed(&plan, &outcome(&["/p/a", "/p/c"], &["/p/b"])),
            "the only shared one failed, so what did go was measured exactly"
        );
    }

    #[test]
    fn nothing_removed_needs_no_hedge() {
        let plan = plan_of(vec![candidate("/p/a", true)]);

        assert!(!shared_was_removed(&plan, &outcome(&[], &[])));
        assert!(!shared_was_removed(
            &CleanPlan::default(),
            &outcome(&[], &[])
        ));
    }
}
