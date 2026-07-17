//! `disk-tools` — the CLI frontend.
//!
//! Parses arguments into a [`disk_tools_core::ScanOptions`], scans, and prints
//! either a size-sorted tree report or JSON. The progress/skipped summary
//! (Task 9) is not wired yet.

mod args;
mod render;

use args::{Args, validate_root};
use clap::Parser;
use disk_tools_core::scan;
use render::json::render_json;
use render::tree::{RenderOptions, render_tree};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args = Args::parse();
    // Copy the display-only knobs before `into_scan_options` consumes `args`.
    let json = args.json;
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

    let tree = scan(&options);
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
    ExitCode::SUCCESS
}

/// Terminal width, or a fixed fallback when stdout is not a tty (piped output).
fn terminal_width() -> usize {
    terminal_size::terminal_size()
        .map(|(width, _)| width.0 as usize)
        .unwrap_or(80)
}
