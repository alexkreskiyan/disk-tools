//! `disk-tools` — the CLI frontend.
//!
//! Parses arguments into a [`disk_tools_core::ScanOptions`], scans, and prints
//! either a size-sorted tree report or JSON. A spinner and the skipped-entries
//! summary go to stderr, keeping stdout clean for pipes.

mod args;
mod render;

use args::{Args, validate_root};
use clap::Parser;
use disk_tools_core::scan;
use indicatif::ProgressBar;
use render::json::render_json;
use render::skipped::render_skipped;
use render::tree::{RenderOptions, render_tree};
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    let args = Args::parse();
    // Copy the display-only knobs before `into_scan_options` consumes `args`.
    let json = args.json;
    let verbose = args.verbose;
    let render_options = RenderOptions {
        number: args.number,
        depth: args.depth,
        min_size: args.min_size,
        apparent: args.apparent,
        width: terminal_width(),
    };
    let options = args.into_scan_options();

    if let Err(message) = validate_root(&options.root) {
        eprintln!("disk-tools: {message}");
        return ExitCode::from(2);
    }

    // indicatif draws to stderr and hides itself when stderr isn't a tty, so a
    // piped run shows no spinner and stdout stays clean.
    let spinner = ProgressBar::new_spinner();
    spinner.set_message("Scanning…");
    spinner.enable_steady_tick(Duration::from_millis(100));
    let tree = scan(&options);
    spinner.finish_and_clear();

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

    if let Err(err) = write_report(&report) {
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
