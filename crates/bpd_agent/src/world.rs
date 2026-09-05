//! stopping the world, and being honest about the part of it that will not stop
//!
//! non-stop is the default because a live program should go on living while one
//! of its threads is inspected. the opposite is needed too: a data structure
//! several threads write to cannot be read coherently while they are writing to
//! it. so this is the explicit mode
//!
//! ## how a running thread is caught
//!
//! there is nothing in cpython that suspends a thread. what there is, is an
//! event: `LINE` armed for the whole program, so every thread executing python
//! reaches a callback, and the callback does not return until the world is
//! released. arming it means calling `restart_events()`, which undoes every
//! `DISABLE` in the process — the cost of the mode, paid back as those lines
//! disable themselves again afterwards
//!
//! ## what will not be caught, and is never counted as held
//!
//! a thread parked in a C call has already released the GIL and executes no
//! python, so it reaches no event and nothing available here can stop it. it is
//! **running**, and it is reported as running in native code. a debugger that
//! counted it among the stopped threads would be claiming a whole-program
//! snapshot it did not take, which is the exact failure this mode exists to
//! avoid

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bpd_core::Mode;
use pyo3::prelude::*;

/// whether a thread reaching a line should park
///
/// read on the event path, so it is an atomic rather than a lock: the common
/// case is that the world is not stopped and the answer costs a load
static PARKING: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
struct State {
    /// the stops that asked for the world, released when the last one resumes
    ///
    /// a set rather than one stop, because two held threads can both want a
    /// coherent view and the first to resume must not take it from the second
    requesters: Vec<u64>,
    /// the threads that were running python when the world was stopped and did
    /// not arrive within the interval that was allowed
    native: Vec<u64>,
    /// the threads parked inside a line callback right now
    parked: Vec<u64>,
}

static STATE: Mutex<State> = Mutex::new(State {
    requesters: Vec::new(),
    native: Vec::new(),
    parked: Vec::new(),
});

/// notified by a thread as it parks, waited on by the thread stopping the world
static ARRIVED: Condvar = Condvar::new();

/// notified when the world is released, waited on by every parked thread
static RELEASED: Condvar = Condvar::new();

fn state() -> MutexGuard<'static, State> {
    STATE
        .lock()
        .expect("the world lock is only ever held to add a thread to a list or read one")
}

/// whether a thread reaching a line should park
pub(crate) fn parking() -> bool {
    PARKING.load(Ordering::Relaxed)
}

/// how the program was moving, for an answer taken now
pub(crate) fn mode() -> Mode {
    let state = state();
    if state.requesters.is_empty() {
        Mode::NonStop
    } else {
        Mode::StopTheWorld {
            native: state.native.clone(),
        }
    }
}

/// hold this thread until the world is released
///
/// the GIL is released first, so parking a thread does not stop the ones that
/// have not arrived yet from getting here
pub(crate) fn park(python: Python<'_>, thread: u64) {
    python.detach(|| wait_parked(thread));
}

/// the wait itself, with nothing of the interpreter in it
///
/// what a parked thread is waiting for is [`let_go`], and nothing else can
/// wake it — which is why that is called on every path out of a release,
/// whatever else the release ran into
fn wait_parked(thread: u64) {
    let mut held = state();
    held.parked.push(thread);
    ARRIVED.notify_all();
    while PARKING.load(Ordering::Relaxed) {
        held = RELEASED
            .wait(held)
            .expect("the world lock is only ever held to add a thread to a list or read one");
    }
    held.parked.retain(|parked| *parked != thread);
}

/// wake every parked thread, and stop new arrivals parking
fn let_go() {
    PARKING.store(false, Ordering::Relaxed);
    // taken and dropped so that a thread between its check of `PARKING` and
    // its wait cannot miss the notification: it holds the lock for both, and
    // this cannot be sent while it does
    drop(state());
    RELEASED.notify_all();
}

/// what stopping the world managed to stop
#[derive(Debug)]
pub(crate) struct Stopped {
    /// the threads that arrived at a line and parked
    pub(crate) parked: Vec<u64>,
    /// the threads that never arrived, and are running in native code
    pub(crate) native: Vec<u64>,
}

/// stop every thread that can be stopped, and name the ones that cannot
///
/// `targets` is every thread that was running python and is not already held.
/// `arm` puts the global `LINE` instrumentation in place and is the caller's,
/// because arming needs the interpreter and this module deliberately does the
/// waiting rather than the interpreter work
pub(crate) fn stop(
    python: Python<'_>,
    requester: u64,
    targets: Vec<u64>,
    settle: Duration,
    arm: impl FnOnce(Python<'_>) -> PyResult<()>,
) -> PyResult<Stopped> {
    {
        let mut held = state();
        held.requesters.push(requester);
    }
    // set before the instrumentation is armed, so a thread that reaches a line
    // as it goes on parks rather than telling the interpreter never to offer
    // that line again
    PARKING.store(true, Ordering::Relaxed);
    if let Err(error) = arm(python) {
        if last_to_leave(requester) {
            let_go();
        }
        return Err(error);
    }

    let deadline = Instant::now() + settle;
    let parked = python.detach(|| {
        let mut held = state();
        loop {
            let arrived: Vec<u64> = targets
                .iter()
                .copied()
                .filter(|thread| held.parked.contains(thread))
                .collect();
            if arrived.len() == targets.len() {
                return arrived;
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return arrived;
            };
            held = ARRIVED
                .wait_timeout(held, left)
                .expect("the world lock is only ever held to add a thread to a list or read one")
                .0;
        }
    });

    let native: Vec<u64> = targets
        .into_iter()
        .filter(|thread| !parked.contains(thread))
        .collect();

    let mut held = state();
    // recorded once, and deliberately not recomputed per answer: a thread that
    // arrives after this is one bpd said was running when it was. overstating
    // what was moving is the safe direction to be wrong in
    held.native.clone_from(&native);
    Ok(Stopped { parked, native })
}

/// let the world go, if this stop was the last one holding it
///
/// called by the thread whose stop is ending, because putting the
/// instrumentation back needs the interpreter and the connection's reader
/// thread has no GIL to do it with
pub(crate) fn release(
    python: Python<'_>,
    stop: u64,
    disarm: impl FnOnce(Python<'_>) -> PyResult<()>,
) -> PyResult<()> {
    release_with(stop, || disarm(python))
}

/// the release, with nothing of the interpreter in it
///
/// **the parked threads are let go whatever the disarm says.** they are
/// waiting on that one notification and nothing else can give it, so a disarm
/// that fails and returns before it would leave every one of them parked for
/// the rest of the process — inside a line callback, holding whatever locks
/// the program had. the failure is still the answer; it is just not allowed
/// to take the program with it
fn release_with<E>(stop: u64, disarm: impl FnOnce() -> Result<(), E>) -> Result<(), E> {
    if !last_to_leave(stop) {
        return Ok(());
    }
    let disarmed = disarm();
    let_go();
    disarmed
}

/// take one requester out, and say whether it was the last one holding the
/// world
///
/// a stop that never asked for the world is not a requester and leaves nothing
fn last_to_leave(stop: u64) -> bool {
    let mut held = state();
    let Some(index) = held.requesters.iter().position(|asked| *asked == stop) else {
        return false;
    };
    held.requesters.remove(index);
    if !held.requesters.is_empty() {
        return false;
    }
    held.native.clear();
    true
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn a_disarm_that_fails_still_lets_the_parked_threads_go() {
        // the interpreter is not needed to park a thread or to release the
        // world: the wait and the wake are the condvar's, and the only thing
        // the interpreter is asked for is the disarm, which here refuses
        state().requesters.push(41);
        PARKING.store(true, Ordering::Relaxed);

        let (woke, woken) = mpsc::channel();
        let parked = std::thread::spawn(move || {
            wait_parked(7);
            woke.send(()).expect("the test is waiting on this");
        });
        // parked for real before the release, or the release could win a race
        // that is not what is under test
        let arrived = Instant::now() + Duration::from_secs(5);
        while !state().parked.contains(&7) {
            assert!(Instant::now() < arrived, "the thread never parked");
            std::thread::sleep(Duration::from_millis(1));
        }

        let outcome = release_with(41, || Err("the interpreter refused the disarm"));
        assert_eq!(outcome, Err("the interpreter refused the disarm"));
        assert!(
            woken.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the disarm failed and the parked thread was left parked for ever"
        );
        parked.join().expect("the parked thread returned");
        assert!(state().parked.is_empty());
        assert!(state().requesters.is_empty());
    }
}
