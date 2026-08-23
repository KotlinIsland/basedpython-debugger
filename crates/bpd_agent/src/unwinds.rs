//! forcing the frames above one out, so that it can be run again where it stands
//!
//! [`crate::inplace`] resets the frame the thread is executing. a frame further
//! down cannot be reset while frames are live above it: cpython **crashes**
//! rather than refuses when a frame that is not executing is moved, measured on
//! 3.13, 3.14 and 3.15. so the frames above go first, and this is what drives
//! that
//!
//! the thread has to be **let go** for it — a frame leaves by returning, and
//! returning is the interpreter running. so unlike a one-frame reset, this
//! arranges something and a stop arrives later:
//!
//! 1. the innermost frame is moved to a clean exit before the thread is let go,
//!    so it returns
//! 2. every `LINE` event on the way down is offered here. one in a frame still
//!    above the target moves that frame to its own clean exit, so it returns too
//! 3. one in the **target** is where the reset is made, and that is the stop
//!
//! ## what runs in between, and why it is asked about first
//!
//! between a frame returning and the next `LINE` event in the frame below it,
//! the rest of that frame's call line runs — with the value the forced return
//! produced, which is a value the program never computed. bpd cannot be there
//! for it: there is no event between the two
//!
//! so it is decided in advance, off the bytecode, for **every** frame in the
//! chain rather than only the first. [`crate::bytecode::tail_after`] is the
//! question and [`bpd_core::Unresettable::ATailWouldRun`] is the refusal. a tail
//! that is loads and stores into the frame's own locals is nothing: the frame is
//! being discarded, and the target's own locals are unbound by the reset anyway.
//! a tail that calls something, or writes a global, a cell or a name, is the
//! program doing something it would never have done, and the whole request is
//! refused rather than the tail being allowed to run
//!
//! ## which frame, not which code object
//!
//! every identity here is a frame object compared with `is`, and every reference
//! is **strong**, for the reason [`crate::steps`] holds one: the address of a
//! freed frame is handed straight to the next one, so a recursive call would
//! look like the frame that was being watched for

use std::cell::RefCell;

use bpd_core::{Abandoned, StopReason};
use pyo3::prelude::*;

use crate::armed::{self, Interest};
use crate::{events, frames, inplace, session, sources};

/// one thread's unwind, held on that thread
#[derive(Debug)]
struct Unwind {
    /// the interpreter's identity for the thread, which keys the interest
    thread: u64,
    /// the frame that will be reset once everything above it has gone
    target: Py<PyAny>,
    /// the frames still above it, innermost first
    ///
    /// emptied as they go. a frame whose line ends in a return leaves without
    /// being forced, so this shrinks by more than one at a time
    above: Vec<Py<PyAny>>,
    /// `co_qualname` of what is being reset, for the stop that says so
    function: String,
    /// the code objects it has armed, so they can be put back as they were
    watching: Vec<Py<PyAny>>,
}

thread_local! {
    /// the unwind this thread is making, if it is making one
    static UNWIND: RefCell<Option<Unwind>> = const { RefCell::new(None) };
}

/// whether this thread is unwinding to a frame
pub(crate) fn armed_here() -> bool {
    UNWIND.with(|cell| cell.borrow().is_some())
}

/// what an unwind does with the line event it is offered
#[derive(Debug)]
pub(crate) enum Reached {
    /// this event is not the one an unwind is waiting for
    NotMine,
    /// a frame above the target was moved to its exit, and this line will not run
    Forced,
    /// the target was reset, and this is what the thread is held with
    Reset(StopReason),
    /// the unwind cannot complete, and this is what the thread is held with
    Abandoned(StopReason),
}

/// begin an unwind on the thread that is about to be let go
///
/// called from inside the stop that asked for it, with the innermost frame
/// already moved to its exit. `above` is the frames between the target and the
/// top, innermost first, and the first of them is the one that has been moved
pub(crate) fn arm(
    python: Python<'_>,
    thread: u64,
    target: &Bound<'_, PyAny>,
    above: &[Bound<'_, PyAny>],
    function: String,
) -> PyResult<()> {
    // a step in flight on this thread is taken off first, for the reason
    // `steps::arm` takes off a restart: they share one registry entry
    crate::steps::cancel(python)?;

    let mut unwind = Unwind {
        thread,
        target: target.clone().unbind(),
        above: above.iter().map(|frame| frame.clone().unbind()).collect(),
        function,
        watching: Vec::new(),
    };
    armed::hold(thread, Interest::default());
    // the target and everything still above it. a frame that leaves without a
    // line event costs one arming nobody reads, which is cheaper than working
    // out in advance which of them those are
    watch(python, &mut unwind, &target.getattr("f_code")?)?;
    for frame in above {
        let code = frame.getattr("f_code")?;
        watch(python, &mut unwind, &code)?;
    }
    UNWIND.with(|cell| *cell.borrow_mut() = Some(unwind));

    // a line of a frame that has already run told the interpreter never to offer
    // it again, and the reset is made at one of those
    armed::restart_locations(python)?;
    session::refresh_events(python)
}

/// take this thread's unwind off, wherever it got to
///
/// a no-op when there is none, so a path that decided to hold the thread for
/// some other reason can call it unconditionally — one left armed keeps
/// `armed_anywhere()` true and stops the interpreter forgetting a single
/// location for the rest of the run
pub(crate) fn cancel(python: Python<'_>) -> PyResult<()> {
    let taken = UNWIND.with(|cell| cell.borrow_mut().take());
    match taken {
        Some(unwind) => release(python, &unwind),
        None => Ok(()),
    }
}

/// a line is about to run, and an unwind may want it instead
///
/// asked **before** the breakpoints decide anything, for the reason a rewind is:
/// a frame forced out here does not run this line at all, so a breakpoint that
/// fired would be reporting the program at a line it never executed
pub(crate) fn reaching(python: Python<'_>) -> PyResult<Reached> {
    let Some(mut unwind) = UNWIND.with(|cell| cell.borrow_mut().take()) else {
        return Ok(Reached::NotMine);
    };
    let frame = events::current_frame(python)?;

    if frame.is(unwind.target.bind(python)) {
        // everything above has gone, or this event could not have happened in
        // the target — it is only reached when the frames above it have returned
        let outcome = inplace::reset(&frame)?;
        release(python, &unwind)?;
        return Ok(match outcome {
            Ok(reset) => Reached::Reset(StopReason::FrameReset(reset)),
            Err(why) => {
                let at = frames::describe_where(&frame)?;
                Reached::Abandoned(StopReason::RestartAbandoned {
                    function: unwind.function.clone(),
                    wanted: reported_line(&frame, at.line)?,
                    file: at.file,
                    line: at.line,
                    why: Abandoned::Refused {
                        error: refusal_as(python, &why),
                    },
                })
            }
        });
    }

    let above = unwind
        .above
        .iter()
        .position(|held| frame.is(held.bind(python)));
    let Some(above) = above else {
        // the target's code object in another frame, or a frame this unwind is
        // not about at all
        UNWIND.with(|cell| *cell.borrow_mut() = Some(unwind));
        return Ok(Reached::NotMine);
    };

    // everything inside this one has already gone, so they are dropped with it
    let frames_gone = above + 1;
    match force_out(python, &frame)? {
        Ok(()) => {
            unwind.above.drain(..frames_gone);
            UNWIND.with(|cell| *cell.borrow_mut() = Some(unwind));
            Ok(Reached::Forced)
        }
        Err(bpd_core::Restarted::Refused { error, .. }) => {
            let at = frames::describe_where(&frame)?;
            let stop = StopReason::RestartAbandoned {
                function: unwind.function.clone(),
                wanted: reported_line(&frame, at.line)?,
                file: at.file,
                line: at.line,
                why: Abandoned::Refused { error },
            };
            release(python, &unwind)?;
            Ok(Reached::Abandoned(stop))
        }
        Err(other) => unreachable!("a refused force-out is `Refused`: {other:?}"),
    }
}

/// a frame an unwind still needs was left before it could finish
///
/// only the **target** matters. a frame above it leaving is the whole point, and
/// one below it cannot be left before the target is
pub(crate) fn left_frame(python: Python<'_>) -> PyResult<Option<StopReason>> {
    let Some(unwind) = UNWIND.with(|cell| cell.borrow_mut().take()) else {
        return Ok(None);
    };
    let leaving = events::current_frame(python)?;
    if !leaving.is(unwind.target.bind(python)) {
        UNWIND.with(|cell| *cell.borrow_mut() = Some(unwind));
        return Ok(None);
    }

    // the frame that was to be run again has gone instead. an exception came
    // through it, or a tail bpd read as inert raised anyway — either way there
    // is nothing left to reset, and saying so is the point
    let at = frames::describe_where(&leaving)?;
    let reason = StopReason::RestartAbandoned {
        function: unwind.function.clone(),
        wanted: reported_line(&leaving, at.line)?,
        file: at.file,
        line: at.line,
        why: Abandoned::CallerLeft,
    };
    release(python, &unwind)?;
    Ok(Some(reason))
}

/// move a frame to a point in its own code where a return's value is loaded
///
/// the same act a one-frame restart makes on the frame it forces out, and it
/// fails the same way: cpython accepting the move is cpython's answer and it
/// gives it at the time, so a refusal is reported rather than assumed away
pub(crate) fn force_out(
    python: Python<'_>,
    frame: &Bound<'_, PyAny>,
) -> PyResult<Result<(), bpd_core::Restarted>> {
    let code = frame.getattr("f_code")?;
    // a namespace refusal means the exits cannot be trusted, and an empty list
    // is the same answer here as having none: the move is refused and says so
    let exits: Vec<crate::bytecode::Exit> =
        crate::bytecode::exit_tails(&code, &frames::namespaces_of(frame)?)?.unwrap_or_default();
    let mut refused = None;
    for exit in &exits {
        match crate::linetable::move_to(frame, &code, exit.offset)? {
            Ok(()) => return Ok(Ok(())),
            // bpd could not establish the mechanism, which is not cpython
            // refusing this offset and is not a reason to try the next one
            Err(crate::linetable::Unmarked::Unusable(part)) => {
                refused = Some(pyo3::exceptions::PyRuntimeError::new_err(format!(
                    "the exit on line {} could not be addressed: {part}",
                    exit.line
                )));
                break;
            }
            Err(crate::linetable::Unmarked::Refused { error }) => refused = Some(error),
        }
    }
    let error = refused.unwrap_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(
            "the frame has no exit, which the request established that it had",
        )
    });
    Ok(Err(bpd_core::Restarted::Refused {
        tried: exits.iter().map(|exit| exit.line).collect(),
        error: crate::conditions::capture(python, &error),
    }))
}

/// arm one code object for this unwind
fn watch(python: Python<'_>, unwind: &mut Unwind, code: &Bound<'_, PyAny>) -> PyResult<()> {
    let wanted = events::Local {
        // the only event an unwind reads. every frame it touches is reached at
        // one, and the reset is made at one
        line: true,
        py_return: false,
        py_start: false,
    };
    armed::amend(unwind.thread, |interest| {
        interest.watching.insert(code.as_ptr() as usize, wanted);
    });
    unwind.watching.push(code.clone().unbind());
    session::refresh_code(python, code)
}

/// put back everything an unwind armed, and forget it
fn release(python: Python<'_>, unwind: &Unwind) -> PyResult<()> {
    armed::release(unwind.thread);
    for code in &unwind.watching {
        session::refresh_code(python, code.bind(python))?;
    }
    session::refresh_events(python)
}

/// a refusal from the reset, in the shape an abandoned restart carries
fn refusal_as(python: Python<'_>, why: &bpd_core::Unresettable) -> bpd_core::PythonError {
    crate::conditions::capture(
        python,
        &pyo3::exceptions::PyRuntimeError::new_err(why.to_string()),
    )
}

/// a line of the interpreter's, said the way a client is told it
fn reported_line(frame: &Bound<'_, PyAny>, line: u32) -> PyResult<u32> {
    let file: String = frame.getattr("f_code")?.getattr("co_filename")?.extract()?;
    Ok(sources::locate(file, line).line)
}
