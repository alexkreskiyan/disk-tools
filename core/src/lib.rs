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
