//! `disk-tools` — the CLI frontend.
//!
//! Parses arguments into a [`disk_tools_core::ScanOptions`], scans, and prints
//! either a size-sorted tree report or JSON. A spinner and the skipped-entries
//! summary go to stderr, keeping stdout clean for pipes.

mod args;
mod config;
mod env;
mod explain;
mod render;
mod ui;

use args::{Args, Cleanup, Environment, Intent, Mode, Report, validate_root};
use clap::Parser;
use disk_tools_core::{
    CleanOutcome, CleanPlan, DuplicateOptions, Duplicates, ScanOptions, ScanTree, Searched,
    SkippedEntry, apply, duplicates, plan, plan_duplicates, scan,
};
use indicatif::ProgressBar;
use render::clean::{render_clean, render_outcome};
use render::dup::render_dup;
use render::json::{render_dup_plan, render_json, render_outcome_json, render_plan};
use render::skipped::render_skipped;
use render::tree::{RenderOptions, render_tree};
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

fn main() -> ExitCode {
    let args = Args::parse();
    let verbose = args.verbose;
    let explaining = args.explain;

    // The core reads no clock, no environment and no config file, so all three
    // are resolved here and handed over. The clock once, at the top, so every
    // rule in one run sees the same "now".
    let user_dirs = env::user_dirs();
    let xdg = env::xdg_config_home();
    let config_path = config::locate(args.config.as_deref(), &user_dirs, xdg.clone());
    // Kept for `--explain`, which names the file that was actually **read** —
    // located and absent is an ordinary state, and saying "none found" is the
    // answer a user chasing a rule that does nothing needs.
    let named = config_path.clone().filter(|path| path.exists());

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
            match err.about() {
                Some(subject) => eprintln!("disk-tools: {subject}: {err}"),
                None => eprintln!("disk-tools: {err}"),
            }
            return ExitCode::from(2);
        }
    };

    // Before anything is walked, read or removed. Explaining and then acting
    // would make this a log line rather than a check.
    if explaining {
        let said = explain::explain(&mode, named.as_deref());
        return match write_report(&said) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }

    match mode {
        Mode::Scan {
            options,
            number,
            json,
        } => run_scan(options, number, json, verbose),
        Mode::Clean(cleanup) => run_clean(*cleanup, verbose),
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

fn run_clean(cleanup: Cleanup, verbose: bool) -> ExitCode {
    // Borrowed apart for readability, not to change anything: `cleanup` itself
    // travels on to `remove`, which needs the two fields not named here.
    let Cleanup {
        roots,
        roots_from_rules,
        options,
        intent,
        report,
        ..
    } = &cleanup;
    let (roots_from_rules, intent, report) = (*roots_from_rules, *intent, *report);
    // Which report is printed is settled by the mode, not by the plan: an empty
    // duplicate run still has to print the duplicate report, which is the one
    // that says why it might be empty. What it needs beyond the plan is filled
    // in as the roots are searched.
    let mut keeping = cleanup.duplicates.as_ref().map(|_| Keeping {
        now: cleanup.options.detect.now,
        pools: Vec::new(),
    });

    if roots.is_empty() {
        // Not an error and not an empty plan. "Nothing to clean" would be a
        // claim about the disk; this is a statement about the configuration, and
        // the two remedies are different things to go and do.
        //
        // Which list is named matters: under `--dup` the clean rules' roots are
        // not what was consulted, and sending someone to edit them would send
        // them to the wrong half of their file.
        let (list, verb) = match cleanup.duplicates {
            Some(_) => ("duplicate rule", "search"),
            None => ("rule", "clean"),
        };
        eprintln!(
            "disk-tools: no {list} names a directory to {verb}.\n\
             Pass a path, or give one of its parts a `root` other than \"*\"."
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
    // Kept **only** when the report will unfold inside a candidate. A tree costs
    // ~630 bytes per entry — 1.4 GB for a home directory — and holding every
    // root's through the removal to satisfy a display flag nobody passed would
    // be exactly the cost the lazy browser was built to avoid. Below `-d 2` the
    // trees are dropped as they are planned, as before.
    let unfolding = report.depth >= 2;
    let mut trees = Vec::new();
    for root in roots {
        // Named apart from the cleanup's own `options`, which is the
        // `CleanOptions` this walk is about to be planned against.
        let walk = ScanOptions {
            root: root.clone(),
            ..ScanOptions::default()
        };
        // No `validate_root` here. A path the user *named* is checked before
        // this function is reached; a root that came from a rule is a
        // description that may have gone stale, and one missing directory is no
        // reason to leave the others uncleaned. `scan` reports it as a skip.
        let mut tree = scan_with_spinner(&walk, "Scanning…");
        plans.push(match &cleanup.duplicates {
            // The two sources of a candidate, and the one place either is
            // chosen. Both produce the same plan, so everything after this line
            // — the refusal, the report, the removal — is unaware of which ran.
            Some(duplicating) => {
                let found = search_with_spinner(&tree, duplicating, options);
                skipped.extend(found.skipped.iter().cloned());
                if let Some(keeping) = &mut keeping {
                    merge_pools(&mut keeping.pools, found.pools.clone());
                }
                plan_duplicates(&tree, &found, options)
            }
            None => plan(&tree, options),
        });
        skipped.extend(std::mem::take(&mut tree.skipped));
        if unfolding {
            trees.push(tree);
        }
    }

    let planned = CleanPlan::merge(plans);

    if intent == Intent::Removing {
        return remove(&planned, &cleanup, &trees, &skipped, keeping, verbose);
    }

    // The count of what `--safe` hid comes back with the plan. It used to come
    // from planning a second time without the flag, which cost a full extra pass
    // of the git guard — measured at ~23 ms per repository, so `--safe` was the
    // slowest mode of the three despite being the cautious one. It also meant
    // subtracting two independently-measured numbers, which could disagree.
    let hidden = options.safe_only.then_some(planned.filtered_out);

    match text(&planned, hidden, Intent::Preview, report, &trees, keeping) {
        Ok(shown) => emit(&shown, &skipped, verbose),
        Err(err) => json_failed(err),
    }
}

/// What a duplicate report needs beyond the plan.
///
/// `Some` exactly when `--dup` was passed, so **which report is printed is
/// settled by the mode** rather than inferred from the plan's contents. An empty
/// duplicate run has to say "no duplicates found" and name why, which a plan
/// with no candidates in it cannot distinguish from any other empty plan.
#[derive(Clone)]
struct Keeping {
    /// The clock the keeper's date is shown against, taken once at the top of
    /// the run like every other "now" here.
    ///
    /// The keeper *rule* is not here: each pool has its own, so it travels on
    /// the candidate instead.
    now: SystemTime,

    /// Every pool that was searched, and how much fell into it.
    ///
    /// Not in the plan, and rightly: the plan says what will happen, and this
    /// says what was **looked at**. It is the only thing that can tell an empty
    /// report caused by a disk with no duplicates from one caused by a
    /// configuration that searched nowhere.
    pools: Vec<Searched>,
}

/// The plan as text or as JSON, in whichever of the two shapes it is about.
fn text(
    planned: &CleanPlan,
    hidden_by_safe: Option<usize>,
    intent: Intent,
    report: Report,
    inside: &[ScanTree],
    keeping: Option<Keeping>,
) -> serde_json::Result<String> {
    if report.json {
        return match &keeping {
            Some(keeping) => render_dup_plan(planned, &keeping.pools).map(|payload| payload + "\n"),
            None => render_plan(planned).map(|payload| payload + "\n"),
        };
    }
    Ok(human(
        planned,
        hidden_by_safe,
        intent,
        report,
        inside,
        keeping,
    ))
}

/// The report as a person reads it, whatever `--json` said.
///
/// Split out because one caller needs exactly that: the plan printed to stderr
/// immediately before a removal is context for the operation, not the document,
/// and emitting JSON there would put a second value on a stream that already
/// carries one on stdout.
fn human(
    planned: &CleanPlan,
    hidden_by_safe: Option<usize>,
    intent: Intent,
    report: Report,
    inside: &[ScanTree],
    keeping: Option<Keeping>,
) -> String {
    match keeping {
        Some(Keeping { now, pools }) => {
            render_dup(planned, hidden_by_safe, intent, report, now, &pools)
        }
        None => render_clean(planned, hidden_by_safe, intent, report, inside),
    }
}

/// The plan, in whichever shape was asked for.
fn render(
    planned: &CleanPlan,
    report: Report,
    intent: Intent,
    inside: &[ScanTree],
    keeping: Option<Keeping>,
) -> serde_json::Result<String> {
    text(planned, None, intent, report, inside, keeping)
}

/// A path that is not UTF-8 cannot be a JSON string.
///
/// An error and a non-zero exit rather than a document with a path silently
/// missing from it — this output exists to be acted on by something that cannot
/// notice the gap.
fn json_failed(err: serde_json::Error) -> ExitCode {
    eprintln!("disk-tools: cannot encode JSON: {err}");
    ExitCode::FAILURE
}

/// The one path in this program that deletes anything.
///
/// The plan is printed **before** it is carried out, so the last thing a user
/// sees before the removal is the list of what is about to go — the same report
/// a dry run would have given them.
fn remove(
    planned: &CleanPlan,
    cleanup: &Cleanup,
    inside: &[ScanTree],
    skipped: &[SkippedEntry],
    keeping: Option<Keeping>,
    verbose: bool,
) -> ExitCode {
    let report = cleanup.report;
    if planned.candidates.is_empty() {
        // Nothing to remove, so this is the same "Nothing to clean." a preview
        // prints — and it goes through `render` so that `--json` gets a
        // document rather than a sentence.
        return match render(planned, report, Intent::Preview, inside, keeping) {
            Ok(shown) => emit(&shown, skipped, verbose),
            Err(err) => json_failed(err),
        };
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
    if confirm > 0 && !cleanup.confirm_tier_allowed {
        // Nothing happened, so what there is to report is the plan — the same
        // document `preview` would have produced. A consumer tells the two
        // apart by the exit code, which is the thing it has to read anyway.
        let code = match render(planned, report, Intent::Preview, inside, keeping) {
            Ok(shown) => emit(&shown, skipped, verbose),
            Err(err) => return json_failed(err),
        };
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

    // To stderr: this is context for the operation, not the report, and it stays
    // text even under `--json` — stdout is reserved for the one document, and a
    // second JSON value on the way to it would make the stream unparseable.
    eprint!(
        "{}",
        human(planned, None, Intent::Removing, report, inside, keeping)
    );
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

    let shown = if report.json {
        match render_outcome_json(&outcome) {
            Ok(payload) => payload + "\n",
            Err(err) => return json_failed(err),
        }
    } else {
        render_outcome(&outcome, shared_was_removed(planned, &outcome))
    };
    let code = emit(&shown, skipped, verbose);
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

/// Add one root's pool counts to the run's.
///
/// `clean` with no path walks a root per rule, so one pool can be filled from
/// several of them — and a report saying "everywhere: 40 files" twice would be
/// describing the walk rather than the search.
fn merge_pools(into: &mut Vec<Searched>, found: Vec<Searched>) {
    for pool in found {
        match into.iter_mut().find(|kept| kept.rule == pool.rule) {
            Some(kept) => kept.files += pool.files,
            None => into.push(pool),
        }
    }
    into.sort_by(|a, b| a.rule.cmp(&b.rule));
}

/// Hash what needs hashing, saying how much of it there is.
///
/// The only phase in this tool bounded by disk throughput rather than metadata
/// calls, so its spinner carries a running total rather than one word: a scan
/// takes a second and this can take minutes on a large tree.
///
/// The message is rebuilt per file, which is far more often than the 100 ms
/// tick redraws — cheap next to the read that produced it.
fn search_with_spinner(
    tree: &ScanTree,
    duplicating: &args::Duplicating,
    options: &disk_tools_core::CleanOptions,
) -> Duplicates {
    let spinner = ProgressBar::new_spinner();
    spinner.set_message("Comparing…");
    spinner.enable_steady_tick(Duration::from_millis(100));

    let found = duplicates(
        tree,
        &DuplicateOptions {
            // The rules prune and claim nothing: this is the same detection the
            // other source would have run, put to the opposite use.
            detect: options.detect.clone(),
            rules: duplicating.rules.clone(),
            min_size: duplicating.min_size,
            keep: duplicating.keep,
            keep_in: duplicating.keep_in.clone(),
        },
        &|hashed| {
            spinner.set_message(format!(
                "Comparing… {} read",
                render::tree::format_size(hashed.running_total)
            ));
        },
    );

    spinner.finish_and_clear();
    found
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
    use std::path::PathBuf;

    fn candidate(path: &str, shared: bool) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            rule: "node-modules".into(),
            tier: Tier::Trash,
            purge: false,
            duplicate_of: None,
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
