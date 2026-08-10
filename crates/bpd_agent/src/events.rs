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

use std::sync::OnceLock;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyModule;

use crate::DEBUGGER_TOOL_ID;

/// everything the event path needs from python, bound once
#[derive(Debug)]
struct Handles {
    disable: Py<PyAny>,
    line: Py<PyAny>,
    py_start: Py<PyAny>,
    set_events: Py<PyAny>,
    set_local_events: Py<PyAny>,
    restart_events: Py<PyAny>,
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
    on_py_start: &Bound<'_, PyAny>,
    on_line: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let all = monitoring.getattr("events")?;
    let line = all.getattr("LINE")?;
    let py_start = all.getattr("PY_START")?;

    monitoring.call_method1(
        "register_callback",
        (DEBUGGER_TOOL_ID, &py_start, on_py_start),
    )?;
    monitoring.call_method1("register_callback", (DEBUGGER_TOOL_ID, &line, on_line))?;

    let builtins = PyModule::import(python, "builtins")?;
    let handles = Handles {
        disable: monitoring.getattr("DISABLE")?.unbind(),
        line: line.unbind(),
        py_start: py_start.unbind(),
        set_events: monitoring.getattr("set_events")?.unbind(),
        set_local_events: monitoring.getattr("set_local_events")?.unbind(),
        restart_events: monitoring.getattr("restart_events")?.unbind(),
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
/// there are exactly two, and they are set together because `set_events`
/// replaces the whole mask: turning one on by itself would turn the other off.
/// a caller that knew about only one of them would silently disarm the other
///
/// - `py_start` is on exactly while there is a breakpoint set. it is how a code
///   object is discovered at all — PEP 669 has no "code object created" event —
///   so a session with breakpoints pays one native call per code object first
///   reached, and a session with none pays nothing
/// - `line` is on only while the world is stopped. it is how a running thread is
///   caught at all, and it is the whole cost of that mode
pub(crate) fn watch_globally(python: Python<'_>, py_start: bool, line: bool) -> PyResult<()> {
    let handles = handles();
    let mut events: u32 = 0;
    if py_start {
        events |= handles.py_start.bind(python).extract::<u32>()?;
    }
    if line {
        events |= handles.line.bind(python).extract::<u32>()?;
    }
    handles
        .set_events
        .bind(python)
        .call1((DEBUGGER_TOOL_ID, events))?;
    Ok(())
}

/// turn `LINE` on or off for one code object
///
/// local rather than global: a program with three breakpoints in it instruments
/// three code objects, and every other one in the process is untouched
pub(crate) fn watch_lines(
    python: Python<'_>,
    code: &Bound<'_, PyAny>,
    watching: bool,
) -> PyResult<()> {
    let handles = handles();
    let events = if watching {
        handles.line.bind(python).clone()
    } else {
        0i32.into_pyobject(python)?.into_any()
    };
    handles
        .set_local_events
        .bind(python)
        .call1((DEBUGGER_TOOL_ID, code, events))?;
    Ok(())
}

/// re-enable every location that returned `DISABLE`, process wide
///
/// this is a blunt instrument and it is the right one here: a line that was
/// reported once and disabled has to start firing again the moment a breakpoint
/// lands on it, and there is no per-location undo. it is the wrong instrument
/// for anything per-frame — see the stepping section of the architecture doc
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
