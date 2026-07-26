//! `disk-tools ui` — a terminal file manager over the same core.
//!
//! Task 1 gave it a frame that always closes; Task 2 puts a directory in it.
//! Directory sizes, rule colours and the rule editor arrive after.
//!
//! All the thinking lives in [`app`], [`listing`] and [`sort`], as plain
//! functions over values — CI has no terminal, so anything only a screen could
//! check would be untested. What is left here is drawing and key dispatch.

mod app;
mod listing;
mod sort;
mod term;

use crate::render::tree::format_size;
use app::App;
use listing::Entry;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use sort::Order;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::time::Duration;
use term::{Crossterm, Screen};

/// Run the browser at `root` until the user asks to leave.
///
/// The caller has already checked that `root` exists and that stdout is a
/// terminal — both refusals belong *outside* the alternate screen, or their
/// message would be printed onto a screen that is about to be torn down.
pub fn run(root: &Path) -> io::Result<()> {
    let mut app = App::open(root)?;

    term::install_panic_hook();
    let mut screen = Screen::enter(Crossterm)?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    loop {
        terminal.draw(|frame| draw(frame, &app))?;

        // A timeout rather than a blocking read: without one, a resize or a
        // future background message could not reach the loop while it waits.
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && !handle(&mut app, key.code)
        {
            break;
        }
    }

    // Explicit rather than left to `Drop`, so a failure to restore is *reported*
    // rather than swallowed. `Drop` still runs, and does nothing the second time.
    screen.leave()
}

/// Apply one key. Returns whether to keep going.
///
/// Separated from the loop so the bindings are one readable table rather than
/// something to reconstruct from a match buried in I/O.
fn handle(app: &mut App, code: KeyCode) -> bool {
    match code {
        KeyCode::Char('q') => return false,

        KeyCode::Down | KeyCode::Char('j') => app.move_down(),
        KeyCode::Up | KeyCode::Char('k') => app.move_up(),
        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => app.enter(),
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => app.leave(),

        KeyCode::Char('n') => app.sort_by(Order::Name),
        KeyCode::Char('s') => app.sort_by(Order::Size),
        KeyCode::Char('c') => app.sort_by(Order::Created),
        KeyCode::Char('m') => app.sort_by(Order::Modified),

        _ => {}
    }
    true
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(frame.area());

    frame.render_widget(Paragraph::new(header(app)), layout[0]);

    let rows: Vec<ListItem<'_>> = app
        .entries()
        .iter()
        .map(|entry| ListItem::new(row(entry)))
        .collect();
    let list = List::new(rows)
        .block(Block::default().borders(Borders::ALL))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(app.cursor()));
    frame.render_stateful_widget(list, layout[1], &mut state);

    frame.render_widget(
        Paragraph::new("q quit  ↵ enter  ← up  n/s/c/m sort (again to reverse)")
            .style(Style::default().add_modifier(Modifier::DIM)),
        layout[2],
    );
}

/// Path, order, and anything that went wrong.
///
/// The notice takes the line when there is one: a message about a directory that
/// would not open matters more than a reminder of the sort order.
fn header(app: &App) -> String {
    if let Some(notice) = app.notice() {
        return format!("{}  —  {notice}", app.cwd().display());
    }

    let applied = app.applied();
    let arrow = if app.reverse() { "↓" } else { "↑" };
    let mut header = format!(
        "{}  [{}{arrow}]",
        app.cwd().display(),
        applied.order.label()
    );
    if applied.fell_back {
        // Saying which order is in force is not enough when it is not the one
        // that was asked for.
        header.push_str("  (no creation times here)");
    }
    header
}

/// One row: size, then name. Directories carry no size until they are measured.
fn row(entry: &Entry) -> String {
    let size = match entry.size {
        Some(bytes) => format_size(bytes),
        None => String::new(),
    };
    let name = entry.name.to_string_lossy();
    let mark = if entry.is_dir { "/" } else { "" };
    format!("{size:>8}  {name}{mark}")
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
    use ratatui::backend::TestBackend;

    /// Draw into a fake terminal of a known size and read the text back.
    ///
    /// `ratatui`'s own answer to "CI has no terminal", and better than the
    /// alternative this task started with: a real pty from `script` reports a
    /// size of 0x0, so nothing renders and layout cannot be seen at all.
    fn painted(root: &Path, width: u16, height: u16) -> Vec<String> {
        let app = App::open(root).expect("open");
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("draw");

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

    #[test]
    fn the_screen_shows_a_header_a_list_and_the_keys() {
        let dir = fixture();

        let lines = painted(dir.path(), 70, 10);

        assert!(
            lines[0].contains("[name↑]"),
            "header names the order: {lines:?}"
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

    /// A directory has no size until it is measured, and an empty column says
    /// that better than a `0B` would.
    #[test]
    fn a_directory_carries_no_size_yet() {
        let dir = fixture();

        let lines = painted(dir.path(), 70, 10);
        let row = lines
            .iter()
            .find(|line| line.contains("sub/"))
            .expect("the directory is listed");

        assert!(
            !row.contains('B') && !row.contains('K'),
            "no size on a directory row: {row:?}"
        );
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
