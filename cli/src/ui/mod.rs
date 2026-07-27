//! `disk-tools ui` — a terminal file manager over the same core.
//!
//! Task 1 gave it a frame that always closes, Task 2 put a directory in it, and
//! Task 3 made the directories measure themselves. Rule colours and the rule
//! editor arrive after.
//!
//! All the thinking lives in [`app`], [`listing`], [`sort`], [`layout`] and
//! [`measure`], as plain functions over values — CI has no terminal, so anything
//! only a screen could check would be untested. What is left here is drawing and
//! key dispatch.

mod app;
mod edit;
mod layout;
mod listing;
mod measure;
mod sort;
mod term;

use crate::args::Reload;
use app::App;
use disk_tools_core::{Rules, State};
use edit::{Dialog, Field, Guide};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use sort::Order;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::time::{Duration, SystemTime};
use term::{Crossterm, Screen};

/// Run the browser at `root` until the user asks to leave.
///
/// The caller has already checked that `root` exists and that stdout is a
/// terminal — both refusals belong *outside* the alternate screen, or their
/// message would be printed onto a screen that is about to be torn down.
pub fn run(root: &Path, rules: Rules, reload: Reload, now: SystemTime) -> io::Result<()> {
    let mut app = App::open(root, rules, now, reload.user_dirs.clone())?;

    term::install_panic_hook();
    let mut screen = Screen::enter(Crossterm)?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    loop {
        // Read once per frame rather than held on the `App`: ages are relative,
        // so a browser left open overnight would otherwise still say "2m".
        let now = SystemTime::now();
        // The list band: everything but the path, the header and the key line.
        app.set_page((terminal.size()?.height as usize).saturating_sub(3));
        app.absorb_sizes();
        terminal.draw(|frame| draw(frame, &app, now))?;

        // A timeout rather than a blocking read: without one, a resize or a
        // future background message could not reach the loop while it waits.
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && !handle(&mut app, key.code, &reload)
        {
            break;
        }
    }

    // Before the screen is handed back, so the process is not still walking a
    // tree while the user has their prompt again. `Drop` would do it too; doing
    // it here means it happens in a knowable order.
    app.stop_sizing();

    // Explicit rather than left to `Drop`, so a failure to restore is *reported*
    // rather than swallowed. `Drop` still runs, and does nothing the second time.
    screen.leave()
}

/// Apply one key. Returns whether to keep going.
///
/// Separated from the loop so the bindings are one readable table rather than
/// something to reconstruct from a match buried in I/O.
fn handle(app: &mut App, code: KeyCode, reload: &Reload) -> bool {
    // A dialog takes every key. Leaving the browser's bindings live underneath
    // would make `q` quit from inside a half-typed rule.
    if app.dialog().is_some() {
        dialog(app, code);
        return true;
    }

    // While a filter is being typed, letters are letters. Anything else would
    // mean a directory called `q` could not be searched for.
    if app.is_filtering() {
        filtering(app, code);
        return true;
    }

    match code {
        KeyCode::Char('q') => return false,

        KeyCode::Down | KeyCode::Char('j') => app.move_down(),
        KeyCode::Up | KeyCode::Char('k') => app.move_up(),
        KeyCode::PageDown => app.page_down(),
        KeyCode::PageUp => app.page_up(),
        KeyCode::Home => app.jump_to_top(),
        KeyCode::End => app.jump_to_bottom(),

        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => app.enter(),
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => app.leave(),

        KeyCode::Char('/') => app.start_filtering(),
        // The rule dialog: what does this program think of this path, and what
        // would make it think otherwise.
        KeyCode::Char('a') => app.open_rules(),
        // The same key whether the filter is being typed or merely in force.
        KeyCode::Esc => app.filter_clear(),

        // Sizes are kept for the session, so this is the only thing that makes
        // one stale on purpose.
        KeyCode::Char('r') => app.remeasure(),
        // Editing the config and restarting to see the effect is the loop this
        // removes. Only the rules are re-read; the listing has not changed.
        KeyCode::Char('R') => reread(app, reload),

        KeyCode::Char('n') => app.sort_by(Order::Name),
        KeyCode::Char('s') => app.sort_by(Order::Size),
        KeyCode::Char('c') => app.sort_by(Order::Created),
        KeyCode::Char('m') => app.sort_by(Order::Modified),

        _ => {}
    }
    true
}

/// Keys while the rule dialog is open.
///
/// `Esc` always closes without writing anything, at either step — a dialog you
/// cannot leave by the obvious key is a dialog people learn to fear.
fn dialog(app: &mut App, code: KeyCode) {
    match app.dialog() {
        Some(Dialog::Choosing(_)) => match code {
            KeyCode::Esc => app.close_dialog(),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => app.open_form(),
            KeyCode::Down | KeyCode::Char('j') => app.choose_down(),
            KeyCode::Up | KeyCode::Char('k') => app.choose_up(),
            _ => {}
        },
        Some(Dialog::Editing(_)) => match code {
            KeyCode::Esc => app.close_dialog(),
            KeyCode::Enter => app.confirm_form(),
            KeyCode::Tab | KeyCode::Down => app.form_next(),
            KeyCode::BackTab | KeyCode::Up => app.form_previous(),
            // The one key both choice fields answer to. Left and right because
            // a two-valued field is not a list to scroll.
            KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => app.form_toggle(),
            KeyCode::Backspace => app.form_pop(),
            KeyCode::Char(ch) => app.form_push(ch),
            _ => {}
        },
        None => {}
    }
}

/// Keys while a filter is being typed.
///
/// `Enter` keeps the filter and hands the keys back to navigation; `Esc` throws
/// it away. Arrows still move, so a match can be picked without stopping typing.
fn filtering(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char(ch) => app.filter_push(ch),
        KeyCode::Backspace => app.filter_pop(),
        KeyCode::Enter => app.filter_accept(),
        KeyCode::Esc => app.filter_clear(),

        KeyCode::Down => app.move_down(),
        KeyCode::Up => app.move_up(),
        KeyCode::PageDown => app.page_down(),
        KeyCode::PageUp => app.page_up(),

        _ => {}
    }
}

/// Four bands: where we are, the column labels, the rows, the keys.
///
/// The labels are their own line — the first version of this screen carried the
/// sort order as `[name↑]` on the path line, and it was not findable. Columns
/// are borderless so the labels sit directly over their cells; a box would inset
/// the rows by one and nothing would line up.
fn draw(frame: &mut Frame<'_>, app: &App, now: SystemTime) {
    // The legend costs a row, so it is only there when the colours it explains
    // are. A directory no rule reaches has nothing to explain.
    let legend_rows = u16::from(app.any_rule_applies());
    let bands = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(legend_rows),
        Constraint::Length(1),
    ])
    .split(frame.area());

    frame.render_widget(Paragraph::new(where_we_are(app)), bands[0]);

    let applied = app.applied();
    let cols = layout::columns(bands[2].width as usize);
    frame.render_widget(
        Paragraph::new(layout::header(&cols, applied.order, app.reverse()))
            .style(Style::default().add_modifier(Modifier::REVERSED)),
        bands[1],
    );

    // The denominator for every percentage on screen: what is known right now.
    // A directory still being walked is excluded, so the figure does not move
    // under the rows that are already settled.
    let sized: u64 = app
        .entries()
        .iter()
        .filter(|entry| !entry.measuring)
        .filter_map(|entry| entry.size)
        .sum();

    let rows_visible = bands[2].height as usize;
    let rows: Vec<ListItem<'_>> = app
        .entries()
        .iter()
        .map(|entry| {
            ListItem::new(layout::row(entry, now, sized, &cols)).style(colour(entry.state))
        })
        .collect();
    if legend_rows > 0 {
        frame.render_widget(Line::from(legend()), bands[3]);
    }

    let list = List::new(rows)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        // Keep the cursor away from the edges: scrolling with it pinned to the
        // last row shows only where you have been, never where you are going.
        .scroll_padding(rows_visible / 2);
    let mut state = ListState::default().with_selected(Some(app.cursor()));
    frame.render_stateful_widget(list, bands[2], &mut state);

    frame.render_widget(
        Paragraph::new(keys(app)).style(Style::default().add_modifier(Modifier::DIM)),
        bands[4],
    );

    // Last, and over everything: a dialog that shared the screen with the
    // listing would leave the user reading two things at once, one of which is
    // no longer taking keys.
    if let Some(dialog) = app.dialog() {
        draw_dialog(frame, dialog);
    }
}

/// The rule dialog, centred over the listing.
fn draw_dialog(frame: &mut Frame<'_>, dialog: &Dialog) {
    let (title, lines) = match dialog {
        Dialog::Choosing(chooser) => (
            format!("rules for {}", chooser.name),
            chooser
                .rows()
                .into_iter()
                .enumerate()
                .map(|(at, row)| marked(at == chooser.cursor(), row))
                .collect::<Vec<Line<'static>>>(),
        ),
        Dialog::Editing(form) => {
            let mut lines: Vec<Line<'static>> = Field::ALL
                .iter()
                .map(|field| {
                    let mut line = marked(
                        *field == form.focus(),
                        format!("{:>19}  {}", field.label(), form.value(*field)),
                    );
                    if form.is_wrong(*field) {
                        // Flagged in its own right, not only by the line below:
                        // on a form this long, a sentence at the bottom is a
                        // long way from the field it is about.
                        line = line.patch_style(Style::default().fg(Color::Red));
                    }
                    line
                })
                .collect();

            lines.push(Line::from(""));
            lines.push(match form.guide() {
                Guide::Wrong(why) => {
                    Line::styled(format!("  {why}"), Style::default().fg(Color::Red))
                }
                // What the value comes to, so `10M` can be seen to mean what was
                // meant before Enter is pressed.
                Guide::Reading(reading) => {
                    Line::styled(format!("  = {reading}"), Style::default().fg(Color::Green))
                }
                // A field whose syntax is only in the README is a field that
                // gets typed wrong.
                Guide::Hint(hint) => Line::styled(
                    format!("  {hint}"),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            });
            lines.push(Line::styled(
                if form.focus().is_choice() {
                    "  space change  ↵ confirm  esc cancel"
                } else {
                    "  tab next  ↵ confirm  esc cancel"
                },
                Style::default().add_modifier(Modifier::DIM),
            ));

            (
                if form.is_edit() {
                    format!("edit rule `{}`", form.value(Field::Name))
                } else {
                    "new rule".to_owned()
                },
                lines,
            )
        }
    };

    let area = centred(frame.area(), 70, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title)),
        area,
    );
}

/// A row with a cursor marker, so the selection survives a terminal with no
/// colour and a reader who is not looking for one.
fn marked(focused: bool, text: String) -> Line<'static> {
    let line = Line::from(format!("{} {text}", if focused { ">" } else { " " }));
    if focused {
        line.patch_style(Style::default().add_modifier(Modifier::BOLD))
    } else {
        line
    }
}

/// A box of at most `width` x `height`, centred — and never larger than what it
/// is centred in, since a dialog wider than the terminal shows its left edge and
/// nothing else.
fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Read the config again and repaint against it.
///
/// A bad file leaves the rules that were working in place and says why. Dropping
/// them would mean a typo silently turns every colour off, which looks exactly
/// like "my rules stopped matching" — the one thing the user is here to
/// diagnose.
fn reread(app: &mut App, reload: &Reload) {
    match crate::config::load(reload.path.as_deref(), &reload.user_dirs, None)
        .map_err(|err| err.to_string())
        .and_then(|config| {
            Rules::new(config.rules, &reload.user_dirs).map_err(|err| err.to_string())
        }) {
        Ok(rules) => {
            app.reload_rules(rules);
            app.say(match &reload.path {
                Some(path) => format!("reloaded {}", path.display()),
                None => "reloaded the built-in rules".to_owned(),
            });
        }
        Err(problem) => app.say(format!("config unchanged — {problem}")),
    }
}

/// What each rule state looks like.
///
/// `Untracked` keeps the terminal's own foreground rather than taking a colour
/// of its own: most rows are untracked, and colouring those too would leave the
/// few that matter with nothing to stand out against.
fn colour(state: State) -> Style {
    match state {
        State::Untracked => Style::default(),
        State::InScope => Style::default().fg(Color::Blue),
        State::Included => Style::default().fg(Color::Yellow),
        State::Excluded => Style::default().fg(Color::Green),
    }
}

/// The colours, named.
///
/// Every state carries its word as well as its colour — a legend of four
/// swatches is no legend at all to a reader who cannot tell them apart.
fn legend() -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        "rules: ",
        Style::default().add_modifier(Modifier::DIM),
    )];
    for state in [
        State::Included,
        State::Excluded,
        State::InScope,
        State::Untracked,
    ] {
        spans.push(Span::styled(format!("{}  ", state.label()), colour(state)));
    }
    spans
}

/// What the keys do — different while a filter is being typed, because most of
/// them are then just letters.
fn keys(app: &App) -> &'static str {
    if app.is_filtering() {
        "esc cancel  ↵ keep  ↑↓ move"
    } else {
        "q quit  ↵ enter  ← up  / filter  a rules  n/s/c/m sort  r sizes  R config"
    }
}

/// The path, and anything that went wrong.
///
/// The notice takes the line when there is one: a message about a directory that
/// would not open matters more than knowing where you are, which the previous
/// frame already said.
fn where_we_are(app: &App) -> String {
    if let Some(notice) = app.notice() {
        return format!("{}  —  {notice}", app.cwd().display());
    }

    let mut line = app.cwd().display().to_string();
    // The filter changes what the whole screen means, so it is said next to
    // where — a list of four things in a directory of four hundred is otherwise
    // indistinguishable from a directory of four.
    if app.is_filtering() {
        line.push_str(&format!("  /{}\u{2588}", app.filter()));
    } else if !app.filter().is_empty() {
        line.push_str(&format!("  /{}  (esc clears)", app.filter()));
    }
    if app.applied().fell_back {
        // The arrow in the header says which order is in force. It cannot say
        // that it is not the one that was asked for.
        line.push_str("  (no creation times here)");
    }
    line
}

/// Why `ui` will not start.
///
/// Both are decided before the terminal is touched: a message printed after
/// entering the alternate screen is erased by leaving it.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The path named does not exist, or cannot be probed.
    Unusable(String),
    /// stdout is not a terminal.
    NotATerminal,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Unusable(problem) => write!(f, "{problem}"),
            Refusal::NotATerminal => write!(
                f,
                "ui needs a terminal; stdout is a pipe or a file.\n\
                 For something you can redirect, use `scan` or `clean`."
            ),
        }
    }
}

/// Everything that has to hold before the screen is entered.
///
/// Split from [`run`] so it can be tested where there is no terminal: whether
/// stdout is one is passed in rather than read, the way `cli::env` already keeps
/// environment lookups out of the logic they feed.
pub fn check(root: &Path, stdout_is_terminal: bool) -> Result<(), Refusal> {
    if let Err(problem) = crate::args::validate_root(root) {
        return Err(Refusal::Unusable(problem));
    }
    if !stdout_is_terminal {
        // Escape sequences in a pipe produce something no reader can use and no
        // author intended.
        return Err(Refusal::NotATerminal);
    }
    Ok(())
}

/// The real check, for `main`.
pub fn stdout_is_terminal() -> bool {
    io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_tools_core::UserDirs;
    use ratatui::backend::TestBackend;

    /// Draw into a fake terminal of a known size and read the text back.
    ///
    /// `ratatui`'s own answer to "CI has no terminal", and better than the
    /// alternative this task started with: a real pty from `script` reports a
    /// size of 0x0, so nothing renders and layout cannot be seen at all.
    fn painted(root: &Path, width: u16, height: u16) -> Vec<String> {
        paint(
            App::open(root, Rules::default(), now(), UserDirs::default()).expect("open"),
            width,
            height,
        )
    }

    /// The same, once every background walk has posted its total.
    ///
    /// Bounded, so a worker that never finishes fails rather than hangs.
    fn painted_settled(root: &Path, width: u16, height: u16) -> Vec<String> {
        let mut app = App::open(root, Rules::default(), now(), UserDirs::default()).expect("open");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            app.absorb_sizes();
            if app.entries().iter().all(|entry| !entry.measuring) {
                return paint(app, width, height);
            }
            assert!(std::time::Instant::now() < deadline, "sizes never settled");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn paint(app: App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &app, SystemTime::now()))
            .expect("draw");

        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_750_000_000)
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("big.bin"), vec![b'x'; 40_960]).expect("write");
        dir
    }

    /// The three bands, and what each carries.
    /// Diagnostic, not an assertion: prints the screen so it can be looked at.
    ///
    /// The alternative is a real terminal, and there is none in CI — nor in the
    /// pseudo-terminal `script` provides, which reports a size of 0x0 and
    /// renders nothing. Ignored by default because it asserts nothing.
    ///
    /// ```text
    /// cargo test -p disk-tools --bin disk-tools ui::tests::show -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "diagnostic: prints the screen, asserts nothing"]
    fn show() {
        for line in painted(Path::new("."), 76, 14) {
            println!("|{line}");
        }
    }

    /// The rule form, printed. Ignored for the same reason as `show`.
    #[test]
    #[ignore = "diagnostic: prints the screen, asserts nothing"]
    fn show_form() {
        let mut app =
            App::open(Path::new("."), Rules::default(), now(), UserDirs::default()).expect("open");
        point_at_first_directory(&mut app);
        app.open_rules();
        app.open_form();
        // Something in a measured field, so the reading line has work to do.
        for _ in 0..6 {
            app.form_next();
        }
        for ch in "10M".chars() {
            app.form_push(ch);
        }

        for line in paint(app, 76, 22) {
            println!("|{line}");
        }
    }

    #[test]
    fn the_screen_shows_a_path_a_header_a_list_and_the_keys() {
        let dir = fixture();

        let lines = painted(dir.path(), 70, 10);

        assert!(
            lines[0].contains(&dir.path().display().to_string()),
            "the first line says where we are: {lines:?}"
        );
        assert!(
            lines[1].contains("size") && lines[1].contains("name↑") && lines[1].contains("total"),
            "the second labels the columns and marks the sorted one: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("sub/")),
            "a directory is marked as one: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("40.0K") && line.contains("big.bin")),
            "a file carries its size, formatted as `scan` formats it: {lines:?}"
        );
        assert!(
            lines.last().is_some_and(|line| line.contains("q quit")),
            "the keys are on screen rather than in a manual: {lines:?}"
        );
    }

    /// Sizing runs in the background, so the first frame after opening shows a
    /// spinner rather than a number that is not known yet.
    #[test]
    fn a_directory_being_measured_shows_a_spinner() {
        let dir = fixture();

        let lines = painted(dir.path(), 70, 10);
        let row = lines
            .iter()
            .find(|line| line.contains("sub/"))
            .expect("the directory is listed");

        assert!(
            row.chars()
                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)),
            "no spinner on a directory being walked: {row:?}"
        );
    }

    /// And once the walk finishes, the same row carries a size and its share of
    /// what is on screen.
    #[test]
    fn a_measured_directory_shows_a_size_and_a_share() {
        let dir = fixture();
        std::fs::write(dir.path().join("sub/inner.bin"), vec![b'x'; 8192]).expect("write");

        let lines = painted_settled(dir.path(), 70, 10);
        let row = lines
            .iter()
            .find(|line| line.contains("sub/"))
            .expect("the directory is listed");

        assert!(row.contains('K'), "a size in the first column: {row:?}");
        assert!(row.contains('%'), "and a share in the last: {row:?}");
        assert!(
            !row.chars()
                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)),
            "the spinner is gone: {row:?}"
        );
    }

    /// Percentages are against the sum of the rows on screen, so they add up to
    /// a hundred once nothing is left to measure.
    #[test]
    fn the_shares_on_a_settled_screen_add_up() {
        let dir = fixture();
        std::fs::write(dir.path().join("sub/inner.bin"), vec![b'x'; 8192]).expect("write");

        let lines = painted_settled(dir.path(), 70, 12);
        let total: u32 = lines
            .iter()
            .filter_map(|line| line.rsplit_once('\u{2502}'))
            // The cell is a bar and then a number, so take the digits off the
            // end rather than trying to parse past the blocks.
            .filter_map(|(_, cell)| cell.trim().strip_suffix('%'))
            .filter_map(|cell| {
                cell.chars()
                    .rev()
                    .take_while(char::is_ascii_digit)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<String>()
                    .parse::<u32>()
                    .ok()
            })
            .sum();

        // Each share is rounded to a whole percent, so the sum lands near 100
        // rather than on it.
        assert!((98..=102).contains(&total), "{total}% across {lines:?}");
    }

    /// A label that does not sit over its own cells is a legend, not a header —
    /// and the reason this screen was rebuilt.
    #[test]
    fn the_labels_sit_over_the_cells_they_name() {
        let dir = fixture();

        let lines = painted(dir.path(), 70, 10);
        // By character: `↑` is three bytes, so byte offsets would differ between
        // the header and a row that lines up perfectly on screen.
        let bars = |line: &str| {
            line.chars()
                .enumerate()
                .filter(|(_, ch)| *ch == '│')
                .map(|(at, _)| at)
                .collect::<Vec<_>>()
        };

        let header = bars(&lines[1]);
        assert!(!header.is_empty(), "{lines:?}");
        for row in lines.iter().skip(2).take_while(|line| !line.is_empty()) {
            assert_eq!(bars(row), header, "{row:?} against {:?}", lines[1]);
        }
    }

    /// The parent is a row like any other, so it is visibly there to be entered.
    #[test]
    fn the_parent_is_on_screen_when_there_is_one() {
        let dir = fixture();

        let lines = painted(dir.path(), 70, 10);

        assert!(lines.iter().any(|line| line.contains("../")), "{lines:?}");
    }

    /// The one thing a fixed-width screen must never do.
    #[test]
    fn a_narrow_terminal_does_not_panic() {
        let dir = fixture();

        for width in [1, 4, 12, 30] {
            let lines = painted(dir.path(), width, 6);
            assert_eq!(lines.len(), 6, "width {width}");
        }
    }

    #[test]
    fn a_short_terminal_does_not_panic() {
        let dir = fixture();

        for height in [1, 2, 3] {
            painted(dir.path(), 40, height);
        }
    }

    /// A filter changes what the whole screen means: four rows in a directory
    /// of four hundred must not look like a directory of four.
    #[test]
    fn the_filter_is_on_screen_while_it_is_being_typed() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        app.start_filtering();
        app.filter_push('s');

        let lines = paint(app, 70, 10);

        assert!(lines[0].contains("/s"), "{:?}", lines[0]);
        assert!(
            lines.last().is_some_and(|line| line.contains("esc cancel")),
            "and the keys are the filtering ones: {:?}",
            lines.last()
        );
    }

    /// Filtering without typing still has to say so, or the missing rows look
    /// like an empty directory.
    #[test]
    fn an_accepted_filter_says_how_to_get_rid_of_it() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        app.start_filtering();
        app.filter_push('s');
        app.filter_accept();

        let lines = paint(app, 70, 10);

        assert!(lines[0].contains("/s"), "{:?}", lines[0]);
        assert!(lines[0].contains("esc"), "{:?}", lines[0]);
    }

    /// Colour alone excludes anyone who cannot distinguish it, so the states
    /// are named on screen.
    #[test]
    fn the_legend_names_every_state_when_a_rule_applies() {
        let dir = fixture();
        let rules = Rules::new(
            vec![disk_tools_core::Rule {
                name: "junk".into(),
                root: Some(dir.path().to_string_lossy().into_owned()),
                includes: vec!["**/sub/".into()],
                ..disk_tools_core::Rule::default()
            }],
            &disk_tools_core::UserDirs::default(),
        )
        .expect("compiles");

        let lines = paint(
            App::open(dir.path(), rules, now(), UserDirs::default()).expect("open"),
            78,
            10,
        );
        let legend = lines
            .iter()
            .find(|line| line.contains("rules:"))
            .expect("a legend is on screen");

        for word in ["included", "excluded", "in scope", "untracked"] {
            assert!(legend.contains(word), "{word} missing from {legend:?}");
        }
    }

    /// And a row is not spent explaining colours that are not on screen.
    #[test]
    fn no_rule_here_means_no_legend_row() {
        let dir = fixture();

        let lines = painted(dir.path(), 70, 10);

        assert!(
            !lines.iter().any(|line| line.contains("rules:")),
            "{lines:?}"
        );
    }

    /// The dialog is over the listing, not beside it: two things to read, one
    /// of which is no longer taking keys, is worse than one.
    #[test]
    fn the_form_is_drawn_over_the_listing() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        // Opening lands on `..`, which is not a thing in this listing and so has
        // no rule to write about it.
        point_at_first_directory(&mut app);
        app.open_rules();
        app.open_form();

        let lines = paint(app, 78, 20);

        assert!(
            lines.iter().any(|line| line.contains("new rule")),
            "{lines:?}"
        );
        for label in ["name", "root", "includes", "tier", "enabled"] {
            assert!(
                lines.iter().any(|line| line.contains(label)),
                "{label} missing from {lines:?}"
            );
        }
        assert!(
            lines.iter().any(|line| line.contains("esc cancel")),
            "and the way out is on screen: {lines:?}"
        );
    }

    /// A flagged field says so where the field is. On a nine-field form a
    /// sentence at the bottom is a long way from what it is about.
    #[test]
    fn a_rejected_field_is_marked_and_explained() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        point_at_first_directory(&mut app);
        app.open_rules();
        app.open_form();
        for _ in 0..20 {
            app.form_pop();
        }
        app.confirm_form();

        let lines = paint(app, 78, 20);

        assert!(
            lines.iter().any(|line| line.contains("needs a name")),
            "{lines:?}"
        );
    }

    /// A terminal smaller than the dialog shows its left edge and nothing else,
    /// unless the dialog is clamped.
    #[test]
    fn a_dialog_larger_than_the_terminal_does_not_overflow_it() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        // Opening lands on `..`, which is not a thing in this listing and so has
        // no rule to write about it.
        point_at_first_directory(&mut app);
        app.open_rules();
        app.open_form();

        let lines = paint(app, 20, 6);

        assert_eq!(lines.len(), 6);
        assert!(lines.iter().all(|line| line.chars().count() <= 20));
    }

    fn point_at_first_directory(app: &mut App) {
        while app
            .selected()
            .is_some_and(|entry| !entry.is_dir || entry.name == "..")
        {
            app.move_down();
        }
    }

    #[test]
    fn a_usable_directory_on_a_terminal_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert_eq!(check(dir.path(), true), Ok(()));
    }

    /// Named and absent is a typo, and it has to be said before the screen
    /// opens — afterwards, leaving the alternate screen erases it.
    #[test]
    fn a_path_that_is_not_there_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope");

        let refusal = check(&missing, true).expect_err("must refuse");

        assert!(
            matches!(&refusal, Refusal::Unusable(problem) if problem.contains("nope")),
            "{refusal:?}"
        );
    }

    /// A pipe gets a sentence, not escape sequences.
    #[test]
    fn a_pipe_is_refused_and_pointed_somewhere_useful() {
        let dir = tempfile::tempdir().expect("tempdir");

        let refusal = check(dir.path(), false).expect_err("must refuse");

        assert_eq!(refusal, Refusal::NotATerminal);
        let message = refusal.to_string();
        assert!(
            message.contains("scan") && message.contains("clean"),
            "{message}"
        );
        assert!(
            !message.contains('\x1b'),
            "the refusal itself must not emit escapes: {message:?}"
        );
    }

    /// The missing path is checked **first**: on a pipe with a bad path, the
    /// path is the more useful thing to hear about.
    #[test]
    fn an_unusable_path_is_reported_ahead_of_the_pipe() {
        let dir = tempfile::tempdir().expect("tempdir");

        let refusal = check(&dir.path().join("nope"), false).expect_err("must refuse");

        assert!(matches!(refusal, Refusal::Unusable(_)), "{refusal:?}");
    }
}
