//! `disk-tools-core` — the scanning engine behind the `disk-tools` CLI.
//!
//! The library never logs or prints. Anything that goes wrong during a scan
//! (an unreadable directory, a file that vanishes mid-walk) comes back as data
//! in [`ScanTree::skipped`]; deciding how to show it is the frontend's job.
//!
//! The core also knows nothing about config files, config paths or terminals —
//! it is driven entirely by an explicit [`ScanOptions`].

// `deny` rather than `forbid`: Windows exposes no safe way to read a file's
// allocated size, so `size::allocated` must call `GetCompressedFileSizeW`
// through FFI. `forbid` cannot be lifted by an `#[allow]` — that is the whole
// difference between the two — so the crate denies `unsafe` and that single
// function opts out explicitly. Everywhere else, including all of Unix, this
// still fails the build.
#![deny(unsafe_code)]

mod options;
// Nothing calls this yet — Task 3's walk is its first consumer. Drop the
// `allow` once it does.
#[allow(dead_code)]
mod size;
mod tree;

pub use options::ScanOptions;
pub use tree::{ScanNode, ScanTree, SkipReason, SkippedEntry};
