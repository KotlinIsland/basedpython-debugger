//! what a stop is
//!
//! a stop is one thread reporting where it is and then not returning from the
//! callback until the engine says to. while it is stopped the engine can change
//! the breakpoint set, which is answered in place and does not end the stop
//!
//! ## what the stop does and does not hold
//!
//! the thread that hit the breakpoint is genuinely held: it is inside the
//! monitoring callback and cannot leave. it also holds the GIL, so on a
//! gil-enabled build no *other* python thread makes progress either — but that
//! is a side effect of the interpreter, not a stop that `bpd` implemented, and
//! on a free-threaded build it does not happen at all. a thread sitting in a C
//! call has released the GIL and is running on both
//!
//! so the report names **the thread that stopped**, and claims nothing about
//! the others. real stop coordination — suspending every thread and saying
//! which ones are in native code and cannot be suspended — is its own piece of
//! work, and until it exists a stop that claimed to hold the program would be
//! the debugger lying about the one thing it is for

use bpd_protocol::message::{FromAgent, FromEngine, StopReason};
use pyo3::prelude::*;

use crate::{attach, breakpoints, events};

/// report a stop and block until the engine resumes the program
///
/// on return, the interpreter's instrumentation matches whatever breakpoint set
/// the engine left behind
pub(crate) fn stop(python: Python<'_>, reason: StopReason) -> PyResult<()> {
    let mut held = attach::hold();
    held.send(&FromAgent::Stopped { reason });

    loop {
        match held.receive() {
            FromEngine::Resume => break,
            FromEngine::SetBreakpoints { breakpoints } => {
                let resolved = breakpoints::apply(python, breakpoints)?;
                held.send(&FromAgent::BreakpointsResolved { resolved });
            }
            // `FromEngine` is non-exhaustive, so a newer engine could ask for
            // something this build cannot do. carrying on regardless would
            // resume a program whose debugger asked for the opposite
            other => attach::lost(&format!(
                "the debugger asked for {other:?}, which this agent does not understand"
            )),
        }
    }
    drop(held);

    // discovery costs a native call per code object first reached, and it buys
    // nothing once there is nothing left that could stop
    events::watch_every_call(python, breakpoints::any_set())
}

/// tell the engine that loading a file changed what a breakpoint resolves to
///
/// the program is running when this is sent, so it is an event rather than an
/// answer. the control connection is taken **after** the breakpoint state is
/// worked out and released, so a thread that is stopped — holding the
/// connection and waiting to take the breakpoint state — is never waiting on a
/// thread that holds the breakpoint state and is waiting for the connection
pub(crate) fn announce_rebinding(resolved: Vec<bpd_protocol::message::Resolved>) {
    if resolved.is_empty() {
        return;
    }
    attach::hold().send(&FromAgent::BreakpointsResolved { resolved });
}
