//! what every thread of the debuggee is doing, as a sample
//!
//! a stop holds one thread and says the rest keep running. this is how that
//! claim is checked, and it is the general half of the problem the non-stop
//! model creates: **the held thread still holds its locks**. stopped inside
//! `with lock:`, every other thread wanting that lock piles up behind it, and
//! from the outside it looks exactly like `bpd` hanging
//!
//! cpython exposes no owner for a lock. there is no registry of them, and
//! `_thread.lock` records nothing about who took it, so "thread 7 is waiting
//! for a lock thread 3 holds" is not knowable and is not claimed. what a
//! debugger *can* do is look twice: a thread that was in the same place at both
//! samples got nowhere in between, and where it was is where to look. that is a
//! symptom, it is reported as a symptom, and it is the thing the user is
//! actually staring at
//!
//! the one lock cpython does make knowable is the import lock, because the
//! import machinery runs in python frames — see [`crate::frames::holding`]

use std::collections::BTreeMap;
use std::time::Duration;

use bpd_protocol::message::{FromAgent, Progress, ThreadState, Where};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::{events, frames, stops, world};

/// where a thread was, and how far into it, at one sample
///
/// the frame's address and `f_lasti` together, rather than a line number: a
/// thread going round a one-line loop is on the same line every time it is
/// looked at, and calling that "still" would be reporting a busy thread as a
/// stuck one
#[derive(Debug, Clone, PartialEq, Eq)]
struct Spot {
    frame: usize,
    offset: i32,
    at: Option<Where>,
}

/// every thread the interpreter knows about, twice, `settle` apart
pub(crate) fn census(python: Python<'_>, settle: Duration) -> PyResult<FromAgent> {
    let first = sample(python)?;
    if !settle.is_zero() {
        python.detach(|| std::thread::sleep(settle));
    }
    let second = sample(python)?;

    let mut threads = Vec::with_capacity(second.len());
    for (thread, spot) in second {
        let held = stops::held_for(thread);
        let progress = if held.is_some() {
            Progress::Held
        } else if first.get(&thread) == Some(&spot) {
            Progress::Still
        } else {
            Progress::Moved
        };
        threads.push(ThreadState {
            thread,
            held,
            at: spot.at,
            progress,
        });
    }

    Ok(FromAgent::Threads {
        threads,
        settle_ms: u32::try_from(settle.as_millis())
            .expect("the interval came from a u32 of milliseconds"),
        mode: world::mode(),
    })
}

/// the threads that were running python and are not held by a stop
///
/// what stopping the world has to wait for, and what a pause says it expects to
/// hear from. a thread already held by a stop is held; a thread with no python
/// frame at all does not exist as far as `sys._current_frames` is concerned and
/// cannot be waited for
///
/// `except` is the thread asking, when one is asking. a pause is armed from a
/// thread of the agent's own and excludes nothing
pub(crate) fn running(python: Python<'_>, except: Option<u64>) -> PyResult<Vec<u64>> {
    Ok(sample(python)?
        .into_keys()
        .filter(|thread| Some(*thread) != except && stops::held_for(*thread).is_none())
        .collect())
}

/// one look at every thread's innermost frame
fn sample(python: Python<'_>) -> PyResult<BTreeMap<u64, Spot>> {
    let frames = events::current_frames(python)?;
    let frames = frames.cast::<PyDict>()?;
    let mut sampled = BTreeMap::new();

    for (thread, frame) in frames {
        let thread: u64 = thread.extract()?;
        // the agent's own bootstrap is the outermost frame of the process and
        // never a location of the program's, so a thread whose innermost frame
        // is that one has no python frame worth reporting
        let at = if frames::is_bootstrap(&frame) {
            None
        } else {
            Some(frames::describe_where(&frame)?)
        };
        sampled.insert(
            thread,
            Spot {
                frame: frame.as_ptr() as usize,
                offset: frame.getattr("f_lasti")?.extract()?,
                at,
            },
        );
    }
    Ok(sampled)
}
