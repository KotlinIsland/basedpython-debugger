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
mod breakpoints;
mod code;
mod conditions;
mod events;
mod files;
mod frames;
mod run;
mod session;
mod values;

use bpd_protocol::message::StopReason;
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
    use super::{
        BUILT_FOR, DEBUGGER_TOOL_ID, TOOL_NAME, arm, attach, frames, monitoring, run,
        running_version,
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

        // this is the one python frame that belongs to bpd — the `-c` bootstrap
        // the interpreter was entered through — and it is remembered here,
        // before the program has a frame of its own, so that no stack ever
        // reports it. `main` is a native function and the interpreter pushes no
        // frame to call one, so the frame running right now is the bootstrap's
        frames::remember_bootstrap(python)?;

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

/// turn on code object discovery, and with it the entry stop
///
/// `PY_START` is armed globally, because PEP 669 has no "code object created"
/// event and a program's code objects are not there to be scoped to yet.
/// registering each one on its first call and returning `DISABLE` costs one
/// native call per code object, once, and it is the only way to see the ones
/// `exec` builds while the program runs
fn arm(python: Python<'_>) -> PyResult<()> {
    let monitoring = monitoring(python)?;
    events::install(
        python,
        &monitoring,
        wrap_pyfunction!(on_py_start, python)?.as_any(),
        wrap_pyfunction!(on_line, python)?.as_any(),
    )?;
    events::watch_every_call(python, true)
}

/// the `PY_START` callback, called by the interpreter with no python frame in
/// between
///
/// it does not materialise a frame. registering a code object needs the object
/// and its filename, deciding whether this is the program's entry needs the
/// filename, and neither needs to know anything about the frame that is about
/// to run — which is the whole reason this is affordable at all
///
/// the offset is part of the signature PEP 669 requires and is always zero for
/// `PY_START`, so there is nothing it could be read for
#[pyfunction]
fn on_py_start<'py>(
    python: Python<'py>,
    code: &Bound<'py, PyAny>,
    _offset: i32,
) -> PyResult<Bound<'py, PyAny>> {
    let newly_loaded = code::register(code)?;

    if !attach::has_stopped_at_entry()
        && let Some(target) = attach::target()
    {
        let attribute = code.getattr("co_filename")?;
        let filename: std::borrow::Cow<'_, str> = attribute.extract()?;
        if std::path::Path::new(filename.as_ref()) == target {
            // the program's own code object is registered before the stop, so a
            // breakpoint set during the entry stop has the whole of the main
            // module — its functions, classes and comprehensions — to bind to.
            // that also means a rebinding pass here would have nothing to say,
            // because the set was resolved with this file already registered
            attach::mark_stopped_at_entry();
            session::stop(python, StopReason::Entry)?;
            return Ok(events::disable(python));
        }
    }

    if let Some(loaded) = newly_loaded {
        session::announce_rebinding(breakpoints::rebind(python, &loaded)?);
    }
    Ok(events::disable(python))
}

/// the `LINE` callback, armed only on the code objects that hold a breakpoint
///
/// the common answer is "this line is not a breakpoint", and answering it costs
/// a lookup on the code object's address and the line number. saying `DISABLE`
/// means the interpreter never offers that line again, so the cost of a
/// breakpoint is bounded by the number of *distinct lines executed once* in the
/// handful of code objects that hold one
#[pyfunction]
fn on_line<'py>(
    python: Python<'py>,
    code: &Bound<'py, PyAny>,
    line: u32,
) -> PyResult<Bound<'py, PyAny>> {
    // this line is being run by a condition or a log message of ours, not by
    // the program. it is not a hit, and it must not be disabled either — the
    // program will reach it for real later and has to be offered it then
    if conditions::evaluating() {
        return Ok(python.None().into_bound(python));
    }

    let Some(plans) = breakpoints::hit(code.as_ptr() as usize, line) else {
        return Ok(events::disable(python));
    };

    let file: String = code.getattr("co_filename")?.extract()?;
    let thread = events::thread_ident(python)?;
    let at = conditions::Location {
        file: &file,
        line,
        thread,
    };

    let mut stopping = Vec::new();
    let mut failure = None;
    {
        // held across every expression of every breakpoint on this line, so a
        // condition that calls a function with a breakpoint in it runs to an
        // answer rather than stopping inside itself
        let _suppressed = conditions::suppress();
        let mut place = conditions::Place::unfetched(python);

        for plan in &plans {
            match plan.fire(python, &mut place, &at)? {
                conditions::Fired::Nothing => {}
                conditions::Fired::Stop => stopping.push(plan.id),
                conditions::Fired::Logged(record) => session::log(record),
                // the remaining breakpoints on this line are left alone: the
                // program is about to be held here anyway, and a log record
                // produced during a hit the client is being told is broken
                // would be a record nobody can trust
                conditions::Fired::Failed(raised) => {
                    failure = Some((plan.id, raised));
                    break;
                }
            }
        }
    }

    if let Some((breakpoint, raised)) = failure {
        session::stop(
            python,
            StopReason::EvaluationFailed {
                breakpoint,
                part: raised.part,
                expression: raised.expression,
                file,
                line,
                thread,
                error: raised.error,
            },
        )?;
    } else if !stopping.is_empty() {
        session::stop(
            python,
            StopReason::Breakpoint {
                breakpoints: stopping,
                file,
                line,
                thread,
            },
        )?;
    }

    // deliberately not `DISABLE`: a breakpoint that fired once still exists,
    // and so does one whose condition was false this time
    Ok(python.None().into_bound(python))
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
