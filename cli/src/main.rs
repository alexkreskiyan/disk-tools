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
use disk_tools_core::{CleanOptions, ScanOptions, ScanTree, plan, scan};
use indicatif::ProgressBar;
use render::clean::render_clean;
use render::json::render_json;
use render::skipped::render_skipped;
use render::tree::{RenderOptions, render_tree};
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

fn main() -> ExitCode {
    let args = Args::parse();
    // The display-only knobs are the CLI's own; copy them before `resolve`
    // consumes the parsed args into what the core needs.
    let json = args.json;
    let verbose = args.verbose;
    let render_options = RenderOptions {
        number: args.number,
        depth: args.depth,
        min_size: args.min_size,
        apparent: args.apparent,
        width: terminal_width(),
    };

    // The core reads no clock and no environment, so both are resolved here and
    // handed over. Once, at the top, so every rule sees the same "now".
    match args.resolve(SystemTime::now(), env::user_dirs()) {
        Ok(Mode::Scan(options)) => run_scan(options, json, verbose, &render_options),
        Ok(Mode::Clean { scan, clean, apply }) => run_clean(scan, clean, apply, verbose),
        // clap's own formatting and exit code — a usage error looks the same
        // whether clap raised it or we did.
        Err(err) => err.exit(),
    }
}

fn run_scan(
    options: ScanOptions,
    json: bool,
    verbose: bool,
    render_options: &RenderOptions,
) -> ExitCode {
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
        render_tree(&tree, render_options)
    };

    emit(&report, &tree, verbose)
}

fn run_clean(options: ScanOptions, clean: CleanOptions, apply: bool, verbose: bool) -> ExitCode {
    if apply {
        // Task 7 implements removal. Until then the flag must not look like it
        // worked: silently accepting an argument whose entire purpose is
        // deletion, and then doing nothing, is the one behaviour that cannot
        // ship — a user would believe their disk had been cleaned.
        eprintln!("disk-tools: --apply is not implemented yet; nothing was removed");
        return ExitCode::FAILURE;
    }

    // The git guard runs a `git status` per repository, so a tree of many
    // projects spends real time after the walk finishes.
    let Some(tree) = scan_or_report(&options, "Scanning…") else {
        return ExitCode::from(2);
    };

    let report = {
        let planned = plan(&tree, &clean);
        // `--safe` drops confirm-tier candidates without recording them, which
        // is deliberate — it is the user's own narrowing, not a refusal. But
        // they still deserve to know something was there, so the count comes
        // from planning once more without it.
        let hidden = clean.safe_only.then(|| {
            let unfiltered = plan(
                &tree,
                &CleanOptions {
                    safe_only: false,
                    ..clean.clone()
                },
            );
            // `saturating_sub`, not `-`. Each `plan` re-runs the git guard
            // against the world as it is *then*, so a repository whose state
            // changes between the two calls can leave the unfiltered plan the
            // shorter of the two. On `usize` that subtraction wraps in a release
            // build, and the report would announce eighteen quintillion hidden
            // candidates — exactly the kind of nonsense this report exists to
            // not print. A count that is low by one in a rare race is the
            // cheaper wrong answer.
            unfiltered
                .candidates
                .len()
                .saturating_sub(planned.candidates.len())
        });
        render_clean(&planned, hidden)
    };

    emit(&report, &tree, verbose)
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
