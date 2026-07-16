//! `disk-tools` — the CLI frontend.
//!
//! Parses arguments into a [`disk_tools_core::ScanOptions`] and validates the
//! root before anything runs. Rendering the scan (Task 7) and JSON output
//! (Task 8) are not wired yet — this is the argument surface and its validation.

mod args;

use args::{Args, validate_root};
use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args = Args::parse();
    let options = args.into_scan_options();

    if let Err(message) = validate_root(&options.root) {
        eprintln!("disk-tools: {message}");
        return ExitCode::from(2);
    }

    ExitCode::SUCCESS
}
