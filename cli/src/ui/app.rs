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

use super::listing::{self, Entry};
use super::sort::{Applied, Order, sort};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The parent row. A real entry rather than a special key, so `Enter` on it
/// needs no case of its own.
pub const PARENT: &str = "..";

pub struct App {
    cwd: PathBuf,
    entries: Vec<Entry>,
    cursor: usize,
    order: Order,
    reverse: bool,
    applied: Applied,

    /// The last thing that went wrong, for the header. Cleared by anything that
    /// succeeds, so it describes the present rather than accumulating.
    notice: Option<String>,
}

impl App {
    /// Open at `root`.
    ///
    /// Fails only if `root` itself cannot be read — the caller has already
    /// checked it exists, and there would be nothing to show.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        let mut app = App {
            cwd: root.to_path_buf(),
            entries: Vec::new(),
            cursor: 0,
            order: Order::Name,
            reverse: false,
            applied: Applied {
                order: Order::Name,
                fell_back: false,
            },
            notice: None,
        };
        app.entries = app.read(root)?;
        // Sorted, then the cursor put at the top. `resort` would instead hold
        // onto whatever `read_dir` happened to return first and follow it to
        // wherever the sort put it — which is a cursor landing at random.
        app.applied = sort(&mut app.entries, app.order, app.reverse);
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

    pub fn move_down(&mut self) {
        if self.cursor + 1 < self.entries.len() {
            self.cursor += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
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
        let held = self.selected().map(|entry| entry.name.clone());
        self.applied = sort(&mut self.entries, self.order, self.reverse);

        self.cursor = held
            .and_then(|name| self.entries.iter().position(|entry| entry.name == name))
            .unwrap_or(0);
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
                self.entries = entries;
                self.cursor = 0;
                self.notice = None;
                self.applied = sort(&mut self.entries, self.order, self.reverse);
                if let Some(name) = land_on
                    && let Some(at) = self.entries.iter().position(|entry| entry.name == name)
                {
                    self.cursor = at;
                }
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

        let app = App::open(dir.path()).expect("open");

        // `..` is a directory named `..`, so it sorts among them.
        assert_eq!(names(&app), ["..", "alpha", "zulu", "big.bin", "small.bin"]);
        assert_eq!(app.cursor(), 0);
    }

    /// The most visible way this screen can fail: re-sorting throws the cursor
    /// somewhere arbitrary and the user loses what they were looking at.
    #[test]
    fn the_cursor_stays_on_the_same_entry_when_the_order_changes() {
        let dir = fixture();
        let mut app = App::open(dir.path()).expect("open");

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
        let mut app = App::open(dir.path()).expect("open");

        assert!(!app.reverse());
        app.sort_by(Order::Name);
        assert!(app.reverse(), "the same key again turns it round");

        app.sort_by(Order::Size);
        assert!(!app.reverse(), "a different key starts ascending");
    }

    #[test]
    fn entering_and_leaving_a_directory() {
        let dir = fixture();
        let mut app = App::open(dir.path()).expect("open");

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
        let mut app = App::open(dir.path()).expect("open");
        point_at(&mut app, "alpha");
        app.enter();

        assert_eq!(cursor_name(&app), "..");
        app.enter();

        assert_eq!(app.cwd(), dir.path());
    }

    #[test]
    fn a_file_under_the_cursor_is_not_something_to_enter() {
        let dir = fixture();
        let mut app = App::open(dir.path()).expect("open");
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

        let mut app = App::open(dir.path()).expect("open");
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
        let mut app = App::open(dir.path()).expect("open");
        app.notice = Some("stale".into());

        point_at(&mut app, "alpha");
        app.enter();

        assert_eq!(app.notice(), None);
    }

    #[test]
    fn the_cursor_cannot_leave_the_list() {
        let dir = fixture();
        let mut app = App::open(dir.path()).expect("open");

        app.move_up();
        assert_eq!(app.cursor(), 0, "nothing above the first row");

        for _ in 0..50 {
            app.move_down();
        }
        assert_eq!(app.cursor(), app.entries().len() - 1, "nor below the last");
    }

    /// The root of the filesystem has no parent, so there is no `..` and
    /// leaving is a no-op rather than an error.
    #[test]
    fn there_is_no_way_up_from_the_top() {
        let mut app = App::open(Path::new("/")).expect("open /");

        assert!(
            !names(&app).contains(&PARENT.to_owned()),
            "no parent row at the root"
        );
        app.leave();
        assert_eq!(app.cwd(), Path::new("/"));
    }
}
