//! `disk-tools ui` — a terminal file manager over the same core.
//!
//! v0.4 Task 1 builds only the frame: the screen opens on a directory, says how
//! to leave, and leaves cleanly however it is ended. Listing, sizes, rule
//! colours and the rule editor arrive in the tasks after it.
//!
//! Deliberately dull, because the only thing this stage has to prove is that a
//! user can get back out — see [`term`] for why that is the hard part.

mod term;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
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
    term::install_panic_hook();
    let mut screen = Screen::enter(Crossterm)?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    loop {
        terminal.draw(|frame| draw(frame, root))?;

        // A timeout rather than a blocking read: without one, a resize or a
        // future background message could not reach the loop while it waits.
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && matches!(key.code, KeyCode::Char('q'))
        {
            break;
        }
    }

    // Explicit rather than left to `Drop`, so a failure to restore is *reported*
    // rather than swallowed. `Drop` still runs, and does nothing the second time.
    screen.leave()
}

fn draw(frame: &mut Frame<'_>, root: &Path) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", root.display()));
    let body = Paragraph::new("q — quit").block(block);
    frame.render_widget(body, frame.area());
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
