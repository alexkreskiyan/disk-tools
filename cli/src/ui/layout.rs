//! The browser's table: which columns fit, and what goes in each cell.
//!
//! One labelled header row over fixed-width columns, because the first version
//! of this screen put the sort order in a corner of the path line and it was not
//! findable. A column that is labelled where its numbers are needs no legend.
//!
//! Widths are fixed for everything except the name, which takes what is left —
//! names are the one field with no useful upper bound, and the eye scans a
//! ragged right edge better than a ragged left one.
//!
//! **Ages are relative** ("3d", "5mo"), not dates. A date needs a timezone, and
//! nothing in this workspace can supply one: `SystemTime` is UTC, and printing
//! UTC to someone browsing their own disk is wrong by up to half a day, silently.
//! Relative is also what a disk-cleanup tool is actually asked about — "how
//! stale is this" — and it fits in five columns instead of sixteen.

use super::listing::Entry;
use super::sort::Order;
use crate::render::tree::{fit, format_size};
use std::time::SystemTime;
use unicode_width::UnicodeWidthStr;

/// Right-aligned, wide enough for the label and for `16384.0P`.
const SIZE: usize = 8;
const CREATED: usize = 7;
const MODIFIED: usize = 8;
/// The share-of-parent bar and its percentage. Empty until directory sizes
/// exist (v0.4 Task 3); the column is reserved now so the header does not move
/// under the user when they arrive.
const TOTAL: usize = 12;
const SEP: &str = " │ ";
/// Below this the name is unreadable, so a column is dropped instead.
const MIN_NAME: usize = 8;

/// Which columns fit, and how much of the width is left for the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Columns {
    pub size: bool,
    pub created: bool,
    pub modified: bool,
    pub total: bool,
    pub name: usize,
}

/// Fit the table to `width`.
///
/// Columns are dropped rather than squeezed, widest and least-load-bearing
/// first: the bar is decoration, then the two ages, and the size last — on a
/// screen this narrow, "how big" is the one question still worth answering. The
/// name is never dropped.
pub fn columns(width: usize) -> Columns {
    let mut cols = Columns {
        size: true,
        created: true,
        modified: true,
        total: true,
        name: MIN_NAME,
    };

    let cramped = |cols: &Columns| fixed(cols) + MIN_NAME > width;
    if cramped(&cols) {
        cols.total = false;
    }
    if cramped(&cols) {
        cols.created = false;
    }
    if cramped(&cols) {
        cols.modified = false;
    }
    if cramped(&cols) {
        cols.size = false;
    }

    // At least one column of name even when nothing fits: a row of separators
    // with no name at all would be worse than a truncated one.
    cols.name = width.saturating_sub(fixed(&cols)).max(1);
    cols
}

/// Everything but the name, each with the separator that precedes it.
///
/// The name is always present, so the number of separators is exactly the
/// number of other columns.
fn fixed(cols: &Columns) -> usize {
    let sep = UnicodeWidthStr::width(SEP);
    [
        (cols.size, SIZE),
        (cols.created, CREATED),
        (cols.modified, MODIFIED),
        (cols.total, TOTAL),
    ]
    .iter()
    .filter(|(present, _)| *present)
    .map(|(_, width)| width + sep)
    .sum()
}

/// The labelled header, with the arrow on whichever column is sorting.
///
/// Each label sits over its own cells and takes its alignment from them, so the
/// arrow marks a column rather than floating in a line of its own.
pub fn header(cols: &Columns, order: Order, reverse: bool) -> String {
    let arrow = if reverse { "↓" } else { "↑" };
    // The labels come from `Order` itself, so a column can never be headed by
    // one name and sorted by another.
    let mark = |column: Order| {
        if order == column {
            format!("{}{arrow}", column.label())
        } else {
            column.label().to_owned()
        }
    };

    let mut parts = Vec::new();
    if cols.size {
        parts.push(format!("{:>SIZE$}", mark(Order::Size)));
    }
    parts.push(fit(&mark(Order::Name), cols.name));
    if cols.created {
        parts.push(format!("{:>CREATED$}", mark(Order::Created)));
    }
    if cols.modified {
        parts.push(format!("{:>MODIFIED$}", mark(Order::Modified)));
    }
    if cols.total {
        parts.push(format!("{:<TOTAL$}", "total"));
    }
    parts.join(SEP)
}

/// One entry, in the same columns as the header.
pub fn row(entry: &Entry, now: SystemTime, cols: &Columns) -> String {
    let mut parts = Vec::new();
    if cols.size {
        let size = entry.size.map(format_size).unwrap_or_default();
        parts.push(format!("{size:>SIZE$}"));
    }

    let mark = if entry.is_dir { "/" } else { "" };
    parts.push(fit(
        &format!("{}{mark}", entry.name.to_string_lossy()),
        cols.name,
    ));

    if cols.created {
        parts.push(format!("{:>CREATED$}", age(now, entry.created)));
    }
    if cols.modified {
        parts.push(format!("{:>MODIFIED$}", age(now, entry.modified)));
    }
    if cols.total {
        // Filled in once directories are measured.
        parts.push(" ".repeat(TOTAL));
    }
    parts.join(SEP)
}

/// How long ago, in one unit and at most four columns.
///
/// A timestamp in the future is a clock that disagrees with the filesystem's,
/// not a fact about the file — "now" is the honest reading of it, and a negative
/// age would be worse.
fn age(now: SystemTime, then: Option<SystemTime>) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::time::Duration;

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 * 365 * 24 * 60 * 60)
    }

    fn ago(secs: u64) -> Option<SystemTime> {
        Some(now() - Duration::from_secs(secs))
    }

    fn entry(name: &str) -> Entry {
        Entry {
            name: OsString::from(name),
            is_dir: false,
            size: Some(4096),
            modified: ago(3 * 24 * 60 * 60),
            created: ago(90 * 24 * 60 * 60),
        }
    }

    /// The whole point of the header: the label sits over its own cells, so the
    /// column widths must be identical in both.
    #[test]
    fn every_cell_lines_up_under_its_label() {
        let cols = columns(100);

        let header = header(&cols, Order::Name, false);
        let row = row(&entry("readme.md"), now(), &cols);

        // Counted in characters, not bytes: `↑` is three bytes wide and would
        // shift every offset after it in the header but not in the row.
        let positions = |line: &str| {
            line.chars()
                .enumerate()
                .filter(|(_, ch)| *ch == '│')
                .map(|(at, _)| at)
                .collect::<Vec<_>>()
        };
        assert_eq!(positions(&header), positions(&row));
        assert_eq!(positions(&header).len(), 4, "size, name, created, modified");
    }

    #[test]
    fn the_sorted_column_is_the_one_carrying_the_arrow() {
        let cols = columns(100);

        let by_size = header(&cols, Order::Size, false);
        assert!(by_size.contains("size↑"), "{by_size}");
        assert!(!by_size.contains("name↑"), "{by_size}");

        let reversed = header(&cols, Order::Modified, true);
        assert!(reversed.contains("modified↓"), "{reversed}");
    }

    /// Nothing here is squeezed: a column either fits at its full width or goes.
    #[test]
    fn columns_drop_from_the_least_load_bearing_as_the_screen_narrows() {
        assert_eq!(
            columns(120),
            Columns {
                size: true,
                created: true,
                modified: true,
                total: true,
                name: 120 - (8 + 3) - (7 + 3) - (8 + 3) - (12 + 3)
            }
        );

        let narrower = columns(50);
        assert!(!narrower.total, "the bar is decoration");
        assert!(narrower.size, "and the size is not");

        let narrow = columns(24);
        assert!(!narrow.created && !narrow.modified);
        assert!(narrow.size, "how big is the last question worth answering");
    }

    /// A one-column terminal is a resize artefact, not a use case — but it must
    /// not produce a zero-width name or a panic.
    #[test]
    fn the_name_survives_any_width() {
        for width in [0, 1, 5, 12, 40] {
            let cols = columns(width);
            assert!(cols.name >= 1, "width {width}");
            let row = row(&entry("some-long-name.txt"), now(), &cols);
            assert!(!row.is_empty(), "width {width}");
        }
    }

    #[test]
    fn a_directory_is_marked_and_carries_no_size() {
        let cols = columns(100);
        let dir = Entry {
            is_dir: true,
            size: None,
            ..entry("src")
        };

        let row = row(&dir, now(), &cols);

        assert!(row.contains("src/"), "{row}");
        assert!(
            row.starts_with(' '),
            "the size cell is blank, not a zero: {row}"
        );
    }

    #[test]
    fn ages_step_through_one_unit_at_a_time() {
        for (secs, expected) in [
            (0, "now"),
            (59, "now"),
            (60, "1m"),
            (59 * 60, "59m"),
            (60 * 60, "1h"),
            (23 * 3600, "23h"),
            (24 * 3600, "1d"),
            (29 * 24 * 3600, "29d"),
            (30 * 24 * 3600, "1mo"),
            (364 * 24 * 3600, "12mo"),
            (365 * 24 * 3600, "1y"),
            (900 * 24 * 3600, "2y"),
        ] {
            assert_eq!(age(now(), ago(secs)), expected, "{secs} seconds");
        }
    }

    /// Every age has to fit its column, or the arithmetic above is decorative.
    #[test]
    fn no_age_overflows_its_column() {
        for secs in [0, 61, 4_000, 100_000, 3_000_000, 40_000_000, 4_000_000_000] {
            let age = age(now(), ago(secs));
            assert!(age.len() <= CREATED, "{age:?} at {secs} seconds");
        }
    }

    /// A file dated in the future means the clock disagrees with the
    /// filesystem's. That is not a negative age.
    #[test]
    fn a_timestamp_in_the_future_reads_as_now() {
        let later = now() + Duration::from_secs(86_400);

        assert_eq!(age(now(), Some(later)), "now");
    }

    #[test]
    fn an_absent_timestamp_leaves_the_cell_empty() {
        assert_eq!(age(now(), None), "");
    }
}
