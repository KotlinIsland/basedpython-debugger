//! the stack an asyncio task was created on
//!
//! `await` preserves a stack and `create_task` severs it. a task that raises has
//! a traceback of **one frame** and the process exits **0**, because an
//! exception nobody retrieved is handed to a loop handler rather than raised —
//! measured on 3.13, 3.14 and 3.15. the frames that scheduled the work are gone
//! from the stack by the time anything stops in it
//!
//! ## how the stack is caught, and why this way
//!
//! `BaseEventLoop.create_task` is a **python** function, so its own code object
//! takes local `PY_RETURN` events and the callback is handed what it returned —
//! the `Task` — with the stack that made it still on the thread. one event gives
//! both halves
//!
//! it is the loop's method rather than `asyncio.create_task` because **every**
//! route reaches it — measured, against `create_task`, `ensure_future`,
//! `loop.create_task` and a task group's own. see [`notice`]
//!
//! the routes that do not work were measured before this one was built:
//!
//! - **`Task._source_traceback`** is exactly this record and is `None` unless
//!   asyncio debug mode is on. bpd cannot turn that on: `loop.get_debug()` is
//!   something the program reads, and debug mode changes what the program does
//! - **`Task._asyncio_awaited_by` and `asyncio.tools.get_all_awaited_by`** —
//!   3.14's external introspection — are the **await** tree, which is who is
//!   waiting on a task *now*. the case this exists for is a task nobody ever
//!   awaits, and it is `None` for exactly that task
//!
//! ## what it is keyed on
//!
//! the **task**, weakly. the coroutine's frame is the same object at creation as
//! at run — measured on all four interpreters — and is still the wrong key: a
//! frame address is not an identity, which
//! `the_interpreter_hands_a_freed_frames_address_to_the_next_one` already
//! records, and holding the frame alive to keep one meaningful would mean
//! holding every task the program ever made
//!
//! a weak key also answers what becomes of a record when its task is collected.
//! it goes with it, which is the truth: there is nothing left to ask about

use std::collections::BTreeMap;
use std::sync::RwLock;

use bpd_core::Scheduling;
use pyo3::prelude::*;

use crate::events;

/// what this module remembers
#[derive(Default)]
struct State {
    /// `BaseEventLoop.create_task`'s code object, once asyncio has been imported
    hook: Option<Py<PyAny>>,
    /// the stack each live task was created on, by the task's address
    ///
    /// the address is only ever used while [`Forget`] guarantees the entry is
    /// removed when that task dies, so it can never name a different one — and
    /// the weak reference is held **here**, because a weak reference that is
    /// itself collected never calls its callback and the entry would then
    /// outlive its task
    recorded: BTreeMap<usize, (Py<PyAny>, Vec<Scheduling>)>,
}

static STATE: RwLock<State> = RwLock::new(State {
    hook: None,
    recorded: BTreeMap::new(),
});

fn read() -> std::sync::RwLockReadGuard<'static, State> {
    STATE
        .read()
        .unwrap_or_else(|_| unreachable!("nothing panics holding the task state"))
}

fn write() -> std::sync::RwLockWriteGuard<'static, State> {
    STATE
        .write()
        .unwrap_or_else(|_| unreachable!("nothing panics holding the task state"))
}

/// drop a task's record when the task is collected
///
/// the callback a weak reference is made with. it holds the address the record
/// is filed under rather than the task, because by the time this runs there is
/// no task left to ask
#[pyclass]
struct Forget {
    at: usize,
}

#[pymethods]
impl Forget {
    /// what a dying weak reference calls
    fn __call__(&self, _reference: &Bound<'_, PyAny>) {
        // bound, so the entry is dropped after the guard. this already runs
        // from the collector, and dropping the `Py` it holds under the lock is
        // how one collection re-enters this and finds the lock held
        let gone = write().recorded.remove(&self.at);
        drop(gone);
    }
}

/// notice the one function every task is made in, as its module is registered
///
/// **`BaseEventLoop.create_task` rather than `asyncio.create_task`**, and that
/// was measured rather than guessed: every route to a task goes through it.
/// `asyncio.create_task`, `asyncio.ensure_future`, `loop.create_task` and a task
/// group's own `create_task` were each driven with `PY_RETURN` armed on all of
/// them, and the loop's method is the only one all four reach
///
/// one hook is not merely tidier. watching the outer functions too would record
/// a stack per route and leave the innermost frame being whichever asyncio
/// function was used, which is a fact about asyncio rather than about the
/// program — the leading frames are dropped instead, by [`record`]
///
/// **nothing here imports anything**, and that is not a preference. the first
/// version asked `sys.modules` and reached for the attribute, which meant
/// calling into the import machinery from inside a `PY_START` — measured, that
/// raises `KeyError: '__import__'` part way through `import asyncio` and takes
/// the program down with it. a debugger that breaks every asyncio program is a
/// worse thing than one that never had this feature
///
/// so the code object is taken from the one place it can be had for free: it is
/// nested in `asyncio/base_events.py`'s own module code object, which the agent
/// is handed when that module runs — before any task exists
///
/// answers whether it found one, which is what tells the caller the
/// instrumentation has to be refreshed
pub(crate) fn notice(code: &Bound<'_, PyAny>) -> PyResult<bool> {
    if read().hook.is_some() {
        return Ok(false);
    }
    // the module body, and the one file the loop's method is defined in. both
    // are checked before anything is walked, because every module in the
    // program would otherwise pay for the search
    if code.getattr("co_qualname")?.extract::<String>()? != "<module>" {
        return Ok(false);
    }
    let filename: String = code.getattr("co_filename")?.extract()?;
    if !filename.ends_with("asyncio/base_events.py") {
        return Ok(false);
    }

    let Some(found) = nested(code, "BaseEventLoop.create_task")? else {
        return Ok(false);
    };
    write().hook = Some(found.unbind());
    Ok(true)
}

/// the code object of that qualified name, anywhere under this one
///
/// a method is nested twice — the module holds the class body and the class body
/// holds the method — so this recurses rather than looking at one level
fn nested<'py>(code: &Bound<'py, PyAny>, wanted: &str) -> PyResult<Option<Bound<'py, PyAny>>> {
    for constant in code.getattr("co_consts")?.try_iter()? {
        let constant = constant?;
        let Ok(qualname) = constant.getattr("co_qualname") else {
            continue;
        };
        if qualname.extract::<String>()? == wanted {
            return Ok(Some(constant));
        }
        if let Some(deeper) = nested(&constant, wanted)? {
            return Ok(Some(deeper));
        }
    }
    Ok(None)
}

/// the code object tasks are made in, once there is one
pub(crate) fn hook(python: Python<'_>) -> Option<Py<PyAny>> {
    read().hook.as_ref().map(|hook| hook.clone_ref(python))
}

/// whether this code object is the one tasks are made in
pub(crate) fn is_hook(address: usize) -> bool {
    read()
        .hook
        .as_ref()
        .is_some_and(|hook| hook.as_ptr() as usize == address)
}

/// the events this module wants on one code object
pub(crate) fn local(address: usize) -> events::Local {
    events::Local {
        line: false,
        py_return: is_hook(address),
        py_start: false,
    }
}

/// how far up the creating stack is worth keeping
///
/// the frames above a `create_task` are the program's own, and beneath them is
/// the event loop — which is the same for every task and says nothing about who
/// scheduled this one. the bound is what stops a deep recursion turning one
/// record into a stack the client has to scroll past
const KEEP: usize = 32;

/// remember the stack this task was created on
///
/// called from the `PY_RETURN` of the loop's `create_task`, so the frames that
/// made it are above this one
///
/// **every leading asyncio frame is dropped**, not just the one this fired in.
/// a task reached through `ensure_future` has that function between the program
/// and the loop, and one through a task group has two more — a record that began
/// at whichever asyncio function was used would be saying the scheduler was
/// asyncio. what a reader wants is the first frame that is the program's own
pub(crate) fn record(python: Python<'_>, returned: &Bound<'_, PyAny>) -> PyResult<()> {
    let at = returned.as_ptr() as usize;

    let mut frames = Vec::new();
    let mut frame = python
        .import("sys")?
        .getattr("_getframe")?
        .call1((1_u32,))
        .ok();
    while let Some(one) = frame {
        if one.is_none() {
            break;
        }
        let code = one.getattr("f_code")?;
        let file: String = code.getattr("co_filename")?.extract()?;
        // asyncio's own frames, on the way out to the program's. dropped only
        // while they are *leading*: a program with a file of its own under an
        // `asyncio/` directory keeps every frame once the first one has been
        // taken, because by then the record has reached the program
        if frames.is_empty() && file.contains("/asyncio/") {
            frame = one.getattr("f_back").ok().filter(|back| !back.is_none());
            continue;
        }
        frames.push(Scheduling {
            file,
            line: one.getattr("f_lineno")?.extract()?,
            function: code.getattr("co_qualname")?.extract()?,
        });
        if frames.len() == KEEP {
            break;
        }
        frame = one.getattr("f_back").ok().filter(|back| !back.is_none());
    }

    // the weak reference is what keeps this from holding the task alive, and
    // `Forget` is what keeps the address from ever naming a different one
    let weakref = python.import("weakref")?.getattr("ref")?;
    let forget = Py::new(python, Forget { at })?;
    let reference = weakref.call1((returned, forget))?;

    // bound rather than written as one statement, so that whatever this
    // displaces is dropped **after** the guard. dropping a `Py` decrements a
    // refcount, which can run the collector, which can call [`Forget`] — and
    // this lock is not reentrant, so doing that under the guard is a deadlock
    // waiting for a program that reuses an address
    let displaced = write().recorded.insert(at, (reference.unbind(), frames));
    drop(displaced);
    Ok(())
}

/// the stack the task running on this thread was created on
///
/// `asyncio.current_task()` rather than anything about frames: it is a read, on
/// a path that already runs python because a stop evaluates conditions, and it
/// answers the one question a frame cannot
///
/// empty when the program is not in a task, is not running asyncio at all, or
/// made this task before bpd was watching
pub(crate) fn scheduled_by(python: Python<'_>) -> (bool, Vec<Scheduling>) {
    // **before anything is asked of asyncio.** finding the current task means
    // reaching for the module, and reaching for a module that is not there
    // *imports* it — bpd adding a module to `sys.modules` and running its body,
    // in a program that never asked for one. measured, before this guard: a
    // stack walk in a synchronous program left `asyncio` in `sys.modules`
    //
    // the hook is the evidence that asyncio is already there: it is a constant
    // nested in `asyncio/base_events.py`, so having one means that module has
    // run. without
    // it there is no task to be in, and nothing to ask
    if read().hook.is_none() {
        return (false, Vec::new());
    }
    let Some(task) = current(python) else {
        return (false, Vec::new());
    };
    let frames = read()
        .recorded
        .get(&(task.as_ptr() as usize))
        .map(|(_reference, frames)| frames.clone())
        .unwrap_or_default();
    // in a task either way. an empty list here is a task made by a route this
    // does not watch — `ensure_future`, `loop.create_task`, a task group — and
    // saying "in a task, and bpd did not see it made" is a different fact from
    // "not in a task"
    (true, frames)
}

/// the task running on this thread, if there is one
///
/// every failure here is the same answer — there is no task — and none of them
/// is exceptional: a program with no asyncio has no module, one outside a
/// coroutine has no running loop, and `current_task` returns `None` for a thread
/// the loop is not running on
fn current(python: Python<'_>) -> Option<Bound<'_, PyAny>> {
    let task = python
        .import("asyncio")
        .ok()?
        .getattr("current_task")
        .ok()?
        .call0()
        .ok()?;
    (!task.is_none()).then_some(task)
}
