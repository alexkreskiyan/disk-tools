//! The skipped-entries summary printed to stderr after a scan.
//!
//! The core returns what it couldn't read as data; this turns it into a short
//! human note. It goes to stderr so it never contaminates `--json` stdout.

use disk_tools_core::{SkipReason, SkippedEntry};
use std::fmt::Write;

/// How many skipped paths are listed without `--verbose`.
const PREVIEW: usize = 10;

/// A human summary of what the scan couldn't read, or `None` when nothing was
/// skipped. Caps at [`PREVIEW`] paths unless `verbose`.
pub fn render_skipped(skipped: &[SkippedEntry], verbose: bool) -> Option<String> {
    if skipped.is_empty() {
        return None;
    }
    let total = skipped.len();
    let noun = if total == 1 { "entry" } else { "entries" };
    let mut out = String::new();
    // Writing to a `String` is infallible.
    let _ = writeln!(out, "{total} {noun} skipped:");

    let shown = if verbose { total } else { total.min(PREVIEW) };
    for entry in &skipped[..shown] {
        let _ = writeln!(
            out,
            "  {} ({})",
            entry.path.display(),
            reason(&entry.reason)
        );
    }
    if shown < total {
        let _ = writeln!(
            out,
            "  … and {} more (use --verbose to list all)",
            total - shown
        );
    }
    Some(out)
}

/// A short, lower-case label for a skip reason.
fn reason(reason: &SkipReason) -> &str {
    match reason {
        SkipReason::PermissionDenied => "permission denied",
        SkipReason::NotFound => "not found",
        SkipReason::Other(message) => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skips(n: usize) -> Vec<SkippedEntry> {
        (0..n)
            .map(|i| SkippedEntry {
                path: PathBuf::from(format!("/root/locked-{i}")),
                reason: SkipReason::PermissionDenied,
            })
            .collect()
    }

    fn lines(summary: &str) -> Vec<&str> {
        summary.lines().collect()
    }

    #[test]
    fn no_skips_yields_none() {
        assert!(render_skipped(&[], false).is_none());
        assert!(render_skipped(&[], true).is_none());
    }

    #[test]
    fn skipped_summary_caps_at_ten_with_count() {
        let summary = render_skipped(&skips(15), false).expect("summary");

        assert!(
            summary.starts_with("15 entries skipped:"),
            "header must state the total:\n{summary}"
        );
        // One header + 10 path lines + one "… and N more" line.
        let path_lines = lines(&summary)
            .into_iter()
            .filter(|l| l.trim_start().starts_with("/root/locked-"))
            .count();
        assert_eq!(
            path_lines, 10,
            "at most ten paths without --verbose:\n{summary}"
        );
        assert!(
            summary.contains("… and 5 more"),
            "the remainder is counted:\n{summary}"
        );
    }

    #[test]
    fn verbose_lists_all_skipped() {
        let summary = render_skipped(&skips(15), true).expect("summary");

        let path_lines = lines(&summary)
            .into_iter()
            .filter(|l| l.trim_start().starts_with("/root/locked-"))
            .count();
        assert_eq!(path_lines, 15, "--verbose lists every path:\n{summary}");
        assert!(
            !summary.contains("more"),
            "nothing is elided under --verbose:\n{summary}"
        );
    }

    #[test]
    fn exactly_ten_has_no_more_line() {
        let summary = render_skipped(&skips(10), false).expect("summary");
        assert!(
            !summary.contains("more"),
            "ten skips fit the preview exactly, no remainder:\n{summary}"
        );
    }

    #[test]
    fn eleven_skips_shows_exactly_one_more() {
        // The boundary just past the cap: ten shown, a single leftover.
        let summary = render_skipped(&skips(11), false).expect("summary");

        let path_lines = lines(&summary)
            .into_iter()
            .filter(|l| l.trim_start().starts_with("/root/locked-"))
            .count();
        assert_eq!(
            path_lines, 10,
            "eleven skips still cap the preview at ten:\n{summary}"
        );
        assert!(
            summary.contains("… and 1 more"),
            "exactly one entry is left over, and the wording must be singular-friendly:\n{summary}"
        );
    }

    #[test]
    fn more_count_is_total_minus_shown_for_various_totals() {
        // The "… and N more" arithmetic (total - shown) must hold for totals
        // well past the cap, not just the one value already covered elsewhere.
        for total in [12, 20, 47, 100] {
            let summary = render_skipped(&skips(total), false).expect("summary");
            let expected = format!("… and {} more", total - PREVIEW);
            assert!(
                summary.contains(&expected),
                "total={total}: expected `{expected}` in:\n{summary}"
            );
        }
    }

    /// The line count is exactly: one header + `min(N, 10)` path lines + (one
    /// "more" line iff there's a remainder). A stray blank line or a
    /// double-counted path would slip past substring checks but not this.
    #[test]
    fn summary_line_count_is_header_plus_shown_plus_optional_more() {
        // Under the cap: header + N paths, no "more" line.
        let summary = render_skipped(&skips(3), false).expect("summary");
        assert_eq!(lines(&summary).len(), 1 + 3);

        // Exactly at the cap: header + 10 paths, no "more" line.
        let summary = render_skipped(&skips(10), false).expect("summary");
        assert_eq!(lines(&summary).len(), 1 + 10);

        // Past the cap: header + 10 paths + one "more" line.
        let summary = render_skipped(&skips(25), false).expect("summary");
        assert_eq!(lines(&summary).len(), 1 + 10 + 1);

        // --verbose past the cap: header + every path, no "more" line.
        let summary = render_skipped(&skips(25), true).expect("summary");
        assert_eq!(lines(&summary).len(), 1 + 25);
    }

    #[test]
    fn singular_entry_wording() {
        let summary = render_skipped(&skips(1), false).expect("summary");
        assert!(
            summary.starts_with("1 entry skipped:"),
            "one skip uses the singular noun:\n{summary}"
        );
    }

    #[test]
    fn reason_labels_cover_every_variant() {
        let entries = vec![
            SkippedEntry {
                path: PathBuf::from("/a"),
                reason: SkipReason::PermissionDenied,
            },
            SkippedEntry {
                path: PathBuf::from("/b"),
                reason: SkipReason::NotFound,
            },
            SkippedEntry {
                path: PathBuf::from("/c"),
                reason: SkipReason::Other("disk on fire".to_owned()),
            },
        ];
        let summary = render_skipped(&entries, true).expect("summary");

        assert!(summary.contains("/a (permission denied)"), "{summary}");
        assert!(summary.contains("/b (not found)"), "{summary}");
        assert!(summary.contains("/c (disk on fire)"), "{summary}");
    }

    /// A real scan mixes skip reasons freely; the preview cap must not favor
    /// one reason over another or drop a differently-reasoned entry that
    /// happens to land within the first ten.
    #[test]
    fn not_found_and_other_reasons_both_survive_the_preview_together() {
        let mut entries = skips(8); // 8 PermissionDenied entries
        entries.push(SkippedEntry {
            path: PathBuf::from("/root/vanished"),
            reason: SkipReason::NotFound,
        });
        entries.push(SkippedEntry {
            path: PathBuf::from("/root/weird"),
            reason: SkipReason::Other("disk on fire".to_owned()),
        });
        // 10 entries total: exactly the cap, so both extra reasons must appear.

        let summary = render_skipped(&entries, false).expect("summary");

        assert!(
            summary.contains("/root/vanished (not found)"),
            "a NotFound entry within the preview must be listed:\n{summary}"
        );
        assert!(
            summary.contains("/root/weird (disk on fire)"),
            "an Other entry within the preview must be listed:\n{summary}"
        );
        assert!(
            !summary.contains("more"),
            "ten entries fit the preview exactly:\n{summary}"
        );
    }
}
