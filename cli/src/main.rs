//! `disk-tools` — the CLI frontend.
//!
//! Parses arguments into a [`disk_tools_core::ScanOptions`], scans, and prints
//! either a size-sorted tree report or JSON. A spinner and the skipped-entries
//! summary go to stderr, keeping stdout clean for pipes.

mod args;
mod config;
mod env;
mod render;

use args::{Args, Environment, Mode, validate_root};
use clap::Parser;
use disk_tools_core::{
    CleanOptions, CleanPlan, Removal, ScanOptions, ScanTree, SkippedEntry, Tier, apply, plan, scan,
};
use indicatif::ProgressBar;
use render::clean::{Intent, render_clean, render_outcome};
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
            roots_from_rules,
            clean,
            apply,
            removal,
        } => run_clean(&roots, roots_from_rules, *clean, apply, removal, verbose),
        Mode::ConfigInit { target, force } => run_config_init(&target, force),
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
    apply: bool,
    removal: Removal,
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
        // walks all of it — slow rather than dangerous, the default still being
        // a dry run, but no one should have to guess why it is taking a minute.
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

    if apply {
        return remove(&planned, removal, &skipped, verbose);
    }

    // The count of what `--safe` hid comes back with the plan. It used to come
    // from planning a second time without the flag, which cost a full extra pass
    // of the git guard — measured at ~23 ms per repository, so `--safe` was the
    // slowest mode of the three despite being the cautious one. It also meant
    // subtracting two independently-measured numbers, which could disagree.
    let hidden = clean.safe_only.then_some(planned.filtered_out);

    emit(
        &render_clean(&planned, hidden, Intent::DryRun),
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
    removal: Removal,
    skipped: &[SkippedEntry],
    verbose: bool,
) -> ExitCode {
    if planned.candidates.is_empty() {
        return emit(
            &render_clean(planned, None, Intent::DryRun),
            skipped,
            verbose,
        );
    }

    // To stderr: this is context for the operation, not the report. It also
    // keeps stdout to the outcome alone for anything reading it.
    eprint!("{}", render_clean(planned, None, Intent::AboutToApply));
    let confirm = planned
        .candidates
        .iter()
        .filter(|c| c.tier == Tier::Confirm)
        .count();
    if confirm > 0 {
        // The concept asks for per-target confirmation on this tier; v0.2's
        // confirmation is having read the list above and typed `--apply`. Saying
        // the number out loud is what keeps that from being a blind yes.
        eprintln!("{confirm} of these are not regenerable — removing anyway, as asked.");
    }

    if removal == Removal::Purge {
        // The last word before something becomes unrecoverable. `--purge` is a
        // deliberate reversal of this tool's central promise, so it is said
        // plainly rather than assumed understood.
        eprintln!("Deleting outright — these will NOT go to the trash and cannot be put back.");
    }

    // A spinner, not a bar. Trashing is **one** batched call: a bar would fill
    // instantly as the candidates were submitted and then sit still for the
    // whole operation, which is worse than not drawing one. `--purge` is
    // per-item and could show a bar, but two different progress shapes for one
    // command would be its own confusion.
    let spinner = ProgressBar::new_spinner();
    spinner.set_message(format!("Removing {} items…", planned.candidates.len()));
    spinner.enable_steady_tick(Duration::from_millis(100));
    let outcome = apply(planned, removal, |_| {});
    spinner.finish_and_clear();

    // Whether the freed figure needs the same hedge the dry run's total carried:
    // a removed candidate that shares content with something outside it did not
    // free everything its size claimed.
    let shared_removed = planned
        .candidates
        .iter()
        .any(|c| c.shared && outcome.removed.contains(&c.path));

    let code = emit(
        &render_outcome(&outcome, shared_removed, removal == Removal::Purge),
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
