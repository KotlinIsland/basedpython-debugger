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
}

static HANDLES: OnceLock<Handles> = OnceLock::new();

/// resolve every handle and register the callbacks, before the program runs
///
/// `_thread.get_ident` rather than `threading.get_ident`: `_thread` is builtin
/// and always present, where `threading` is an ordinary module that a stop
/// would otherwise have to import — from inside a callback, which is the thing
/// this module exists to avoid
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

/// turn `PY_START` on or off for the whole program
///
/// it is on exactly while there is a breakpoint set. it is how a code object is
/// discovered at all — PEP 669 has no "code object created" event — so a
/// session that has breakpoints pays one native call per code object first
/// reached, and a session that has none pays nothing
pub(crate) fn watch_every_call(python: Python<'_>, watching: bool) -> PyResult<()> {
    let handles = handles();
    let events = if watching {
        handles.py_start.bind(python).clone()
    } else {
        0i32.into_pyobject(python)?.into_any()
    };
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
