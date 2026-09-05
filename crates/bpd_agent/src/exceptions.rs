//! stopping where an exception is raised, and where one leaves the program
//!
//! two settings, and they answer different questions, because cpython only lets
//! them be different questions
//!
//! ## raised is knowable, and it is knowable once
//!
//! `RAISE` fires when an exception is set in a frame — and cpython fires it
//! **again in every frame the exception propagates into**, with the same
//! exception object, as it looks for a handler. so a naive "stop on every
//! raise" stops once per frame of the stack for one `raise` statement
//!
//! measured rather than assumed:
//! `the_interpreter_raises_an_exception_event_in_every_frame_it_passes_through`
//! runs it in a bare interpreter. what bpd reports is where an exception is
//! **raised**, which is the frame the `raise` is in and the point at which the
//! whole stack is still standing — and not the frames it propagates into
//!
//! the same object can be raised more than once. a program that keeps an
//! exception and raises it in a loop raises it every time round, and a debugger
//! that remembered the object rather than the raise would report the first and
//! miss the rest. so the event is read with the instruction it names: the same
//! object arriving at a `RAISE_VARARGS` is a `raise` statement of the program
//! and a new stop, and arriving anywhere else — at the call that ran the frame
//! it came out of — is propagation. a bare `raise` is a `RERAISE` event, which
//! is not listened for, and is a continuation the same way
//!
//! what that cannot tell apart is C code raising a kept object again: a
//! coroutine's exception stored by asyncio's task and raised into the awaiting
//! coroutine arrives at the `await`, with no `raise` statement to point at.
//! `an_object_c_code_raises_again_is_reported_as_the_same_exception_propagating`
//! pins it
//!
//! the exception a thread last reported is held by a strong reference for as
//! long as it is the last one. a pointer would be cheaper and wrong: a freed
//! object's address is handed straight back to the next one, and a new
//! exception at the old address would be read as the old one still propagating
//!
//! ## uncaught is not knowable at the raise, and is not guessed
//!
//! whether an exception will be caught is decided by what happens after it is
//! raised. a debugger that answered at the raise would be scanning exception
//! tables and predicting, and a wrong prediction here is a stop that says
//! "nothing will catch this" about something a library catches a frame later
//!
//! so it is answered where it is known: at the `PY_UNWIND` that takes the
//! exception out of a frame with no caller bpd would report. the cost of
//! knowing rather than predicting is that the frames it came through have
//! already been popped — what is left of them is the exception's own traceback,
//! which is what the stop carries
//!
//! **an exception that escapes a `threading.Thread`'s target is not uncaught**,
//! and is not reported as one: `threading` catches it in `_bootstrap_inner` and
//! hands it to `threading.excepthook`. that is cpython's behaviour rather than
//! a limit of this design, and it is
//! `an_exception_a_worker_thread_lets_escape_is_caught_by_threading_itself`

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use pyo3::prelude::*;
use pyo3::types::PyBytes;

include!(concat!(env!("OUT_DIR"), "/opcodes.rs"));

/// stop where an exception is raised
static RAISED: AtomicBool = AtomicBool::new(false);

/// stop where an exception leaves the outermost frame
static UNCAUGHT: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// the exception this thread was last stopped for
    ///
    /// held, not pointed at, so an address that came round again cannot be
    /// mistaken for the exception that used to live there
    static REPORTED: RefCell<Option<Py<PyAny>>> = const { RefCell::new(None) };
}

/// whether a raise should stop the thread that made it
pub(crate) fn raised() -> bool {
    RAISED.load(Ordering::Relaxed)
}

/// whether an exception leaving the outermost frame should stop its thread
pub(crate) fn uncaught() -> bool {
    UNCAUGHT.load(Ordering::Relaxed)
}

/// set both, together, because the request carries both
pub(crate) fn watch(raised: bool, uncaught: bool) {
    RAISED.store(raised, Ordering::Relaxed);
    UNCAUGHT.store(uncaught, Ordering::Relaxed);
}

/// whether this `RAISE` event is an exception being raised, rather than one
/// this thread has already been stopped for propagating into another frame
///
/// the propagation of one exception raises the event once per frame with the
/// same object. a `raise` statement raising that object again is a new raise,
/// and the instruction the event names is what says which this is
pub(crate) fn newly_raised(
    python: Python<'_>,
    code: &Bound<'_, PyAny>,
    offset: i32,
    exception: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let seen_before = REPORTED.with(|cell| {
        cell.borrow()
            .as_ref()
            .is_some_and(|last| exception.is(last.bind(python)))
    });
    if seen_before && !raised_by_a_statement(code, offset)? {
        return Ok(false);
    }
    REPORTED.with(|cell| *cell.borrow_mut() = Some(exception.clone().unbind()));
    Ok(true)
}

/// whether the instruction a `RAISE` event names is a `raise` statement
///
/// `co_code` rather than the adaptive bytecode: `RAISE_VARARGS` has no
/// specialised form, but the interpreter's own copy is the one that could gain
/// one, and `co_code` is the one it promises is the compiled program
fn raised_by_a_statement(code: &Bound<'_, PyAny>, offset: i32) -> PyResult<bool> {
    let bytecode = code.getattr("co_code")?;
    let bytecode = bytecode.cast::<PyBytes>()?;
    let Ok(offset) = usize::try_from(offset) else {
        unreachable!(
            "a RAISE event named instruction offset {offset}, and offsets are not negative"
        );
    };
    let Some(opcode) = bytecode.as_bytes().get(offset) else {
        unreachable!(
            "a RAISE event named instruction offset {offset} in a code object of {} bytes",
            bytecode.len()?
        );
    };
    Ok(*opcode == RAISE_VARARGS)
}
