//! answering a request about the process while the program runs
//!
//! almost every request in the agent is answered on a thread bpd is already
//! holding, because almost every request is about a frame and a frame belongs
//! to a thread. the ones that are not — the breakpoint table, the source maps,
//! the exception filters, what a forked child does, the thread census, the code
//! of a file, the trail and the ui's ring — are about the **process**, and
//! [`bpd_protocol::message::FromEngine::about_the_process`] is the one place
//! that says which those are
//!
//! a held thread answers them when there is one, because it is already there
//! and already attached. when there is none they are answered here, on a thread
//! of the agent's own, and the program goes on running underneath it. before
//! this module they were refused, and what that cost was not theoretical: an
//! editor asks `threads` before it asks anything else, so a program with
//! nothing held could not be asked its threads, could not be given a
//! breakpoint, and — because the editor was still waiting for the answer to
//! `threads` — was never sent the pause that would have held one
//!
//! ## it is the same implementation
//!
//! [`crate::session::about_the_process`] is what runs, whichever thread runs
//! it. a second implementation for the unheld case is how a breakpoint comes to
//! bind differently depending on whether the program happened to be stopped
//!
//! ## one thread, and the order requests arrive in
//!
//! a queue and one thread rather than a thread per request. the engine has one
//! request outstanding at a time, so the queue rarely holds more than one — but
//! "rarely" is not an argument, and two answers computed at once would be two
//! answers written onto one connection in whatever order they finished
//!
//! the thread is started by the **first** request that needs it and stays for
//! the rest of the session. it is idle on a condition variable with no
//! interpreter attached, so what it costs while nothing is asked is a thread
//! that is not scheduled
//!
//! ## it is not on the process while it forks
//!
//! for the reason the connection's reader is not — see [`crate::attach`]:
//! cpython counts the process's operating system threads at `os.fork()`, and a
//! program can put the resulting warning in its own data. so [`stand_down`]
//! joins it before a fork and [`resume_answering`] starts it again in the
//! parent, and a request that arrives inside the window waits in the queue for
//! the thread that comes after it — the same lossless delay a request arriving
//! on the stood-down connection already has
//!
//! **a fork that lands while an answer is in flight gives the GIL back to wait
//! for it.** the thread is inside `Python::attach` then, and the thread that is
//! forking is the one holding what it is waiting for: joining without letting
//! go would be the debugger deadlocking a program that forked at the wrong
//! moment. the window that opens is one other threads of the *program* can run
//! in, which is a window a bare `os.fork()` does not have — and it opens only
//! for a program that already has threads of its own, which is the program
//! cpython raises the fork warning about in the first place. a single-threaded
//! program has nothing that could run in it, and that is the program whose
//! record the warning would otherwise change
//!
//! and the fork **waits for the answer**, all of it: a thread cannot be joined
//! half way through one, and an answer abandoned half way would be a request
//! the engine waits on for ever. for most requests that is the time it takes
//! to bind a breakpoint or read a ring; for a thread census it is the settle
//! interval the client asked for. both halves are pinned in
//! `crates/bpd_engine/tests/forks.rs` — a fork with the thread idle, and one in
//! the middle of a census, which deadlocks if the GIL is kept here
//!
//! a forked child gets a queue and a thread of its own — [`abandon`], from
//! [`crate::attach::detach`] — because the requests its parent had queued are
//! its parent's

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, MutexGuard};

use bpd_core::Refusal;
use bpd_protocol::message::{FromAgent, FromEngine};
use pyo3::prelude::*;

use crate::attach;
use crate::cells::ForkCell;
use crate::own_thread::{self, OwnThread};

/// what is waiting to be answered, and who is answering it
///
/// **nothing under this lock needs the interpreter**, which is what makes it
/// safe to take from a fork handler: a thread holding it always makes progress
struct Answering {
    /// the requests waiting, oldest first
    queue: VecDeque<FromEngine>,
    /// a request has been taken off the queue and is being answered
    ///
    /// the answer needs the interpreter, so this is what [`stand_down`] reads
    /// to decide whether it has to give the GIL back to wait for one
    in_flight: bool,
    /// the thread is to return the moment it can
    stopping: bool,
    /// the thread that takes requests off the queue, while there is one
    running: Option<OwnThread<()>>,
    /// how many forks are between their `before` handler and their
    /// `after_in_parent` one
    #[cfg(unix)]
    forking: usize,
    /// woken by a request arriving and by a stand-down
    wake: Arc<Condvar>,
}

fn nothing_asked() -> Answering {
    Answering {
        queue: VecDeque::new(),
        in_flight: false,
        stopping: false,
        running: None,
        #[cfg(unix)]
        forking: 0,
        wake: Arc::new(Condvar::new()),
    }
}

/// in a [`ForkCell`] for the reason the reader's own state is: a fork can land
/// while a concurrent fork holds it, and the child's copy would be locked by
/// neither of them
static ANSWERING: ForkCell<Answering> = ForkCell::new(nothing_asked);

const HELD_FOR: &str = "nothing panics while holding the unheld answerer's lock: every path through one is a push, a pop, or a field read";

fn answering() -> MutexGuard<'static, Answering> {
    ANSWERING.get().lock().expect(HELD_FOR)
}

/// answer a request about the process, with nothing held
///
/// called from the connection's reader, which has no interpreter and must not
/// wait for one
pub(crate) fn answer(request: FromEngine) {
    let mut state = answering();
    state.queue.push_back(request);
    state.wake.notify_one();
    if state.running.is_some() {
        return;
    }
    #[cfg(unix)]
    if state.forking > 0 {
        // the fork's `after_in_parent` handler starts it, because a thread
        // started inside the window is one cpython counts
        return;
    }
    start(&mut state);
}

/// put the answering thread on the process
///
/// the caller holds the lock for the whole transition, so there is never a
/// moment at which two threads are taking from the queue. `stopping` is cleared
/// here rather than after a join, because this is the one place it has to be
/// false and the only place that knows it is not about to be set again by a
/// concurrent fork
fn start(state: &mut Answering) {
    state.stopping = false;
    // the reader thread outlives this, so a thread that cannot be started is a
    // request the debugger would wait on for ever
    let spawned = own_thread::spawn("bpd-answer", answer_requests);
    match spawned {
        Ok(handle) => state.running = Some(handle),
        Err(error) => attach::fatal(&format!(
            "a request about a program that is running needs a thread of the \
             agent's own to answer it, and one could not be started: {error}"
        )),
    }
}

/// answer requests for as long as this thread holds the queue
fn answer_requests() {
    loop {
        let request = {
            let mut state = answering();
            loop {
                if state.stopping {
                    // the queue is left exactly as it is: what is in it is
                    // answered by the thread that comes after the fork
                    return;
                }
                if let Some(request) = state.queue.pop_front() {
                    state.in_flight = true;
                    break request;
                }
                let wake = Arc::clone(&state.wake);
                state = wake.wait(state).expect(HELD_FOR);
            }
        };

        answer_one(request);

        answering().in_flight = false;
    }
}

/// attach to the interpreter, answer one request, and detach
///
/// `try_attach` rather than `attach`: the interpreter can be finalizing, and a
/// thread that asks for one that is going never gets it back. that is refused
/// rather than passed over in silence, because the engine is waiting for an
/// answer to this one and the alternative is for it to learn only when the
/// connection closes under it
///
/// an error answering is handed back as one, exactly as it is on a held thread
/// — this is not inside a monitoring callback, so there is no frame of the
/// program's for it to be raised into, and there is nothing else it could be
fn answer_one(request: FromEngine) {
    let wanted = request
        .about_the_process()
        .unwrap_or_else(|| unreachable!("only a request about the process is answered here"))
        .to_string();

    let attached = Python::try_attach(|python| {
        if let Err(error) = crate::session::about_the_process(python, request) {
            attach::send(&FromAgent::Refused {
                reason: Refusal::CouldNotAnswer {
                    wanted: wanted.clone(),
                    error: crate::conditions::capture(python, &error),
                },
            });
        }
    });

    if attached.is_none() {
        attach::send(&FromAgent::Refused {
            reason: Refusal::ProgramEnding { wanted },
        });
    }
}

/// take the answering thread off the process, because it is about to fork
///
/// called from `os.register_at_fork(before=…)`, on the thread that is forking,
/// with the GIL held. the GIL is given back **only** when an answer is in
/// flight, because that answer is what the join is waiting for and the GIL is
/// what it is waiting on — see the module note
#[cfg(unix)]
pub(crate) fn stand_down(python: Python<'_>) {
    if attach::detached() {
        return;
    }

    let (handle, in_flight) = {
        let mut state = answering();
        state.forking += 1;
        state.stopping = true;
        state.wake.notify_all();
        let Some(handle) = state.running.take() else {
            // never started, or another fork is already in flight and has taken
            // it off
            return;
        };
        let in_flight = state.in_flight;
        (handle, in_flight)
    };

    if in_flight {
        python.detach(|| join(handle));
    } else {
        join(handle);
    }
}

/// wait for the answering thread to have gone
///
/// a join rather than a flag, because what cpython counts is operating system
/// threads, and a join that waits for the kernel — [`OwnThread::join`] — is
/// the only thing that says one has gone
#[cfg(unix)]
fn join(handle: OwnThread<()>) {
    // a panic in the agent is a broken invariant, and this is the one place it
    // would otherwise be swallowed
    if handle.join().is_err() {
        attach::fatal(
            "the thread that answers a running program panicked. requests about \
             the process would go unanswered, and the program is not being left \
             to run undebugged",
        );
    }
}

/// put the answering thread back, now that the fork is over
///
/// called from `os.register_at_fork(after_in_parent=…)`, in the process that
/// did the forking. it is started again only when something is waiting for it:
/// a session that never asked anything of a running program does not get a
/// thread because it forked
#[cfg(unix)]
pub(crate) fn resume_answering() {
    if attach::detached() {
        return;
    }

    let mut state = answering();
    assert!(
        state.forking > 0,
        "every fork's `after_in_parent` handler follows its own `before` one"
    );
    state.forking -= 1;
    if state.forking > 0 || state.queue.is_empty() {
        return;
    }
    start(&mut state);
}

/// give up the queue this process was forked holding
///
/// the requests in it were addressed to the process this one was forked from,
/// and the thread that was answering them did not survive the fork. the cell is
/// **replaced** rather than emptied, for the reasons [`crate::cells`] gives
#[cfg(unix)]
pub(crate) fn abandon() {
    ANSWERING.abandon();
}
