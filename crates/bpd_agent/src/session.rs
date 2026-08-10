//! what a stop is
//!
//! a stop is one thread reporting where it is and then not returning from the
//! callback until the engine says to. while it is stopped the engine can change
//! the breakpoint set, which is answered in place and does not end the stop
//!
//! ## the thread it holds, and the ones it does not
//!
//! a stop holds **one thread**. every other thread in the process keeps
//! running, which is what gdb calls non-stop mode and DAP exposes as
//! `supportsSingleThreadExecutionRequests`. it is the default because a live
//! program should go on living while one of its handlers is inspected
//!
//! **the GIL is released for the whole of a stop.** it would otherwise be the
//! thing deciding the threading behaviour: a gil-enabled build would freeze the
//! process by accident and a free-threaded one would not, and "threads keep
//! running, except on the interpreter most people have" is a capability ladder.
//! so the thread that is held gives the GIL back and takes it again only to
//! answer something
//!
//! the honest consequences are written down rather than discovered:
//!
//! - the held thread's **stack** is stable. it is inside a callback and cannot
//!   return, so nothing tears its frames down underneath an inspection
//! - everything the frames point at is a **sample**. every answer says which
//!   mode it was taken in
//! - **the held thread still holds its locks.** stopping inside `with lock:`
//!   blocks every thread that wants it, and that is the model's real cost. what
//!   is knowable is reported on the stop itself, and the rest is visible through
//!   [`crate::threads`]
//! - stop-the-world is available explicitly, in [`crate::world`]

use std::time::Duration;

use bpd_protocol::message::{FromAgent, FromEngine, LogRecord, StopReason};
use pyo3::prelude::*;

use crate::{attach, breakpoints, events, frames, stops, threads, world};

/// tell the engine what a logpoint had to say, and carry straight on
///
/// the control connection is taken for the write and released. nothing is read
/// back, so a logpoint on a line executed a million times costs a million
/// writes and **no** round trips — which is the whole reason the message is
/// formatted in here rather than by asking the engine to evaluate something
pub(crate) fn log(record: LogRecord) {
    attach::send(&FromAgent::Logged { record });
}

/// put the global instrumentation back to what the session currently needs
///
/// `set_events` replaces the whole mask, so the two things that can be armed
/// globally are decided together. anything that changes either one comes
/// through here rather than setting its own bit and disarming the other
pub(crate) fn refresh_events(python: Python<'_>) -> PyResult<()> {
    events::watch_globally(python, breakpoints::any_set(), world::parking())
}

/// report a stop and hold this thread until the engine resumes it
///
/// on return, the interpreter's instrumentation matches whatever breakpoint set
/// the engine left behind
pub(crate) fn stop(python: Python<'_>, thread: u64, reason: StopReason) -> PyResult<()> {
    let ticket = stops::enter(thread, reason, frames::holding(python)?);
    let mut stopped = frames::begin(python, ticket.stop);

    loop {
        // the GIL is given back for the whole of the wait. the rest of the
        // program runs while this thread is held, on every build
        let command = python.detach(|| ticket.next());
        let request = match command {
            stops::Command::Resume => break,
            stops::Command::Answer(request) => request,
        };

        match request {
            FromEngine::SetBreakpoints { breakpoints } => {
                let resolved = breakpoints::apply(python, breakpoints)?;
                attach::send(&FromAgent::BreakpointsResolved { resolved });
            }
            FromEngine::Stack { top, .. } => {
                let answer = stopped.stack(top)?;
                attach::send(&answer);
            }
            FromEngine::Variables {
                frame,
                scope,
                detail,
            } => {
                let answer = stopped.variables(frame, scope, detail)?;
                attach::send(&answer);
            }
            FromEngine::Evaluate {
                frame,
                expression,
                detail,
            } => {
                let answer = stopped.evaluate(frame, &expression, detail)?;
                attach::send(&answer);
            }
            FromEngine::SetVariable {
                frame,
                scope,
                name,
                value,
                detail,
            } => {
                let answer = stopped.set_variable(frame, scope, &name, &value, detail)?;
                attach::send(&answer);
            }
            FromEngine::Threads { settle_ms } => {
                let answer = threads::census(python, Duration::from_millis(settle_ms.into()))?;
                attach::send(&answer);
            }
            FromEngine::StopTheWorld { settle_ms, .. } => {
                let answer = stop_the_world(python, thread, ticket.stop, settle_ms)?;
                attach::send(&answer);
            }
            // the router only ever sends what a held thread can answer, so
            // anything else is a bug in the routing rather than in the engine
            other => unreachable!("a held thread was handed {other:?} to answer"),
        }
    }

    // the world goes when the last stop that asked for it does, and putting the
    // instrumentation back needs the interpreter — which the connection's
    // reader thread, where a resume arrives, does not have
    world::release(python, ticket.stop, refresh_events)?;

    // the frames go with the stop that minted their ids: a frame id names the
    // stop it belongs to, and there is nothing here for one from an older stop
    // to be answered against
    drop(stopped);

    // discovery costs a native call per code object first reached, and it buys
    // nothing once there is nothing left that could stop
    refresh_events(python)
}

/// hold every thread that can be held, and name the ones that cannot be
fn stop_the_world(
    python: Python<'_>,
    thread: u64,
    stop: u64,
    settle_ms: u32,
) -> PyResult<FromAgent> {
    let targets = threads::running(python, thread)?;
    let stopped = world::stop(
        python,
        stop,
        targets,
        Duration::from_millis(settle_ms.into()),
        |python| {
            refresh_events(python)?;
            // a thread going round a loop inside a code object that holds a
            // breakpoint has already told the interpreter never to offer those
            // lines again, and arming `LINE` globally does not undo that. it
            // would reach no event and be reported as running in native code
            // when it is running python — so the process has its disabled
            // locations restarted, which is the whole cost of this mode
            events::restart(python)
        },
    )?;

    let mut held = stops::held_threads();
    held.extend(stopped.parked);
    held.sort_unstable();
    held.dedup();

    Ok(FromAgent::WorldStopped {
        held,
        native: stopped.native,
    })
}

/// tell the engine that loading a file changed what a breakpoint resolves to
///
/// the program is running when this is sent, so it is an event rather than an
/// answer
pub(crate) fn announce_rebinding(resolved: Vec<bpd_protocol::message::Resolved>) {
    if resolved.is_empty() {
        return;
    }
    attach::send(&FromAgent::BreakpointsResolved { resolved });
}

/// the program has ended, and these threads were never let go
///
/// the interpreter is about to finalize, which joins the program's non-daemon
/// threads. a held one cannot be joined, so the process would sit there looking
/// exactly like a hang in bpd when it is the debuggee waiting for a resume that
/// never came. saying so is the difference between a hang and a fact
pub(crate) fn finishing() {
    let held = stops::held_threads();
    if !held.is_empty() {
        attach::send(&FromAgent::Finishing { held });
    }
    attach::mark_finished();
}
