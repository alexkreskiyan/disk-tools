//! Presentation of a [`disk_tools_core::ScanTree`].
//!
//! The core returns data; everything here decides how it reaches a consumer —
//! [`tree`] for a human terminal report, [`json`] for machine-readable output.

pub mod clean;
pub mod dup;
pub mod json;
pub mod skipped;
pub mod tree;

/// How long ago, in one unit and at most four columns.
///
/// A timestamp in the future is a clock that disagrees with the filesystem's,
/// not a fact about the file — "now" is the honest reading of it, and a negative
/// age would be worse.
///
/// Shared by the browser's date columns and the duplicate report's keeper line,
/// for the reason one `parse_size` serves the flag and the config file: two
/// would drift, and a user comparing the two screens would be comparing two
/// different roundings.
pub(crate) fn age(now: std::time::SystemTime, then: Option<std::time::SystemTime>) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    let Some(then) = then else {
        return String::new();
    };
    let Ok(elapsed) = now.duration_since(then) else {
        return "now".to_owned();
    };

    match elapsed.as_secs() {
        secs if secs < MINUTE => "now".to_owned(),
        secs if secs < HOUR => format!("{}m", secs / MINUTE),
        secs if secs < DAY => format!("{}h", secs / HOUR),
        secs if secs < 30 * DAY => format!("{}d", secs / DAY),
        secs if secs < 365 * DAY => format!("{}mo", secs / (30 * DAY)),
        secs => format!("{}y", secs / (365 * DAY)),
    }
}
