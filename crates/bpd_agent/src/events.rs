//! the `sys.monitoring` handles the event path uses, resolved once
//!
//! every name here is looked up at arm time and never again. this is not a
//! micro-optimisation, it is a correctness rule that was learned the hard way:
//! an earlier agent resolved `sys.monitoring.DISABLE` inside the `PY_START`
//! callback, which re-entered the import system from within a monitoring
//! callback while the interpreter was importing, and corrupted line numbers a
//! long way from the cause — a `SyntaxError` printed with `line 0` and no
//! source text
//!
//! so the rule is: a callback may read a code object's attributes and touch
//! native state, and may call a python object that was resolved before any of
//! this started. it may not look one up

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyModule;

use crate::DEBUGGER_TOOL_ID;

/// everything the event path needs from python, bound once
///
/// the event masks are integers rather than the objects `sys.monitoring.events`
/// holds, because arming is a decision about the **whole** mask and the bits
/// have to be OR-ed together
#[derive(Debug)]
struct Handles {
    disable: Py<PyAny>,
    line: u32,
    instruction: u32,
    py_start: u32,
    py_return: u32,
    py_resume: u32,
    py_unwind: u32,
    py_throw: u32,
    raised: u32,
    set_events: Py<PyAny>,
    set_local_events: Py<PyAny>,
    restart_events: Py<PyAny>,
    register_callback: Py<PyAny>,
    free_tool_id: Py<PyAny>,
    get_ident: Py<PyAny>,
    get_frame: Py<PyAny>,
    get_frames: Py<PyAny>,
    compile: Py<PyAny>,
    eval: Py<PyAny>,
    int_repr: Py<PyAny>,
    float_repr: Py<PyAny>,
    repr: Py<PyAny>,
    list: Py<PyAny>,
}

static HANDLES: OnceLock<Handles> = OnceLock::new();

/// every code object this tool has non-zero local events on, by address
///
/// [`watch_locally`] is the only place `set_local_events` is called, so this
/// set is complete **by construction** rather than by every part of the agent
/// that arms a code object remembering to say so. an address is a sound key for
/// the reason it is elsewhere in the agent: the map holds a strong reference to
/// everything in it, which is what stops the allocator handing the same address
/// to a different code object
///
/// it exists for one reader — [`disarm`], which takes this tool's
/// instrumentation off a process that has stopped being a debuggee. cpython has
/// no way to clear a tool before `sys.monitoring.clear_tool_id` in 3.14, and
/// the minimum here is 3.13, so the set has to be kept
static ARMED_LOCALLY: Mutex<BTreeMap<usize, Py<PyAny>>> = Mutex::new(BTreeMap::new());

/// the same set, as a snapshot that can be read **without taking a lock**
///
/// [`disarm`] runs in a forked child, and a fork keeps only the calling thread:
/// a lock another thread held at the instant of the fork is one the child's copy
/// would wait on for ever. on a gil build that cannot happen to this particular
/// lock, because `os.fork()` holds the GIL and so does everything that takes it
/// — but on a free-threaded build there is no GIL to hold, another thread can be
/// arming a code object while a third forks, and a first-class target does not
/// get an argument that holds on one build
///
/// so the writer publishes an immutable `Vec` and the child reads the pointer.
/// the box is filled before its pointer is stored and the box it replaces is
/// released only after, so whichever one a fork copies is a complete one
///
/// what it holds is a **superset** of what is armed rather than exactly it —
/// see the ordering rule in [`watch_locally`] — because a fork can land between
/// any two instructions there and losing a code object that really is armed is
/// the one outcome that matters. clearing local events on a code object that
/// has none is a no-op
///
/// null until a code object is first armed, which is most sessions
static ARMED_SNAPSHOT: AtomicPtr<Vec<Py<PyAny>>> = AtomicPtr::new(std::ptr::null_mut());

/// the code objects with local events on, for the thread that changes them
fn armed_locally() -> MutexGuard<'static, BTreeMap<usize, Py<PyAny>>> {
    ARMED_LOCALLY
        .lock()
        .expect("the armed set is only held for map operations, which do not panic")
}

/// publish the set for a forked child to read
///
/// called under the lock, so the `Vec` handed over and the map it was built
/// from are the same set. the swap comes before the release of what it replaced
/// — the other order would leave a fork that landed between them copying a box
/// that had already been freed
fn publish(python: Python<'_>, armed: &BTreeMap<usize, Py<PyAny>>) {
    let snapshot: Vec<Py<PyAny>> = armed.values().map(|code| code.clone_ref(python)).collect();
    let replaced = ARMED_SNAPSHOT.swap(Box::into_raw(Box::new(snapshot)), Ordering::AcqRel);
    if replaced.is_null() {
        return;
    }
    // SAFETY: every non-null value this pointer ever holds came from
    // `Box::into_raw`, on the line above. the swap has already taken this one
    // out, and this runs under the lock every other `publish` needs, so no
    // other call can be holding it. [`take_snapshot`] is the only other place a
    // box is taken back and it runs in a **forked child**, which frees its own
    // copy of the heap and nothing of this process's
    #[expect(
        unsafe_code,
        reason = "an owned box behind an atomic pointer is what makes the set \
                  readable from a fork handler without a lock — see above"
    )]
    drop(unsafe { Box::from_raw(replaced) });
}

/// the set as a forked child sees it, taking ownership of the child's copy
///
/// the one read of this set that takes no lock, and the reason there is a
/// snapshot at all. it is reached only from [`disarm`], which is reached only
/// from the fork handler — so the process running this is always a child that
/// has just been made, with one thread and its own copy of the heap
fn take_snapshot() -> Vec<Py<PyAny>> {
    let published = ARMED_SNAPSHOT.swap(std::ptr::null_mut(), Ordering::AcqRel);
    if published.is_null() {
        return Vec::new();
    }
    // SAFETY: the invariant [`publish`] keeps — every non-null value came from
    // `Box::into_raw` — and the swap means nothing else in this process can
    // reach it. the process that published it is a different one, and freeing a
    // child's copy of the heap is nothing to it
    #[expect(unsafe_code, reason = "see `publish`")]
    let owned = unsafe { Box::from_raw(published) };
    *owned
}

/// the native functions the interpreter calls, one per event bpd listens for
///
/// every one of them is registered at arm time whether or not the event is
/// armed yet, because registering a callback is not arming an event and an
/// event armed without one raises rather than being ignored
#[derive(Debug)]
pub(crate) struct Callbacks<'py> {
    /// a python function begins
    pub(crate) py_start: &'py Bound<'py, PyAny>,
    /// a line is about to run
    pub(crate) line: &'py Bound<'py, PyAny>,
    /// a single instruction is about to run
    pub(crate) instruction: &'py Bound<'py, PyAny>,
    /// a python function returns
    pub(crate) py_return: &'py Bound<'py, PyAny>,
    /// a generator or coroutine is resumed
    pub(crate) py_resume: &'py Bound<'py, PyAny>,
    /// a frame is left by an exception
    pub(crate) py_unwind: &'py Bound<'py, PyAny>,
    /// a generator or coroutine is resumed by `throw()`
    pub(crate) py_throw: &'py Bound<'py, PyAny>,
    /// an exception is raised
    pub(crate) raised: &'py Bound<'py, PyAny>,
}

/// what is armed for the whole program
///
/// one struct rather than a call per event, because `set_events` **replaces**
/// the whole mask: arming one bit on its own disarms every other. everything
/// that changes any of these goes through one place that decides all of them
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one field per `sys.monitoring` event, which is the point: the \
              mask is replaced wholesale, so every event that can be armed \
              globally has to be decided in one place and none of them can be \
              left out"
)]
pub(crate) struct Global {
    /// how a code object is discovered at all, and how a step in catches the
    /// frame it enters
    ///
    /// PEP 669 has no "code object created" event, so a session with
    /// breakpoints pays one native call per code object first reached and a
    /// session with none pays nothing
    pub(crate) py_start: bool,
    /// how a running thread is caught, for stopping the world and for a pause
    pub(crate) line: bool,
    /// how a frame left by an exception is seen
    ///
    /// `PY_UNWIND` **cannot be a local event** — `set_local_events` refuses it
    /// — so a step that needs to know its frame was unwound arms it for the
    /// whole program. it is also how an exception leaving the outermost frame
    /// is found
    pub(crate) py_unwind: bool,
    /// how a generator resumed by `throw()` is seen, which a step in enters
    pub(crate) py_throw: bool,
    /// how a generator or coroutine resumption is seen, which a step in enters
    pub(crate) py_resume: bool,
    /// how a raise is seen, for the exception breakpoints
    ///
    /// `RAISE` cannot be a local event either
    pub(crate) raised: bool,
}

/// what is armed for one code object
///
/// local rather than global: a program with three breakpoints in it
/// instruments three code objects, and every other one in the process is
/// untouched. the same replacement rule applies per code object, so the
/// breakpoints and the steps that want events on one are decided together
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the same reason [`Global`] carries it: one field per \
              `sys.monitoring` event a code object can have, and \
              `set_local_events` replaces the whole mask, so every one of them \
              is decided together or the ones left out are turned off"
)]
pub(crate) struct Local {
    /// every line of it is reported
    pub(crate) line: bool,
    /// every instruction of it is reported
    ///
    /// the most expensive thing this tool can ask for, and it is asked for in
    /// the smallest window there is: a frame has just returned into this code
    /// object and the step that followed it wants the **next instruction** of
    /// the caller, because a return lands mid-line and there is no line event
    /// left in it. armed at the return and taken off at the first one
    pub(crate) instruction: bool,
    /// its return is reported
    ///
    /// a `yield` is deliberately not: it suspends a frame rather than finishing
    /// it, and a step follows the frame it is in across a suspension
    pub(crate) py_return: bool,
    /// every entry into it is reported
    ///
    /// `PY_START` is armed globally for code object discovery, and **locally**
    /// for the two code objects the django template hooks are on — where the
    /// question is not "has this code object been seen" but "which node is
    /// about to render", which is a different question about every call
    pub(crate) py_start: bool,
}

impl std::ops::BitOr for Local {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self {
            line: self.line || other.line,
            instruction: self.instruction || other.instruction,
            py_return: self.py_return || other.py_return,
            py_start: self.py_start || other.py_start,
        }
    }
}

/// resolve every handle and register the callbacks, before the program runs
///
/// `_thread.get_ident` rather than `threading.get_ident`: `_thread` is builtin
/// and always present, where `threading` is an ordinary module that a stop
/// would otherwise have to import — from inside a callback, which is the thing
/// this module exists to avoid. `sys` and `builtins` are already imported by
/// the time any interpreter exists, so neither adds a module the debuggee would
/// not otherwise have
pub(crate) fn install(
    python: Python<'_>,
    monitoring: &Bound<'_, PyAny>,
    callbacks: &Callbacks<'_>,
) -> PyResult<()> {
    let all = monitoring.getattr("events")?;
    let bit = |name: &str| -> PyResult<u32> { all.getattr(name)?.extract() };

    for (name, callback) in [
        ("PY_START", callbacks.py_start),
        ("LINE", callbacks.line),
        ("INSTRUCTION", callbacks.instruction),
        ("PY_RETURN", callbacks.py_return),
        ("PY_RESUME", callbacks.py_resume),
        ("PY_UNWIND", callbacks.py_unwind),
        ("PY_THROW", callbacks.py_throw),
        ("RAISE", callbacks.raised),
    ] {
        monitoring.call_method1(
            "register_callback",
            (DEBUGGER_TOOL_ID, bit(name)?, callback),
        )?;
    }

    let builtins = PyModule::import(python, "builtins")?;
    let handles = Handles {
        disable: monitoring.getattr("DISABLE")?.unbind(),
        line: bit("LINE")?,
        instruction: bit("INSTRUCTION")?,
        py_start: bit("PY_START")?,
        py_return: bit("PY_RETURN")?,
        py_resume: bit("PY_RESUME")?,
        py_unwind: bit("PY_UNWIND")?,
        py_throw: bit("PY_THROW")?,
        raised: bit("RAISE")?,
        set_events: monitoring.getattr("set_events")?.unbind(),
        set_local_events: monitoring.getattr("set_local_events")?.unbind(),
        restart_events: monitoring.getattr("restart_events")?.unbind(),
        // resolved here for the reason everything else in this struct is, and
        // for one more: [`disarm`] runs inside a fork handler, where looking a
        // name up would mean an attribute lookup on a module in a process whose
        // interpreter has just been reassembled
        register_callback: monitoring.getattr("register_callback")?.unbind(),
        free_tool_id: monitoring.getattr("free_tool_id")?.unbind(),
        get_ident: PyModule::import(python, "_thread")?
            .getattr("get_ident")?
            .unbind(),
        get_frame: PyModule::import(python, "sys")?
            .getattr("_getframe")?
            .unbind(),
        get_frames: PyModule::import(python, "sys")?
            .getattr("_current_frames")?
            .unbind(),
        compile: builtins.getattr("compile")?.unbind(),
        eval: builtins.getattr("eval")?.unbind(),
        // the unbound slots, not `str()` and not `repr()`: a subclass is free
        // to override either, and then the number bpd reported would not be
        // the number the object holds
        int_repr: builtins.getattr("int")?.getattr("__repr__")?.unbind(),
        float_repr: builtins.getattr("float")?.getattr("__repr__")?.unbind(),
        repr: builtins.getattr("repr")?.unbind(),
        list: builtins.getattr("list")?.unbind(),
    };

    HANDLES
        .set(handles)
        .map_err(|_| PyRuntimeError::new_err("the agent was armed twice"))
}

fn handles() -> &'static Handles {
    HANDLES
        .get()
        .expect("nothing reaches the event path before `install` has run: the callbacks are registered by `install` itself")
}

/// `sys.monitoring.DISABLE`, the answer to "never report this location again"
pub(crate) fn disable(python: Python<'_>) -> Bound<'_, PyAny> {
    handles().disable.bind(python).clone()
}

/// set the events armed for the whole program, in one call
///
/// they are set together because `set_events` replaces the whole mask: turning
/// one on by itself would turn the others off, and a caller that knew about one
/// of them would silently disarm the rest
pub(crate) fn watch_globally(python: Python<'_>, wanted: Global) -> PyResult<()> {
    let handles = handles();
    let mut events: u32 = 0;
    for (armed, bit) in [
        (wanted.py_start, handles.py_start),
        (wanted.line, handles.line),
        (wanted.py_unwind, handles.py_unwind),
        (wanted.py_throw, handles.py_throw),
        (wanted.py_resume, handles.py_resume),
        (wanted.raised, handles.raised),
    ] {
        if armed {
            events |= bit;
        }
    }
    handles
        .set_events
        .bind(python)
        .call1((DEBUGGER_TOOL_ID, events))?;
    Ok(())
}

/// set the events armed for one code object, in one call
///
/// the same replacement rule `set_events` has, per code object: a step that
/// wanted returns on a code object a breakpoint already watches lines in has to
/// ask for both, or arming its own would turn the breakpoint's off
///
/// the ordering around [`ARMED_SNAPSHOT`] is the other rule, and it is not
/// cosmetic. the snapshot has to stay a **superset** of what the interpreter
/// really has armed, because a fork can land between any two instructions here
/// on a free-threaded build: so a code object joining the set is published
/// **before** the interpreter is told, and one leaving it **after**. a child
/// that lands in either window finds a code object listed that has nothing on
/// it, and clearing that one is a no-op — the other order would lose one that
/// really was armed
pub(crate) fn watch_locally(
    python: Python<'_>,
    code: &Bound<'_, PyAny>,
    wanted: Local,
) -> PyResult<()> {
    let handles = handles();
    let mut events: u32 = 0;
    for (armed, bit) in [
        (wanted.line, handles.line),
        (wanted.instruction, handles.instruction),
        (wanted.py_return, handles.py_return),
        (wanted.py_start, handles.py_start),
    ] {
        if armed {
            events |= bit;
        }
    }

    let address = code.as_ptr() as usize;
    if events != 0 {
        let mut armed = armed_locally();
        // an entry already there is the same code object with a different mask.
        // what a forked child does with it does not depend on the mask, so
        // there is nothing to republish
        if let Entry::Vacant(empty) = armed.entry(address) {
            empty.insert(code.clone().unbind());
            publish(python, &armed);
        }
    }

    handles
        .set_local_events
        .bind(python)
        .call1((DEBUGGER_TOOL_ID, code, events))?;

    if events == 0 {
        let mut armed = armed_locally();
        if armed.remove(&address).is_some() {
            publish(python, &armed);
        }
    }
    Ok(())
}

/// take this tool off the process entirely
///
/// what a process that has stopped being a debuggee does with the
/// instrumentation it inherited. the four things `sys.monitoring` holds are
/// independent and all four have to go, in this order:
///
/// 1. the **local** events on every code object the session armed. these are
///    the ones that outlive everything else: `free_tool_id` explicitly does not
///    clear them, so an id given back with local events still set on it is an
///    id the next tool to claim it would receive this session's breakpoints on
/// 2. the **global** events, which `set_events` replaces wholesale
/// 3. the **callbacks**. a registered callback with no event armed is inert,
///    and removing them is what makes an event that somehow survived inert too
///    — measured: cpython delivers nothing for a tool with no callback
/// 4. the **tool id**, so `sys.monitoring.get_tool(0)` stops naming `bpd` in a
///    process bpd is not debugging
///
/// it is not `sys.monitoring.clear_tool_id`, which does all four in one call
/// and arrived in 3.14. the minimum here is 3.13, and a second mechanism for
/// the newer release would be two things to keep true instead of one
///
/// **this runs in a forked child and takes no lock.** the code objects come
/// from [`ARMED_SNAPSHOT`], which exists for exactly that reason, and the map
/// behind it is deliberately left alone: it is a copy of the parent's and this
/// process is never going to arm anything again
pub(crate) fn disarm(python: Python<'_>) -> PyResult<()> {
    let handles = handles();

    for code in &take_snapshot() {
        handles.set_local_events.bind(python).call1((
            DEBUGGER_TOOL_ID,
            code.bind(python),
            0_u32,
        ))?;
    }

    watch_globally(python, Global::default())?;

    for bit in [
        handles.py_start,
        handles.line,
        handles.instruction,
        handles.py_return,
        handles.py_resume,
        handles.py_unwind,
        handles.py_throw,
        handles.raised,
    ] {
        handles
            .register_callback
            .bind(python)
            .call1((DEBUGGER_TOOL_ID, bit, python.None()))?;
    }

    handles
        .free_tool_id
        .bind(python)
        .call1((DEBUGGER_TOOL_ID,))?;
    Ok(())
}

/// re-enable every location that returned `DISABLE`, process wide
///
/// this is a blunt instrument and it is the right one in the two places it is
/// used. a line that was reported once and disabled has to start firing again
/// the moment a breakpoint lands on it, or the moment a step needs to be
/// offered it, and PEP 669 has no per-location undo
///
/// there **is** a per-code-object one, and it is deliberately not used: taking
/// a code object's local events to zero and setting them again re-enables every
/// location in it, measured by
/// `clearing_a_code_objects_local_events_undoes_its_disables`. it would be much
/// cheaper than this, and on a free-threaded build another thread can execute
/// that code object between the two calls and miss a breakpoint. a missed
/// breakpoint is not a price this project pays for a faster step
pub(crate) fn restart(python: Python<'_>) -> PyResult<()> {
    handles().restart_events.bind(python).call0()?;
    Ok(())
}

/// the interpreter's identity for the calling thread
pub(crate) fn thread_ident(python: Python<'_>) -> PyResult<u64> {
    handles().get_ident.bind(python).call0()?.extract()
}

/// the innermost frame of every thread that has one
///
/// the only way to see a thread bpd is not holding. what it reports about one
/// is a sample by construction: the thread is running, and it has moved on by
/// the time the dictionary is built
pub(crate) fn current_frames(python: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    handles().get_frames.bind(python).call0()
}

/// the python frame that is running right now
///
/// the callbacks are native, so the interpreter pushes no frame to call them
/// and `sys._getframe()` is the frame that reached the event. this is the one
/// thing on the event path that materialises a frame, and it is only reached
/// after a line has already matched a bound breakpoint — deciding *whether* it
/// matched needs the code object's address and a line number and nothing else
pub(crate) fn current_frame(python: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    handles().get_frame.bind(python).call0()
}

/// compile an expression, once, when the breakpoint is set
///
/// `filename` is what a traceback out of this expression will name, so it says
/// which breakpoint the expression belongs to rather than `<string>`
pub(crate) fn compile_expression(
    python: Python<'_>,
    source: &str,
    filename: &str,
) -> PyResult<Py<PyAny>> {
    Ok(handles()
        .compile
        .bind(python)
        .call1((source, filename, "eval"))?
        .unbind())
}

/// the exact digits of an integer, whatever its type says about itself
pub(crate) fn int_repr(python: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<String> {
    handles().int_repr.bind(python).call1((value,))?.extract()
}

/// a float as python writes it, so `inf`, `nan` and `-0.0` survive
pub(crate) fn float_repr(python: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<String> {
    handles().float_repr.bind(python).call1((value,))?.extract()
}

/// `repr(value)`, which is user code and is only reached when a request asked
pub(crate) fn repr(python: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<String> {
    handles().repr.bind(python).call1((value,))?.extract()
}

/// `list(value)`, the only snapshot of a set cpython offers
///
/// there is no concrete accessor for set storage — no `PySet_GetItem` — so a
/// set is read by iterating it. that is why only an exact `set` or `frozenset`
/// is read this way: for those, iteration is the interpreter's own code
pub(crate) fn to_list<'py>(
    python: Python<'py>,
    value: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    handles().list.bind(python).call1((value,))
}

/// evaluate a compiled expression against a frame's own namespaces
///
/// `locals` is the frame's `f_locals`, which on 3.13 and later is PEP 667's
/// write-through proxy rather than a snapshot — so the expression sees the
/// values the frame holds now, including cell and free variables
pub(crate) fn evaluate<'py>(
    python: Python<'py>,
    code: &Py<PyAny>,
    globals: &Bound<'py, PyAny>,
    locals: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    handles()
        .eval
        .bind(python)
        .call1((code.bind(python), globals, locals))
}
