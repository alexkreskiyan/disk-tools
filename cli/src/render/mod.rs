//! Presentation of a [`disk_tools_core::ScanTree`].
//!
//! The core returns data; everything here decides how it reaches a consumer —
//! [`tree`] for a human terminal report, [`json`] for machine-readable output.

pub mod json;
pub mod tree;
