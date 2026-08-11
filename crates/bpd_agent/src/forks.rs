//! what happens to the agent when the debuggee forks
//!
//! `fork` copies the process and keeps **only the calling thread**. what the
//! child inherits is measured rather than assumed — on 3.13, 3.14 and 3.15, on
//! a gil build and a free-threaded one:
//!
//! - the `sys.monitoring` tool id, still held, still named `bpd`
//! - every global and local event, unchanged, with the callbacks still
//!   registered and still firing
//! - the breakpoint table, the code registry and the stop registry, because
//!   they are memory
//! - **the file descriptors of the control connection**, both of them
//!
//! and what it does not inherit is the thread that reads that connection. so a
//! forked child would be an armed debuggee that can write to the session socket
//! and can never be answered: `bpd_protocol` frames are length prefixed, two
//! writers interleaving mid-frame desynchronise the stream, and a child that
//! reached a breakpoint would report a stop and then wait for a resume that no
//! process is able to send
//!
//! **that is reachable without any propagation being built.** a forked child
//! that runs a line holding a breakpoint calls `session::stop` today
//!
//! ## what the child does instead
//!
//! it stops being a debuggee, before `os.fork()` has returned to python. it
//! gives up the tool id and every event with it, closes its copies of the
//! session's descriptors, and then runs exactly as it would have if the program
//! had never been launched under `bpd` — same monitoring state, same argv, same
//! exit code, same output. `crates/bpd_engine/tests/forks.rs` compares a forked
//! child of a debugged run against a forked child of a bare one and requires
//! the two records to be identical
//!
//! it is **not** debugged, and nothing pretends otherwise. the report the
//! parent already makes for `os.fork` is what says so, and closing this hole is
//! what makes that report true rather than merely quiet. debugging a forked
//! child is designed in `scratch.subprocess.md` and is not this
//!
//! ## why `os.register_at_fork` and not `pthread_atfork`
//!
//! both exist and only one of them can do the work.
//!
//! a `pthread_atfork` child handler is called by the C library from **inside**
//! `fork()`, before cpython has put its own runtime back together. at that
//! point the GIL, the import lock and the per-interpreter locks are in whatever
//! state the fork left them, and this handler's whole job — `set_local_events`,
//! `set_events`, `register_callback`, `free_tool_id` — is calls into the
//! interpreter that need the GIL held by the calling thread. a handler there
//! could close the descriptors and nothing else, which would leave the child
//! armed
//!
//! cpython runs `after_in_child` from `PyOS_AfterFork_Child`, **after**
//! reinitialising the runtime, its evaluation locks and the import lock, and
//! before `os.fork()` returns to python. measured on 3.13, 3.14, 3.15 and a
//! free-threaded 3.14: the handler sees one thread, runs attached to the
//! interpreter, and finds the tool id and the local events exactly as the
//! parent left them. so there is no window in which the child can reach a
//! breakpoint while it is still armed, and there is no ordering to guess at —
//! `pthread_atfork` handlers run in an order relative to cpython's own that
//! nothing specifies
//!
//! ## what this relies on being safe in a forked child
//!
//! a fork handler is a constrained place and the constraints are stated rather
//! than hoped for:
//!
//! - **the interpreter.** it is attached to the only surviving thread — the
//!   GIL, on a build that has one — and cpython's own locks have been
//!   reinitialised, which is what `PyOS_AfterFork_Child` is for. every python
//!   object this handler calls was resolved at attach, so nothing here looks a
//!   name up or imports anything
//! - **no lock, anywhere.** a fork keeps only the calling thread, so a lock
//!   another thread held at the instant of the fork is one the child's copy
//!   would wait on for ever. that is not a free-threading nicety: the writing
//!   end of the control connection is written to by the reader thread, which
//!   does not hold the GIL, so even on a gil build a fork can land while it is
//!   locked. so the descriptors are closed by number and the code objects to
//!   disarm come from [`crate::events`]'s published snapshot, which is an
//!   atomic pointer read
//! - **the descriptors.** closing one is a `close(2)`, which needs nothing of
//!   the interpreter at all
//! - **allocation.** the handler allocates, and it is the platform's own
//!   allocator that makes that safe: both the one on macos and glibc's take
//!   their own locks across a fork. that is not a favour to this module — it is
//!   what makes cpython able to run arbitrary python in `after_in_child` at all
//!
//! ## a child of a child
//!
//! the handler is inherited too, and it is idempotent: the second run finds the
//! session already given up and returns. that is [`crate::attach::detach`]'s
//! answer rather than a flag of this module's, so there is one place that
//! decides whether this process still owns a session
//!
//! ## the parent is not left changed either
//!
//! since 3.12 cpython counts the process's operating system threads at
//! `os.fork()` and raises a `DeprecationWarning` when there is more than one.
//! the agent reads the control connection on a thread of its own, so a debuggee
//! is multi-threaded where a bare run of the same program is not — and a
//! program can put that warning in **its own data** with
//! `warnings.catch_warnings(record=True)`, which makes it a parity violation
//! rather than a line of output
//!
//! so the reader thread is not on the process while it forks. `before` stands
//! it down and `after_in_parent` starts it again, and
//! `a_program_that_forks_records_exactly_the_warnings_it_would_have` in
//! `crates/bpd/tests/launch_parity.rs` compares what the program itself
//! recorded, both ways. what the window costs is
//! [`crate::attach::stand_down`]'s to state
//!
//! the three handlers compose in one direction only, and it is the one cpython
//! decides: a fork runs every `before` handler, then either the child's
//! handlers or the parent's, never both. so the child never starts a reader —
//! it has given the session up by then, and both halves check that before they
//! touch anything

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyModule};

use crate::{attach, events};

/// arrange for a fork to change nothing the program can see, and for a forked
/// child to stop being a debuggee
///
/// called once, from the agent's entry point, after the connection is up and
/// before the program runs — a program that forks in its first statement is one
/// this has to have been ready for
///
/// all three handlers go on in one call, so cpython holds them in one place and
/// their order relative to each other is its own: `before` handlers run in
/// reverse registration order, which puts this one **last**, nearest the fork,
/// and the two `after` ones run in registration order, which puts them first
pub(crate) fn install(python: Python<'_>) -> PyResult<()> {
    let arguments = PyDict::new(python);
    arguments.set_item("before", wrap_pyfunction!(going_to_fork, python)?)?;
    arguments.set_item("after_in_parent", wrap_pyfunction!(forked, python)?)?;
    arguments.set_item(
        "after_in_child",
        wrap_pyfunction!(gave_up_the_session, python)?,
    )?;

    // `os` is already imported — the entry point uses it to take bpd's own
    // variables back out of `os.environ` — so this adds nothing to the
    // debuggee's `sys.modules`
    PyModule::import(python, "os")?.call_method("register_at_fork", (), Some(&arguments))?;
    Ok(())
}

/// this process is about to fork, and the agent must not be a thread on it
///
/// the GIL is held for the whole of this and is deliberately **not** given
/// back. everything it reaches is a socket write and a `pthread_join`, none of
/// which needs the interpreter — so releasing it would only add a wait for the
/// GIL inside `os.fork()` that a bare run of the program does not have, and a
/// C extension holding the GIL somewhere else would then be able to hold the
/// program's fork up
///
/// see [`crate::attach::stand_down`] for what a request arriving between here
/// and [`forked`] does, which is: wait in the kernel's receive buffer and be
/// read afterwards
#[pyfunction]
fn going_to_fork() {
    attach::stand_down();
}

/// the fork is over in the process that did it, and the agent is a thread again
///
/// this runs in the **parent** only. the child registers
/// [`gave_up_the_session`] instead, and there is nothing there to start: it has
/// given the session up and closed the descriptors a reader would read
#[pyfunction]
fn forked() {
    attach::resume_reading();
}

/// this process is the forked child, and it is not being debugged
///
/// the session goes first and the instrumentation second, and that is the order
/// a failure decides. nothing runs between them — the child is one thread and
/// this is the whole of what it is doing — so it is not a race being avoided:
/// it is that if `disarm` raises, the process it leaves behind is one that
/// still has events armed and **cannot write a frame**, which is survivable,
/// where the other order would leave one that can write into a socket it does
/// not own
///
/// a raise here reaches the child's stderr through cpython's unraisable hook
/// and nothing else, because `os.fork()` has nowhere to return an error to.
/// that is the right outcome for a `sys.monitoring` call failing on a tool this
/// process holds, which is a broken invariant rather than anything the program
/// did
///
/// it is a native function rather than a python one for the reason everything
/// else the interpreter calls into is: there is no python here, only a callable
/// the interpreter can hold
#[pyfunction]
fn gave_up_the_session(python: Python<'_>) -> PyResult<()> {
    if !attach::detach() {
        return Ok(());
    }
    events::disarm(python)
}
