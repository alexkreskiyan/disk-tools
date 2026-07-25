//! `disk-tools-core` — the scanning engine behind the `disk-tools` CLI.
//!
//! The library never logs or prints. Anything that goes wrong during a scan
//! (an unreadable directory, a file that vanishes mid-walk) comes back as data
//! in [`ScanTree::skipped`]; deciding how to show it is the frontend's job.
//!
//! The core also knows nothing about config files, config paths or terminals —
//! it is driven entirely by an explicit [`ScanOptions`].

// `deny` rather than `forbid`: Windows exposes neither a file's allocated size
// nor its identity through anything safe, so both must come through FFI.
// `forbid` cannot be lifted by an `#[allow]` — that is the whole difference
// between the two — so the crate denies `unsafe` and the functions that need it
// opt out one at a time, never a whole module.
//
// The exemptions, all `#[cfg(windows)]`:
//   size::allocated              GetCompressedFileSizeW  — per-file fallback
//   windows_dir::read_facts      GetFileInformationByHandleEx
//   windows_dir::collect_batch   walking that call's output buffer
//   windows_dir::volume_serial   GetFileInformationByHandle
//
// Everywhere else, including all of Unix, `unsafe` still fails the build. Any
// further exemption deserves the same scrutiny these got — the buffer walk in
// particular is the only one doing pointer arithmetic rather than a single call,
// and is bounds-checked against the capacity we passed in rather than trusting
// the OS to stay inside it.
#![deny(unsafe_code)]

mod dedup;
mod options;
mod size;
mod tree;
mod walk;
#[cfg(windows)]
mod windows_dir;

pub use options::ScanOptions;
pub use tree::{ScanNode, ScanTree, SkipReason, SkippedEntry};

/// Scan `options.root` and return a size-annotated tree plus whatever was
/// skipped.
///
/// The one public entry point. Runs the three internal phases in order — walk
/// the tree in parallel, resolve hardlink attribution, then aggregate bottom-up
/// — because attribution must settle before any directory total is summed.
/// Never fails: an unreadable root (or anything else that goes wrong) comes back
/// as a [`SkippedEntry`] in [`ScanTree::skipped`], not an error.
pub fn scan(options: &ScanOptions) -> ScanTree {
    let mut walked = walk::walk(options);
    dedup::attribute(&mut walked.entries);
    let root = tree::aggregate(walked.entries, options.root.as_path());
    ScanTree {
        root,
        skipped: walked.skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Diagnostic, not an assertion: prints where `scan` spends its time.
    ///
    /// The three phases are private, so nothing outside this crate can time
    /// them separately — hence a test rather than an example or a bench.
    /// Ignored by default because it needs a real tree and asserts nothing.
    ///
    /// ```text
    /// just bench-phases ~/.cargo
    /// RAYON_NUM_THREADS=1 just bench-phases ~/.cargo   # what actually scales
    /// ```
    #[test]
    #[ignore = "diagnostic: needs DT_PHASE_PATH, prints timings, asserts nothing"]
    fn phase_split() {
        let root = std::env::var("DT_PHASE_PATH")
            .expect("set DT_PHASE_PATH to the tree to scan, e.g. DT_PHASE_PATH=~/.cargo");
        let options = ScanOptions {
            root: root.clone().into(),
            ..ScanOptions::default()
        };

        let start = Instant::now();
        let mut walked = walk::walk(&options);
        let walk = start.elapsed();
        let entries = walked.entries.len();

        let start = Instant::now();
        dedup::attribute(&mut walked.entries);
        let dedup = start.elapsed();

        let start = Instant::now();
        let tree = tree::aggregate(walked.entries, options.root.as_path());
        let aggregate = start.elapsed();

        let total = walk + dedup + aggregate;
        let share = |d: std::time::Duration| 100.0 * d.as_secs_f64() / total.as_secs_f64();

        println!(
            "\n{root} — {entries} entries, {} skipped",
            walked.skipped.len()
        );
        println!("  walk       {:>9.1?}  {:>5.1}%", walk, share(walk));
        println!("  dedup      {:>9.1?}  {:>5.1}%", dedup, share(dedup));
        println!(
            "  aggregate  {:>9.1?}  {:>5.1}%",
            aggregate,
            share(aggregate)
        );
        println!("  total      {total:>9.1?}");
        println!(
            "  threads    {}\n  root total {} bytes",
            rayon::current_num_threads(),
            tree.allocated
        );
    }
}
