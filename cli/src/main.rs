//! `disk-tools` — the CLI frontend.
//!
//! Parses arguments into a [`disk_tools_core::ScanOptions`], scans, and prints
//! either a size-sorted tree report or JSON. A spinner and the skipped-entries
//! summary go to stderr, keeping stdout clean for pipes.

mod args;
mod env;
mod render;

use args::{Args, Mode, validate_root};
use clap::Parser;
use disk_tools_core::{
    CleanOptions, CleanPlan, Removal, ScanOptions, ScanTree, Tier, apply, plan, scan,
};
use indicatif::ProgressBar;
use render::clean::{Intent, render_clean, render_outcome};
use render::json::render_json;
use render::skipped::render_skipped;
use render::tree::{RenderOptions, render_tree};
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

fn main() -> ExitCode {
    let args = Args::parse();
    let verbose = args.verbose;

    // The core reads no clock and no environment, so both are resolved here and
    // handed over. Once, at the top, so every rule sees the same "now".
    match args.resolve(SystemTime::now(), env::user_dirs()) {
        Mode::Scan {
            options,
            number,
            json,
        } => run_scan(options, number, json, verbose),
        Mode::Clean {
            scan,
            clean,
            apply,
            removal,
        } => run_clean(scan, *clean, apply, removal, verbose),
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

    emit(&report, &tree, verbose)
}

fn run_clean(
    options: ScanOptions,
    clean: CleanOptions,
    apply: bool,
    removal: Removal,
    verbose: bool,
) -> ExitCode {
    // The git guard runs a `git status` per repository, so a tree of many
    // projects spends real time after the walk finishes.
    let Some(tree) = scan_or_report(&options, "Scanning…") else {
        return ExitCode::from(2);
    };

    if apply {
        return remove(&plan(&tree, &clean), removal, &tree, verbose);
    }

    let planned = plan(&tree, &clean);
    // The count of what `--safe` hid comes back with the plan. It used to come
    // from planning a second time without the flag, which cost a full extra pass
    // of the git guard — measured at ~23 ms per repository, so `--safe` was the
    // slowest mode of the three despite being the cautious one. It also meant
    // subtracting two independently-measured numbers, which could disagree.
    let hidden = clean.safe_only.then_some(planned.filtered_out);

    emit(
        &render_clean(&planned, hidden, Intent::DryRun),
        &tree,
        verbose,
    )
}

/// The one path in this program that deletes anything.
///
/// The plan is printed **before** it is carried out, so the last thing a user
/// sees before the removal is the list of what is about to go — the same report
/// a dry run would have given them.
fn remove(planned: &CleanPlan, removal: Removal, tree: &ScanTree, verbose: bool) -> ExitCode {
    if planned.candidates.is_empty() {
        return emit(&render_clean(planned, None, Intent::DryRun), tree, verbose);
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
        tree,
        verbose,
    );
    if !outcome.is_complete() {
        // A partial removal is not a success, whatever else went right.
        return ExitCode::FAILURE;
    }
    code
}

/// Scan, with a spinner, or report why the root is unusable.
fn scan_or_report(options: &ScanOptions, message: &'static str) -> Option<ScanTree> {
    if let Err(problem) = validate_root(&options.root) {
        eprintln!("disk-tools: {problem}");
        return None;
    }

    // indicatif draws to stderr and hides itself when stderr isn't a tty, so a
    // piped run shows no spinner and stdout stays clean.
    let spinner = ProgressBar::new_spinner();
    spinner.set_message(message);
    spinner.enable_steady_tick(Duration::from_millis(100));
    let tree = scan(options);
    spinner.finish_and_clear();

    Some(tree)
}

/// Report to stdout, skips to stderr.
fn emit(report: &str, tree: &ScanTree, verbose: bool) -> ExitCode {
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
    if let Some(summary) = render_skipped(&tree.skipped, verbose) {
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
