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

    if json {
        match render_json(&tree) {
            Ok(payload) => println!("{payload}"),
            Err(err) => {
                eprintln!("disk-tools: cannot encode JSON: {err}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        print!("{}", render_tree(&tree, &render_options));
    }

    // Always to stderr — visible on a terminal, out of a stdout pipe (so `--json`
    // stdout stays valid JSON).
    if let Some(summary) = render_skipped(&tree.skipped, verbose) {
        eprint!("{summary}");
    }
    ExitCode::SUCCESS
}

/// Terminal width, or a fixed fallback when stdout is not a tty (piped output).
fn terminal_width() -> usize {
    terminal_size::terminal_size()
        .map(|(width, _)| width.0 as usize)
        .unwrap_or(80)
}
