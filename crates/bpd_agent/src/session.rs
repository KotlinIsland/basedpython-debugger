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

use bpd_core::{LogRecord, StopReason};
use bpd_protocol::message::{FromAgent, FromEngine};
use pyo3::prelude::*;

use crate::{
    attach, breakpoints, events, exceptions, frames, pause, steps, stops, templates, threads, world,
};

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
/// `set_events` replaces the whole mask, so everything that can be armed
/// globally is decided here, together. anything that changes one of them comes
/// through this rather than setting its own bit and disarming the rest
///
/// what wants what:
///
/// - `PY_START` discovers code objects while a breakpoint is set, and is how a
///   step in catches the frame it enters
/// - `LINE` catches a running thread, for stopping the world and for a pause
/// - `PY_UNWIND` is how a step sees its frame left by an exception, and how an
///   exception leaving the outermost frame is found. it **cannot** be a local
///   event — `set_local_events` refuses it — so a step pays for it process wide
/// - `PY_RESUME` and `PY_THROW` are the other two ways a frame is entered, which
///   a step in follows
/// - `RAISE` is the exception breakpoint, and cannot be local either
pub(crate) fn refresh_events(python: Python<'_>) -> PyResult<()> {
    let stepping = steps::armed_anywhere();
    let entering = steps::entering_anywhere();
    events::watch_globally(
        python,
        events::Global {
            py_start: breakpoints::any_set() || entering,
            line: world::parking() || pause::pausing(),
            py_unwind: stepping || exceptions::uncaught(),
            py_throw: entering,
            py_resume: entering,
            raised: exceptions::raised(),
        },
    )
}

/// put one code object's instrumentation back to what the session needs
///
/// `set_local_events` replaces a code object's whole mask the way `set_events`
/// replaces the program's, and two things want events on one: a breakpoint
/// bound into it, and a step being made in it. arming either on its own would
/// silently disarm the other — a step through a function that holds a
/// breakpoint would turn the breakpoint off
pub(crate) fn refresh_code(python: Python<'_>, code: &Bound<'_, PyAny>) -> PyResult<()> {
    let address = code.as_ptr() as usize;
    events::watch_locally(
        python,
        code,
        breakpoints::local(address) | steps::local(address) | templates::local(address),
    )
}

/// report a stop and hold this thread until the engine resumes it
///
/// on return, the interpreter's instrumentation matches whatever breakpoint set
/// the engine left behind
pub(crate) fn stop(python: Python<'_>, thread: u64, reason: StopReason) -> PyResult<()> {
    let ticket = stops::enter(thread, reason, frames::holding(python)?);
    let mut stopped = frames::begin(python, ticket.stop);
    let mut stepping = None;

    loop {
        // the GIL is given back for the whole of the wait. the rest of the
        // program runs while this thread is held, on every build
        let command = python.detach(|| ticket.next());
        let request = match command {
            stops::Command::Resume => break,
            stops::Command::Step(kind) => {
                stepping = Some(kind);
                break;
            }
            stops::Command::Answer(request) => request,
        };

        match request {
            FromEngine::SetBreakpoints { breakpoints } => {
                let resolved = breakpoints::apply(python, breakpoints)?;
                attach::send(&FromAgent::BreakpointsResolved { resolved });
            }
            FromEngine::SetExceptionBreakpoints { raised, uncaught } => {
                exceptions::watch(raised, uncaught);
                refresh_events(python)?;
                attach::send(&FromAgent::ExceptionBreakpointsSet { raised, uncaught });
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
            FromEngine::TemplateContext { frame, detail } => {
                let answer = stopped.template_context(frame, detail)?;
                attach::send(&answer);
            }
            FromEngine::Source { frame, around } => {
                let answer = stopped.source(frame, around)?;
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

    // armed while this thread is still inside the callback, so the first event
    // the program reaches after it returns is already being watched for
    if let Some(kind) = stepping {
        return steps::arm(python, thread, kind);
    }

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
    let targets = threads::running(python, Some(thread))?;
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
pub(crate) fn announce_rebinding(resolved: Vec<bpd_core::Resolved>) {
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
    // a process that gave up the session has nothing to finish. it is also the
    // one process that must not read the stop registry: the entries in its copy
    // name threads that did not survive the fork, and the lock over them can be
    // held by a thread without the GIL — so a forked child asking what it is
    // holding could wait on a lock nothing will ever release
    if attach::detached() {
        return;
    }

    let held = stops::held_threads();
    if !held.is_empty() {
        attach::send(&FromAgent::Finishing { held });
    }
    attach::mark_finished();
}
