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

use pyo3::exceptions::PyImportError;
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
    use super::{BUILT_FOR, DEBUGGER_TOOL_ID, TOOL_NAME, monitoring, running_version};
    use pyo3::exceptions::{PyImportError, PyRuntimeError};
    use pyo3::prelude::*;

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
