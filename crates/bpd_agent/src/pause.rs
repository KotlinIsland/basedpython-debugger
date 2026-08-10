//! holding a thread of a program that is not stopped
//!
//! every other request in the agent is answered on a thread bpd is already
//! holding. a pause is the one that cannot be: it exists precisely for the
//! program that is running and has nothing held, and there is no thread of the
//! debuggee's waiting to be asked
//!
//! ## how a running thread is caught
//!
//! the same way [`crate::world`] catches one, because it is the only way there
//! is: nothing in cpython suspends a thread, so `LINE` is armed for the whole
//! program and the first thread to reach one is held. arming it needs
//! `restart_events()` as well, or a thread going round a loop inside a code
//! object that already returned `DISABLE` is never offered a line again — that
//! is measured, in a bare interpreter, by
//! `arming_an_event_globally_does_not_undo_a_disable`
//!
//! ## it holds one thread, like every other stop
//!
//! whichever thread arrives first. that is the non-stop model rather than a
//! shortfall of it, and which thread it turns out to be belongs to the
//! operating system — so the acknowledgement says which threads were running
//! python when the pause was armed, and a client can tell whether a stop is
//! coming at all. an empty list means every thread is parked in a C call and
//! nothing will reach a line until one of them comes back
//!
//! ## the thread that arms it
//!
//! a rust thread of its own, for the length of one arming. the connection's
//! reader must not do it: taking the GIL there would stop it routing anything
//! else for as long as some thread of the program holds the GIL in a C call,
//! and a resume that could not be delivered is the debugger hanging

use std::sync::atomic::{AtomicBool, Ordering};

use bpd_protocol::message::FromAgent;
use pyo3::prelude::*;

use crate::{attach, events, session, threads};

/// whether a thread reaching a line should be held
///
/// read on the line event path, so it is an atomic: the common case is that
/// nothing asked for a pause and the answer costs a load
static PAUSING: AtomicBool = AtomicBool::new(false);

/// whether a thread reaching a line should be held
pub(crate) fn pausing() -> bool {
    PAUSING.load(Ordering::Relaxed)
}

/// take the pause, if this thread is the first to get here
///
/// a swap rather than a load and a store: two threads can reach a line at the
/// same moment on a free-threaded build, and a pause holds one of them
pub(crate) fn claim() -> bool {
    PAUSING.swap(false, Ordering::Relaxed)
}

/// arm a pause, on a thread of the agent's own
///
/// called from the connection's reader, which has no interpreter and must not
/// wait for one
pub(crate) fn request() {
    // the reader thread outlives this, so a pause that cannot be spawned is a
    // request the debugger would wait on for ever
    let spawned = std::thread::Builder::new()
        .name("bpd-pause".to_string())
        .spawn(arm);
    if let Err(error) = spawned {
        attach::fatal(&format!(
            "a pause needs a thread of the agent's own to arm it, and one could \
             not be started: {error}"
        ));
    }
}

/// arm the instrumentation and say who was running when it went on
fn arm() {
    // the interpreter is gone or going: the program has ended, and there is
    // nothing left to hold. saying nothing is right — the engine is about to
    // see the connection close
    let armed = Python::try_attach(|python| -> PyResult<()> {
        // said before the instrumentation goes on, so the acknowledgement
        // cannot arrive behind the stop it is describing. it is also what the
        // answer means: the threads that were running when the pause was asked
        // for
        attach::send(&FromAgent::Pausing {
            running: threads::running(python, None)?,
        });

        PAUSING.store(true, Ordering::Relaxed);
        session::refresh_events(python)?;
        // a thread looping inside a code object that already told the
        // interpreter never to offer its lines again would reach nothing at
        // all, and a pause would sit there catching a program that is running
        events::restart(python)
    });

    if let Some(Err(error)) = armed {
        PAUSING.store(false, Ordering::Relaxed);
        attach::fatal(&format!(
            "a pause could not arm `sys.monitoring`: {error}. the debugger asked \
             for a thread and there is nothing that could tell it which one it got"
        ));
    }
}

/// put the instrumentation back, on the thread that claimed the pause
pub(crate) fn disarm(python: Python<'_>) -> PyResult<()> {
    session::refresh_events(python)
}
