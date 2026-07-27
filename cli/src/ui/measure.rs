//! Sizing the directories on screen, off the drawing thread.
//!
//! **Navigation does not cancel anything.** Walking into a directory while its
//! neighbours were being measured used to throw that work away, so coming back
//! out started it again from nothing — the user watched the same number be
//! computed twice and never saw it finish. A walk in flight is a walk that will
//! be wanted; it runs to the end and its answer goes into the cache whether or
//! not anyone is still looking at that directory.
//!
//! That makes **the absolute path the identity of a result**, not the row it
//! came from. A total belongs to a place on disk and stays true after the screen
//! has moved on.
//!
//! **A walk of a directory subsumes the walks of everything inside it.** Asking
//! for `~` measures `~/Projects` on the way; asking separately for the children
//! of `Projects` while that is under way would do the same work a second time.
//! So a path with an ancestor already being walked is not queued — it is already
//! being measured, and it says so. The core reports each directory as that
//! directory finishes, so the inner answers arrive as the outer walk reaches
//! them rather than all at the end.
//!
//! **One worker, a queue, newest first.** Several workers would each be walking
//! a tree through the same rayon pool, competing for it. A queue bounds that to
//! one, and pushing new requests to the front means the directory being looked
//! at is measured before whatever is left over from the last one — which then
//! resumes rather than being lost.
//!
//! **Totals are remembered for the session.** A size is expensive and does not
//! change on its own, so walking into a directory and back out must not pay for
//! it twice. What the user deletes, the user knows about, and `r` is how they
//! say so.
//!
//! The clock lives here. [`disk_tools_core::measure`] calls back on every
//! directory — thousands a second on a warm cache — and this throttles that to
//! something a screen can use, because the core reads no clock and should not.

use disk_tools_core::{Finished, measure};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often a running total may reach the screen. Below this it is a blur, and
/// above it the figure looks stuck.
const TICK: Duration = Duration::from_millis(80);

/// What one walk has to say, batched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// What was asked for. Absolute, because the screen may have moved on.
    pub root: PathBuf,
    /// How far the walk of `root` has got. Not a total until `complete`.
    pub running_total: u64,
    /// Whether the walk of `root` has finished.
    pub complete: bool,

    /// Directories whose subtrees are done, `root` included once it is.
    ///
    /// Every one of these is final, even if the walk is later cut short: that
    /// subtree finished, whatever happened to its neighbours.
    pub directories: Vec<(PathBuf, u64)>,
}

/// What the worker and the browser share.
struct Work {
    /// Newest first: the directory being looked at is measured before whatever
    /// is left over from the last one.
    queue: VecDeque<PathBuf>,

    /// Whether a worker is alive to take from the queue.
    ///
    /// Under the same lock as `queue`, so a worker deciding to stop and a
    /// request arriving cannot both conclude that the other will handle it.
    working: bool,
}

/// The background sizing of the directories that have been asked about.
pub struct Sizer {
    updates: Receiver<Update>,
    post: Sender<Update>,
    work: Arc<Mutex<Work>>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,

    /// Every total computed this session, by absolute path.
    ///
    /// Grows with the directories visited and is never evicted: a path and a
    /// `u64` against a walk of the subtree is not a trade worth thinking about,
    /// and a browser is closed long before the map is interesting.
    known: HashMap<PathBuf, u64>,

    /// Figures still climbing. Shown, but never treated as totals.
    running: HashMap<PathBuf, u64>,

    /// Queued or being walked.
    pending: HashSet<PathBuf>,
}

impl Sizer {
    pub fn new() -> Self {
        let (post, updates) = channel();
        Sizer {
            updates,
            post,
            work: Arc::new(Mutex::new(Work {
                queue: VecDeque::new(),
                working: false,
            })),
            cancel: Arc::new(AtomicBool::new(false)),
            worker: None,
            known: HashMap::new(),
            running: HashMap::new(),
            pending: HashSet::new(),
        }
    }

    /// The best figure for `path`: its total, or how far a walk has got.
    pub fn size_of(&self, path: &Path) -> Option<u64> {
        self.known
            .get(path)
            .or_else(|| self.running.get(path))
            .copied()
    }

    /// Whether a walk that will produce `path` is queued or under way.
    ///
    /// An ancestor being walked counts: that walk reaches here on its way, and
    /// reports this directory when it does.
    pub fn is_measuring(&self, path: &Path) -> bool {
        self.pending.contains(path) || self.covered(path)
    }

    /// Whether some directory above `path` is already being walked.
    fn covered(&self, path: &Path) -> bool {
        path.ancestors()
            .skip(1)
            .any(|above| self.pending.contains(above))
    }

    /// Ask for the sizes of `paths`, skipping anything known or already asked
    /// for.
    ///
    /// Nothing is cancelled: a walk in flight is a walk that will be wanted.
    pub fn request(&mut self, paths: Vec<PathBuf>) {
        let wanted: Vec<PathBuf> = paths
            .into_iter()
            .filter(|path| {
                !self.known.contains_key(path)
                    && !self.pending.contains(path)
                    // A walk already under way above here will measure this on
                    // its way through. Queueing it would be the same work twice.
                    && !self.covered(path)
            })
            .collect();
        if wanted.is_empty() {
            return;
        }

        let mut spawn = false;
        {
            let mut work = self.work.lock().expect("no panics under this lock");
            // Reversed, so the front of the queue ends up in listing order.
            for path in wanted.iter().rev() {
                work.queue.push_front(path.clone());
            }
            if !work.working {
                work.working = true;
                spawn = true;
            }
        }
        self.pending.extend(wanted);

        if spawn {
            // Any previous worker has already stopped taking from the queue, so
            // this one replaces it. Its handle is dropped rather than joined:
            // waiting here is what used to make a keypress stick.
            self.worker = Some(self.start());
        }
    }

    fn start(&self) -> JoinHandle<()> {
        let work = Arc::clone(&self.work);
        let cancel = Arc::clone(&self.cancel);
        let post = self.post.clone();

        std::thread::spawn(move || {
            loop {
                let next = {
                    let mut work = work.lock().expect("no panics under this lock");
                    match work.queue.pop_front() {
                        Some(path) if !cancel.load(Ordering::Relaxed) => path,
                        // Decided under the lock a request would have to take to
                        // add work, so nothing is left queued with no one to
                        // take it.
                        _ => {
                            work.working = false;
                            return;
                        }
                    }
                };

                // Batched behind the tick rather than sent per directory: a
                // home directory is a hundred thousand of them, and the screen
                // redraws twelve times a second.
                let batch = Mutex::new((Instant::now(), Vec::new(), 0u64));

                // Scoped so the closure stops borrowing `next` before the final
                // update takes ownership of it.
                let measured = {
                    let report = |done: Finished<'_>| {
                        let mut batch = batch.lock().expect("no panics under this lock");
                        batch.1.push((done.path.to_path_buf(), done.allocated));
                        batch.2 = done.running_total;

                        if batch.0.elapsed() < TICK {
                            return;
                        }
                        batch.0 = Instant::now();
                        let _ = post.send(Update {
                            root: next.clone(),
                            running_total: batch.2,
                            complete: false,
                            directories: std::mem::take(&mut batch.1),
                        });
                    };
                    measure(&next, &cancel, &report)
                };

                // Whatever the tick did not carry, plus the verdict. Sent even
                // when the walk was cut short: the subtrees in it did finish,
                // and only `complete` says whether `root` itself did.
                let leftover =
                    std::mem::take(&mut batch.lock().expect("no panics under this lock").1);
                let _ = post.send(Update {
                    root: next,
                    running_total: measured.allocated,
                    complete: measured.complete,
                    directories: leftover,
                });
            }
        })
    }

    /// Take in everything the worker has posted.
    ///
    /// Returns whether any walk finished — the only thing that can change an
    /// order, and so the only thing worth re-sorting for.
    pub fn absorb(&mut self) -> bool {
        let mut completed = false;
        loop {
            let update = match self.updates.try_recv() {
                Ok(update) => update,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return completed,
            };

            // Each of these is a subtree that finished, whatever became of the
            // walk carrying it — so they are totals even when `complete` is not.
            for (path, allocated) in update.directories {
                self.running.remove(&path);
                self.pending.remove(&path);
                self.known.insert(path, allocated);
                completed = true;
            }

            if update.complete {
                self.running.remove(&update.root);
                self.pending.remove(&update.root);
                completed = true;
            } else if !self.known.contains_key(&update.root) {
                self.running.insert(update.root, update.running_total);
            }
        }
    }

    /// Drop what is known about `path`, so it is walked again when asked for.
    pub fn forget(&mut self, path: &Path) {
        self.known.remove(path);
        self.running.remove(path);
    }

    /// Stop the worker and wait for it.
    ///
    /// The one place that waits, and the reason the core reads its cancel flag
    /// as often as it does: nothing may outlive the browser.
    pub fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Sizer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wait until every path asked about has a total.
    ///
    /// Bounded rather than a bare loop — a test that spins on a background
    /// thread hangs the whole suite when the thread never posts.
    fn settle(sizer: &mut Sizer, paths: &[PathBuf]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            sizer.absorb();
            if paths.iter().all(|path| sizer.known.contains_key(path)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("never settled: {paths:?}");
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["alpha", "beta"] {
            let sub = dir.path().join(name);
            std::fs::create_dir(&sub).expect("mkdir");
            std::fs::write(sub.join("f.bin"), vec![b'x'; 8192]).expect("write");
        }
        dir
    }

    /// Enough directories that a walk is still going when the next thing
    /// happens.
    fn big(root: &Path, name: &str) -> PathBuf {
        let tree = root.join(name);
        std::fs::create_dir(&tree).expect("mkdir");
        for n in 0..800 {
            let sub = tree.join(format!("sub{n}"));
            std::fs::create_dir(&sub).expect("mkdir");
            std::fs::write(sub.join("f.bin"), vec![b'x'; 4096]).expect("write");
        }
        tree
    }

    #[test]
    fn a_directory_is_sized_and_then_known() {
        let dir = fixture();
        let alpha = dir.path().join("alpha");
        let mut sizer = Sizer::new();

        assert!(sizer.size_of(&alpha).is_none());
        sizer.request(vec![alpha.clone()]);
        assert!(sizer.is_measuring(&alpha), "and says it is working on it");

        settle(&mut sizer, std::slice::from_ref(&alpha));

        assert!(sizer.size_of(&alpha).is_some_and(|size| size >= 8192));
        assert!(!sizer.is_measuring(&alpha));
    }

    #[test]
    fn every_requested_directory_is_sized() {
        let dir = fixture();
        let paths = vec![dir.path().join("alpha"), dir.path().join("beta")];
        let mut sizer = Sizer::new();

        sizer.request(paths.clone());
        settle(&mut sizer, &paths);

        assert!(paths.iter().all(|path| sizer.size_of(path).is_some()));
    }

    /// The bug this design exists to prevent: a walk abandoned because the user
    /// stepped into a directory, and started again from nothing on the way out.
    #[test]
    fn a_walk_in_flight_survives_the_screen_moving_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = big(dir.path(), "tree");
        let mut sizer = Sizer::new();

        sizer.request(vec![tree.clone()]);
        // Whatever the user does next, it is not a reason to stop.
        sizer.request(vec![dir.path().join("elsewhere")]);
        settle(&mut sizer, std::slice::from_ref(&tree));

        assert!(sizer.size_of(&tree).is_some_and(|size| size >= 800 * 4096));
    }

    /// Asking again for something already under way must not queue it twice.
    #[test]
    fn asking_twice_walks_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = big(dir.path(), "tree");
        let mut sizer = Sizer::new();

        sizer.request(vec![tree.clone()]);
        sizer.request(vec![tree.clone()]);
        sizer.request(vec![tree]);

        let queued = sizer.work.lock().expect("lock").queue.len();
        assert!(queued <= 1, "{queued} copies queued");
    }

    /// And something already known is not walked at all.
    #[test]
    fn asking_for_a_known_size_starts_nothing() {
        let dir = fixture();
        let alpha = dir.path().join("alpha");
        let mut sizer = Sizer::new();

        sizer.request(vec![alpha.clone()]);
        settle(&mut sizer, std::slice::from_ref(&alpha));

        sizer.request(vec![alpha.clone()]);

        assert!(!sizer.is_measuring(&alpha));
        assert!(sizer.work.lock().expect("lock").queue.is_empty());
    }

    /// The screen must not wait on a subtree walk to answer a keypress.
    #[test]
    fn requesting_never_waits_for_what_is_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = big(dir.path(), "tree");
        let mut sizer = Sizer::new();

        sizer.request(vec![tree]);
        let started = Instant::now();
        sizer.request(vec![dir.path().join("elsewhere")]);
        let waited = started.elapsed();

        assert!(waited < Duration::from_millis(50), "took {waited:?}");
    }

    /// Forgetting is what `r` is for; without it a deleted subtree keeps its old
    /// total forever.
    #[test]
    fn forgetting_lets_a_directory_be_walked_again() {
        let dir = fixture();
        let alpha = dir.path().join("alpha");
        let mut sizer = Sizer::new();

        sizer.request(vec![alpha.clone()]);
        settle(&mut sizer, std::slice::from_ref(&alpha));
        sizer.forget(&alpha);
        assert!(sizer.size_of(&alpha).is_none());

        sizer.request(vec![alpha.clone()]);
        settle(&mut sizer, std::slice::from_ref(&alpha));

        assert!(sizer.size_of(&alpha).is_some());
    }

    /// A queue the previous worker left behind has to be picked up again, or a
    /// request that arrives a moment too late is never served.
    #[test]
    fn a_request_after_the_worker_has_finished_starts_another() {
        let dir = fixture();
        let alpha = dir.path().join("alpha");
        let beta = dir.path().join("beta");
        let mut sizer = Sizer::new();

        sizer.request(vec![alpha.clone()]);
        settle(&mut sizer, &[alpha]);
        // By now the worker has run out of work and stopped, or is about to.
        sizer.request(vec![beta.clone()]);
        settle(&mut sizer, std::slice::from_ref(&beta));

        assert!(sizer.size_of(&beta).is_some());
    }

    #[test]
    fn stopping_waits_for_the_worker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = big(dir.path(), "tree");
        let mut sizer = Sizer::new();

        sizer.request(vec![tree]);
        sizer.stop();

        assert!(sizer.worker.is_none(), "waited for, and gone");
    }

    /// Nothing was completed, so nothing may claim to be a size.
    #[test]
    fn a_cancelled_walk_never_becomes_a_total() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = big(dir.path(), "tree");
        let mut sizer = Sizer::new();

        sizer.request(vec![tree.clone()]);
        sizer.stop();
        sizer.absorb();

        assert!(
            !sizer.known.contains_key(&tree),
            "a partial figure is not a size"
        );
    }

    /// The bug: walking into `Projects` while `~` was measuring it queued each
    /// of its children as well, so the same tree was walked twice — and coming
    /// back out, the outer walk was still going over ground already covered.
    #[test]
    fn a_walk_already_running_above_is_not_asked_for_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = big(dir.path(), "tree");
        let mut sizer = Sizer::new();

        sizer.request(vec![tree.clone()]);
        // What entering it looks like: every child asked about at once.
        let children: Vec<PathBuf> = (0..800).map(|n| tree.join(format!("sub{n}"))).collect();
        sizer.request(children.clone());

        assert!(
            sizer.work.lock().expect("lock").queue.len() <= 1,
            "the children are already covered by the walk above them"
        );
        assert!(
            children.iter().all(|path| sizer.is_measuring(path)),
            "and they say so, so the screen shows a spinner rather than nothing"
        );

        settle(&mut sizer, std::slice::from_ref(&tree));
        assert!(children.iter().all(|path| sizer.size_of(path).is_some()));
    }

    /// The inner answers have to arrive while the outer walk is still going, or
    /// stepping inside shows an empty column until the whole thing finishes.
    #[test]
    fn subtrees_are_known_before_the_walk_above_them_finishes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = big(dir.path(), "tree");
        let mut sizer = Sizer::new();

        sizer.request(vec![tree.clone()]);

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            sizer.absorb();
            let inner = sizer.size_of(&tree.join("sub0")).is_some();
            if inner && sizer.size_of(&tree).is_none() {
                return; // a child was known while the parent still was not
            }
            if sizer.size_of(&tree).is_some() {
                // The walk finished before the check could catch it in the act;
                // the children still have to be there.
                assert!(inner);
                return;
            }
            assert!(Instant::now() < deadline, "never settled");
        }
    }

    /// The walk of one directory visits everything beneath it, and says so.
    #[test]
    fn a_completed_walk_reports_every_directory_it_visited() {
        let dir = fixture();
        let alpha = dir.path().join("alpha");
        std::fs::create_dir(alpha.join("inner")).expect("mkdir");
        std::fs::write(alpha.join("inner/f.bin"), vec![b'x'; 4096]).expect("write");
        let mut sizer = Sizer::new();

        sizer.request(vec![alpha.clone()]);
        settle(&mut sizer, std::slice::from_ref(&alpha));

        assert!(
            sizer
                .size_of(&alpha.join("inner"))
                .is_some_and(|size| size >= 4096),
            "the subtree is known without ever being asked for"
        );
    }
}
