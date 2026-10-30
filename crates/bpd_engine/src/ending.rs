//! the programs bpd started, and asking them to end
//!
//! a debugged program is bpd's child, and nothing else names it: whatever stops
//! bpd — an editor's Stop, `docker stop`, a supervisor, `kill` — reaches bpd
//! and only bpd. so the asking is done from here, for every program bpd started
//! and has not seen end, and it is done **once**: a client's `terminate` and a
//! `SIGTERM` to bpd arriving together are one request to end, and a program
//! that is handling the first must not be interrupted by the second
//!
//! a program is signalled only while it is unreaped. the lock on its [`Child`]
//! is held across the check and the signal, so it cannot be reaped in between —
//! a reaped process id is one the system can hand to another process, which the
//! signal would then reach

use std::io;
use std::process::{Child, Command};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

/// a program bpd started
#[derive(Debug)]
pub(crate) struct Started {
    child: Mutex<Child>,
    /// whether it has been asked to end — a `SIGTERM`, from either route
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "windows has nothing to ask with")
    )]
    asked: AtomicBool,
}

impl Started {
    /// the process, for whatever reads or reaps it
    pub(crate) fn child(&self) -> MutexGuard<'_, Child> {
        // a `Child` is never left half-written: every path through it is one
        // call into std
        self.child.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// every program this process started, for as long as anything holds one
static STARTED: Mutex<Vec<Weak<Started>>> = Mutex::new(Vec::new());

fn started() -> MutexGuard<'static, Vec<Weak<Started>>> {
    STARTED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// start the program `command` describes, as one an ending is passed on to
///
/// the registry is held while it starts, so a signal arriving in the middle
/// waits for it and reaches it rather than finding nothing started and ending
/// bpd without it
pub(crate) fn spawn(command: &mut Command) -> io::Result<Arc<Started>> {
    let mut all = started();
    let child = command.spawn()?;
    let started = Arc::new(Started {
        child: Mutex::new(child),
        asked: AtomicBool::new(false),
    });
    all.retain(|held| held.strong_count() > 0);
    all.push(Arc::downgrade(&started));
    Ok(started)
}

/// a signal that asks a process to end
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// `SIGTERM`: asked to end, by whatever is in charge of it
    Terminate,
    /// `SIGINT`: Ctrl+C
    Interrupt,
    /// `SIGHUP`: its terminal went away
    Hangup,
    /// `SIGQUIT`: Ctrl+\
    Quit,
}

impl Ending {
    /// its number, which is what `128 +` an exit is made of
    #[cfg(unix)]
    pub fn number(self) -> i32 {
        self.signal().as_raw()
    }

    #[cfg(unix)]
    fn signal(self) -> rustix::process::Signal {
        use rustix::process::Signal;
        match self {
            Self::Terminate => Signal::TERM,
            Self::Interrupt => Signal::INT,
            Self::Hangup => Signal::HUP,
            Self::Quit => Signal::QUIT,
        }
    }
}

/// what passing an ending on came to
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Passed {
    /// bpd has started no program, so there is nothing to pass it on to
    NothingStarted,
    /// every program bpd started had already ended, or had already been asked
    Nowhere,
    /// it reached this many programs
    To(usize),
}

/// pass a signal bpd was sent on to every program it started that is still
/// running
///
/// a `SIGTERM` reaches each program at most once, whichever route asked first —
/// see [`ask_to_end`]. the others are passed on every time: they are not
/// requests to end bpd's way but the program's own keys and terminal
pub fn pass_on(ending: Ending) -> Passed {
    let all = started();
    if all.is_empty() {
        return Passed::NothingStarted;
    }
    let reached = all
        .iter()
        .filter_map(Weak::upgrade)
        .filter(|started| signal(started, ending))
        .count();
    if reached == 0 {
        Passed::Nowhere
    } else {
        Passed::To(reached)
    }
}

/// ask one program to end, the graceful way: a `SIGTERM` it can handle
///
/// `false` when it was not sent — the program has ended, or was already asked
pub(crate) fn ask_to_end(started: &Started) -> bool {
    signal(started, Ending::Terminate)
}

#[cfg(unix)]
fn signal(started: &Started, ending: Ending) -> bool {
    let mut child = started.child();
    // `try_wait` reaps a program that has ended and std keeps its status for
    // the `wait` that comes after. one still running stays unreaped for as long
    // as the lock is held, so its id is still its own when the signal goes
    if !matches!(child.try_wait(), Ok(None)) {
        return false;
    }
    if ending == Ending::Terminate
        && started
            .asked
            .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return false;
    }
    let Some(pid) = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    rustix::process::kill_process(pid, ending.signal()).is_ok()
}

/// windows has no signal to pass on: Ctrl+C and Ctrl+Break reach every process
/// on the console, the program among them, and there is nothing a process can
/// send another that it may handle and then end on its own
#[cfg(not(unix))]
const fn signal(_started: &Started, _ending: Ending) -> bool {
    false
}
