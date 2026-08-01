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
mod layout;
mod listing;
mod measure;
mod removal;
mod sort;
mod term;

use crate::args::Reload;
use crate::render::tree::format_size;
use app::App;
use disk_tools_core::{Rules, State};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
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
        // The list band: everything but the path, the header, the current
        // directory and the key line.
        app.set_page((terminal.size()?.height as usize).saturating_sub(4));
        app.absorb_sizes();
        app.settle_removal();
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
    // A removal takes every key while it is on screen. Leaving the browser's
    // bindings live underneath would let `q` quit out of a half-typed
    // confirmation, and `j` move the cursor off the row being asked about.
    if app.removal().is_some() {
        removing(app, code);
        return true;
    }

    // Blocked: two keys, and both of them are ways out. Everything else would
    // act on rules the tool no longer has.
    if app.blocked().is_some() {
        match code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('R') => reread(app, reload),
            _ => {}
        }
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
        // Remove what the rules claim under this row. Capital, like `R`: the
        // keys that do something out of the ordinary are the shifted ones, and
        // `Backspace` — the obvious guess — already means "up one level", which
        // is the last thing a destructive key should share a finger with.
        KeyCode::Char('D') => app.begin_removal(),
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

/// Keys while a removal is on screen.
///
/// `Esc` always abandons it, at every stage — including while the plan is still
/// being walked, where it costs nothing because nothing has happened yet.
fn removing(app: &mut App, code: KeyCode) {
    use removal::Removal;

    match app.removal() {
        Some(Removal::Asking { destroys, .. }) => {
            let destroys = *destroys;
            match code {
                KeyCode::Esc => app.dismiss_removal(),
                KeyCode::Enter => app.confirm_removal(),
                // The gentle case takes the letter as agreement. The destroying
                // one takes only the word, so `y` there is just a letter of it.
                KeyCode::Char('y') if !destroys => app.confirm_removal(),
                KeyCode::Backspace => app.removal_pop(),
                KeyCode::Char(ch) => app.removal_push(ch),
                _ => {}
            }
        }
        // Planning, done, or nothing to do: one key out, and no key in.
        Some(_) => {
            if matches!(code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                app.dismiss_removal();
            }
        }
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

/// Five bands: where we are, the column labels, the current directory, the rows,
/// the keys — plus the legend when there is something to explain.
///
/// The labels are their own line — the first version of this screen carried the
/// sort order as `[name↑]` on the path line, and it was not findable. Columns
/// are borderless so the labels sit directly over their cells; a box would inset
/// the rows by one and nothing would line up.
///
/// The current directory gets a row of its own, in the same columns, because
/// everything on this screen was about its contents and nothing was about it.
/// `..` is the way out of here, not a description of here.
fn draw(frame: &mut Frame<'_>, app: &App, now: SystemTime) {
    // A removal is drawn over everything, because everything under it is frozen
    // and a screen that looked live would invite keys that go nowhere.
    if let Some(pending) = app.blocked().is_none().then(|| app.removal()).flatten() {
        draw_removal(frame, pending);
        return;
    }

    // Blocked: the listing is not drawn at all. Leaving it under the message
    // would leave a screenful of colours standing as an answer, and they are
    // answers from rules the tool no longer has.
    if let Some(why) = app.blocked() {
        draw_blocked(frame, why);
        return;
    }

    // The legend costs a row, so it is only there when the colours it explains
    // are. A directory no rule reaches has nothing to explain.
    let legend_rows = u16::from(app.any_rule_applies());
    let bands = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(legend_rows),
        Constraint::Length(1),
    ])
    .split(frame.area());

    frame.render_widget(Paragraph::new(where_we_are(app)), bands[0]);

    let applied = app.applied();
    let cols = layout::columns(bands[3].width as usize);
    frame.render_widget(
        Paragraph::new(layout::header(&cols, applied.order, app.reverse()))
            .style(Style::default().add_modifier(Modifier::REVERSED)),
        bands[1],
    );

    // Against its own total, so the bar reads full: this row *is* the hundred
    // per cent the rows below it are shares of.
    let here = app.here();
    frame.render_widget(
        Paragraph::new(layout::row(here, now, here.size.unwrap_or(0), &cols)).style(
            colour(here.state)
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::DIM),
        ),
        bands[2],
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

    let rows_visible = bands[3].height as usize;
    let rows: Vec<ListItem<'_>> = app
        .entries()
        .iter()
        .map(|entry| {
            ListItem::new(layout::row(entry, now, sized, &cols)).style(colour(entry.state))
        })
        .collect();
    if legend_rows > 0 {
        frame.render_widget(Line::from(legend()), bands[4]);
    }

    let list = List::new(rows)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        // Keep the cursor away from the edges: scrolling with it pinned to the
        // last row shows only where you have been, never where you are going.
        .scroll_padding(rows_visible / 2);
    let mut state = ListState::default().with_selected(Some(app.cursor()));
    frame.render_stateful_widget(list, bands[3], &mut state);

    frame.render_widget(
        Paragraph::new(keys(app)).style(Style::default().add_modifier(Modifier::DIM)),
        bands[5],
    );
}

/// A removal, at whatever stage it has reached.
///
/// The plan is shown **grouped by rule**, which is what `preview -d 0` prints
/// about the same paths: the modal and the report must not be able to describe
/// one plan differently.
fn draw_removal(frame: &mut Frame<'_>, pending: &removal::Removal) {
    use removal::Removal;

    let bands = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(frame.area());

    let (title, style) = match pending {
        Removal::Asking { destroys: true, .. } => (
            "This destroys files. There is no way back.",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Removal::Asking { .. } => (
            "Remove what the rules claim here?",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Removal::Planning { .. } => ("Working out what would go…", Style::default()),
        Removal::Done { .. } => ("Done.", Style::default().add_modifier(Modifier::BOLD)),
        Removal::Nothing { .. } => ("Nothing here is claimed by any rule.", Style::default()),
    };
    frame.render_widget(
        Paragraph::new(format!("{title}\n{}", pending.path().display()))
            .style(style)
            .wrap(ratatui::widgets::Wrap { trim: false }),
        bands[0],
    );

    let body: Vec<Line<'static>> = match pending {
        Removal::Asking { plan, .. } => {
            let mut lines: Vec<Line<'static>> = removal::shares(plan)
                .into_iter()
                .map(|share| {
                    Line::from(format!(
                        "  {:>8}  {:<16} {:>4} {}  {}",
                        format_size(share.allocated),
                        share.rule,
                        share.count,
                        if share.count == 1 { "item " } else { "items" },
                        if share.purge {
                            "destroyed"
                        } else {
                            "to the Trash"
                        },
                    ))
                })
                .collect();
            lines.push(Line::from(String::new()));
            lines.push(Line::from(format!(
                "  Frees {}",
                format_size(plan.reclaimable)
            )));
            if !plan.excluded.is_empty() {
                lines.push(Line::from(format!(
                    "  {} refused and left alone",
                    plan.excluded.len()
                )));
            }
            lines
        }
        Removal::Done { outcome, .. } => {
            let mut lines = vec![Line::from(format!(
                "  Removed {} of them, freeing {}.",
                outcome.count(),
                format_size(outcome.reclaimed())
            ))];
            if !outcome.trashed.paths.is_empty() {
                lines.push(Line::from(format!(
                    "  {} in the Trash, and can be put back.",
                    outcome.trashed.paths.len()
                )));
            }
            if !outcome.purged.paths.is_empty() {
                lines.push(Line::from(format!(
                    "  {} destroyed.",
                    outcome.purged.paths.len()
                )));
            }
            for failure in &outcome.failed {
                lines.push(Line::from(format!(
                    "  not removed: {} — {}",
                    failure.path.display(),
                    failure.reason
                )));
            }
            lines
        }
        Removal::Planning { .. } => vec![Line::from(
            "  Walking the tree and asking git about any repository in it.",
        )],
        Removal::Nothing { .. } => vec![
            Line::from(
                "  Only what a rule claims can be removed from here — which is what keeps the",
            ),
            Line::from("  tiers and the denylist from being decoration on a file deleter."),
        ],
    };
    frame.render_widget(Paragraph::new(body), bands[1]);

    let keys = match pending {
        Removal::Asking {
            destroys: true,
            typed,
            ..
        } => format!(
            "  type `{}` to confirm: {typed:<8}   esc cancel",
            removal::Removal::WORD
        ),
        Removal::Asking { .. } => "  y confirm    esc cancel".to_owned(),
        Removal::Planning { .. } => "  esc cancel".to_owned(),
        _ => "  esc close".to_owned(),
    };
    frame.render_widget(
        Paragraph::new(keys).style(Style::default().add_modifier(Modifier::DIM)),
        bands[2],
    );
}

/// The whole screen, given over to a config that cannot be read.
///
/// The error goes out **in full** — a parse error names a line and a column, and
/// truncating it to fit a notice line would remove the only part worth having.
fn draw_blocked(frame: &mut Frame<'_>, why: &str) {
    let bands = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(frame.area());

    frame.render_widget(
        Paragraph::new(
            "The configuration could not be read, so nothing on screen would mean anything.",
        )
        .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
        .wrap(ratatui::widgets::Wrap { trim: false }),
        bands[0],
    );
    frame.render_widget(
        Paragraph::new(why.to_owned()).wrap(ratatui::widgets::Wrap { trim: false }),
        bands[1],
    );
    frame.render_widget(
        Paragraph::new("Fix the file, then press R. q leaves.")
            .style(Style::default().add_modifier(Modifier::DIM)),
        bands[2],
    );
}

/// Read the config again and repaint against it.
///
/// A file that no longer parses **blocks** the browser rather than leaving a
/// note under a screen still painted in the previous rules' colours. See
/// [`App::block`] for why that reversed a v0.4 decision.
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
        Err(problem) => app.block(problem),
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
    if app.blocked().is_some() {
        return "R re-read the config  q quit";
    }
    if app.is_filtering() {
        "esc cancel  ↵ keep  ↑↓ move"
    } else {
        "q quit  ↵ enter  ← up  / filter  n/s/c/m sort  r sizes  R config  D remove"
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
    use disk_tools_core::{Part, Rule, UserDirs};
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

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
            app.settle_removal();
            if app.entries().iter().all(|entry| !entry.measuring) {
                return paint(app, width, height);
            }
            assert!(std::time::Instant::now() < deadline, "sizes never settled");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    // ---- a config that cannot be read ------------------------------------

    /// A file with a genuine mistake in it, for the two tests below.
    fn broken_config(home: &Path) -> PathBuf {
        let path = home.join("config.yml");
        std::fs::write(&path, "clean-rules:\n  - name: mine\n    tier: trash\n").expect("write");
        path
    }

    fn reload_from(path: Option<PathBuf>) -> Reload {
        Reload {
            path,
            user_dirs: UserDirs::default(),
        }
    }

    /// The blocked screen, printed. Ignored for the same reason as `show`.
    #[test]
    #[ignore = "diagnostic: prints the screen, asserts nothing"]
    fn show_blocked() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        let reload = reload_from(Some(broken_config(dir.path())));
        reread(&mut app, &reload);
        for line in paint(app, 78, 12) {
            println!("|{line}");
        }
    }

    /// The whole screen, because every colour on the previous one was a claim
    /// about rules the tool no longer has.
    #[test]
    fn a_config_that_stops_parsing_blocks_the_browser() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        let reload = reload_from(Some(broken_config(dir.path())));

        reread(&mut app, &reload);

        assert!(app.blocked().is_some());
        let lines = paint(app, 78, 12);
        assert!(
            lines.iter().any(|line| line.contains("could not be read")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("`parts` is required")),
            "the error goes out in full, not summarised: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("press R")),
            "and the way out is on screen: {lines:?}"
        );
    }

    /// Two keys, and both of them are ways out.
    #[test]
    fn a_blocked_browser_takes_only_reload_and_quit() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        let reload = reload_from(Some(broken_config(dir.path())));
        reread(&mut app, &reload);
        let before = app.cursor();

        for code in [
            KeyCode::Down,
            KeyCode::Char('j'),
            KeyCode::Enter,
            KeyCode::Char('/'),
            KeyCode::Char('s'),
            KeyCode::Char('r'),
        ] {
            assert!(handle(&mut app, code, &reload), "{code:?} must not quit");
            assert_eq!(app.cursor(), before, "{code:?} moved something");
            assert!(app.blocked().is_some(), "{code:?} left the blocked state");
        }

        assert!(
            !handle(&mut app, KeyCode::Char('q'), &reload),
            "q still leaves"
        );
    }

    /// The only way out that is not the door: a file that parses.
    #[test]
    fn a_config_that_parses_again_unblocks_it() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        let path = broken_config(dir.path());
        let reload = reload_from(Some(path.clone()));
        reread(&mut app, &reload);
        assert!(app.blocked().is_some());

        std::fs::write(
            &path,
            "clean-rules:\n  - name: mine\n    tier: trash\n    parts:\n      - root: \"*\"\n        includes: [\"**/node_modules/\"]\n",
        )
        .expect("write");
        handle(&mut app, KeyCode::Char('R'), &reload);

        assert!(app.blocked().is_none(), "the rules are readable again");
        assert!(
            app.any_rule_applies(),
            "and in force: the listing is painted by them"
        );
    }

    /// The destroying modal, printed. Ignored for the same reason as `show`.
    #[test]
    #[ignore = "diagnostic: prints the screen, asserts nothing"]
    fn show_removal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("project/node_modules")).expect("mkdir");
        std::fs::write(root.join("project/node_modules/a.bin"), vec![b'x'; 400_000])
            .expect("write");

        let rules = Rules::new(
            vec![Rule {
                name: "node-modules".into(),
                tier: disk_tools_core::Tier::Purge,
                parts: vec![Part {
                    root: Some(root.to_string_lossy().into_owned()),
                    includes: vec!["**/node_modules/".into()],
                    ..Part::default()
                }],
                ..Rule::default()
            }],
            &UserDirs::default(),
        )
        .expect("compiles");

        let mut app = App::open(root, rules, now(), UserDirs::default()).expect("open");
        app.move_down();
        app.begin_removal();
        for _ in 0..200 {
            app.settle_removal();
            if !matches!(app.removal(), Some(removal::Removal::Planning { .. })) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        app.removal_push('p');
        app.removal_push('u');

        for line in paint(app, 78, 12) {
            println!("|{line}");
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

        let lines = painted_settled(dir.path(), 80, 12);
        let total: u32 = lines
            .iter()
            // Past the path, the labels and the current directory — that last
            // one is the hundred per cent these are shares of, not one of them.
            .skip(3)
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
                parts: vec![Part {
                    root: Some(dir.path().to_string_lossy().into_owned()),
                    includes: vec!["**/sub/".into()],
                    ..Part::default()
                }],
                ..Rule::default()
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
