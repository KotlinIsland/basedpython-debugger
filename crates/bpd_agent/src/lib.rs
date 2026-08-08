//! the native agent loaded into the debuggee
//!
//! everything on an event path in this crate is rust. the interpreter calls
//! straight into it, with no python frame per event — that is the whole reason
//! `bpd` can afford to be correct about what it reports
//!
//! setup is a different matter. claiming a `sys.monitoring` tool id happens
//! once per session, so it goes through the ordinary python api rather than
//! through the C interface. calling python from rust is only banned where it
//! costs something per event

mod attach;
mod run;

use pyo3::exceptions::{PyImportError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::PyModule;

/// the `major.minor` this artifact was compiled against
///
/// set by `build.rs` from the interpreter `PYO3_PYTHON` selected
const BUILT_FOR: &str = env!("BPD_AGENT_PYTHON");

/// the tool id `sys.monitoring` reserves for debuggers
///
/// `bpd` claims this one or none. quietly taking a different id would mean a
/// client that asked for a debugger got something with another tool's event
/// semantics
const DEBUGGER_TOOL_ID: u8 = 0;

/// the name registered against the tool id, and what another tool sees when it
/// asks who holds it
const TOOL_NAME: &str = "bpd";

#[pymodule]
mod bpd_agent {
    use super::{
        BUILT_FOR, DEBUGGER_TOOL_ID, TOOL_NAME, arm, attach, monitoring, run, running_version,
    };
    use pyo3::exceptions::{PyImportError, PyRuntimeError, PySystemExit};
    use pyo3::prelude::*;

    /// the whole of the debuggee's entry point
    ///
    /// the interpreter is entered with `python -c "import bpd_agent;
    /// bpd_agent.main()"`, so this runs before anything of the user's program.
    /// it attaches, arms the entry stop, and then enters the program the way
    /// the interpreter would have
    #[pyfunction]
    fn main(python: Python<'_>) -> PyResult<()> {
        verify_interpreter(python)?;

        let endpoint = required_env(bpd_protocol::env::ENDPOINT)?;
        let token = required_env(bpd_protocol::env::TOKEN)?;
        let target = required_env(bpd_protocol::env::TARGET)?;

        // taken out of the environment before any user code can see them. a
        // program that behaves differently because it noticed the debugger is a
        // program the debugger changed
        for name in bpd_protocol::env::ALL {
            forget_env(python, name)?;
        }

        // absolutised once, here, so the path the entry stop matches on is the
        // same string the compiled code object carries. the spelling the user
        // typed is kept too, because `sys.argv[0]` must not be absolutised
        let as_given = target;
        let target = std::path::absolute(&as_given).map_err(|error| {
            PySystemExit::new_err(format!("bpd: could not resolve `{as_given}`: {error}"))
        })?;

        attach::attach(&endpoint, &token, target.clone())
            .map_err(|error| PySystemExit::new_err(format!("bpd: could not attach: {error}")))?;
        claim(python)?;
        arm(python)?;

        match run::script(python, &as_given, &target) {
            Ok(()) => Ok(()),
            Err(error) => Err(run::report_uncaught(python, error)),
        }
    }

    /// read a variable the launcher is contracted to have set
    fn required_env(name: &str) -> PyResult<String> {
        std::env::var(name).map_err(|_| {
            PySystemExit::new_err(format!(
                "bpd: `{name}` is not set. `bpd_agent.main()` is the entry point                  the launcher uses and is not meant to be called by hand"
            ))
        })
    }

    /// remove a variable from the process and from `os.environ`
    fn forget_env(python: Python<'_>, name: &str) -> PyResult<()> {
        PyModule::import(python, "os")?
            .getattr("environ")?
            .call_method1("pop", (name, python.None()))?;
        Ok(())
    }

    /// the `major.minor` this artifact was compiled against
    #[pyfunction]
    fn built_for() -> &'static str {
        BUILT_FOR
    }

    /// the tool id this agent claims
    #[pyfunction]
    const fn debugger_tool_id() -> u8 {
        DEBUGGER_TOOL_ID
    }

    /// fail unless this interpreter is the one the agent was compiled against
    ///
    /// a mismatched minor version sometimes fails to import outright and
    /// sometimes loads and then reads the wrong offsets, which is far worse. the
    /// check is cheap and it happens before anything is instrumented
    #[pyfunction]
    fn verify_interpreter(python: Python<'_>) -> PyResult<()> {
        let running = running_version(python)?;
        if running == BUILT_FOR {
            return Ok(());
        }
        Err(PyImportError::new_err(format!(
            "this bpd agent was built for python {BUILT_FOR} and is being \
             imported by python {running}. the agent is not abi3 — it reads \
             interpreter state whose layout changes between releases — so the \
             build has to match the interpreter exactly"
        )))
    }

    /// claim the debugger tool id, or report who already holds it
    #[pyfunction]
    fn claim(python: Python<'_>) -> PyResult<()> {
        if let Some(holder) = holder(python)? {
            return Err(PyRuntimeError::new_err(format!(
                "`sys.monitoring` tool id {DEBUGGER_TOOL_ID} is already held by \
                 `{holder}`. bpd claims that id or none: tools are not \
                 interchangeable, and taking a different one would mean \
                 debugging with another tool's event semantics. stop `{holder}` \
                 and try again"
            )));
        }

        monitoring(python)?.call_method1("use_tool_id", (DEBUGGER_TOOL_ID, TOOL_NAME))?;
        Ok(())
    }

    /// give the tool id back
    #[pyfunction]
    fn release(python: Python<'_>) -> PyResult<()> {
        monitoring(python)?.call_method1("free_tool_id", (DEBUGGER_TOOL_ID,))?;
        Ok(())
    }

    /// the name holding the debugger tool id, if anything does
    #[pyfunction]
    fn holder(python: Python<'_>) -> PyResult<Option<String>> {
        monitoring(python)?
            .call_method1("get_tool", (DEBUGGER_TOOL_ID,))?
            .extract()
    }
}

/// turn on the entry stop
///
/// `PY_START` is the only event armed, and it is armed globally because the
/// program's own code object does not exist yet — there is nothing to scope it
/// to. every callback that is not the program's entry returns `DISABLE`, so the
/// cost is one native call per code object first reached during startup, and
/// the events are turned off entirely once the entry stop has happened
fn arm(python: Python<'_>) -> PyResult<()> {
    let monitoring = monitoring(python)?;
    let py_start = monitoring.getattr("events")?.getattr("PY_START")?;
    let callback = wrap_pyfunction!(on_py_start, python)?;

    // resolved once, here, and never again. the callback runs on entry to every
    // code object the program reaches, and doing `import sys` plus two attribute
    // lookups per event would be python work on an event path — which this
    // architecture does not do, and which re-enters the import system from
    // inside a monitoring callback while the interpreter is importing
    DISABLE
        .set(monitoring.getattr("DISABLE")?.unbind())
        .map_err(|_| PyRuntimeError::new_err("the agent was armed twice"))?;

    monitoring.call_method1("register_callback", (DEBUGGER_TOOL_ID, &py_start, callback))?;
    monitoring.call_method1("set_events", (DEBUGGER_TOOL_ID, py_start))?;
    Ok(())
}

/// turn every event back off
///
/// the entry stop happens once. leaving `PY_START` armed would keep paying a
/// native call for every code object the program ever reaches, for nothing
fn disarm(python: Python<'_>) -> PyResult<()> {
    monitoring(python)?.call_method1("set_events", (DEBUGGER_TOOL_ID, 0))?;
    Ok(())
}

/// the `PY_START` callback, called by the interpreter with no python frame in
/// between
///
/// it does not materialise a frame. deciding whether this is the entry needs
/// the code object's filename and nothing else, and the common answer is no
#[pyfunction]
fn on_py_start<'py>(
    python: Python<'py>,
    code: &Bound<'py, PyAny>,
    _offset: i32,
) -> PyResult<Bound<'py, PyAny>> {
    let disable = disable(python);

    if attach::has_stopped_at_entry() {
        return Ok(disable);
    }
    let Some(target) = attach::target() else {
        return Ok(disable);
    };

    let attribute = code.getattr("co_filename")?;
    let filename: std::borrow::Cow<'_, str> = attribute.extract()?;
    if std::path::Path::new(filename.as_ref()) != target {
        return Ok(disable);
    }

    attach::stop_at_entry();
    disarm(python)?;
    Ok(disable)
}

/// `sys.monitoring.DISABLE`, resolved at arm time
///
/// a `OnceLock` rather than a lookup, because this is read on every event
static DISABLE: std::sync::OnceLock<Py<PyAny>> = std::sync::OnceLock::new();

fn disable(python: Python<'_>) -> Bound<'_, PyAny> {
    DISABLE
        .get()
        .expect("the callback cannot run before arm installed DISABLE")
        .bind(python)
        .clone()
}

/// `sys.monitoring`, or an error naming what is missing
fn monitoring(python: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    let sys = PyModule::import(python, "sys")?;
    sys.getattr("monitoring").map_err(|_| {
        PyImportError::new_err(
            "this interpreter has no `sys.monitoring`. PEP 669 is the entire \
             event backbone of bpd and there is no `sys.settrace` fallback",
        )
    })
}

/// the running interpreter's `major.minor`
fn running_version(python: Python<'_>) -> PyResult<String> {
    let info = PyModule::import(python, "sys")?.getattr("version_info")?;
    let major: u8 = info.getattr("major")?.extract()?;
    let minor: u8 = info.getattr("minor")?.extract()?;
    Ok(format!("{major}.{minor}"))
}
