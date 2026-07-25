//! The default human report: a size-sorted tree with parent-relative bars.
//!
//! Each entry shows its share of its parent (the parent itself is 100%) as a
//! percentage and a right-aligned bar. The bar/percent block is pinned to the
//! terminal's right edge; long names are truncated with `…` to keep it aligned.
//!
//! `-n`, `--depth` and `--min-size` filter *what is printed* — never the totals,
//! which stay full-subtree (du semantics). `--apparent` switches the unit used
//! for both ranking and the displayed size.

use disk_tools_core::{ScanNode, ScanTree};
use std::fmt::Write;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display knobs for the tree report. Every field filters output only; the tree
/// itself (and its totals) is untouched.
pub struct RenderOptions {
    /// `-n`: at most this many lines are printed.
    pub number: Option<usize>,
    /// `--depth`: deepest level shown, with the root at level 0.
    pub depth: Option<usize>,
    /// `--min-size`: hide entries whose size (in the ranking unit) is below this.
    pub min_size: u64,
    /// `--apparent`: rank and size by apparent bytes rather than allocated.
    pub apparent: bool,
    /// Terminal columns; a fixed fallback is used when stdout is not a tty.
    pub width: usize,
}

/// Fixed-width fields that flank the flexible name column.
const SIZE_COLS: usize = 8; // right-aligned size, fits up to "16384.0P"
const BAR_COLS: usize = 20; // the bar itself
const PERCENT_COLS: usize = 4; // "100%"
/// The gutters in `"{size}  {name}  {bar} {percent}"` — two 2-column gaps and
/// one 1-column gap, summed so the layout arithmetic has a single source.
const GUTTER_COLS: usize = 5;

/// Render `tree` to a string, ready to print.
pub fn render_tree(tree: &ScanTree, opts: &RenderOptions) -> String {
    let mut out = String::new();
    let mut budget = opts.number.unwrap_or(usize::MAX);
    // The root is its own reference point, so it renders as 100%.
    let root_size = size_of(&tree.root, opts.apparent);
    emit(&tree.root, 0, root_size, opts, &mut budget, &mut out);
    out
}

/// Print `node` (as a share of `parent_size`), then recurse into its size-ranked
/// children — stopping at the line budget, the display depth, or the threshold.
fn emit(
    node: &ScanNode,
    depth: usize,
    parent_size: u64,
    opts: &RenderOptions,
    budget: &mut usize,
    out: &mut String,
) {
    if *budget == 0 {
        return;
    }
    // Its own size doubles as the reference for its children's shares.
    let node_size = write_line(node, depth, parent_size, opts, out);
    *budget -= 1;

    // Children would sit at depth + 1; stop once that passes the display depth.
    if opts.depth.is_some_and(|limit| depth >= limit) {
        return;
    }

    let mut children: Vec<(u64, &ScanNode)> = node
        .children
        .iter()
        .map(|child| (size_of(child, opts.apparent), child))
        .collect();
    children.sort_by_key(|&(size, _)| std::cmp::Reverse(size));

    for (size, child) in children {
        if *budget == 0 {
            break;
        }
        // Below the threshold → hide the entry and its subtree. Pruning the
        // subtree is sound: a child's size never exceeds its parent's total, so
        // nothing larger hides beneath a hidden node.
        if size < opts.min_size {
            continue;
        }
        emit(child, depth + 1, node_size, opts, budget, out);
    }
}

/// Write `node`'s line and return its size (in the ranking unit), which the
/// caller reuses as the reference for the node's children.
fn write_line(
    node: &ScanNode,
    depth: usize,
    parent_size: u64,
    opts: &RenderOptions,
    out: &mut String,
) -> u64 {
    let size = size_of(node, opts.apparent);
    let size_str = format_size(size);
    let content = format!("{}{}", "  ".repeat(depth), display_name(node, depth));

    // Writing to a `String` is infallible.
    match name_columns(opts.width) {
        Some(cols) => {
            let name = fit(&content, cols);
            let bar = bar(size, parent_size);
            let percent = percent(size, parent_size);
            // `PERCENT_COLS` counts the trailing `%`, so the number field is one
            // narrower — kept in step so the layout arithmetic stays honest.
            let _ = writeln!(
                out,
                "{size_str:>sz$}  {name}  {bar} {percent:>pw$}%",
                sz = SIZE_COLS,
                pw = PERCENT_COLS - 1
            );
        }
        // Too narrow for a bar: fall back to size and name only.
        None => {
            let _ = writeln!(out, "{size_str:>sz$}  {content}", sz = SIZE_COLS);
        }
    }
    size
}

/// Columns left for the (indented) name once the fixed fields are placed, or
/// `None` when the terminal is too narrow to fit a bar at all.
fn name_columns(width: usize) -> Option<usize> {
    let fixed = SIZE_COLS + BAR_COLS + PERCENT_COLS + GUTTER_COLS;
    width.checked_sub(fixed).filter(|&cols| cols > 0)
}

/// Fit `text` into exactly `cols` **terminal columns** — pad short text, or
/// truncate long text with a trailing `…`.
///
/// Measured in display width, not characters: CJK and emoji take two columns
/// each and combining marks take none, so a char count would misplace the
/// bar/percent block against the right edge. Truncation keeps whole characters,
/// so a name is never split mid-codepoint.
fn fit(text: &str, cols: usize) -> String {
    let width = UnicodeWidthStr::width(text);
    if width <= cols {
        return format!("{text}{}", " ".repeat(cols - width));
    }
    if cols == 0 {
        return String::new();
    }
    // One column goes to the ellipsis; fill the rest with whole characters.
    let budget = cols - 1;
    let mut kept = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > budget {
            break;
        }
        kept.push(ch);
        used += ch_width;
    }
    // A double-width glyph straddling the boundary is dropped rather than
    // half-drawn, leaving a column to pad so the total still lands on `cols`.
    format!("{kept}{}…", " ".repeat(budget - used))
}

/// The root prints its whole path (the thing the user asked to scan); every
/// other entry prints just its final component.
fn display_name(node: &ScanNode, depth: usize) -> String {
    if depth == 0 {
        return node.path.display().to_string();
    }
    node.path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| node.path.display().to_string())
}

/// `size` as a percentage of `parent` (0–100). The root, whose reference is its
/// own size, is 100%.
fn percent(size: u64, parent: u64) -> u64 {
    fraction(size, parent).map_or(0, |f| (f * 100.0).round() as u64)
}

/// A fixed-width bar whose filled blocks are **right-aligned** — the solid part
/// sits against the right of the column, next to the percentage.
fn bar(size: u64, parent: u64) -> String {
    let filled = fraction(size, parent).map_or(0, |f| (f * BAR_COLS as f64).round() as usize);
    let blocks = "█".repeat(filled.min(BAR_COLS));
    format!("{blocks:>cols$}", cols = BAR_COLS)
}

/// `size / parent` as a fraction. An entry equal to its reference — the root,
/// or a sole child carrying all its parent's bytes — is a full `1.0`, which also
/// covers the `0 / 0` case; otherwise `None` when there's no scale.
fn fraction(size: u64, parent: u64) -> Option<f64> {
    if size == parent {
        return Some(1.0);
    }
    (parent != 0).then(|| size as f64 / parent as f64)
}

fn size_of(node: &ScanNode, apparent: bool) -> u64 {
    if apparent {
        node.apparent
    } else {
        node.allocated
    }
}

/// Human-readable, 1024-based — the same scale `--min-size`'s parser accepts.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    // Climb while the value, rounded to the one decimal we print, still reaches
    // 1024 — otherwise a value like 1048575 would show "1024.0K", not "1.0M".
    while unit < UNITS.len() - 1 && (value * 10.0).round() / 10.0 >= 1024.0 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1}{}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// `modified` and `links` are `None` throughout these fixtures on purpose:
    /// the tree report shows neither, so a value here would suggest the renderer
    /// cares about something it does not.
    fn file(path: &str, allocated: u64, apparent: u64) -> ScanNode {
        ScanNode {
            path: PathBuf::from(path),
            allocated,
            apparent,
            is_dir: false,
            modified: None,
            links: None,
            children: Vec::new(),
        }
    }

    fn dir(path: &str, allocated: u64, apparent: u64, children: Vec<ScanNode>) -> ScanNode {
        ScanNode {
            path: PathBuf::from(path),
            allocated,
            apparent,
            is_dir: true,
            modified: None,
            links: None,
            children,
        }
    }

    fn tree(root: ScanNode) -> ScanTree {
        ScanTree {
            root,
            skipped: Vec::new(),
            link_groups: Vec::new(),
        }
    }

    fn opts() -> RenderOptions {
        RenderOptions {
            number: None,
            depth: None,
            min_size: 0,
            apparent: false,
            width: 80,
        }
    }

    fn lines(output: &str) -> Vec<&str> {
        output.lines().filter(|l| !l.trim().is_empty()).collect()
    }

    #[test]
    fn n_limits_entries_shown() {
        let root = dir(
            "root",
            6000,
            6000,
            vec![
                file("root/a", 3000, 3000),
                file("root/b", 2000, 2000),
                file("root/c", 1000, 1000),
            ],
        );
        let out = render_tree(
            &tree(root),
            &RenderOptions {
                number: Some(2),
                ..opts()
            },
        );

        assert_eq!(lines(&out).len(), 2, "at most 2 entries, got:\n{out}");
    }

    #[test]
    fn depth_limits_display_not_totals() {
        // root > mid(dir) > deep(file). At depth 1 the deep file is hidden, but
        // mid's printed size still reflects the deep bytes it contains.
        let deep = file("root/mid/deep", 4096, 4000);
        let mid = dir("root/mid", 4096, 4000, vec![deep]);
        let root = dir("root", 4096, 4000, vec![mid]);

        let out = render_tree(
            &tree(root),
            &RenderOptions {
                depth: Some(1),
                ..opts()
            },
        );

        assert!(
            !out.contains("deep"),
            "depth-2 entry must be hidden:\n{out}"
        );
        assert!(out.contains("mid"), "depth-1 entry must be shown:\n{out}");
        // mid's full-subtree total (4096 → "4.0K") is still printed.
        assert!(
            out.contains("4.0K"),
            "the shown dir total must be the full subtree:\n{out}"
        );
    }

    #[test]
    fn min_size_hides_entries_without_changing_totals() {
        let big = file("root/big", 10_000, 10_000);
        let small = file("root/small", 100, 100);
        let root = dir("root", 10_100, 10_100, vec![big, small]);

        let out = render_tree(
            &tree(root),
            &RenderOptions {
                min_size: 1000,
                ..opts()
            },
        );

        assert!(out.contains("big"), "large entry must be shown:\n{out}");
        assert!(!out.contains("small"), "small entry must be hidden:\n{out}");
        // The root total still includes the hidden 100 bytes: 10_100 → "9.9K".
        assert!(
            out.contains("9.9K"),
            "parent total must be unchanged by the display filter:\n{out}"
        );
    }

    #[test]
    fn siblings_sorted_desc() {
        let root = dir(
            "root",
            3000,
            3000,
            vec![file("root/small", 1000, 1000), file("root/big", 2000, 2000)],
        );
        let out = render_tree(&tree(root), &opts());
        let rendered = lines(&out);

        let big = rendered.iter().position(|l| l.contains("big")).unwrap();
        let small = rendered.iter().position(|l| l.contains("small")).unwrap();
        assert!(big < small, "largest sibling first:\n{out}");
    }

    #[test]
    fn percent_is_share_of_parent() {
        // root is 100%; a is 25% of it (250/1000), b is 75%.
        let root = dir(
            "root",
            1000,
            1000,
            vec![file("root/a", 250, 250), file("root/b", 750, 750)],
        );
        let out = render_tree(&tree(root), &opts());

        let pct_of = |needle: &str| {
            out.lines()
                .find(|l| l.contains(needle))
                .unwrap()
                .trim_end()
                .rsplit(' ')
                .next()
                .unwrap()
                .to_owned()
        };
        assert_eq!(pct_of("root "), "100%", "the root is its own 100%:\n{out}");
        assert_eq!(pct_of(" a "), "25%", "a is a quarter of root:\n{out}");
        assert_eq!(pct_of(" b "), "75%", "b is three quarters of root:\n{out}");
    }

    #[test]
    fn nested_percent_is_relative_to_immediate_parent() {
        // walk is 24/80 = 30% of src, not of root.
        let walk = file("root/src/walk", 24, 24);
        let src = dir("root/src", 80, 80, vec![walk]);
        let root = dir("root", 84, 84, vec![src]);

        let out = render_tree(&tree(root), &opts());
        let walk_line = out.lines().find(|l| l.contains("walk")).unwrap();
        assert!(
            walk_line.trim_end().ends_with("30%"),
            "walk is 30% of its parent src, got:\n{walk_line}"
        );
    }

    #[test]
    fn bar_fill_is_right_aligned() {
        // Half full: 10 blocks pushed to the right, 10 leading spaces.
        let half = bar(1, 2);
        assert_eq!(half, format!("{:>20}", "█".repeat(10)));
        assert!(half.starts_with(' '), "padding is on the left: {half:?}");
        assert!(half.ends_with('█'), "blocks sit at the right: {half:?}");
    }

    #[test]
    fn bar_is_full_at_hundred_percent_and_empty_at_zero() {
        assert_eq!(bar(1, 1), "█".repeat(20));
        assert_eq!(bar(0, 100), " ".repeat(20));
    }

    #[test]
    fn zero_parent_neither_panics_nor_divides() {
        // parent == 0 must short-circuit — never `size / 0`.
        assert_eq!(percent(5, 0), 0);
        assert_eq!(bar(5, 0), " ".repeat(20));
    }

    #[test]
    fn lines_are_flush_to_the_right_edge() {
        // Every wide-path line fills exactly `width` columns, so the bar/percent
        // block lands on the right edge.
        let root = dir(
            "root",
            3000,
            3000,
            vec![file("root/a", 2000, 2000), file("root/b", 1000, 1000)],
        );
        let out = render_tree(&tree(root), &opts());
        for line in lines(&out) {
            assert_eq!(
                UnicodeWidthStr::width(line),
                80,
                "every line must fill the width:\n{line:?}"
            );
        }
    }

    #[test]
    fn long_name_is_truncated_to_keep_alignment() {
        let long = "root/".to_owned() + &"a".repeat(200);
        let root = dir("root", 1000, 1000, vec![file(&long, 1000, 1000)]);
        let out = render_tree(&tree(root), &opts());

        let child = out.lines().find(|l| l.contains('a')).unwrap();
        assert!(
            child.contains('…'),
            "an over-long name is truncated:\n{child}"
        );
        assert_eq!(
            UnicodeWidthStr::width(child),
            80,
            "truncation keeps the line at the width:\n{child}"
        );
    }

    /// East-Asian glyphs occupy two terminal columns each. Counting them as one
    /// character would push the bar/percent block past the right edge — the
    /// line would *look* longer than the terminal and wrap.
    #[test]
    fn wide_glyph_names_keep_lines_at_the_terminal_width() {
        let root = dir(
            "root",
            3000,
            3000,
            vec![
                // 12 chars, 24 columns.
                file("root/文書ファイル名前一覧表資料", 2000, 2000),
                file("root/ascii.txt", 1000, 1000),
            ],
        );
        let out = render_tree(&tree(root), &opts());
        for line in lines(&out) {
            assert_eq!(
                UnicodeWidthStr::width(line),
                80,
                "a wide-glyph line must still fill exactly the width:\n{line:?}"
            );
        }
    }

    /// The same for a name long enough to be truncated: the ellipsis must land
    /// on the width boundary measured in columns, not characters.
    #[test]
    fn wide_glyph_names_are_truncated_by_column_not_by_character() {
        let long = "root/".to_owned() + &"漢".repeat(100);
        let root = dir("root", 1000, 1000, vec![file(&long, 1000, 1000)]);
        let out = render_tree(&tree(root), &opts());

        let child = out.lines().find(|l| l.contains('漢')).unwrap();
        assert!(
            child.contains('…'),
            "an over-long name is truncated:\n{child}"
        );
        assert_eq!(
            UnicodeWidthStr::width(child),
            80,
            "truncating wide glyphs keeps the line at the width:\n{child}"
        );
    }

    /// A double-width glyph that would straddle the truncation boundary is
    /// dropped rather than half-drawn, and the gap is padded — so an odd
    /// budget still yields an exactly-width line.
    #[test]
    fn a_wide_glyph_straddling_the_boundary_is_dropped_and_padded() {
        // Budget 4 leaves 3 columns before the ellipsis: two glyphs need 4, so
        // one is dropped and a pad space takes its place.
        assert_eq!(fit("漢字漢字", 4), "漢 …");
        assert_eq!(UnicodeWidthStr::width(fit("漢字漢字", 4).as_str()), 4);
    }

    /// Zero-width combining marks add no columns, so a name carrying them is
    /// padded on its visible width rather than its character count.
    #[test]
    fn combining_marks_do_not_consume_columns() {
        // "e" + U+0301 (combining acute) renders as one column, not two.
        assert_eq!(UnicodeWidthStr::width(fit("e\u{301}", 4).as_str()), 4);
    }

    #[test]
    fn apparent_flag_switches_ranking_unit() {
        // xfile is bigger by allocated, yfile by apparent — so the order flips.
        let root = dir(
            "root",
            6144,
            9010,
            vec![file("root/xfile", 4096, 10), file("root/yfile", 2048, 9000)],
        );

        let rank = |apparent: bool, node: ScanNode| {
            let out = render_tree(&tree(node), &RenderOptions { apparent, ..opts() });
            let rows = lines(&out);
            let x = rows.iter().position(|l| l.contains("xfile")).unwrap();
            let y = rows.iter().position(|l| l.contains("yfile")).unwrap();
            (x, y, out)
        };

        let (x_alloc, y_alloc, alloc_out) = rank(false, root.clone());
        assert!(x_alloc < y_alloc, "by allocated, xfile first:\n{alloc_out}");

        let (x_app, y_app, app_out) = rank(true, root);
        assert!(y_app < x_app, "by apparent, yfile first:\n{app_out}");
        // The printed size uses the chosen unit: yfile shows ~8.8K apparent.
        assert!(app_out.contains("8.8K"), "apparent size shown:\n{app_out}");
    }

    #[test]
    fn narrow_width_drops_the_bar_without_panicking() {
        let root = dir("root", 2000, 2000, vec![file("root/a", 1000, 1000)]);

        // 37 is the exact threshold (SIZE_COLS+2+2+BAR_COLS+1+PERCENT_COLS): a
        // bar needs one more column than that, so 37 still drops it.
        for width in [0, 1, 10, 20, 36, 37] {
            let out = render_tree(&tree(root.clone()), &RenderOptions { width, ..opts() });
            assert!(
                out.contains("root"),
                "root renders at width {width}:\n{out}"
            );
            assert!(
                !out.contains('█'),
                "no bar fits below the threshold at width {width}:\n{out}"
            );
        }
    }

    #[test]
    fn number_zero_produces_empty_output() {
        let root = dir("root", 1000, 1000, vec![file("root/a", 1000, 1000)]);
        let out = render_tree(
            &tree(root),
            &RenderOptions {
                number: Some(0),
                ..opts()
            },
        );
        assert!(out.is_empty(), "a zero budget prints nothing, got:\n{out}");
    }

    #[test]
    fn wide_width_keeps_the_bar() {
        let root = dir("root", 2000, 2000, vec![file("root/a", 1000, 1000)]);
        let out = render_tree(
            &tree(root),
            &RenderOptions {
                width: 80,
                ..opts()
            },
        );
        assert!(out.contains('█'), "a bar is drawn when it fits:\n{out}");
    }

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(10), "10B");
        assert_eq!(format_size(1023), "1023B");
        assert_eq!(format_size(1024), "1.0K");
        assert_eq!(format_size(524_288), "512.0K");
        assert_eq!(format_size(1_048_576), "1.0M");
        assert_eq!(format_size(1_073_741_824), "1.0G");
    }

    #[test]
    fn format_size_rolls_over_at_the_rounding_boundary() {
        // Just under 1 MiB must not print "1024.0K"; it rolls over to "1.0M".
        assert_eq!(format_size(1_048_575), "1.0M");
        assert_eq!(format_size(1_073_741_823), "1.0G");
        assert_eq!(format_size(1_100_000), "1.0M");
    }

    #[test]
    fn format_size_reaches_the_petabyte_ceiling() {
        // The unit index must stop at P; an off-by-one would index past `UNITS`.
        assert_eq!(format_size(u64::MAX), "16384.0P");
        assert_eq!(format_size(1 << 50), "1.0P");
    }

    #[test]
    fn root_shows_full_path_children_show_basename() {
        let root = dir(
            "some/nested/root",
            1000,
            1000,
            vec![file("some/nested/root/child.bin", 1000, 1000)],
        );
        let out = render_tree(&tree(root), &opts());

        assert!(
            out.contains("some/nested/root"),
            "the root prints its full path:\n{out}"
        );
        let child_line = out.lines().find(|l| l.contains("child.bin")).unwrap();
        assert!(
            !child_line.contains("some/nested/root/child.bin"),
            "a child shows only its basename:\n{child_line}"
        );
    }

    #[test]
    fn empty_root_is_still_a_full_hundred_percent() {
        let root = dir("root", 0, 0, vec![file("root/empty", 0, 0)]);
        let out = render_tree(&tree(root), &opts());
        assert!(out.contains("root"));
        assert!(out.contains("empty"));
        // The root is its own reference even at zero bytes — 100%, not 0%.
        let root_line = out.lines().next().unwrap();
        assert!(
            root_line.trim_end().ends_with("100%"),
            "an empty root is still 100%:\n{root_line}"
        );
    }
}
