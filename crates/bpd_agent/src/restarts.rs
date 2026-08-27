//! a restart in flight, which is a thing that happens over three events
//!
//! restarting a frame is not a jump and cannot be done while a thread is held.
//! the interpreter has to actually **run** for a fresh frame to exist, so the
//! operation is arranged inside the stop that asked for it, the thread is let
//! go, and what it does next is watched for:
//!
//! 1. the frame was moved to a clean exit before the thread was let go, so
//!    it **returns**. that move is an `f_lineno` jump like any other and runs no
//!    block's cleanup: a `with` the frame was inside gets no `__exit__` and a
//!    `try` gets no `finally` — measured with a plain class context manager, two
//!    `__enter__` and one `__exit__`
//! 2. the caller is executing again, and the first `LINE` event in it is where
//!    the rewind is made. it has to be a line event: cpython answers `can only
//!    jump from a 'line' trace event` to anything else, which is why the request
//!    refused a call the caller has no statement after
//! 3. the caller re-executes the call line, the interpreter builds a frame that
//!    has never run, and the thread is held at its first line
//!
//! ## which frame, not which code object
//!
//! every identity here is a frame object compared with `is`, and every reference
//! is **strong**, for the reason [`crate::steps`] holds one: the address of a
//! freed frame is handed straight to the next one, so a recursive call would
//! look like the frame that was being watched for
//!
//! step 3 is the one that needs two things rather than one. a fresh frame is
//! recognised by its code object **and** by its `f_back` being the caller that
//! was rewound — a recursion that enters the same code object from somewhere
//! else is not the frame this call made
//!
//! ## the rewind wins the line it is made on
//!
//! the rewind happens at a `LINE` event, before that line runs, and the line
//! then does not run at all. so it is decided **before** the breakpoints on that
//! line are: a breakpoint that fired there would be reporting the program at a
//! line it never executed. the landing is decided with the steps instead, where
//! a breakpoint on the same line wins — there the line really is about to run

use std::cell::RefCell;

use bpd_core::{Abandoned, StopReason};
use pyo3::prelude::*;

use crate::armed::{self, Interest};
use crate::conditions::capture;
use crate::{events, frames, session, sources};

/// one thread's restart, held on that thread
#[derive(Debug)]
struct Restart {
    /// the interpreter's identity for the thread, which keys the interest
    thread: u64,
    /// the caller frame the rewind is made in
    caller: Py<PyAny>,
    /// the line of the caller to rewind to, in the interpreter's own numbering
    ///
    /// the breakpoint table and `f_lineno` are both keyed by the line the
    /// interpreter has, which for a basedpython build is not the line reported
    call_line: u32,
    /// the offset the caller's line was read from
    ///
    /// carried so that the landing can be **compared** rather than assumed. the
    /// caller is suspended in a call when the analysis runs, so its destination
    /// is the one thing about this operation that cannot be verified before the
    /// frame is forced out — it is verified here instead
    from: u32,
    /// `co_qualname` of what is being restarted, for the stop that says so
    function: String,
    /// the code object the fresh frame runs
    code: Py<PyAny>,
    /// the code objects it has armed, so they can be put back as they were
    watching: Vec<Py<PyAny>>,
    /// how far along it is
    phase: Phase,
}

/// how far along a restart is
#[derive(Debug)]
enum Phase {
    /// waiting for a line event in the caller, which is where the rewind is made
    Rewinding,
    /// the caller was rewound; waiting for the frame its call builds
    Entering,
    /// the fresh frame exists; waiting for the first line of it
    Landing {
        /// the frame itself, so a recursive call is not mistaken for it
        frame: Py<PyAny>,
    },
}

thread_local! {
    /// the restart this thread is making, if it is making one
    static RESTART: RefCell<Option<Restart>> = const { RefCell::new(None) };
}

/// whether this thread is restarting a frame
pub(crate) fn armed_here() -> bool {
    RESTART.with(|cell| cell.borrow().is_some())
}

/// what a restart does with the line event it is offered
#[derive(Debug)]
pub(crate) enum Rewind {
    /// this event is not the one a restart is waiting for
    NotMine,
    /// the caller was rewound, and the line this event is for will not run
    Rewound,
    /// the restart cannot complete, and this is what the thread is held with
    Abandoned(StopReason),
}

/// begin a restart on the thread that is about to be let go
///
/// called from inside the stop that asked for it, with the frame already moved
/// to its exit. the caller is the frame the rewind will be made in, and it
/// is `f_back` of the frame that was forced out
pub(crate) fn arm(
    python: Python<'_>,
    thread: u64,
    caller: &Bound<'_, PyAny>,
    call_line: u32,
    from: u32,
    function: String,
    code: &Bound<'_, PyAny>,
) -> PyResult<()> {
    // a step in flight on this thread is taken off first, for the reason
    // `steps::arm` takes off a restart: they share one registry entry
    crate::steps::cancel(python)?;

    let mut restart = Restart {
        thread,
        caller: caller.clone().unbind(),
        call_line,
        from,
        function,
        code: code.clone().unbind(),
        watching: Vec::new(),
        phase: Phase::Rewinding,
    };
    armed::hold(thread, Interest::default());
    // only the caller, until the rewind has been made. the fresh frame does not
    // exist yet and `PY_START` for the whole program is what would catch it, so
    // it is paid for from the rewind onwards rather than from here
    watch(python, &mut restart, &caller.getattr("f_code")?, false)?;
    RESTART.with(|cell| *cell.borrow_mut() = Some(restart));

    // a line of the caller that has already run told the interpreter never to
    // offer it again, and the rewind is made from one of those
    armed::restart_locations(python)?;
    session::refresh_events(python)
}

/// take this thread's restart off, wherever it got to
///
/// a no-op when there is none, so a path that decided to hold the thread for
/// some other reason can call it unconditionally — and every such path does,
/// because one left armed keeps `armed_anywhere()` true and stops the
/// interpreter forgetting a single location for the rest of the run
///
/// **a cancelled restart is not separately announced, and that is a known gap.**
/// the client was told `Arranged`, and if something else holds the thread before
/// the fresh frame is entered it learns only about the stop it got. saying so
/// needs a report the debugger makes without being asked, which is a
/// [`bpd_core::Told`] and a `carriage_of` arm in both front ends; it is written
/// down in the restart section of `docs/development/jumps.md` rather than left
/// for somebody to discover
pub(crate) fn cancel(python: Python<'_>) -> PyResult<()> {
    let taken = RESTART.with(|cell| cell.borrow_mut().take());
    match taken {
        Some(restart) => release(python, &restart),
        None => Ok(()),
    }
}

/// a line is about to run, and a restart may want to rewind instead of it
///
/// asked **before** anything else on the line decides anything, because a
/// rewind means the line does not run — see the module note
pub(crate) fn rewinding(python: Python<'_>) -> PyResult<Rewind> {
    if !armed_here() {
        return Ok(Rewind::NotMine);
    }
    let Some(mut restart) = RESTART.with(|cell| cell.borrow_mut().take()) else {
        return Ok(Rewind::NotMine);
    };
    if !matches!(restart.phase, Phase::Rewinding) {
        RESTART.with(|cell| *cell.borrow_mut() = Some(restart));
        return Ok(Rewind::NotMine);
    }

    let frame = events::current_frame(python)?;
    if !frame.is(restart.caller.bind(python)) {
        // the caller's code object in another frame — a recursion, or another
        // thread's would never reach this thread local at all
        RESTART.with(|cell| *cell.borrow_mut() = Some(restart));
        return Ok(Rewind::NotMine);
    }

    // the assignment runs the warnings machinery, which is the program's own
    // code, for the reason any other jump suppresses it
    let moved = {
        let _suppressed = crate::conditions::suppress();
        frame.setattr("f_lineno", restart.call_line)
    };
    if let Err(error) = moved {
        let at = frames::describe_where(&frame)?;
        let reason = StopReason::RestartAbandoned {
            function: restart.function.clone(),
            wanted: reported_line(&frame, restart.call_line)?,
            file: at.file,
            line: at.line,
            why: Abandoned::Refused {
                error: capture(python, &error),
            },
        };
        release(python, &restart)?;
        return Ok(Rewind::Abandoned(reason));
    }

    // **verified, not assumed** — and no test reaches this, deliberately said
    // here rather than left to be discovered.
    //
    // the argument that it cannot fire: cpython accepts a move only to a
    // candidate whose stack is shallower than or equal to where the frame is
    // (measured — jumping *out* of a `with` works, back *in* answers
    // `incompatible stacks`), and a rewind is made at a `LINE` event, where the
    // operand stack is at a block boundary. so any range start in the middle of
    // an expression is incompatible, which leaves the copies cpython makes of a
    // `finally` body — and those are refused up front by
    // [`bpd_core::Unrestartable::CopiedLine`]
    //
    // it is kept because the argument rests on a measurement of cpython's
    // choice rule rather than on a promise cpython makes, and the cost of being
    // wrong is resuming into a span nobody read. an abandoned restart is a bad
    // outcome; running unchecked code is the outcome this whole feature exists
    // to prevent
    let landed: u32 = frame.getattr("f_lasti")?.extract()?;
    if landed != restart.from {
        let at = frames::describe_where(&frame)?;
        let reason = StopReason::RestartAbandoned {
            function: restart.function.clone(),
            wanted: reported_line(&frame, restart.call_line)?,
            file: at.file,
            line: at.line,
            why: Abandoned::LandedElsewhere {
                expected: restart.from,
                landed,
            },
        };
        release(python, &restart)?;
        return Ok(Rewind::Abandoned(reason));
    }

    // the fresh frame is caught by `PY_START` for the whole program: a frame
    // that does not exist yet cannot be watched for on its code object
    restart.phase = Phase::Entering;
    armed::amend(restart.thread, |interest| interest.entering = true);
    let code = restart.code.bind(python).clone();
    watch(python, &mut restart, &code, false)?;
    RESTART.with(|cell| *cell.borrow_mut() = Some(restart));
    session::refresh_events(python)?;
    Ok(Rewind::Rewound)
}

/// this thread entered a frame — the fresh one, if it is the call's own
pub(crate) fn entered_frame(python: Python<'_>) -> PyResult<()> {
    let Some(mut restart) = RESTART.with(|cell| cell.borrow_mut().take()) else {
        return Ok(());
    };
    if !matches!(restart.phase, Phase::Entering) {
        RESTART.with(|cell| *cell.borrow_mut() = Some(restart));
        return Ok(());
    }

    let frame = events::current_frame(python)?;
    let mine = frame.getattr("f_code")?.is(restart.code.bind(python))
        && frame.getattr("f_back")?.is(restart.caller.bind(python));
    if mine {
        // the whole program's `PY_START` is not needed once the frame exists
        armed::amend(restart.thread, |interest| interest.entering = false);
        restart.phase = Phase::Landing {
            frame: frame.unbind(),
        };
        RESTART.with(|cell| *cell.borrow_mut() = Some(restart));
        return session::refresh_events(python);
    }
    RESTART.with(|cell| *cell.borrow_mut() = Some(restart));
    Ok(())
}

/// a frame a restart still needs was left before it could finish
///
/// two of them, and neither can be recovered from:
///
/// - the **caller** left before the rewind was made. only an exception can do
///   that: the request refused a call the caller has no statement after, so a
///   caller that returns always runs a line first. **no test reaches it**, and
///   the argument has been wrong twice, so it is written out in full. what runs
///   between the forced return and the caller's next line event is the tail of
///   the call line, and `BESIDE_THE_CALL` permits loads and
///   stack shuffles there as well as stores — so the allow list alone guarantees
///   nothing about raising, which is what an earlier cut of this claimed. what
///   does is the [`crate::bytecode::Namespaces`] gates, and it takes all four of
///   them: `unresolvable` for a name bound nowhere, `unbound_cells` for
///   `LOAD_DEREF`, `unbound_fasts` for `LOAD_FAST_CHECK`, and the two exactness
///   flags for a mapping that runs its own code. the third of those was missing
///   until a caller whose tail read a conditionally-bound local answered
///   `Arranged` and then abandoned as `CallerLeft` — a restart that was never
///   possible, discovered by attempting it. with all four passed the tail runs
///   nothing of the program that raises, and the exception has to come from
///   outside its control flow: a signal handler at the eval breaker, or a
///   `KeyboardInterrupt`
/// - the same variant is also produced from `Phase::Entering` — after the rewind
///   and before the fresh frame's `PY_START` — where the argument above does not
///   apply at all. there the caller is re-executing the call line, which runs
///   the call itself, and anything that raises out of it leaves the caller. that
///   half is reachable by ordinary program behaviour and is not argued away
/// - the **caller** left after the rewind, before the call it was rewound to
///   built a frame. a restart left armed there would never land, and it would
///   keep the whole process's locations un-forgettable for the rest of the run
///
/// the frame that was forced out is gone either way, and a restart that quietly
/// did half of itself is what this exists to prevent
pub(crate) fn left_frame(python: Python<'_>) -> PyResult<Option<StopReason>> {
    let Some(restart) = RESTART.with(|cell| cell.borrow_mut().take()) else {
        return Ok(None);
    };
    let leaving = events::current_frame(python)?;
    // `Landing` is not here: the fresh frame exists and leaving the caller
    // cannot happen before it, because the caller is suspended in the call that
    // built it
    let waiting_on_the_caller = matches!(restart.phase, Phase::Rewinding | Phase::Entering);
    if !waiting_on_the_caller || !leaving.is(restart.caller.bind(python)) {
        RESTART.with(|cell| *cell.borrow_mut() = Some(restart));
        return Ok(None);
    }

    let at = frames::describe_where(&leaving)?;
    let reason = StopReason::RestartAbandoned {
        function: restart.function.clone(),
        wanted: reported_line(&leaving, restart.call_line)?,
        file: at.file,
        line: at.line,
        why: Abandoned::CallerLeft,
    };
    release(python, &restart)?;
    Ok(Some(reason))
}

/// a line of a code object a restart watches is about to run
///
/// the landing, decided alongside a step's for the reason a step's is: the line
/// really is about to run, so a breakpoint on it is a breakpoint that fired
pub(crate) fn reached_line(python: Python<'_>) -> PyResult<Option<StopReason>> {
    let Some(restart) = RESTART.with(|cell| cell.borrow_mut().take()) else {
        return Ok(None);
    };
    let Phase::Landing { frame: landing } = &restart.phase else {
        RESTART.with(|cell| *cell.borrow_mut() = Some(restart));
        return Ok(None);
    };

    let frame = events::current_frame(python)?;
    if !frame.is(landing.bind(python)) {
        RESTART.with(|cell| *cell.borrow_mut() = Some(restart));
        return Ok(None);
    }

    let at = frames::describe_where(&frame)?;
    let reason = StopReason::Restarted {
        function: restart.function.clone(),
        file: at.file,
        line: at.line,
    };
    release(python, &restart)?;
    Ok(Some(reason))
}

/// arm one code object for this restart
fn watch(
    python: Python<'_>,
    restart: &mut Restart,
    code: &Bound<'_, PyAny>,
    py_start: bool,
) -> PyResult<()> {
    let wanted = events::Local {
        // the only event a restart reads on a code object. the rewind is made
        // from one in the caller and the landing from one in the fresh frame
        line: true,
        // a restart lands on a line of a frame it started itself, never
        // mid-line in a caller, so it never needs the instruction a step out
        // does
        instruction: false,
        // **not** asked for. a restart reads `PY_RETURN` nowhere: the caller
        // being left is caught by `PY_UNWIND`, which cannot be a local event at
        // all — `set_local_events` refuses it — so it is armed globally by
        // `refresh_events` for as long as anything is armed. arming a return
        // here bought an event nothing read
        py_return: false,
        py_start,
    };
    armed::amend(restart.thread, |interest| {
        interest.watching.insert(code.as_ptr() as usize, wanted);
    });
    restart.watching.push(code.clone().unbind());
    session::refresh_code(python, code)
}

/// put back everything a restart armed, and forget it
fn release(python: Python<'_>, restart: &Restart) -> PyResult<()> {
    armed::release(restart.thread);
    for code in &restart.watching {
        session::refresh_code(python, code.bind(python))?;
    }
    session::refresh_events(python)
}

/// a line of the interpreter's, said the way a client is told it
fn reported_line(frame: &Bound<'_, PyAny>, line: u32) -> PyResult<u32> {
    let file: String = frame.getattr("f_code")?.getattr("co_filename")?.extract()?;
    Ok(sources::locate(file, line).line)
}
