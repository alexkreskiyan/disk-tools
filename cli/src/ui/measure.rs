//! Sizing the directories on screen, off the drawing thread.
//!
//! One worker per request, measuring each subdirectory in turn and posting the
//! running total back. The screen never blocks on it: it drains whatever has
//! arrived each frame and draws that.
//!
//! **A new request stops the old one** rather than racing it. Two things are
//! needed for that and neither is sufficient alone: the cancel flag, which stops
//! the work, and the generation number, which discards results already in flight
//! when the flag was set. Without the flag a walk of a huge tree keeps burning
//! the same rayon pool its replacement is queued behind; without the generation,
//! a size for the directory you just left lands in the directory you are in.
//!
//! The clock lives here. [`disk_tools_core::measure`] calls back on every
//! directory — thousands a second on a warm cache — and this throttles that to
//! something a screen can use, because the core reads no clock and should not.

use disk_tools_core::measure;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often a running total may reach the screen. Below this it is a blur, and
/// above it the figure looks stuck.
const TICK: Duration = Duration::from_millis(80);

/// One directory's size, as it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// Which request this belongs to. Anything but the current one is dropped.
    pub generation: u64,
    /// The entry in the listing, by name — the browser may have re-sorted since.
    pub name: OsString,
    pub allocated: u64,
    /// Whether this is the total or a figure still climbing.
    pub complete: bool,
}

/// The background sizing of whatever directory is open.
pub struct Sizer {
    updates: Receiver<Update>,
    post: Sender<Update>,
    generation: u64,
    running: Option<Job>,
}

struct Job {
    cancel: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Sizer {
    pub fn new() -> Self {
        let (post, updates) = channel();
        Sizer {
            updates,
            post,
            generation: 0,
            running: None,
        }
    }

    /// The request whose results are worth keeping.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Measure `names` under `parent`, abandoning anything already in flight.
    ///
    /// Returns the new generation. Sizing nothing still bumps it, so a listing
    /// with no subdirectories invalidates the previous one's results too.
    pub fn start(&mut self, parent: PathBuf, names: Vec<OsString>) -> u64 {
        self.stop();
        self.generation += 1;
        let generation = self.generation;

        if names.is_empty() {
            return generation;
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let post = self.post.clone();

        let thread = std::thread::spawn(move || {
            for name in names {
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                // Scoped so the throttling closure stops borrowing `name` before
                // the final update takes ownership of it.
                let measured = {
                    let sent = Mutex::new(Instant::now());
                    let report = |allocated| {
                        let mut last = sent.lock().expect("no panics under this lock");
                        if last.elapsed() < TICK {
                            return;
                        }
                        *last = Instant::now();
                        let _ = post.send(Update {
                            generation,
                            name: name.clone(),
                            allocated,
                            complete: false,
                        });
                    };
                    measure(&parent.join(&name), &flag, &report)
                };
                // The final figure goes out unthrottled, and is the only one
                // marked complete. A partial total from a cancelled walk is not
                // a size, so it is not sent at all.
                if measured.complete {
                    let _ = post.send(Update {
                        generation,
                        name,
                        allocated: measured.allocated,
                        complete: true,
                    });
                }
            }
        });

        self.running = Some(Job { cancel, thread });
        generation
    }

    /// Everything that has arrived since the last frame.
    ///
    /// Never blocks: a frame with no news draws the same numbers again.
    pub fn drain(&self) -> Vec<Update> {
        let mut updates = Vec::new();
        loop {
            match self.updates.try_recv() {
                Ok(update) => updates.push(update),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return updates,
            }
        }
    }

    /// Stop the worker and wait for it.
    ///
    /// Waiting is the point: the flag is read on entering every directory, so
    /// this returns in about the time one directory takes, and the thread is
    /// provably gone rather than merely asked to leave.
    pub fn stop(&mut self) {
        if let Some(job) = self.running.take() {
            job.cancel.store(true, Ordering::Relaxed);
            let _ = job.thread.join();
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

    /// Wait for a complete update for each of `names`.
    ///
    /// One call for all of them, because `drain` empties the channel: two
    /// separate waits would have the first discard the second's answer.
    ///
    /// Bounded rather than a bare loop — a test that spins on a background
    /// thread hangs the whole suite when the thread never posts.
    fn await_complete(sizer: &Sizer, names: &[&str]) -> Vec<Update> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut done: Vec<Update> = Vec::new();
        while Instant::now() < deadline {
            for update in sizer.drain() {
                if update.complete && names.iter().any(|name| update.name == *name) {
                    done.push(update);
                }
            }
            if done.len() == names.len() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        done
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

    #[test]
    fn a_directory_is_sized_and_reported_complete() {
        let dir = fixture();
        let mut sizer = Sizer::new();

        let generation = sizer.start(dir.path().to_path_buf(), vec![OsString::from("alpha")]);
        let done = await_complete(&sizer, &["alpha"]);

        let update = done.first().expect("a total arrives");
        assert_eq!(update.generation, generation);
        assert!(update.allocated >= 8192, "{}", update.allocated);
    }

    #[test]
    fn every_named_directory_is_sized() {
        let dir = fixture();
        let mut sizer = Sizer::new();

        sizer.start(
            dir.path().to_path_buf(),
            vec![OsString::from("alpha"), OsString::from("beta")],
        );

        let done = await_complete(&sizer, &["alpha", "beta"]);

        assert_eq!(done.len(), 2, "{done:?}");
    }

    /// The generation is what keeps a size for the directory you left out of the
    /// directory you are in.
    #[test]
    fn restarting_moves_to_a_new_generation() {
        let dir = fixture();
        let mut sizer = Sizer::new();

        let first = sizer.start(dir.path().to_path_buf(), vec![OsString::from("alpha")]);
        let second = sizer.start(dir.path().to_path_buf(), vec![OsString::from("beta")]);

        assert!(second > first);
        assert_eq!(sizer.generation(), second);
        for update in sizer.drain() {
            assert!(
                update.generation <= second,
                "nothing from the future: {update:?}"
            );
        }
    }

    /// An empty listing still invalidates: otherwise walking into a directory
    /// with no subdirectories would leave the previous one's figures on screen.
    #[test]
    fn sizing_nothing_still_bumps_the_generation() {
        let dir = fixture();
        let mut sizer = Sizer::new();

        let before = sizer.generation();
        let after = sizer.start(dir.path().to_path_buf(), Vec::new());

        assert_eq!(after, before + 1);
    }

    /// `stop` joins rather than signals, so nothing outlives the browser.
    #[test]
    fn stopping_waits_for_the_worker() {
        let dir = big();
        let mut sizer = Sizer::new();

        sizer.start(dir.path().to_path_buf(), vec![OsString::from("tree")]);
        sizer.stop();

        assert!(sizer.running.is_none(), "the handle is gone, having joined");
    }

    /// Nothing was completed, so nothing may claim to be a size.
    #[test]
    fn a_cancelled_walk_never_reports_a_total() {
        let dir = big();
        let mut sizer = Sizer::new();

        sizer.start(dir.path().to_path_buf(), vec![OsString::from("tree")]);
        sizer.stop();

        assert!(
            sizer.drain().iter().all(|update| !update.complete),
            "a partial figure is not a size"
        );
    }

    /// Enough directories that cancellation lands mid-walk rather than after it.
    ///
    /// Wide rather than deep: 200 nested levels is a path long enough for the
    /// filesystem to refuse, which it duly did.
    fn big() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = dir.path().join("tree");
        std::fs::create_dir(&tree).expect("mkdir");
        for n in 0..800 {
            let sub = tree.join(format!("sub{n}"));
            std::fs::create_dir(&sub).expect("mkdir");
            std::fs::write(sub.join("f.bin"), vec![b'x'; 4096]).expect("write");
        }
        dir
    }
}
