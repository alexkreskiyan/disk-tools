//! `disk-tools-core` — the scanning engine behind the `disk-tools` CLI.
//!
//! The library never logs or prints. Anything that goes wrong during a scan
//! (an unreadable directory, a file that vanishes mid-walk) comes back as data
//! in [`ScanTree::skipped`]; deciding how to show it is the frontend's job.
//!
//! The core also knows nothing about config files, config paths or terminals —
//! it is driven entirely by an explicit [`ScanOptions`].

#![forbid(unsafe_code)]

mod options;
mod tree;

pub use options::ScanOptions;
pub use tree::{ScanNode, ScanTree, SkipReason, SkippedEntry};
