//! Getting into the alternate screen, and — the part that matters — back out.
//!
//! A program that dies inside the alternate screen with raw mode still on
//! leaves the user in their own shell with **no echo and no cursor**: they type
//! `reset` blind, not seeing what they are typing. That is the worst way a disk
//! utility can end, and this module exists to make it unreachable.
//!
//! It takes **two** mechanisms, and neither covers the other:
//!
//! | | Catches | Misses |
//! |---|---|---|
//! | [`Screen`]'s `Drop` | ordinary exit, `?`, unwinding | `panic = "abort"`, `process::exit` |
//! | The panic hook | a panic under any setting | ordinary exit |
//!
//! Both are idempotent, because during a panic they both run — the hook at
//! panic time, `Drop` as the stack unwinds — and leaving the alternate screen
//! twice writes rubbish over whatever the shell has drawn since.
//!
//! Everything here is generic over [`Host`] for one reason: **CI has no
//! terminal**, and the half that breaks echo is raw mode. A test drives a
//! recording `Host` and asserts the exact restore sequence.

use ratatui::crossterm::{
    cursor::Show,
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

/// The four things this program does to a terminal.
///
/// A trait rather than direct `crossterm` calls so that restoration is testable
/// where no terminal exists — which is every CI runner this project uses.
pub trait Host {
    fn enable_raw(&mut self) -> io::Result<()>;
    fn enter_alternate(&mut self) -> io::Result<()>;
    fn leave_alternate(&mut self) -> io::Result<()>;
    fn disable_raw(&mut self) -> io::Result<()>;

    /// Unconditionally, on every way out.
    ///
    /// Nothing here *hides* the cursor — `ratatui` does, on its first draw —
    /// but on most terminals that is a global setting rather than a property of
    /// the alternate buffer, so leaving the screen does not bring it back. A
    /// panic would therefore end with an invisible cursor, which is half of the
    /// failure this module exists to prevent. Showing it costs one sequence and
    /// is harmless when it was never hidden.
    fn show_cursor(&mut self) -> io::Result<()>;
}

/// The real one.
pub struct Crossterm;

impl Host for Crossterm {
    fn enable_raw(&mut self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn enter_alternate(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnterAlternateScreen)?;
        INSIDE.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn leave_alternate(&mut self) -> io::Result<()> {
        INSIDE.store(false, Ordering::SeqCst);
        execute!(io::stdout(), LeaveAlternateScreen)
    }

    fn disable_raw(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(io::stdout(), Show)
    }
}

/// Whether *this process* has crossterm's alternate screen on.
///
/// Global because the panic hook is `'static` and cannot hold a [`Screen`]; it
/// is what stops the hook and `Drop` from both restoring during one panic.
///
/// Owned by [`Crossterm`] rather than by [`Screen`], and that is not tidiness:
/// tests run in parallel threads of one process, so a `Screen` over a test
/// `Host` touching this would let one test's state leak into another's.
static INSIDE: AtomicBool = AtomicBool::new(false);

/// Holds the terminal, and gives it back when dropped.
pub struct Screen<H: Host> {
    host: H,
    inside: bool,
}

impl<H: Host> Screen<H> {
    /// Enter raw mode and the alternate screen, in that order.
    ///
    /// If the second half fails the first is undone before returning: a process
    /// that left raw mode on and then exited reporting an error would have
    /// broken the terminal by way of *reporting* a problem.
    pub fn enter(mut host: H) -> io::Result<Self> {
        host.enable_raw()?;
        if let Err(err) = host.enter_alternate() {
            let _ = host.disable_raw();
            return Err(err);
        }
        Ok(Screen { host, inside: true })
    }

    /// Give the terminal back. Doing it twice does nothing the second time.
    pub fn leave(&mut self) -> io::Result<()> {
        if !self.inside {
            return Ok(());
        }
        self.inside = false;

        // Reverse of `enter`, and every step is attempted even if an earlier
        // one fails: raw mode left on and a hidden cursor are exactly what a
        // user notices, and stopping at the first error would leave one of them.
        let left = self.host.leave_alternate();
        let raw = self.host.disable_raw();
        let cursor = self.host.show_cursor();
        left.and(raw).and(cursor)
    }
}

impl<H: Host> Drop for Screen<H> {
    fn drop(&mut self) {
        // Nothing useful to do with an error here — the process is on its way
        // out, and returning it is not an option a destructor has.
        let _ = self.leave();
    }
}

/// Restore, but only if something is still to restore.
///
/// Split from the hook so the decision can be tested: the hook itself calls
/// `crossterm` against a real terminal, which no test here has.
fn restore_if_inside(host: &mut impl Host, inside: &AtomicBool) {
    if inside.swap(false, Ordering::SeqCst) {
        let _ = host.leave_alternate();
        let _ = host.disable_raw();
        let _ = host.show_cursor();
    }
}

/// Put the terminal back before the panic message is printed.
///
/// Chains to whatever hook was installed rather than replacing it, so the
/// message, the backtrace and any test harness reporting still happen — the
/// terminal is simply usable enough to read them.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_if_inside(&mut Crossterm, &INSIDE);
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum Call {
        EnableRaw,
        EnterAlternate,
        LeaveAlternate,
        DisableRaw,
        ShowCursor,
    }

    /// Shares its log, so a test can still read what happened after the host
    /// has been moved into — or dropped with — a `Screen`.
    #[derive(Default)]
    struct Recorder {
        calls: Rc<RefCell<Vec<Call>>>,
        fail_enter_alternate: bool,
    }

    impl Recorder {
        fn log(&self) -> Rc<RefCell<Vec<Call>>> {
            Rc::clone(&self.calls)
        }

        fn push(&self, call: Call) {
            self.calls.borrow_mut().push(call);
        }
    }

    impl Host for Recorder {
        fn enable_raw(&mut self) -> io::Result<()> {
            self.push(Call::EnableRaw);
            Ok(())
        }

        fn enter_alternate(&mut self) -> io::Result<()> {
            self.push(Call::EnterAlternate);
            if self.fail_enter_alternate {
                return Err(io::Error::other("the terminal said no"));
            }
            Ok(())
        }

        fn leave_alternate(&mut self) -> io::Result<()> {
            self.push(Call::LeaveAlternate);
            Ok(())
        }

        fn disable_raw(&mut self) -> io::Result<()> {
            self.push(Call::DisableRaw);
            Ok(())
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.push(Call::ShowCursor);
            Ok(())
        }
    }

    /// The whole point, in one assertion: what happens to the terminal, in what
    /// order, when the screen simply goes out of scope.
    #[test]
    fn dropping_the_screen_undoes_exactly_what_entering_it_did() {
        let host = Recorder::default();
        let log = host.log();

        drop(Screen::enter(host).expect("enter"));

        assert_eq!(
            *log.borrow(),
            vec![
                Call::EnableRaw,
                Call::EnterAlternate,
                Call::LeaveAlternate,
                Call::DisableRaw,
                Call::ShowCursor
            ],
            "restoration undoes the setup, and shows the cursor ratatui hid"
        );
    }

    /// During a panic both the hook and `Drop` run. Leaving the alternate
    /// screen twice writes over whatever the shell has drawn since.
    #[test]
    fn leaving_twice_does_nothing_the_second_time() {
        let host = Recorder::default();
        let log = host.log();
        let mut screen = Screen::enter(host).expect("enter");

        screen.leave().expect("first");
        let after_first = log.borrow().len();
        screen.leave().expect("second");

        assert_eq!(log.borrow().len(), after_first, "no second restore");
    }

    /// Reporting a failure is no excuse for leaving the terminal in raw mode —
    /// the user would not be able to read the report.
    #[test]
    fn a_failed_enter_undoes_the_half_that_succeeded() {
        let host = Recorder {
            fail_enter_alternate: true,
            ..Recorder::default()
        };
        let log = host.log();

        assert!(Screen::enter(host).is_err(), "entering must fail");

        assert_eq!(
            *log.borrow(),
            vec![Call::EnableRaw, Call::EnterAlternate, Call::DisableRaw],
            "raw mode was turned back off on the way out"
        );
    }

    /// `Drop` runs while the stack unwinds, which is the common panic and the
    /// one a test can prove. `panic = "abort"` is what the hook is for.
    ///
    /// `catch_unwind` rather than `#[should_panic]`, because the assertion has
    /// to happen **after** the unwind. Asserting inside a `Drop` would panic
    /// while panicking, which aborts the process and takes the whole test binary
    /// with it rather than failing one test.
    #[test]
    fn the_screen_is_given_back_while_unwinding() {
        let host = Recorder::default();
        let log = host.log();

        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _screen = Screen::enter(host).expect("enter");
            panic!("deliberate");
        }));

        assert!(unwound.is_err(), "the panic must have happened");
        assert_eq!(
            *log.borrow(),
            vec![
                Call::EnableRaw,
                Call::EnterAlternate,
                Call::LeaveAlternate,
                Call::DisableRaw,
                Call::ShowCursor
            ],
            "unwinding gives the terminal back exactly as an ordinary exit does"
        );
    }

    /// The hook's decision, apart from the `crossterm` calls no test can make.
    #[test]
    fn the_hook_restores_once_and_only_when_inside() {
        let mut host = Recorder::default();
        let log = host.log();
        let inside = AtomicBool::new(true);

        restore_if_inside(&mut host, &inside);
        assert_eq!(
            *log.borrow(),
            vec![Call::LeaveAlternate, Call::DisableRaw, Call::ShowCursor],
            "a panic must not end with an invisible cursor either"
        );
        assert!(!inside.load(Ordering::SeqCst));

        restore_if_inside(&mut host, &inside);
        assert_eq!(
            log.borrow().len(),
            3,
            "already out: a second panic must not restore again"
        );
    }

    #[test]
    fn the_hook_does_nothing_outside_the_screen() {
        let mut host = Recorder::default();
        let log = host.log();

        restore_if_inside(&mut host, &AtomicBool::new(false));

        assert!(log.borrow().is_empty(), "nothing to give back");
    }
}
