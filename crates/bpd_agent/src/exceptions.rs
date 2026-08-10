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
//! runs it in a bare interpreter. what bpd reports is the **first** sighting of
//! an exception on a thread, which is the frame it was raised in and the point
//! at which the whole stack is still standing
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

/// whether this is the first time this thread has seen this exception
///
/// the propagation of one exception raises the event once per frame, and those
/// are the same exception rather than new ones
pub(crate) fn newly_raised(python: Python<'_>, exception: &Bound<'_, PyAny>) -> bool {
    REPORTED.with(|cell| {
        let mut reported = cell.borrow_mut();
        if reported
            .as_ref()
            .is_some_and(|last| exception.is(last.bind(python)))
        {
            return false;
        }
        *reported = Some(exception.clone().unbind());
        true
    })
}
