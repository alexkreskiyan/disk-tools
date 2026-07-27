//! What the browser is looking at, and what the keys do to it.
//!
//! Every method here is a plain function over state — no terminal, no drawing.
//! That is what makes the whole task testable on CI, which has neither.
//!
//! Two behaviours are worth stating because getting them wrong is the most
//! visible way such a screen fails:
//!
//! **The cursor holds onto an entry, not an index.** Re-sorting moves rows
//! about; a cursor pinned to a position lands somewhere arbitrary, and the user
//! loses the thing they were looking at.
//!
//! **An entry that did not happen changes nothing.** Failing to open a
//! directory leaves the path, the listing and the cursor exactly where they
//! were, and says why. Half-moving would be worse than not moving.
//!
//! The listing is kept twice: [`App::listed`] is what the directory contains,
//! and [`App::entries`] is what the filter left of it. Filtering a list in place
//! would mean re-reading the directory to widen the filter again, and a size
//! arriving for a hidden row would have nowhere to land.

use super::listing::{self, Entry};
use super::measure::Sizer;
use super::sort::{Applied, Order, sort};
use disk_tools_core::{Rules, State};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The parent row. A real entry rather than a special key, so `Enter` on it
/// needs no case of its own.
pub const PARENT: &str = "..";

pub struct App {
    cwd: PathBuf,

    /// Everything in the directory, sorted.
    listed: Vec<Entry>,
    /// What the filter left of it — what is on screen, and what `cursor` indexes.
    entries: Vec<Entry>,

    /// What is being matched against. Empty means everything is shown.
    filter: String,
    /// The filter as it is being typed, if it is. `None` is normal navigation.
    typing: Option<String>,

    cursor: usize,
    order: Order,
    reverse: bool,
    applied: Applied,

    /// The last thing that went wrong, for the header. Cleared by anything that
    /// succeeds, so it describes the present rather than accumulating.
    notice: Option<String>,

    /// Directory sizes, computed in the background.
    sizer: Sizer,

    /// What `clean` would say about each row. The same rules that verb uses, so
    /// the two cannot disagree about what is junk.
    rules: Rules,

    /// How many rows the list band has. Set by the drawing code, which is the
    /// only place that knows — a page is a screenful, so it cannot be a
    /// constant.
    page: usize,
}

impl App {
    /// Open at `root`.
    ///
    /// Fails only if `root` itself cannot be read — the caller has already
    /// checked it exists, and there would be nothing to show.
    pub fn open(root: &Path, rules: Rules) -> std::io::Result<Self> {
        let mut app = App {
            cwd: root.to_path_buf(),
            listed: Vec::new(),
            entries: Vec::new(),
            filter: String::new(),
            typing: None,
            cursor: 0,
            order: Order::Name,
            reverse: false,
            applied: Applied {
                order: Order::Name,
                fell_back: false,
            },
            notice: None,
            sizer: Sizer::new(),
            rules,
            page: 1,
        };
        app.listed = app.read(root)?;
        app.classify();
        // Sorted, then the cursor put at the top. `refilter` would instead hold
        // onto whatever `read_dir` happened to return first and follow it to
        // wherever the sort put it — which is a cursor landing at random.
        app.applied = sort(&mut app.listed, app.order, app.reverse);
        app.entries = app.filtered();
        app.size_directories();
        Ok(app)
    }

    fn read(&self, dir: &Path) -> std::io::Result<Vec<Entry>> {
        let mut entries = listing::list(dir)?;
        if dir.parent().is_some() {
            entries.push(Entry {
                name: OsString::from(PARENT),
                is_dir: true,
                size: None,
                modified: None,
                created: None,
                state: State::Untracked,
                measuring: false,
            });
        }
        Ok(entries)
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn applied(&self) -> Applied {
        self.applied
    }

    pub fn reverse(&self) -> bool {
        self.reverse
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    /// What is being matched against, typed or not.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// How many rows fit, from the code that draws them.
    pub fn set_page(&mut self, rows: usize) {
        self.page = rows.max(1);
    }

    pub fn move_down(&mut self) {
        self.move_by(1);
    }

    pub fn move_up(&mut self) {
        self.move_by(-1);
    }

    /// A screenful, less one row of overlap so the eye has something to land on.
    pub fn page_down(&mut self) {
        self.move_by(self.page.saturating_sub(1).max(1) as isize);
    }

    pub fn page_up(&mut self) {
        self.move_by(-(self.page.saturating_sub(1).max(1) as isize));
    }

    pub fn jump_to_top(&mut self) {
        self.cursor = 0;
    }

    pub fn jump_to_bottom(&mut self) {
        self.cursor = self.entries.len().saturating_sub(1);
    }

    /// Move, stopping at the ends rather than wrapping.
    ///
    /// Wrapping would turn "page down once more" into "back to the top", and a
    /// list is not a carousel.
    fn move_by(&mut self, rows: isize) {
        if self.entries.is_empty() {
            self.cursor = 0;
            return;
        }
        let last = self.entries.len() - 1;
        self.cursor = (self.cursor as isize + rows).clamp(0, last as isize) as usize;
    }

    /// Sort by `order`, or reverse it if that is already the order in force.
    ///
    /// One key per order rather than a cycle: with a cycle, "sort by size" costs
    /// a different number of presses depending on where you already were.
    pub fn sort_by(&mut self, order: Order) {
        if self.order == order {
            self.reverse = !self.reverse;
        } else {
            self.order = order;
            self.reverse = false;
        }
        self.resort();
    }

    /// Re-sort, keeping the cursor on whatever it was on.
    ///
    /// Only for a change of order. Opening a directory starts at the top.
    fn resort(&mut self) {
        self.applied = sort(&mut self.listed, self.order, self.reverse);
        self.refilter();
    }

    /// Rebuild what is on screen, keeping the cursor on whatever it was on.
    ///
    /// When the held entry has been filtered away the cursor goes to the top
    /// rather than to whatever happens to be at its old index — the same rule as
    /// re-sorting, for the same reason.
    fn refilter(&mut self) {
        let held = self.selected().map(|entry| entry.name.clone());
        self.entries = self.filtered();

        self.cursor = held
            .and_then(|name| self.entries.iter().position(|entry| entry.name == name))
            .unwrap_or(0);
    }

    /// The rows the filter admits.
    ///
    /// **The parent is always admitted.** Filtering is for finding something
    /// here; it is not a reason to take away the way out, and a filter that
    /// matches nothing would otherwise leave a screen with no key that does
    /// anything.
    ///
    /// Matching is a case-insensitive substring: a user typing `proj` to find
    /// `Projects` is not composing a query.
    fn filtered(&self) -> Vec<Entry> {
        if self.filter.is_empty() {
            return self.listed.clone();
        }
        let wanted = self.filter.to_lowercase();

        self.listed
            .iter()
            .filter(|entry| {
                entry.name == PARENT
                    || entry
                        .name
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&wanted)
            })
            .cloned()
            .collect()
    }

    /// Start typing a filter, from whatever is already in force.
    pub fn start_filtering(&mut self) {
        self.typing = Some(self.filter.clone());
    }

    /// Whether a key belongs to the filter rather than to navigation.
    pub fn is_filtering(&self) -> bool {
        self.typing.is_some()
    }

    /// Add a character to the filter being typed.
    pub fn filter_push(&mut self, ch: char) {
        if let Some(typed) = self.typing.as_mut() {
            typed.push(ch);
            self.filter = typed.clone();
            self.refilter();
        }
    }

    /// Take the last character back.
    pub fn filter_pop(&mut self) {
        if let Some(typed) = self.typing.as_mut() {
            typed.pop();
            self.filter = typed.clone();
            self.refilter();
        }
    }

    /// Keep the filter, stop typing it.
    pub fn filter_accept(&mut self) {
        self.typing = None;
    }

    /// Stop filtering altogether.
    ///
    /// One key for both halves: whether you are typing a filter or living with
    /// one, `Esc` is how you get the whole listing back.
    pub fn filter_clear(&mut self) {
        self.typing = None;
        if !self.filter.is_empty() {
            self.filter.clear();
            self.refilter();
        }
    }

    /// Ask the rules about every row.
    ///
    /// Once per listing, not once per frame: the rules do not change while a
    /// directory is open, and a glob match per row per redraw would be work done
    /// twelve times a second for an answer that never moves.
    fn classify(&mut self) {
        for entry in &mut self.listed {
            entry.state = if entry.name == PARENT {
                // The parent is a way out, not a thing in this listing. Colouring
                // it would say something about a directory that is not on screen.
                State::Untracked
            } else {
                self.rules.state(&self.cwd.join(&entry.name), entry.is_dir)
            };
        }
    }

    /// Whether anything here is under a rule at all.
    ///
    /// The legend costs a row, and a row spent explaining colours that are not
    /// on screen is a row taken from the listing.
    pub fn any_rule_applies(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.state != State::Untracked)
    }

    /// The subdirectories here, by absolute path. The parent is not one of them:
    /// measuring it would walk everything on screen a second time, through
    /// itself.
    fn subdirectories(&self) -> Vec<PathBuf> {
        self.listed
            .iter()
            .filter(|entry| entry.is_dir && entry.name != PARENT)
            .map(|entry| self.cwd.join(&entry.name))
            .collect()
    }

    /// Ask for the subdirectory sizes here, and show whatever is already known.
    ///
    /// Run on arrival. Navigation is not a reason to recompute anything, nor to
    /// stop anything: a walk already under way finishes, and its answer is
    /// waiting the next time this directory is opened.
    pub fn size_directories(&mut self) {
        self.sizer.request(self.subdirectories());
        self.show_sizes();
    }

    /// Forget the sizes here and walk them again.
    ///
    /// The one thing that invalidates a total, because deleting is the one way
    /// it goes stale that the browser cannot see.
    pub fn remeasure(&mut self) {
        for path in self.subdirectories() {
            self.sizer.forget(&path);
        }
        self.size_directories();
    }

    /// Take in whatever the background walks have posted since the last frame.
    pub fn absorb_sizes(&mut self) {
        let completed = self.sizer.absorb();
        self.show_sizes();

        // Only on completion, and only when size is the order in force. Sorting
        // on every climbing figure would have rows swap places continuously;
        // never sorting would leave "by size" showing an order that is no longer
        // true. The cursor holds its entry across the move, as always.
        if completed && self.order == Order::Size {
            self.resort();
        }
    }

    /// Copy what the sizer knows onto the rows.
    ///
    /// Read from the cache rather than pushed into the rows as answers arrive:
    /// a result posted while another directory was on screen has to be here when
    /// the user comes back, and a row is not the place to keep it.
    fn show_sizes(&mut self) {
        for entry in self.listed.iter_mut().chain(self.entries.iter_mut()) {
            if !entry.is_dir || entry.name == PARENT {
                continue;
            }
            let path = self.cwd.join(&entry.name);
            entry.size = self.sizer.size_of(&path);
            entry.measuring = self.sizer.is_measuring(&path);
        }
    }

    /// Stop the background walk and wait for it.
    pub fn stop_sizing(&mut self) {
        self.sizer.stop();
    }

    /// Enter the directory under the cursor, or go up if it is `..`.
    ///
    /// A file does nothing: opening one is not something this browser claims to
    /// do, and a notice about it would be noise on every stray `Enter`.
    pub fn enter(&mut self) {
        let Some(entry) = self.selected() else {
            return;
        };
        if !entry.is_dir {
            return;
        }
        if entry.name == PARENT {
            self.leave();
            return;
        }

        let target = self.cwd.join(&entry.name);
        self.go(target, None);
    }

    /// Up one level, keeping the cursor on the directory just left.
    pub fn leave(&mut self) {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else {
            return;
        };
        // What the user was in is what they will be looking for.
        let came_from = self.cwd.file_name().map(OsString::from);
        self.go(parent, came_from);
    }

    fn go(&mut self, target: PathBuf, land_on: Option<OsString>) {
        match self.read(&target) {
            Ok(entries) => {
                self.cwd = target;
                self.listed = entries;
                // A filter is about the directory it was typed in.
                self.filter.clear();
                self.typing = None;
                self.cursor = 0;
                self.notice = None;
                self.classify();
                self.applied = sort(&mut self.listed, self.order, self.reverse);
                self.entries = self.filtered();
                if let Some(name) = land_on
                    && let Some(at) = self.entries.iter().position(|entry| entry.name == name)
                {
                    self.cursor = at;
                }
                // After the move, not before: a walk of the directory being left
                // would go on competing for the pool with the one being entered.
                self.size_directories();
            }
            Err(err) => {
                // Nothing moves. A refused entry that shifted the cursor would
                // read as though it had half worked.
                self.notice = Some(format!("{}: {err}", target.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory with two subdirectories and two files.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("alpha")).expect("mkdir");
        std::fs::create_dir(dir.path().join("zulu")).expect("mkdir");
        std::fs::write(dir.path().join("big.bin"), vec![b'x'; 40_960]).expect("write");
        std::fs::write(dir.path().join("small.bin"), b"x").expect("write");
        dir
    }

    fn names(app: &App) -> Vec<String> {
        app.entries()
            .iter()
            .map(|entry| entry.name.to_string_lossy().into_owned())
            .collect()
    }

    fn cursor_name(app: &App) -> String {
        app.selected()
            .expect("something under the cursor")
            .name
            .to_string_lossy()
            .into_owned()
    }

    /// Put the cursor on `name`.
    ///
    /// By search rather than by repeated `move_down`: that stops at the last row,
    /// so a name that is not there spins forever. One such loop hung a test run
    /// for over a minute before this existed.
    fn point_at(app: &mut App, name: &str) {
        let at = app
            .entries()
            .iter()
            .position(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("{name} is not in {:?}", names(app)));

        while app.cursor() < at {
            app.move_down();
        }
        while app.cursor() > at {
            app.move_up();
        }
        assert_eq!(cursor_name(app), name);
    }

    #[test]
    fn opens_sorted_by_name_with_directories_first() {
        let dir = fixture();

        let app = App::open(dir.path(), Rules::default()).expect("open");

        // `..` is a directory named `..`, so it sorts among them.
        assert_eq!(names(&app), ["..", "alpha", "zulu", "big.bin", "small.bin"]);
        assert_eq!(app.cursor(), 0);
    }

    /// The most visible way this screen can fail: re-sorting throws the cursor
    /// somewhere arbitrary and the user loses what they were looking at.
    #[test]
    fn the_cursor_stays_on_the_same_entry_when_the_order_changes() {
        let dir = fixture();
        let mut app = App::open(dir.path(), Rules::default()).expect("open");

        point_at(&mut app, "small.bin");

        app.sort_by(Order::Size);
        assert_eq!(cursor_name(&app), "small.bin");

        app.sort_by(Order::Size); // reverses
        assert_eq!(cursor_name(&app), "small.bin");

        app.sort_by(Order::Name);
        assert_eq!(cursor_name(&app), "small.bin");
    }

    #[test]
    fn pressing_the_active_order_again_reverses_it() {
        let dir = fixture();
        let mut app = App::open(dir.path(), Rules::default()).expect("open");

        assert!(!app.reverse());
        app.sort_by(Order::Name);
        assert!(app.reverse(), "the same key again turns it round");

        app.sort_by(Order::Size);
        assert!(!app.reverse(), "a different key starts ascending");
    }

    #[test]
    fn entering_and_leaving_a_directory() {
        let dir = fixture();
        let mut app = App::open(dir.path(), Rules::default()).expect("open");

        point_at(&mut app, "alpha");
        app.enter();

        assert_eq!(app.cwd(), dir.path().join("alpha"));
        assert_eq!(
            names(&app),
            [".."],
            "an empty directory still offers the way back"
        );

        app.leave();
        assert_eq!(app.cwd(), dir.path());
        assert_eq!(
            cursor_name(&app),
            "alpha",
            "and the cursor lands on what was just left"
        );
    }

    /// `..` is an ordinary row, so `Enter` on it needs no case of its own.
    #[test]
    fn entering_the_parent_row_goes_up() {
        let dir = fixture();
        let mut app = App::open(dir.path(), Rules::default()).expect("open");
        point_at(&mut app, "alpha");
        app.enter();

        assert_eq!(cursor_name(&app), "..");
        app.enter();

        assert_eq!(app.cwd(), dir.path());
    }

    #[test]
    fn a_file_under_the_cursor_is_not_something_to_enter() {
        let dir = fixture();
        let mut app = App::open(dir.path(), Rules::default()).expect("open");
        point_at(&mut app, "big.bin");

        app.enter();

        assert_eq!(app.cwd(), dir.path());
        assert_eq!(app.notice(), None, "a stray Enter is not worth a message");
    }

    /// An entry that did not happen must not look like one that did.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_moves_nothing_and_says_why() {
        use std::os::unix::fs::PermissionsExt;

        let dir = fixture();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("mkdir");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
                .expect("restore");
            eprintln!("skipping: privileges ignore the locked directory");
            return;
        }

        let mut app = App::open(dir.path(), Rules::default()).expect("open");
        point_at(&mut app, "locked");
        let before = (app.cwd().to_path_buf(), app.cursor(), names(&app));

        app.enter();
        let after = (app.cwd().to_path_buf(), app.cursor(), names(&app));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");

        assert_eq!(before, after, "nothing moved");
        let notice = app.notice().expect("and the reason is said");
        assert!(notice.contains("locked"), "{notice}");
    }

    /// A successful move clears the last complaint: the notice describes the
    /// present, and does not accumulate.
    #[test]
    fn a_successful_move_clears_the_notice() {
        let dir = fixture();
        let mut app = App::open(dir.path(), Rules::default()).expect("open");
        app.notice = Some("stale".into());

        point_at(&mut app, "alpha");
        app.enter();

        assert_eq!(app.notice(), None);
    }

    #[test]
    fn the_cursor_cannot_leave_the_list() {
        let dir = fixture();
        let mut app = App::open(dir.path(), Rules::default()).expect("open");

        app.move_up();
        assert_eq!(app.cursor(), 0, "nothing above the first row");

        for _ in 0..50 {
            app.move_down();
        }
        assert_eq!(app.cursor(), app.entries().len() - 1, "nor below the last");
    }

    /// Wait for every directory here to finish being measured.
    ///
    /// Bounded, so a worker that never posts fails the test rather than hanging
    /// the suite.
    fn await_sizes(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            app.absorb_sizes();
            if app.entries().iter().all(|entry| !entry.measuring) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("sizes never settled: {:?}", names(app));
    }

    fn size_of(app: &App, name: &str) -> Option<u64> {
        app.entries()
            .iter()
            .find(|entry| entry.name == name)
            .expect("listed")
            .size
    }

    /// Opening asks for the sizes; nothing has to be pressed for the column to
    /// fill.
    #[test]
    fn opening_starts_measuring_every_subdirectory() {
        let dir = fixture();
        std::fs::write(dir.path().join("alpha/inner.bin"), vec![b'x'; 8192]).expect("write");

        let mut app = App::open(dir.path(), Rules::default()).expect("open");

        assert!(
            app.entries()
                .iter()
                .any(|entry| entry.measuring && entry.name == "alpha"),
            "asked for on arrival: {:?}",
            names(&app)
        );
        await_sizes(&mut app);
        assert!(
            size_of(&app, "alpha").is_some_and(|size| size >= 8192),
            "and the answer lands"
        );
    }

    /// The parent is not part of this listing. Walking it would measure
    /// everything on screen a second time, through itself.
    #[test]
    fn the_parent_row_is_never_measured() {
        let dir = fixture();

        let mut app = App::open(dir.path(), Rules::default()).expect("open");
        await_sizes(&mut app);

        let parent = app
            .entries()
            .iter()
            .find(|entry| entry.name == PARENT)
            .expect("the way back");
        assert!(!parent.measuring);
        assert_eq!(parent.size, None);
    }

    /// `r` is the one thing that makes a total stale on purpose, because
    /// deleting is the one way it goes stale that the browser cannot see.
    #[test]
    fn re_measuring_forgets_what_was_there_and_walks_again() {
        let dir = fixture();
        let mut app = App::open(dir.path(), Rules::default()).expect("open");
        await_sizes(&mut app);
        assert!(size_of(&app, "alpha").is_some());

        app.remeasure();

        assert_eq!(size_of(&app, "alpha"), None, "the old figure is dropped");
        assert!(
            app.entries()
                .iter()
                .any(|entry| entry.measuring && entry.name == "alpha")
        );
        await_sizes(&mut app);
        assert!(size_of(&app, "alpha").is_some(), "and computed afresh");
    }

    /// Stepping into a directory and back out must not pay for the parent
    /// twice. Navigation is not a reason to recompute anything.
    #[test]
    fn returning_to_a_directory_reuses_the_sizes_already_computed() {
        let dir = fixture();
        std::fs::write(dir.path().join("alpha/inner.bin"), vec![b'x'; 8192]).expect("write");
        let mut app = App::open(dir.path(), Rules::default()).expect("open");
        await_sizes(&mut app);
        let before = size_of(&app, "alpha").expect("measured once");

        point_at(&mut app, "alpha");
        app.enter();
        app.leave();

        // No `absorb_sizes` between: if this needed a walk, the figure would not
        // be here yet.
        assert_eq!(size_of(&app, "alpha"), Some(before));
        assert!(
            app.entries().iter().all(|entry| !entry.measuring),
            "and nothing is spinning: {:?}",
            names(&app)
        );
    }

    /// "By size" that stops re-ordering as the sizes arrive is showing an order
    /// that is no longer true.
    #[test]
    fn sizes_arriving_reorder_a_listing_sorted_by_size() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, bytes) in [("small", 4096usize), ("large", 200_000)] {
            let sub = dir.path().join(name);
            std::fs::create_dir(&sub).expect("mkdir");
            std::fs::write(sub.join("f.bin"), vec![b'x'; bytes]).expect("write");
        }

        let mut app = App::open(dir.path(), Rules::default()).expect("open");
        app.sort_by(Order::Size);
        await_sizes(&mut app);

        let directories: Vec<String> = names(&app)
            .into_iter()
            .filter(|name| name == "small" || name == "large")
            .collect();
        assert_eq!(directories, ["small", "large"], "ascending, as asked");
    }

    /// Stepping into a directory used to throw away the walk of its
    /// neighbours, so coming back out started from nothing.
    #[test]
    fn a_walk_survives_stepping_into_a_directory_and_back_out() {
        let dir = fixture();
        for n in 0..300 {
            let sub = dir.path().join(format!("alpha/sub{n}"));
            std::fs::create_dir_all(&sub).expect("mkdir");
            std::fs::write(sub.join("f.bin"), vec![b'x'; 4096]).expect("write");
        }

        let mut app = App::open(dir.path(), Rules::default()).expect("open");
        point_at(&mut app, "zulu");
        app.enter();
        app.leave();
        await_sizes(&mut app);

        assert!(
            size_of(&app, "alpha").is_some_and(|size| size >= 300 * 4096),
            "the walk that was running finished anyway"
        );
    }

    /// Typing a few letters is how you find one directory among four hundred.
    #[test]
    fn a_filter_narrows_the_listing_as_it_is_typed() {
        let dir = fixture();
        let mut app = App::open(dir.path(), Rules::default()).expect("open");

        app.start_filtering();
        assert!(app.is_filtering());
        for ch in "zul".chars() {
            app.filter_push(ch);
        }

        assert_eq!(names(&app), ["..", "zulu"], "and the way out stays");
    }

    /// Case is not something a user typing `proj` is thinking about.
    #[test]
    fn matching_ignores_case() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("Projects")).expect("mkdir");
        let mut app = App::open(dir.path(), Rules::default()).expect("open");

        app.start_filtering();
        for ch in "proj".chars() {
            app.filter_push(ch);
        }

        assert!(
            names(&app).contains(&"Projects".to_owned()),
            "{:?}",
            names(&app)
        );
    }

    /// A filter that matches nothing must still leave a key that does something.
    #[test]
    fn a_filter_matching_nothing_still_offers_the_way_out() {
        let dir = fixture();
        let mut app = App::open(dir.path(), Rules::default()).expect("open");

        app.start_filtering();
        for ch in "qqqq".chars() {
            app.filter_push(ch);
        }

        assert_eq!(names(&app), [".."]);
        app.enter();
        assert_eq!(app.cwd(), dir.path().parent().expect("a parent"));
    }

    #[test]
    fn backspace_widens_the_filter_again() {
        let dir = fixture();
        let mut app = App::open(dir.path(), Rules::default()).expect("open");

        app.start_filtering();
        for ch in "zulu".chars() {
            app.filter_push(ch);
        }
        assert_eq!(names(&app).len(), 2);

        for _ in 0..4 {
            app.filter_pop();
        }

        assert_eq!(names(&app).len(), 5, "the whole listing is back");
    }

    /// `Enter` keeps the filter and hands the keys back; `Esc` throws it away.
    #[test]
    fn accepting_keeps_the_filter_and_clearing_drops_it() {
        let dir = fixture();
        let mut app = App::open(dir.path(), Rules::default()).expect("open");
        app.start_filtering();
        app.filter_push('z');

        app.filter_accept();
        assert!(!app.is_filtering(), "keys are navigation again");
        assert_eq!(app.filter(), "z", "but the listing is still narrowed");
        assert_eq!(names(&app), ["..", "zulu"]);

        app.filter_clear();
        assert_eq!(app.filter(), "");
        assert_eq!(names(&app).len(), 5);
    }

    /// A filter is about the directory it was typed in.
    #[test]
    fn moving_directory_drops_the_filter() {
        let dir = fixture();
        let mut app = App::open(dir.path(), Rules::default()).expect("open");
        app.start_filtering();
        app.filter_push('a');
        app.filter_accept();

        point_at(&mut app, "alpha");
        app.enter();

        assert_eq!(app.filter(), "");
        assert!(!app.is_filtering());
    }

    /// A size arriving for a row the filter is hiding still has to land, or
    /// widening the filter shows a directory that has forgotten its total.
    #[test]
    fn a_hidden_row_keeps_the_size_it_was_given() {
        let dir = fixture();
        std::fs::write(dir.path().join("alpha/f.bin"), vec![b'x'; 8192]).expect("write");
        let mut app = App::open(dir.path(), Rules::default()).expect("open");

        app.start_filtering();
        for ch in "zulu".chars() {
            app.filter_push(ch);
        }
        await_sizes(&mut app);
        app.filter_clear();

        assert!(size_of(&app, "alpha").is_some_and(|size| size >= 8192));
    }

    #[test]
    fn a_page_moves_a_screenful_and_stops_at_the_ends() {
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 0..30 {
            std::fs::write(dir.path().join(format!("f{n:02}.bin")), b"x").expect("write");
        }
        let mut app = App::open(dir.path(), Rules::default()).expect("open");
        app.set_page(10);

        app.page_down();
        assert_eq!(app.cursor(), 9, "a screenful, less a row of overlap");

        for _ in 0..10 {
            app.page_down();
        }
        assert_eq!(
            app.cursor(),
            app.entries().len() - 1,
            "and stops at the end"
        );

        for _ in 0..10 {
            app.page_up();
        }
        assert_eq!(app.cursor(), 0, "and at the start");
    }

    #[test]
    fn home_and_end_go_the_whole_way() {
        let dir = fixture();
        let mut app = App::open(dir.path(), Rules::default()).expect("open");

        app.jump_to_bottom();
        assert_eq!(app.cursor(), app.entries().len() - 1);

        app.jump_to_top();
        assert_eq!(app.cursor(), 0);
    }

    /// A page in an empty listing is a no-op, not a panic.
    #[test]
    fn paging_an_empty_listing_does_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::open(dir.path(), Rules::default()).expect("open");
        app.start_filtering();
        for ch in "nothing".chars() {
            app.filter_push(ch);
        }
        // Only the parent row is left; filter it out too by going to the root.
        app.filter_clear();

        app.page_down();
        app.page_up();
        app.jump_to_bottom();
        app.jump_to_top();
    }

    /// Rows carry what the rules say, and the parent row carries nothing —
    /// colouring it would say something about a directory not on screen.
    #[test]
    fn rows_are_classified_and_the_parent_is_not() {
        let dir = fixture();
        let rules = Rules::new(
            vec![disk_tools_core::Rule {
                name: "junk".into(),
                root: Some(dir.path().to_string_lossy().into_owned()),
                includes: vec!["**/alpha/".into()],
                ..disk_tools_core::Rule::default()
            }],
            &disk_tools_core::UserDirs::default(),
        )
        .expect("compiles");

        let app = App::open(dir.path(), rules).expect("open");
        let state_of = |name: &str| {
            app.entries()
                .iter()
                .find(|entry| entry.name == name)
                .expect(name)
                .state
        };

        assert_eq!(state_of("alpha"), State::Included);
        assert_eq!(state_of("zulu"), State::InScope);
        assert_eq!(
            state_of(PARENT),
            State::Untracked,
            "not part of this listing"
        );
        assert!(app.any_rule_applies());
    }

    /// No rule reaches here, so there is nothing for a legend to explain and it
    /// must not take a row from the listing.
    #[test]
    fn a_directory_no_rule_reaches_needs_no_legend() {
        let dir = fixture();

        let app = App::open(dir.path(), Rules::default()).expect("open");

        assert!(app.entries().iter().all(|e| e.state == State::Untracked));
        assert!(!app.any_rule_applies());
    }

    /// The state follows the path, so it has to be recomputed on arrival — a row
    /// keeping the colour of a same-named row in the previous directory would be
    /// worse than no colour at all.
    #[test]
    fn moving_directory_reclassifies() {
        let dir = fixture();
        std::fs::create_dir(dir.path().join("alpha/target")).expect("mkdir");
        let rules = Rules::new(
            vec![disk_tools_core::Rule {
                name: "junk".into(),
                root: Some(dir.path().to_string_lossy().into_owned()),
                includes: vec!["**/target/".into()],
                ..disk_tools_core::Rule::default()
            }],
            &disk_tools_core::UserDirs::default(),
        )
        .expect("compiles");

        let mut app = App::open(dir.path(), rules).expect("open");
        assert!(
            !app.any_rule_applies()
                || app
                    .entries()
                    .iter()
                    .all(|e| e.name == PARENT || e.state != State::Included)
        );

        point_at(&mut app, "alpha");
        app.enter();

        assert_eq!(
            app.entries()
                .iter()
                .find(|entry| entry.name == "target")
                .expect("listed")
                .state,
            State::Included
        );
    }

    /// The root of the filesystem has no parent, so there is no `..` and
    /// leaving is a no-op rather than an error.
    #[test]
    fn there_is_no_way_up_from_the_top() {
        let mut app = App::open(Path::new("/"), Rules::default()).expect("open /");

        assert!(
            !names(&app).contains(&PARENT.to_owned()),
            "no parent row at the root"
        );
        app.leave();
        assert_eq!(app.cwd(), Path::new("/"));
    }
}
