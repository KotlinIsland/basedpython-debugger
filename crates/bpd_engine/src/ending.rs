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
//! signal would then reach. and it is **never held across waiting for the
//! program**, see [`Started::wait`]: a program still running is exactly the one
//! a signal has to be able to reach

use std::io;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

/// a program bpd started
#[derive(Debug)]
pub(crate) struct Started {
    child: Mutex<Child>,
    /// whether it has been asked to end — a `SIGTERM`, from either route
    asked: AtomicBool,
}

impl Started {
    /// the process, for whatever reads or reaps it
    pub(crate) fn child(&self) -> MutexGuard<'_, Child> {
        // a `Child` is never left half-written: every path through it is one
        // call into std
        self.child.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// wait for it to end, and reap it
    ///
    /// looked at rather than blocked on, because `Child::wait` would hold the
    /// lock for as long as the program runs, and everything that ends a program
    /// goes through that lock. the connection closing is what sends the engine
    /// here and a process can run on after that, by closing the descriptor or
    /// in a slow exit: blocking made it the one program nothing could end
    pub(crate) fn wait(&self) -> io::Result<ExitStatus> {
        let mut pause = Duration::from_millis(1);
        loop {
            if let Some(status) = self.child().try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(20));
        }
    }

    /// whether it has been sent the `SIGTERM` that asks it to end, from either
    /// route. never on windows, which has nothing to ask with
    pub(crate) fn asked(&self) -> bool {
        self.asked.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// whether it has not ended yet
    fn running(&self) -> bool {
        matches!(self.child().try_wait(), Ok(None))
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

/// what is left for a signal to reach
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Passed {
    /// bpd has started no program
    NothingStarted,
    /// every program bpd started has ended
    Over,
    /// a program is running, and has been passed the signal if it was to be
    Running,
}

/// pass a signal bpd was sent on to every program it started that is still
/// running, or only say whether there is one when `send` is false — which is
/// a signal something else has already sent them
///
/// a `SIGTERM` reaches each program at most once, whichever route asked first —
/// see [`ask_to_end`]. the others are passed on every time: they are not
/// requests to end bpd's way but the program's own keys and terminal
pub fn pass_on(ending: Ending, send: bool) -> Passed {
    let all = started();
    if all.is_empty() {
        return Passed::NothingStarted;
    }
    let running: Vec<Arc<Started>> = all
        .iter()
        .filter_map(Weak::upgrade)
        .filter(|started| started.running())
        .collect();
    if running.is_empty() {
        return Passed::Over;
    }
    if send {
        for started in &running {
            signal(started, ending);
        }
    }
    Passed::Running
}

/// ask one program to end, the graceful way: a `SIGTERM` it can handle
///
/// nothing is sent to a program that has ended or was already asked
pub(crate) fn ask_to_end(started: &Started) {
    signal(started, Ending::Terminate);
}

#[cfg(unix)]
fn signal(started: &Started, ending: Ending) {
    let mut child = started.child();
    // `try_wait` reaps a program that has ended and std keeps its status for
    // the `wait` that comes after. one still running stays unreaped for as long
    // as the lock is held, so its id is still its own when the signal goes
    if !matches!(child.try_wait(), Ok(None)) {
        return;
    }
    if ending == Ending::Terminate
        && started
            .asked
            .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    let pid = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .expect("the id of a process bpd started and has not reaped is a positive `pid_t`");
    let sent = rustix::process::kill_process(pid, ending.signal());
    debug_assert!(
        sent.is_ok(),
        "a child that has not been reaped can be signalled, and this was {sent:?}"
    );
}

/// windows has no signal to pass on: Ctrl+C and Ctrl+Break reach every process
/// on the console, the program among them, and there is nothing a process can
/// send another that it may handle and then end on its own
#[cfg(not(unix))]
const fn signal(_started: &Started, _ending: Ending) {}
