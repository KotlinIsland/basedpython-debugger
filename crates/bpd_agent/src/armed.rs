//! what one thread's armed operation wants of the interpreter
//!
//! two operations arm instrumentation and then let a thread go: a **step**, and
//! a **restart**. both are begun from inside the stop that asked for one, both
//! follow a particular frame, and both need locations the interpreter may
//! already have been told to forget. what they need of the interpreter is the
//! same shape, so it is one registry rather than two
//!
//! a thread has **at most one** armed operation, and that is **established**
//! rather than assumed: [`crate::steps::arm`] cancels this thread's restart and
//! [`crate::restarts::arm`] cancels its step, so whichever is asked for last is
//! the one that is armed. it was an assertion first, and the assertion was
//! false — a thread stopped by an exception or a pause while a restart was in
//! flight could be asked to step, and in a release build the entry was silently
//! replaced, de-instrumenting the caller so the restart never landed
//!
//! ## what the two flags are for
//!
//! the interpreter is told to forget a location the first time it turns out to
//! be uninteresting, and `DISABLE` is **process wide**. so a line disabled
//! because one thread had no use for it is a line another thread's step would
//! never be offered again — which is a step landing somewhere other than where
//! it said. the flags are read on the event path to decide whether a location
//! may be forgotten, and they are atomics because the common answer is "nothing
//! is armed anywhere" and that answer must cost a load

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use pyo3::prelude::*;

use crate::events;

/// what one armed operation wants of the interpreter
#[derive(Debug, Clone, Default)]
pub(crate) struct Interest {
    /// the code objects it watches, by address, and what for
    pub(crate) watching: BTreeMap<usize, events::Local>,
    /// whether it wants `PY_START`, `PY_RESUME` and `PY_THROW` for the program
    pub(crate) entering: bool,
}

/// what every armed operation in the process wants, readable from any thread
///
/// the thread local of each operation answers "what is this thread doing"; this
/// answers "does anything in the process need events on this code object",
/// which is what decides the interpreter's instrumentation and what stops one
/// thread's line callback from disabling a line another is waiting for
static INTERESTS: Mutex<BTreeMap<u64, Interest>> = Mutex::new(BTreeMap::new());

/// whether anything is armed, without taking the lock
static ARMED: AtomicBool = AtomicBool::new(false);

/// whether anything still wants to catch a frame being entered
static ENTERING: AtomicBool = AtomicBool::new(false);

fn interests() -> MutexGuard<'static, BTreeMap<u64, Interest>> {
    INTERESTS
        .lock()
        .expect("the armed registry is only ever held to look one thread's entry up or write it")
}

/// whether any thread in the process has an operation armed
///
/// what stops a line callback from returning `DISABLE`
pub(crate) fn armed_anywhere() -> bool {
    ARMED.load(Ordering::Relaxed)
}

/// whether anything still needs to see a frame being entered
///
/// the same rule for `PY_START`: discovery disables it per code object, and an
/// operation that was never offered the frame it was waiting for would carry on
/// as though the frame had never been entered
pub(crate) fn entering_anywhere() -> bool {
    ENTERING.load(Ordering::Relaxed)
}

/// what the armed operations in the process want of one code object
pub(crate) fn local(address: usize) -> events::Local {
    interests()
        .values()
        .filter_map(|interest| interest.watching.get(&address))
        .copied()
        .fold(events::Local::default(), |all, one| all | one)
}

/// register one thread's operation, and publish what it wants
///
/// the caller has already taken off whatever else this thread had armed — see
/// the module note — so an entry here is the only one
pub(crate) fn hold(thread: u64, interest: Interest) {
    let replaced = interests().insert(thread, interest);
    assert!(
        replaced.is_none(),
        "arming an operation takes off whatever this thread had armed first, so \
         the registry holds nothing for it by the time this runs"
    );
    republish();
}

/// change what one thread's operation wants, if it still has one
pub(crate) fn amend<F>(thread: u64, amend: F)
where
    F: FnOnce(&mut Interest),
{
    if let Some(interest) = interests().get_mut(&thread) {
        amend(interest);
    }
    republish();
}

/// forget one thread's operation
pub(crate) fn release(thread: u64) {
    interests().remove(&thread);
    republish();
}

/// keep the two flags the event path reads in step with the registry
fn republish() {
    let interests = interests();
    ARMED.store(!interests.is_empty(), Ordering::Relaxed);
    ENTERING.store(
        interests.values().any(|interest| interest.entering),
        Ordering::Relaxed,
    );
}

/// put the interpreter's disabled locations back, for an operation being armed
///
/// a location that has already run told the interpreter never to offer it
/// again — which is exactly what a breakpoint in a function makes happen for
/// every line of it that is not the breakpoint. there is no per-location undo,
/// so an operation that has to be offered one pays for the process-wide one
pub(crate) fn restart_locations(python: Python<'_>) -> PyResult<()> {
    events::restart(python)
}
