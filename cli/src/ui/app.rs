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
use super::removal::Removal;
use super::sort::{Applied, Order, sort};
use disk_tools_core::{CleanOptions, DetectOptions, Facts, Rules, State, UserDirs};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// The parent row. A real entry rather than a special key, so `Enter` on it
/// needs no case of its own.
pub const PARENT: &str = "..";

pub struct App {
    cwd: PathBuf,

    /// The current directory as a row in its own right.
    ///
    /// `..` is the way out, not this. Its figures are the sum of the listing
    /// rather than a walk of their own: measuring the directory being looked at
    /// would walk everything on screen a second time, through itself.
    here: Entry,

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
    ///
    /// Shared with the sizing worker, which needs them to work out what a
    /// subtree would give back — behind an `Arc` rather than cloned per walk,
    /// since a `GlobSet` is not a cheap thing to copy.
    rules: Arc<Rules>,

    /// For `older_than`. Read once: the core reads no clock, and a rule whose
    /// threshold is measured in days does not care about the minutes a browser
    /// stays open.
    now: SystemTime,

    /// Everything a removal needs that the browser does not otherwise have: the
    /// rules to plan by, and the user's directories the denylist is built from.
    ///
    /// Held rather than passed at the keystroke because a plan must be made
    /// against the rules **on screen** — the colours the user is looking at.
    options: CleanOptions,

    /// A removal, from the moment `D` is pressed until it is dismissed.
    removal: Option<Removal>,

    /// Why the browser has stopped taking keys, when it has.
    ///
    /// `Some` only after a reload that failed: the rules on screen are then the
    /// previous ones, and nothing here may pretend otherwise.
    blocked: Option<String>,

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
    pub fn open(
        root: &Path,
        rules: Rules,
        now: SystemTime,
        user_dirs: UserDirs,
    ) -> std::io::Result<Self> {
        let rules = Arc::new(rules);
        let mut app = App {
            cwd: root.to_path_buf(),
            here: blank(root),
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
            options: CleanOptions {
                detect: DetectOptions {
                    rules: (*rules).clone(),
                    now,
                },
                user_dirs,
                ..CleanOptions::default()
            },
            removal: None,
            blocked: None,
            sizer: Sizer::new(Arc::clone(&rules), now),
            rules,
            now,
            page: 1,
        };
        let listed = app.read(root)?;
        app.arrive(root.to_path_buf(), listed, None);
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
                reclaimable: None,
                measuring: false,
            });
        }
        Ok(entries)
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The current directory, as a row.
    pub fn here(&self) -> &Entry {
        &self.here
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

    /// Put something in the header until the next thing that succeeds.
    pub fn say(&mut self, what: String) {
        self.notice = Some(what);
    }

    // ---- removing what the rules claim, from here ------------------------

    pub fn removal(&self) -> Option<&Removal> {
        self.removal.as_ref()
    }

    /// Start a removal of whatever the rules claim under the cursor.
    ///
    /// The **parent row is not a place**: `..` is the way out of here, not a
    /// description of anywhere, and a key that removed through it would act on a
    /// directory the screen is not about.
    pub fn begin_removal(&mut self) {
        let Some(entry) = self.selected() else {
            return;
        };
        if entry.name == PARENT {
            return;
        }
        let path = self.cwd.join(&entry.name);
        self.removal = Some(Removal::begin(&path, self.options.clone()));
    }

    /// Collect whatever the worker has said — a plan, progress, or the outcome.
    ///
    /// The tidying belongs **here** rather than beside the keypress that started
    /// it: removing happens on a worker now, so the moment the disk changed is
    /// the moment its answer arrives, not the moment it was agreed to.
    pub fn settle_removal(&mut self) {
        let Some(removal) = &mut self.removal else {
            return;
        };
        if !removal.settle() || !matches!(removal, Removal::Done { .. }) {
            return;
        }

        // Only the row that changed. A removal under `project/` cannot have
        // altered `other/`, and forgetting the whole listing — which is what
        // `remeasure` is for, on the `r` key — would re-walk every sibling to
        // learn what it already knew. `request` skips what it already holds, so
        // the re-read below costs one walk: the forgotten one.
        let path = removal.path().to_path_buf();
        self.sizer.forget(&path);
        self.refresh();
    }

    /// Agree, and carry it out.
    pub fn confirm_removal(&mut self) {
        let Some(removal) = &mut self.removal else {
            return;
        };
        if !matches!(removal, Removal::Asking { .. }) {
            return;
        }
        // Hands it to a worker and returns: the OS trash is a round-trip to
        // Finder on macOS, and a browser that stopped drawing for it would be
        // indistinguishable from one that had hung.
        removal.carry_out();
    }

    /// Read this directory again without leaving it.
    ///
    /// Not [`Self::arrive`]: that is for a move, and it clears the filter,
    /// because a filter is about the directory it was typed in. Here the
    /// directory is the same one — the user is still looking at the rows they
    /// narrowed to, and having them reappear because something was removed
    /// would be the browser undoing their work for them.
    ///
    /// A removed directory is a row until something looks again, and a removed
    /// file leaves a row carrying a size for a file that is not there.
    fn refresh(&mut self) {
        let Ok(listed) = self.read(&self.cwd.clone()) else {
            // The directory itself has gone — from under us, or because it was
            // what was removed. Leave the rows alone rather than blanking the
            // screen; the next move settles it.
            return;
        };
        self.listed = listed;
        self.here = blank(&self.cwd);
        self.here.state = self.state_of_cwd();

        self.classify();
        self.sizer.request(self.subdirectories());
        apply_sizes(&self.cwd, &self.sizer, &mut self.listed);

        self.applied = sort(&mut self.listed, self.order, self.reverse);
        // Holds the cursor on whatever it was on, and drops it to the top when
        // that row is the one that has just gone.
        self.refilter();
        self.total_here();
    }

    /// Abandon it. A plan still being walked is simply dropped: the worker
    /// finds a closed channel and goes away.
    pub fn dismiss_removal(&mut self) {
        self.removal = None;
    }

    /// Stop taking keys, and say why.
    ///
    /// Reached when the config no longer parses. Every colour, every `clean`
    /// column and every legend row on this screen is a claim about rules the
    /// tool no longer has, and a one-line notice under a screenful of them is
    /// not a correction — it is a caption on something that is now wrong.
    ///
    /// v0.4 decided the other way: a bad file left the working rules in place
    /// with a notice, so a typo would not turn every colour off. That was right
    /// when the alternative was an empty screen. It is wrong now that the
    /// alternative is the CLI's own rule — rules that cannot be read stop the
    /// work — which is what keeps the two saying the same thing about one file.
    pub fn block(&mut self, why: String) {
        self.blocked = Some(why);
    }

    /// Why the browser is not taking keys, if it is not.
    pub fn blocked(&self) -> Option<&str> {
        self.blocked.as_deref()
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
        // The names beside each row, which is what `requires_sibling` asks
        // about. Read from the listing the browser already has, so a `target/`
        // is coloured as junk only when the `Cargo.toml` that makes it one is
        // actually there — the same question `detect` asks, answered the same
        // way.
        let siblings: Vec<OsString> = self.listed.iter().map(|e| e.name.clone()).collect();
        let any_sibling =
            |wanted: &dyn Fn(&OsStr) -> bool| siblings.iter().any(|beside| wanted(beside));

        let states: Vec<State> = self
            .listed
            .iter()
            .map(|entry| {
                if entry.name == PARENT {
                    // The parent is a way out, not a thing in this listing.
                    // Colouring it would say something about a directory that is
                    // not on screen.
                    return State::Untracked;
                }
                self.rules.state(
                    &self.cwd.join(&entry.name),
                    &Facts {
                        is_dir: entry.is_dir,
                        modified: entry.modified,
                        now: self.now,
                        any_sibling: &any_sibling,
                    },
                )
            })
            .collect();

        for (entry, state) in self.listed.iter_mut().zip(states) {
            entry.state = state;
            // A file's claim needs no walk: it is the file, or it is nothing.
            // Directories are filled in by `apply_sizes`, once something has
            // been down there to look.
            if !entry.is_dir {
                entry.reclaimable = (state == State::Included).then_some(entry.size).flatten();
            }
        }
    }

    /// What the rules make of the directory being looked at.
    ///
    /// Answering `requires_sibling` here means reading the parent, which the
    /// browser has not read. That is one listing per arrival, and only when some
    /// rule in force actually asks — most rule sets never do, and a directory
    /// read to answer a question nobody posed is a directory read for nothing.
    fn state_of_cwd(&self) -> State {
        let beside: Vec<OsString> = match self.cwd.parent() {
            Some(parent) if self.rules.wants_siblings() => names_in(parent),
            _ => Vec::new(),
        };
        let any_sibling = |wanted: &dyn Fn(&OsStr) -> bool| beside.iter().any(|name| wanted(name));

        self.rules.state(
            &self.cwd,
            &Facts {
                is_dir: true,
                modified: self.here.modified,
                now: self.now,
                any_sibling: &any_sibling,
            },
        )
    }

    /// Swap in a freshly read rule set and repaint against it.
    ///
    /// The listing is not re-read: rules are about what the files mean, not
    /// about what is there, and rereading would move the cursor for no reason.
    ///
    /// The **sizes** are, though. Every reclaimable figure was worked out
    /// against the rules that have just been replaced, so keeping them would
    /// leave the screen answering the previous question — which is precisely the
    /// question the user changed the rule to stop asking.
    pub fn reload_rules(&mut self, rules: Rules) {
        // Reading a usable file is the only way out of the blocked state, and
        // this is the only place a usable file arrives.
        self.blocked = None;
        // A removal plans against the rules on screen, so the two are replaced
        // together or a plan could outlive the colours that justified it.
        self.options.detect.rules = rules.clone();
        self.rules = Arc::new(rules);
        self.sizer.retarget(Arc::clone(&self.rules));

        self.here.state = self.state_of_cwd();
        self.classify();
        self.size_directories();
        self.entries = self.filtered();
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

    /// The subdirectories here, by absolute path, each with whether a rule
    /// already claims it.
    ///
    /// The parent is not one of them: measuring it would walk everything on
    /// screen a second time, through itself.
    ///
    /// The claim has to travel with the request. Nothing *inside* a
    /// `node_modules` matches `**/node_modules/`, so a walk that was not told
    /// would come back saying there is nothing to clean in it.
    fn subdirectories(&self) -> Vec<(PathBuf, bool)> {
        self.listed
            .iter()
            .filter(|entry| entry.is_dir && entry.name != PARENT)
            .map(|entry| (self.cwd.join(&entry.name), entry.state == State::Included))
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
        for (path, _) in self.subdirectories() {
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
        // Both orders are read off figures the walk produces, so both go stale
        // as it runs and both correct themselves when it finishes.
        if completed && matches!(self.order, Order::Size | Order::Cleanable) {
            self.resort();
        }
    }

    /// Copy what the sizer knows onto the rows, and add them up for `here`.
    ///
    /// Read from the cache rather than pushed into the rows as answers arrive:
    /// a result posted while another directory was on screen has to be here when
    /// the user comes back, and a row is not the place to keep it.
    fn show_sizes(&mut self) {
        apply_sizes(&self.cwd, &self.sizer, &mut self.listed);
        apply_sizes(&self.cwd, &self.sizer, &mut self.entries);
        self.total_here();
    }

    /// The current directory's own figures: the sum of the listing.
    ///
    /// Not a walk of `cwd` — that would cover exactly the rows already being
    /// walked, through them, and pay for the whole thing twice.
    fn total_here(&mut self) {
        let mut allocated = 0u64;
        let mut reclaimable = 0u64;
        let mut settled = true;

        for entry in &self.listed {
            if entry.name == PARENT {
                continue;
            }
            allocated += entry.size.unwrap_or(0);
            reclaimable += entry.reclaimable.unwrap_or(0);
            // A row still being walked, or one whose claim is not in yet, makes
            // both sums a lower bound rather than a total.
            settled &= !entry.measuring && (!entry.is_dir || entry.reclaimable.is_some());
        }

        self.here.size = Some(allocated);
        self.here.reclaimable = Some(reclaimable);
        self.here.measuring = !settled;
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
            Ok(listed) => self.arrive(target, listed, land_on),
            Err(err) => {
                // Nothing moves. A refused entry that shifted the cursor would
                // read as though it had half worked.
                self.notice = Some(format!("{}: {err}", target.display()));
            }
        }
    }

    /// Land in a directory that has just been read.
    ///
    /// The order of the last four steps is the whole of it, and getting it wrong
    /// is what made a chosen sort look as though it had been forgotten:
    ///
    /// 1. classify, because the sizing request needs to know what is claimed;
    /// 2. ask for the sizes, and **copy in whatever is already known**;
    /// 3. *then* sort.
    ///
    /// Sorting before the sizes are in means sorting by size with every
    /// directory reading `None` — which is name order — and nothing afterwards
    /// puts it right, because `absorb_sizes` re-sorts only when a walk finishes
    /// and in a directory measured earlier no walk has to.
    fn arrive(&mut self, target: PathBuf, listed: Vec<Entry>, land_on: Option<OsString>) {
        self.cwd = target;
        self.listed = listed;
        // A filter is about the directory it was typed in.
        self.filter.clear();
        self.typing = None;
        self.cursor = 0;
        self.notice = None;

        self.here = blank(&self.cwd);
        self.here.state = self.state_of_cwd();

        self.classify();
        // After the move, not before: a walk of the directory being left would
        // go on competing for the pool with the one being entered.
        self.sizer.request(self.subdirectories());
        apply_sizes(&self.cwd, &self.sizer, &mut self.listed);

        self.applied = sort(&mut self.listed, self.order, self.reverse);
        self.entries = self.filtered();
        self.total_here();

        if let Some(name) = land_on
            && let Some(at) = self.entries.iter().position(|entry| entry.name == name)
        {
            self.cursor = at;
        }
    }
}

/// Copy the sizer's answers onto the directory rows of one listing.
///
/// A free function so that it can be handed the listing on its own, before the
/// filtered view of it exists.
fn apply_sizes(cwd: &Path, sizer: &Sizer, rows: &mut [Entry]) {
    for entry in rows {
        if !entry.is_dir || entry.name == PARENT {
            continue;
        }
        let path = cwd.join(&entry.name);
        entry.size = sizer.size_of(&path);
        entry.measuring = sizer.is_measuring(&path);
        entry.reclaimable = if entry.state == State::Included {
            // A rule claims the whole thing, so whatever it comes to is what
            // goes. No walk has to say so, and the figure is right from the
            // first frame rather than at the end of one.
            entry.size
        } else {
            sizer.reclaimable_of(&path)
        };
    }
}

/// The entry names in a directory, or none if it cannot be read.
///
/// Names only — no `metadata` call — because the only question being asked of
/// them is what they are called.
fn names_in(dir: &Path) -> Vec<OsString> {
    std::fs::read_dir(dir)
        .map(|listing| listing.flatten().map(|entry| entry.file_name()).collect())
        .unwrap_or_default()
}

/// The current directory as a row, before anything is known about its contents.
///
/// The name is the directory's own, or the whole path at a filesystem root,
/// where there is no last component to show.
fn blank(cwd: &Path) -> Entry {
    let metadata = std::fs::metadata(cwd).ok();

    Entry {
        name: cwd
            .file_name()
            .map(OsString::from)
            .unwrap_or_else(|| OsString::from(cwd.as_os_str())),
        is_dir: true,
        size: None,
        modified: metadata.as_ref().and_then(|m| m.modified().ok()),
        created: metadata.as_ref().and_then(|m| m.created().ok()),
        state: State::Untracked,
        reclaimable: None,
        measuring: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_tools_core::{Part, Rule, Tier, UserDirs};

    /// A fixed clock, so an `older_than` rule decides the same way every run.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_750_000_000)
    }

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

        let app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");

        // `..` is a directory named `..`, so it sorts among them.
        assert_eq!(names(&app), ["..", "alpha", "zulu", "big.bin", "small.bin"]);
        assert_eq!(app.cursor(), 0);
    }

    /// The most visible way this screen can fail: re-sorting throws the cursor
    /// somewhere arbitrary and the user loses what they were looking at.
    #[test]
    fn the_cursor_stays_on_the_same_entry_when_the_order_changes() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");

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
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");

        assert!(!app.reverse());
        app.sort_by(Order::Name);
        assert!(app.reverse(), "the same key again turns it round");

        app.sort_by(Order::Size);
        assert!(!app.reverse(), "a different key starts ascending");
    }

    #[test]
    fn entering_and_leaving_a_directory() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");

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
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        point_at(&mut app, "alpha");
        app.enter();

        assert_eq!(cursor_name(&app), "..");
        app.enter();

        assert_eq!(app.cwd(), dir.path());
    }

    #[test]
    fn a_file_under_the_cursor_is_not_something_to_enter() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
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

        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
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
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        app.notice = Some("stale".into());

        point_at(&mut app, "alpha");
        app.enter();

        assert_eq!(app.notice(), None);
    }

    #[test]
    fn the_cursor_cannot_leave_the_list() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");

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

        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");

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

        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
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
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
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
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
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

        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
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

        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
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
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");

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
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");

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
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");

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
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");

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
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
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
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
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
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");

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
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
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
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");

        app.jump_to_bottom();
        assert_eq!(app.cursor(), app.entries().len() - 1);

        app.jump_to_top();
        assert_eq!(app.cursor(), 0);
    }

    /// A page in an empty listing is a no-op, not a panic.
    #[test]
    fn paging_an_empty_listing_does_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
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
                parts: vec![Part {
                    root: Some(dir.path().to_string_lossy().into_owned()),
                    includes: vec!["**/alpha/".into()],
                    ..Part::default()
                }],
                ..Rule::default()
            }],
            &disk_tools_core::UserDirs::default(),
        )
        .expect("compiles");

        let app = App::open(dir.path(), rules, now(), UserDirs::default()).expect("open");
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

        let app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");

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
                parts: vec![Part {
                    root: Some(dir.path().to_string_lossy().into_owned()),
                    includes: vec!["**/target/".into()],
                    ..Part::default()
                }],
                ..Rule::default()
            }],
            &disk_tools_core::UserDirs::default(),
        )
        .expect("compiles");

        let mut app = App::open(dir.path(), rules, now(), UserDirs::default()).expect("open");
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

    /// The inconsistency the user found: `target/` coloured as junk where
    /// `clean` would not touch it, because the `Cargo.toml` that makes it junk
    /// is not there. The siblings come from the listing the browser already has.
    #[test]
    fn a_predicate_is_answered_from_the_listing_beside_the_row() {
        let rules = |root: &Path| {
            Rules::new(
                vec![disk_tools_core::Rule {
                    name: "rust-target".into(),
                    parts: vec![Part {
                        root: Some(root.to_string_lossy().into_owned()),
                        includes: vec!["**/target/".into()],
                        requires: vec!["Cargo.toml".into()],
                        ..Part::default()
                    }],
                    ..Rule::default()
                }],
                &disk_tools_core::UserDirs::default(),
            )
            .expect("compiles")
        };

        let bare = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(bare.path().join("target")).expect("mkdir");
        let app =
            App::open(bare.path(), rules(bare.path()), now(), UserDirs::default()).expect("open");
        assert_eq!(
            app.entries()
                .iter()
                .find(|e| e.name == "target")
                .expect("listed")
                .state,
            State::InScope,
            "no Cargo.toml beside it, so no rule reaches it"
        );

        let crated = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(crated.path().join("target")).expect("mkdir");
        std::fs::write(crated.path().join("Cargo.toml"), b"[package]").expect("write");
        let app = App::open(
            crated.path(),
            rules(crated.path()),
            now(),
            UserDirs::default(),
        )
        .expect("open");
        assert_eq!(
            app.entries()
                .iter()
                .find(|e| e.name == "target")
                .expect("listed")
                .state,
            State::Included
        );
    }

    /// Reloading repaints without re-reading the directory: rules are about what
    /// the files mean, not about what is there.
    #[test]
    fn reloading_rules_repaints_without_moving_anything() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        point_at(&mut app, "zulu");
        let before = names(&app);
        assert!(!app.any_rule_applies());

        app.reload_rules(
            Rules::new(
                vec![disk_tools_core::Rule {
                    name: "junk".into(),
                    parts: vec![Part {
                        root: Some(dir.path().to_string_lossy().into_owned()),
                        includes: vec!["**/alpha/".into()],
                        ..Part::default()
                    }],
                    ..Rule::default()
                }],
                &disk_tools_core::UserDirs::default(),
            )
            .expect("compiles"),
        );

        assert!(app.any_rule_applies(), "the new rules are in force");
        assert_eq!(names(&app), before, "and nothing moved");
        assert_eq!(cursor_name(&app), "zulu");
    }

    /// The bug the user found: a sort chosen in one directory came out as name
    /// order in the next, because the rows were sorted while every directory
    /// still read `None` and nothing afterwards put it right.
    #[test]
    fn a_chosen_order_survives_walking_into_a_directory_measured_already() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("sub");
        for (name, bytes) in [("small", 4096usize), ("large", 200_000)] {
            let inner = sub.join(name);
            std::fs::create_dir_all(&inner).expect("mkdir");
            std::fs::write(inner.join("f.bin"), vec![b'x'; bytes]).expect("write");
        }

        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        // Sizing `sub` reports everything beneath it on the way, so by now both
        // of its children are known and entering it starts no walk at all.
        await_sizes(&mut app);
        app.sort_by(Order::Size);
        app.sort_by(Order::Size); // biggest first

        point_at(&mut app, "sub");
        app.enter();

        // No `absorb_sizes` between: there is nothing to absorb, which is
        // exactly the case that used to come out unsorted.
        let directories: Vec<String> = names(&app)
            .into_iter()
            .filter(|name| name == "small" || name == "large")
            .collect();
        assert_eq!(directories, ["large", "small"]);
        assert_eq!(app.applied().order, Order::Size, "and the header agrees");
    }

    /// Everything on this screen was about the directory's contents and nothing
    /// was about the directory.
    #[test]
    fn the_current_directory_is_a_row_of_its_own() {
        let dir = fixture();
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        await_sizes(&mut app);

        let here = app.here();
        assert_eq!(here.name, dir.path().file_name().expect("a name"));
        assert!(here.is_dir);
        assert!(here.modified.is_some(), "the directory's own timestamp");
        assert!(
            !app.entries().iter().any(|entry| entry.name == here.name),
            "and it is not in the listing: `..` is the way out, not this"
        );
    }

    /// Its figures are the sum of what is on screen — measuring `cwd` itself
    /// would walk every row a second time, through itself.
    #[test]
    fn the_current_directory_adds_up_its_listing() {
        let dir = fixture();
        std::fs::write(dir.path().join("alpha/inner.bin"), vec![b'x'; 8192]).expect("write");
        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        await_sizes(&mut app);

        let listed: u64 = app
            .entries()
            .iter()
            .filter(|entry| entry.name != PARENT)
            .filter_map(|entry| entry.size)
            .sum();

        assert_eq!(app.here().size, Some(listed));
        assert!(!app.here().measuring, "everything under it has settled");
        assert!(listed >= 40_960 + 8192);
    }

    /// While anything below is still being walked, the figure is a lower bound
    /// and says so.
    #[test]
    fn the_current_directory_is_measuring_while_anything_under_it_is() {
        let dir = fixture();

        let app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");

        assert!(app.here().measuring, "{:?}", names(&app));
    }

    /// A rule set that claims something here.
    fn claiming(root: &Path) -> Rules {
        Rules::new(
            vec![Rule {
                name: "junk".into(),
                parts: vec![Part {
                    root: Some(root.to_string_lossy().into_owned()),
                    includes: vec!["**/node_modules/".into(), "**/*.pyc".into()],
                    ..Part::default()
                }],
                ..Rule::default()
            }],
            &UserDirs::default(),
        )
        .expect("compiles")
    }

    /// The question the browser exists to answer: how much of this goes.
    #[test]
    fn every_row_says_what_clean_would_take_from_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let modules = dir.path().join("app/node_modules");
        std::fs::create_dir_all(&modules).expect("mkdir");
        std::fs::write(modules.join("dep.bin"), vec![b'x'; 16_384]).expect("write");
        std::fs::write(dir.path().join("app/main.rs"), vec![b'x'; 4096]).expect("write");
        std::fs::write(dir.path().join("stale.pyc"), vec![b'x'; 4096]).expect("write");
        std::fs::write(dir.path().join("live.py"), vec![b'x'; 4096]).expect("write");

        let mut app =
            App::open(dir.path(), claiming(dir.path()), now(), UserDirs::default()).expect("open");
        await_sizes(&mut app);
        let row = |name: &str| {
            app.entries()
                .iter()
                .find(|entry| entry.name == name)
                .unwrap_or_else(|| panic!("{name} missing from {:?}", names(&app)))
        };

        // A file the rules claim is claimed whole; one they do not is not.
        assert_eq!(row("stale.pyc").reclaimable, row("stale.pyc").size);
        assert_eq!(row("live.py").reclaimable, None);

        // A directory carries what the walk found inside it, which is the junk
        // and not the source beside it.
        let app_dir = row("app");
        assert!(app_dir.reclaimable.is_some_and(|bytes| bytes >= 16_384));
        assert!(
            app_dir.reclaimable < app_dir.size,
            "{:?} of {:?}",
            app_dir.reclaimable,
            app_dir.size
        );

        // And the directory being looked at is the sum of them.
        assert_eq!(
            app.here().reclaimable,
            Some(
                app_dir.reclaimable.expect("measured")
                    + row("stale.pyc").reclaimable.expect("claimed")
            )
        );
    }

    /// A directory a rule claims outright goes in full — including the parts of
    /// it that no pattern would match on their own.
    #[test]
    fn a_claimed_directory_is_reclaimable_in_full() {
        let dir = tempfile::tempdir().expect("tempdir");
        let modules = dir.path().join("node_modules");
        std::fs::create_dir_all(modules.join("dep")).expect("mkdir");
        std::fs::write(modules.join("dep/f.bin"), vec![b'x'; 16_384]).expect("write");

        let mut app =
            App::open(dir.path(), claiming(dir.path()), now(), UserDirs::default()).expect("open");
        await_sizes(&mut app);
        let row = app
            .entries()
            .iter()
            .find(|entry| entry.name == "node_modules")
            .expect("listed");

        assert_eq!(row.state, State::Included);
        assert_eq!(row.reclaimable, row.size);
        assert!(row.size.is_some_and(|bytes| bytes >= 16_384));
    }

    /// Every reclaimable figure was worked out against the rules that have just
    /// been replaced, so keeping them would leave the screen answering the
    /// question the user changed the rule to stop asking.
    #[test]
    fn changing_the_rules_measures_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let modules = dir.path().join("node_modules");
        std::fs::create_dir(&modules).expect("mkdir");
        std::fs::write(modules.join("dep.bin"), vec![b'x'; 16_384]).expect("write");

        let mut app =
            App::open(dir.path(), Rules::default(), now(), UserDirs::default()).expect("open");
        await_sizes(&mut app);
        let claimed = |app: &App| {
            app.entries()
                .iter()
                .find(|entry| entry.name == "node_modules")
                .expect("listed")
                .reclaimable
        };
        assert_eq!(claimed(&app), Some(0), "nothing was watching it");

        app.reload_rules(claiming(dir.path()));
        await_sizes(&mut app);

        assert!(claimed(&app).is_some_and(|bytes| bytes >= 16_384));
    }

    // ---- removing from the browser ---------------------------------------

    /// A rule set that claims `node_modules` under `root`, as the built-ins do.
    fn claiming_here(root: &Path) -> Rules {
        Rules::new(
            vec![Rule {
                name: "junk".into(),
                tier: Tier::Trash,
                parts: vec![Part {
                    root: Some(root.to_string_lossy().into_owned()),
                    includes: vec!["**/node_modules/".into()],
                    ..Part::default()
                }],
                ..Rule::default()
            }],
            &UserDirs::default(),
        )
        .expect("compiles")
    }

    /// Wait for the removing worker to finish and the browser to take it in.
    fn await_removal(app: &mut App) {
        for _ in 0..500 {
            app.settle_removal();
            match app.removal() {
                Some(Removal::Removing { .. }) | None => {}
                Some(Removal::Done { .. }) => return,
                Some(_) => return,
            }
            if app.removal().is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the removal never finished");
    }

    /// Wait for the worker's plan, which is a real walk on a real directory.
    fn await_plan(app: &mut App) {
        for _ in 0..200 {
            app.settle_removal();
            if !matches!(app.removal(), Some(Removal::Planning { .. })) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the plan never arrived");
    }

    #[test]
    fn removing_asks_before_it_does_anything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("project/node_modules")).expect("mkdir");
        std::fs::write(root.join("project/node_modules/a.bin"), vec![b'x'; 4096]).expect("write");

        let mut app =
            App::open(root, claiming_here(root), now(), UserDirs::default()).expect("open");
        point_at(&mut app, "project");
        app.begin_removal();
        await_plan(&mut app);

        assert!(
            matches!(
                app.removal(),
                Some(Removal::Asking {
                    destroys: false,
                    ..
                })
            ),
            "a trash-tier plan asks, and asks gently: {:?}",
            app.removal()
        );
        assert!(
            root.join("project/node_modules/a.bin").exists(),
            "and nothing has happened yet"
        );
    }

    /// The parent row is the way out of here, not a description of anywhere —
    /// removing "through" it would act on a directory the screen is not about.
    #[test]
    fn the_parent_row_removes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("project/node_modules")).expect("mkdir");

        let mut app = App::open(
            dir.path(),
            claiming_here(dir.path()),
            now(),
            UserDirs::default(),
        )
        .expect("open");
        // Opening lands on `..`.
        app.begin_removal();

        assert!(app.removal().is_none());
    }

    /// Only what a rule claims can go from here. A row nothing claims says so
    /// rather than silently doing nothing, which would be indistinguishable
    /// from a key that did not register.
    #[test]
    fn a_row_no_rule_claims_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir(root.join("ordinary")).expect("mkdir");
        std::fs::write(root.join("ordinary/a.bin"), vec![b'x'; 4096]).expect("write");

        let mut app =
            App::open(root, claiming_here(root), now(), UserDirs::default()).expect("open");
        point_at(&mut app, "ordinary");
        app.begin_removal();
        await_plan(&mut app);

        assert!(matches!(app.removal(), Some(Removal::Nothing { .. })));
        assert!(root.join("ordinary/a.bin").exists());
    }

    /// A destroying plan is still one question with two answers — the tier
    /// changes what the modal *says*, in red, not what it takes to agree.
    #[test]
    fn a_destroying_plan_is_announced_and_still_cancellable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("project/node_modules")).expect("mkdir");
        std::fs::write(root.join("project/node_modules/a.bin"), vec![b'x'; 4096]).expect("write");

        let mut rules = claiming_here(root).to_vec();
        rules[0].tier = Tier::Purge;
        let mut app = App::open(
            root,
            Rules::new(rules, &UserDirs::default()).expect("compiles"),
            now(),
            UserDirs::default(),
        )
        .expect("open");
        point_at(&mut app, "project");
        app.begin_removal();
        await_plan(&mut app);

        assert!(matches!(
            app.removal(),
            Some(Removal::Asking { destroys: true, .. })
        ));

        crate::ui::press(&mut app, 'n');
        assert!(app.removal().is_none(), "no is no, whatever the tier");
        assert!(
            root.join("project/node_modules/a.bin").exists(),
            "and nothing was destroyed on the way to saying it"
        );
    }

    /// The gentle case is a yes-or-no question, and both answers are a letter.
    #[test]
    fn a_trashing_plan_takes_a_letter_either_way() {
        for (key, gone) in [('y', true), ('Y', true)] {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            std::fs::create_dir_all(root.join("project/node_modules")).expect("mkdir");
            std::fs::write(root.join("project/node_modules/a.bin"), vec![b'x'; 4096])
                .expect("write");

            let mut app =
                App::open(root, claiming_here(root), now(), UserDirs::default()).expect("open");
            point_at(&mut app, "project");
            app.begin_removal();
            await_plan(&mut app);
            crate::ui::press(&mut app, key);
            await_removal(&mut app);

            assert_eq!(
                !root.join("project/node_modules/a.bin").exists(),
                gone,
                "`{key}` should have removed it"
            );
        }
    }

    #[test]
    fn n_cancels_without_removing_anything() {
        for key in ['n', 'N'] {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            std::fs::create_dir_all(root.join("project/node_modules")).expect("mkdir");
            std::fs::write(root.join("project/node_modules/a.bin"), vec![b'x'; 4096])
                .expect("write");

            let mut app =
                App::open(root, claiming_here(root), now(), UserDirs::default()).expect("open");
            point_at(&mut app, "project");
            app.begin_removal();
            await_plan(&mut app);
            crate::ui::press(&mut app, key);

            assert!(app.removal().is_none(), "`{key}` should have cancelled");
            assert!(root.join("project/node_modules/a.bin").exists());
        }
    }

    /// A removal under one row cannot have changed another, and re-walking the
    /// siblings to learn what was already known is the whole cost of getting
    /// this wrong.
    #[test]
    fn removing_re_walks_only_what_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("project/node_modules")).expect("mkdir");
        std::fs::write(root.join("project/node_modules/a.bin"), vec![b'x'; 4096]).expect("write");
        std::fs::create_dir(root.join("untouched")).expect("mkdir");
        std::fs::write(root.join("untouched/b.bin"), vec![b'y'; 8192]).expect("write");

        let mut app =
            App::open(root, claiming_here(root), now(), UserDirs::default()).expect("open");
        await_sizes(&mut app);
        let before = app
            .entries()
            .iter()
            .find(|entry| entry.name == "untouched")
            .and_then(|entry| entry.size)
            .expect("measured");

        point_at(&mut app, "project");
        app.begin_removal();
        await_plan(&mut app);
        crate::ui::press(&mut app, 'y');
        await_removal(&mut app);

        // Still there, and still known — no walk was needed to say so.
        let after = app
            .entries()
            .iter()
            .find(|entry| entry.name == "untouched")
            .expect("still listed");
        assert_eq!(after.size, Some(before));
        assert!(
            !after.measuring,
            "an untouched sibling must not be walked again"
        );
    }

    /// The row that was removed goes; the filter that found it stays, because
    /// the directory is the same one it was typed in.
    #[test]
    fn removing_re_reads_the_listing_and_keeps_the_filter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("project/node_modules")).expect("mkdir");
        std::fs::write(root.join("project/node_modules/a.bin"), vec![b'x'; 4096]).expect("write");

        // A rule that claims the row itself, so the row goes with it.
        let rules = Rules::new(
            vec![Rule {
                name: "junk".into(),
                tier: Tier::Trash,
                parts: vec![Part {
                    root: Some(root.to_string_lossy().into_owned()),
                    includes: vec!["**/project/".into()],
                    ..Part::default()
                }],
                ..Rule::default()
            }],
            &UserDirs::default(),
        )
        .expect("compiles");

        let mut app = App::open(root, rules, now(), UserDirs::default()).expect("open");
        app.start_filtering();
        for ch in "pro".chars() {
            app.filter_push(ch);
        }
        app.filter_accept();
        point_at(&mut app, "project");

        app.begin_removal();
        await_plan(&mut app);
        crate::ui::press(&mut app, 'y');
        await_removal(&mut app);

        assert!(
            !root.join("project").exists(),
            "the fixture must actually have gone"
        );
        assert!(
            !app.entries().iter().any(|entry| entry.name == "project"),
            "and the row with it: {:?}",
            app.entries().iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        assert_eq!(
            app.filter(),
            "pro",
            "the filter is about this directory, and this is still it"
        );
    }

    /// Abandoning leaves the disk and the screen exactly as they were.
    #[test]
    fn dismissing_changes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("project/node_modules")).expect("mkdir");
        std::fs::write(root.join("project/node_modules/a.bin"), vec![b'x'; 4096]).expect("write");

        let mut app =
            App::open(root, claiming_here(root), now(), UserDirs::default()).expect("open");
        point_at(&mut app, "project");
        let before = app.cursor();
        app.begin_removal();
        await_plan(&mut app);
        app.dismiss_removal();

        assert!(app.removal().is_none());
        assert_eq!(app.cursor(), before);
        assert!(root.join("project/node_modules/a.bin").exists());
    }

    /// The root of the filesystem has no parent, so there is no `..` and
    /// leaving is a no-op rather than an error.
    #[test]
    fn there_is_no_way_up_from_the_top() {
        let mut app = App::open(Path::new("/"), Rules::default(), now(), UserDirs::default())
            .expect("open /");

        assert!(
            !names(&app).contains(&PARENT.to_owned()),
            "no parent row at the root"
        );
        app.leave();
        assert_eq!(app.cwd(), Path::new("/"));
    }
}
