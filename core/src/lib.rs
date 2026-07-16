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
// `walk` uses `size` and feeds `dedup`, but nothing public reaches the chain
// yet, so the compiler rightly calls it all dead. `scan()` in Task 5 is what
// makes it reachable — drop these `allow`s then, not before.
#[allow(dead_code)]
mod dedup;
#[allow(dead_code)]
mod size;
mod tree;
#[allow(dead_code)]
mod walk;

pub use options::ScanOptions;
pub use tree::{ScanNode, ScanTree, SkipReason, SkippedEntry};
