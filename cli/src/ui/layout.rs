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
use crate::render::age;
use crate::render::tree::{fit, format_size};
use std::time::SystemTime;
use unicode_width::UnicodeWidthStr;

/// Every fixed column is wide enough for **its label plus a sort arrow** as well
/// as for its widest value. A header cell that outgrows its column shifts every
/// separator after it by one and the table stops lining up — which is what
/// `created↑` in seven columns did.
///
/// Right-aligned, wide enough for `16384.0P`.
const SIZE: usize = 8;
/// The same figures, so the same width; the label is shorter than they are.
const CLEAN: usize = 8;
/// `created↑`. An age itself is at most four columns.
const CREATED: usize = 8;
/// `modified↑`.
const MODIFIED: usize = 9;
/// The bar, a space, and `100%`.
const BAR: usize = 7;
const TOTAL: usize = BAR + 1 + 4;
const SEP: &str = " │ ";
/// Below this the name is unreadable, so a column is dropped instead.
const MIN_NAME: usize = 8;

/// Which columns fit, and how much of the width is left for the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Columns {
    pub size: bool,
    pub clean: bool,
    pub created: bool,
    pub modified: bool,
    pub total: bool,
    pub name: usize,
}

/// Fit the table to `width`.
///
/// Columns are dropped rather than squeezed, widest and least-load-bearing
/// first: the bar is decoration, then the two ages, then what could be cleaned,
/// and the size last — on a screen this narrow, "how big" is the one question
/// still worth answering. The name is never dropped.
pub fn columns(width: usize) -> Columns {
    let mut cols = Columns {
        size: true,
        clean: true,
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
        cols.clean = false;
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
        (cols.clean, CLEAN),
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
    let arrow = if reverse { '↓' } else { '↑' };
    // The labels come from `Order` itself, so a column can never be headed by
    // one name and sorted by another.
    //
    // **The arrow's place is always there**, as a space when the column is not
    // the one sorting. Right-aligned cells grow leftwards, so a label that gains
    // a character shifts the whole word one column left — every label on the
    // line moving whenever the sort key changed, which is the second way this
    // header failed to line up.
    let mark = |column: Order| {
        format!(
            "{}{}",
            column.label(),
            if order == column { arrow } else { ' ' }
        )
    };
    let mut parts = Vec::new();
    if cols.size {
        parts.push(format!("{:>SIZE$}", mark(Order::Size)));
    }
    if cols.clean {
        parts.push(format!("{:>CLEAN$}", mark(Order::Cleanable)));
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
///
/// `sized` is the sum of the listing's *known* sizes — see [`share`].
pub fn row(entry: &Entry, now: SystemTime, sized: u64, cols: &Columns) -> String {
    let mut parts = Vec::new();
    if cols.size {
        // The spinner sits beside the figure rather than replacing it, so a
        // directory being walked shows both that it is working and how far it
        // has got.
        let size = match (entry.measuring, entry.size) {
            (true, Some(bytes)) => format!("{} {}", spinner(now), format_size(bytes)),
            (true, None) => spinner(now).to_string(),
            (false, Some(bytes)) => format_size(bytes),
            (false, None) => String::new(),
        };
        parts.push(format!("{size:>SIZE$}"));
    }

    if cols.clean {
        // Blank for nothing and blank for not-known-yet, deliberately. The two
        // are told apart one column to the left: a row still being walked is
        // spinning, and a row that has settled has finished looking. A `0`
        // would read as a verdict on a directory nobody has been into.
        let clean = match entry.reclaimable {
            Some(bytes) if bytes > 0 => format_size(bytes),
            _ => String::new(),
        };
        parts.push(format!("{clean:>CLEAN$}"));
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
        parts.push(match share(entry, sized) {
            Some(fraction) => format!("{} {:>3}%", bar(fraction), (fraction * 100.0).round()),
            None => " ".repeat(TOTAL),
        });
    }
    parts.join(SEP)
}

/// What fraction of the listing this entry accounts for, or `None` when there is
/// nothing honest to say.
///
/// **The denominator is the sum of what is known**, not the parent's total —
/// which nobody has asked for and which would cost a second walk. So the
/// percentages describe the rows on screen, and they climb towards 100% as the
/// unmeasured directories arrive rather than starting there and being wrong.
///
/// An entry still being measured is excluded from both sides: its figure is
/// rising, and a denominator that moved under the other rows would have every
/// percentage on screen drifting for a reason none of them show.
pub fn share(entry: &Entry, sized: u64) -> Option<f64> {
    if entry.measuring || sized == 0 {
        return None;
    }
    entry.size.map(|bytes| bytes as f64 / sized as f64)
}

/// Right-aligned fill, as in the `scan` report.
fn bar(fraction: f64) -> String {
    let filled = (fraction * BAR as f64).round().max(0.0) as usize;
    let blocks = "\u{2588}".repeat(filled.min(BAR));
    format!("{blocks:>BAR$}")
}

/// Which frame of the spinner belongs to this instant.
///
/// Derived from the clock rather than counted per frame: the event loop wakes on
/// a keypress as well as on its timeout, so a counter would race ahead while the
/// user typed and stall while they did not.
fn spinner(now: SystemTime) -> char {
    const FRAMES: [char; 10] = [
        '\u{280b}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283c}', '\u{2834}', '\u{2826}',
        '\u{2827}', '\u{2807}', '\u{280f}',
    ];
    let millis = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0);
    FRAMES[(millis / 100) as usize % FRAMES.len()]
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
            state: disk_tools_core::State::Untracked,
            reclaimable: None,
            measuring: false,
        }
    }

    /// The whole point of the header: the label sits over its own cells, so the
    /// column widths must be identical in both.
    #[test]
    fn every_cell_lines_up_under_its_label() {
        let cols = columns(100);

        let header = header(&cols, Order::Name, false);
        let row = row(&entry("readme.md"), now(), 0, &cols);

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
        assert_eq!(
            positions(&header).len(),
            5,
            "size, clean, name, created, modified"
        );
    }

    /// Where a word starts, counted in columns rather than bytes.
    fn column_of(line: &str, word: &str) -> usize {
        let line: Vec<char> = line.chars().collect();
        let word: Vec<char> = word.chars().collect();
        line.windows(word.len())
            .position(|seen| seen == word)
            .unwrap_or_else(|| panic!("{word:?} is not in {:?}", line.iter().collect::<String>()))
    }

    /// The two ways the arrow used to move the header, both fixed by the same
    /// rule — its place is always reserved.
    ///
    /// It widened its cell: `created↑` is eight columns and `created` was given
    /// seven, so every separator after it shifted one place right. And in a
    /// right-aligned cell it pushed its own label left, so changing the sort key
    /// moved words that had nothing to do with it.
    #[test]
    fn the_arrow_never_moves_anything() {
        const LABELS: [&str; 5] = ["size", "clean", "name", "created", "modified"];
        let cols = columns(100);

        let plain = header(&cols, Order::Name, false);
        let width = plain.chars().count();
        let places: Vec<usize> = LABELS.iter().map(|word| column_of(&plain, word)).collect();

        for order in [Order::Name, Order::Size, Order::Created, Order::Modified] {
            for reverse in [false, true] {
                let header = header(&cols, order, reverse);
                let what = format!("{} reversed={reverse}: {header:?}", order.label());

                assert_eq!(header.chars().count(), width, "{what}");
                assert_eq!(
                    header.chars().filter(|ch| *ch == '│').count(),
                    5,
                    "every separator is still there — {what}"
                );
                assert_eq!(
                    LABELS
                        .iter()
                        .map(|word| column_of(&header, word))
                        .collect::<Vec<_>>(),
                    places,
                    "and no label moved — {what}"
                );
            }
        }
    }

    /// A row says how much of itself `clean` would take, and says nothing
    /// where there is nothing to say.
    #[test]
    fn the_clean_column_carries_what_a_rule_claims() {
        let cols = columns(100);
        let claimed = Entry {
            size: Some(40_960),
            reclaimable: Some(40_960),
            ..entry("node_modules")
        };

        let whole = row(&claimed, now(), 40_960, &cols);
        assert_eq!(
            whole.matches("40.0K").count(),
            2,
            "the size, and all of it reclaimable: {whole}"
        );

        let partly = row(
            &Entry {
                reclaimable: Some(4096),
                ..claimed.clone()
            },
            now(),
            40_960,
            &cols,
        );
        assert!(partly.contains("4.0K"), "{partly}");

        for nothing in [None, Some(0)] {
            let quiet = row(
                &Entry {
                    reclaimable: nothing,
                    ..claimed.clone()
                },
                now(),
                40_960,
                &cols,
            );
            assert_eq!(
                quiet.matches("40.0K").count(),
                1,
                "only the size is left: {quiet}"
            );
        }
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
                clean: true,
                created: true,
                modified: true,
                total: true,
                name: 120 - (SIZE + 3) - (CLEAN + 3) - (CREATED + 3) - (MODIFIED + 3) - (TOTAL + 3)
            }
        );

        let narrower = columns(50);
        assert!(!narrower.total, "the bar is decoration");
        assert!(narrower.clean, "what could be freed is not");
        assert!(narrower.size, "nor is how big it is");

        let narrow = columns(24);
        assert!(!narrow.created && !narrow.modified && !narrow.clean);
        assert!(narrow.size, "how big is the last question worth answering");
    }

    /// A one-column terminal is a resize artefact, not a use case — but it must
    /// not produce a zero-width name or a panic.
    #[test]
    fn the_name_survives_any_width() {
        for width in [0, 1, 5, 12, 40] {
            let cols = columns(width);
            assert!(cols.name >= 1, "width {width}");
            let row = row(&entry("some-long-name.txt"), now(), 0, &cols);
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

        let row = row(&dir, now(), 0, &cols);

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

    /// The percentages describe the rows on screen, so they are against the sum
    /// of what is known — not against a parent total nobody asked for.
    #[test]
    fn a_share_is_of_the_sum_of_the_measured() {
        let entry = Entry {
            size: Some(250),
            ..entry("f")
        };

        assert_eq!(share(&entry, 1000), Some(0.25));
        assert_eq!(share(&entry, 250), Some(1.0));
    }

    /// A rising figure has no share: including it would move the denominator
    /// under every settled row, for a reason none of them show.
    #[test]
    fn an_entry_still_being_measured_has_no_share() {
        let running = Entry {
            size: Some(500),
            measuring: true,
            ..entry("busy")
        };

        assert_eq!(share(&running, 1000), None);
    }

    #[test]
    fn nothing_measured_yet_is_no_share_rather_than_a_division() {
        assert_eq!(share(&entry("f"), 0), None);
        assert_eq!(
            share(
                &Entry {
                    size: None,
                    ..entry("f")
                },
                1000
            ),
            None
        );
    }

    #[test]
    fn the_bar_fills_from_empty_to_full_and_no_further() {
        assert_eq!(bar(0.0).trim(), "");
        assert_eq!(bar(1.0).chars().count(), BAR);
        assert!(bar(1.0).chars().all(|ch| ch == '\u{2588}'));
        assert_eq!(
            bar(2.0).chars().count(),
            BAR,
            "a share over one is still one bar wide"
        );
    }

    /// Both cells sit in fixed columns, so neither may grow when it fills.
    #[test]
    fn a_full_bar_and_a_spinner_still_fit_their_columns() {
        let cols = columns(100);
        let busy = Entry {
            size: Some(999_999_999),
            measuring: true,
            ..entry("working")
        };

        let header = header(&cols, Order::Name, false).chars().count();
        for entry in [&busy, &entry("done")] {
            assert_eq!(
                row(entry, now(), 1_000_000_000, &cols).chars().count(),
                header
            );
        }
    }

    /// A row being walked shows both that it is working and how far it has got.
    #[test]
    fn a_measuring_row_carries_a_spinner_beside_its_figure() {
        let cols = columns(100);
        let busy = Entry {
            size: Some(4096),
            measuring: true,
            ..entry("working")
        };

        let row = row(&busy, now(), 8192, &cols);

        assert!(row.contains("4.0K"), "the figure so far: {row}");
        assert!(
            row.chars()
                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)),
            "and a spinner beside it: {row}"
        );
        assert!(
            !row.contains('%'),
            "but no share, because the figure is still climbing: {row}"
        );
    }

    /// The frame comes from the clock, so it advances whether or not the event
    /// loop happened to wake.
    #[test]
    fn the_spinner_turns_with_time() {
        let frames: Vec<char> = (0..10)
            .map(|tick| spinner(now() + Duration::from_millis(tick * 100)))
            .collect();

        assert_eq!(
            frames
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            10,
            "a second apart, every frame differs: {frames:?}"
        );
        assert_eq!(
            spinner(now()),
            spinner(now() + Duration::from_secs(1)),
            "and it comes round"
        );
    }

    /// The `clean` column sorts now, so it takes the arrow like the others —
    /// and the width was already reserved for it, which is why nothing shifts.
    #[test]
    fn the_clean_column_can_carry_the_arrow() {
        let cols = columns(120);
        let line = header(&cols, Order::Cleanable, true);

        assert!(line.contains("clean↓"), "{line}");
        assert_eq!(
            line.len(),
            header(&cols, Order::Name, false).len(),
            "and every separator stays where it was"
        );
    }
}
