//! stepping, which is one thread's business
//!
//! a stop holds one thread and the rest of the program keeps running, so a step
//! steps **one** thread and says nothing about the others. the state of a step
//! therefore lives in a thread local: the thread that asked is the thread that
//! is held, the thread that arms it, and the thread every event about it
//! arrives on
//!
//! ## a step follows a frame, not a code object
//!
//! the thing a step is "in" is a frame, and a code object is not one. a
//! recursive call re-enters the same code object in a different frame, and a
//! generator or a coroutine re-enters the same code object in the **same**
//! frame. so the step holds the frame object itself and compares identity
//!
//! it holds a **strong** reference to it, and that is load bearing rather than
//! tidy: the address of a frame that has been freed is handed straight back to
//! the next one. two coroutines awaited one after another from the same line
//! get the same frame address, which
//! `a_coroutine_awaited_from_two_places_steps_into_the_right_one` would not
//! catch a pointer comparison out on unless something kept the first alive
//!
//! ## what leaves a frame, and what does not
//!
//! `PY_RETURN` and `PY_UNWIND` finish a frame. `PY_YIELD` does not — it hands
//! control away and the frame is resumed later, still holding its locals and
//! still where the step is. so a step over an `await` lands on the next line of
//! the same coroutine rather than somewhere in the event loop, and a step out
//! of a generator runs it to its end rather than to its next `yield`
//!
//! ## what it costs
//!
//! arming a step calls `restart_events()`. a line of the frame being stepped in
//! may have run before and told the interpreter never to offer it again — which
//! is exactly what a breakpoint in that function makes happen — and a step that
//! silently skipped it would land somewhere other than where it said. there is
//! no per-location undo, so the process-wide one is what there is
//!
//! what a step wants of the interpreter lives in [`crate::armed`], shared with
//! the other operation that arms instrumentation and lets a thread go — a
//! restart. the two are different operations and what they need is the same
//! shape, so the registry the event path reads is one rather than two

use std::cell::RefCell;

use bpd_core::StepKind;
use pyo3::prelude::*;

use crate::armed::{self, Interest};
use crate::{events, frames, session};

/// one thread's step, held on that thread
#[derive(Debug)]
struct Step {
    kind: StepKind,
    /// the interpreter's identity for the thread, which keys the interest
    thread: u64,
    /// the frame the step is following
    frame: Py<PyAny>,
    /// the code objects it has armed, so they can be put back as they were
    armed: Vec<Py<PyAny>>,
    /// whether reaching a line of `frame` is where it lands
    ///
    /// false only for a step out that has not left its frame yet
    landing: bool,
    /// whether a frame this thread enters is what it follows next
    entering: bool,
}

thread_local! {
    /// the step this thread is making, if it is making one
    static STEP: RefCell<Option<Step>> = const { RefCell::new(None) };
}

/// whether this thread is stepping
pub(crate) fn armed_here() -> bool {
    STEP.with(|cell| cell.borrow().is_some())
}

/// begin a step on the thread that is about to be let go
///
/// called from the stop the step was asked for, so `sys._getframe()` is the
/// frame that stopped and the step follows it from here
pub(crate) fn arm(python: Python<'_>, thread: u64, kind: StepKind) -> PyResult<()> {
    debug_assert!(
        !armed_here(),
        "a thread is held while it is stepping, so it cannot be asked to step \
         again until the step it is making has landed"
    );

    // a restart in flight on this thread is taken off first. both want the same
    // registry entry, and a thread asked to step while one was armed used to
    // replace it silently — which de-instrumented the caller and left the
    // restart unable to land
    crate::restarts::cancel(python)?;

    let frame = events::current_frame(python)?;
    let mut step = Step {
        kind,
        thread,
        frame: frame.clone().unbind(),
        armed: Vec::new(),
        landing: !matches!(kind, StepKind::Out),
        entering: matches!(kind, StepKind::In),
    };

    armed::hold(
        thread,
        Interest {
            entering: step.entering,
            ..Interest::default()
        },
    );

    watch(python, &mut step, &frame.getattr("f_code")?)?;
    STEP.with(|cell| *cell.borrow_mut() = Some(step));

    // a line of this frame that has already run told the interpreter never to
    // offer it again. there is no per-location undo, so the step that has to be
    // offered it pays for the process-wide one
    armed::restart_locations(python)?;
    session::refresh_events(python)
}

/// take this thread's step off, wherever it got to
///
/// a no-op when there is none, so the line path can call it whenever something
/// else decided to hold the thread
pub(crate) fn cancel(python: Python<'_>) -> PyResult<()> {
    let taken = STEP.with(|cell| cell.borrow_mut().take());
    match taken {
        Some(step) => release(python, &step),
        None => Ok(()),
    }
}

/// a line of a code object some step watches is about to run
///
/// the step it belongs to, when it belongs to this thread's and this thread's
/// is waiting for exactly this frame
pub(crate) fn reached_line(python: Python<'_>) -> PyResult<Option<StepKind>> {
    decide(python, |python, step| {
        if !step.landing {
            return Ok(Outcome::Carry);
        }
        let frame = events::current_frame(python)?;
        Ok(if frame.is(step.frame.bind(python)) {
            Outcome::Land
        } else {
            // the same code object in another frame: a recursive call, or a
            // second generator built from the same function
            Outcome::Carry
        })
    })
}

/// this thread entered a frame — a call, a resumption, or a `throw()` into one
pub(crate) fn entered_frame(python: Python<'_>) -> PyResult<()> {
    decide(python, |python, step| {
        if !step.entering {
            return Ok(Outcome::Carry);
        }
        let frame = events::current_frame(python)?;
        // the step's own frame waking up is not a frame it entered. it is the
        // one it was already in, and the next line of it is still the landing
        if frame.is(step.frame.bind(python)) {
            return Ok(Outcome::Carry);
        }
        follow(python, step, frame)?;
        Ok(Outcome::Carry)
    })?;
    Ok(())
}

/// a frame finished — returned, or was left by an exception
///
/// deliberately not a yield: that suspends a frame rather than finishing it
pub(crate) fn left_frame(python: Python<'_>) -> PyResult<()> {
    decide(python, |python, step| {
        let frame = events::current_frame(python)?;
        if !frame.is(step.frame.bind(python)) {
            return Ok(Outcome::Carry);
        }

        let caller = frame.getattr("f_back")?;
        if caller.is_none() || frames::is_bootstrap(&caller) {
            // there is no frame above this one that bpd would ever report, so
            // the step has nowhere left to land. it is given up rather than
            // left armed, and what the program does next — finish, or raise —
            // is what the client is told about
            return Ok(Outcome::Abandon);
        }
        follow(python, step, caller)?;
        Ok(Outcome::Carry)
    })?;
    Ok(())
}

/// what a step event decided
enum Outcome {
    /// the step goes on
    Carry,
    /// the step is over and the thread is held here
    Land,
    /// the step can never complete and is given up
    Abandon,
}

/// run `decide` against this thread's step, if it has one
///
/// the step is taken out of the cell for the duration, so nothing that runs
/// inside can find it half updated. everything `decide` calls is interpreter
/// state — a frame, a code object, an instrumentation change — and none of it
/// runs the program's code, so nothing re-enters this
fn decide<F>(python: Python<'_>, decide: F) -> PyResult<Option<StepKind>>
where
    F: FnOnce(Python<'_>, &mut Step) -> PyResult<Outcome>,
{
    let Some(mut step) = STEP.with(|cell| cell.borrow_mut().take()) else {
        return Ok(None);
    };

    match decide(python, &mut step) {
        Ok(Outcome::Land) => {
            let kind = step.kind;
            release(python, &step)?;
            Ok(Some(kind))
        }
        Ok(Outcome::Abandon) => {
            release(python, &step)?;
            Ok(None)
        }
        Ok(Outcome::Carry) => {
            STEP.with(|cell| *cell.borrow_mut() = Some(step));
            Ok(None)
        }
        Err(error) => {
            STEP.with(|cell| *cell.borrow_mut() = Some(step));
            Err(error)
        }
    }
}

/// follow a different frame from here on
fn follow(python: Python<'_>, step: &mut Step, frame: Bound<'_, PyAny>) -> PyResult<()> {
    let code = frame.getattr("f_code")?;
    step.frame = frame.unbind();
    step.landing = true;
    step.entering = false;

    let stale = std::mem::take(&mut step.armed);
    armed::amend(step.thread, |interest| {
        interest.watching.clear();
        interest.entering = false;
    });
    for code in &stale {
        session::refresh_code(python, code.bind(python))?;
    }

    watch(python, step, &code)?;
    session::refresh_events(python)
}

/// arm one code object for this step
fn watch(python: Python<'_>, step: &mut Step, code: &Bound<'_, PyAny>) -> PyResult<()> {
    let wanted = events::Local {
        line: step.landing,
        py_return: true,
        // a step never wants a start on a code object it is following: a frame
        // being entered is caught globally, because "some frame, somewhere"
        // cannot be scoped to a code object that has not been reached yet
        py_start: false,
    };
    armed::amend(step.thread, |interest| {
        interest.watching.insert(code.as_ptr() as usize, wanted);
    });
    step.armed.push(code.clone().unbind());
    session::refresh_code(python, code)
}

/// put back everything a step armed, and forget it
fn release(python: Python<'_>, step: &Step) -> PyResult<()> {
    armed::release(step.thread);

    for code in &step.armed {
        session::refresh_code(python, code.bind(python))?;
    }
    session::refresh_events(python)
}
