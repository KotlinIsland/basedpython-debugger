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
mod cells;
mod code;
mod conditions;
mod events;
mod exceptions;
mod files;
// `fork` is posix, and so is `os.register_at_fork`. there is nothing on windows
// for this to be the answer to
#[cfg(unix)]
mod forks;
mod frames;
mod pause;
mod replace;
mod run;
mod session;
mod source;
mod spawns;
mod steps;
mod stops;
mod templates;
mod threads;
mod values;
mod world;

use bpd_core::StopReason;
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
    #[cfg(unix)]
    use super::forks;
    use super::{
        BUILT_FOR, DEBUGGER_TOOL_ID, TOOL_NAME, arm, attach, frames, monitoring, run,
        running_version, session, spawns,
    };
    use bpd_protocol::env::Form;
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
        let spelled = required_env(bpd_protocol::env::FORM)?;
        let inherited_path = std::env::var(bpd_protocol::env::PYTHON_PATH).ok();

        // taken out of the environment before any user code can see them. a
        // program that behaves differently because it noticed the debugger is a
        // program the debugger changed
        for name in bpd_protocol::env::ALL {
            forget_env(python, name)?;
        }
        forget_agent_path(python, inherited_path.as_deref())?;

        let Some(form) = Form::parse(&spelled) else {
            return Err(PySystemExit::new_err(format!(
                "bpd: `{}` is `{spelled}`, which is not a launch form. the \
                 engine and the agent ship together and disagree about this \
                 one, which means the staged agent is not the one this bpd \
                 built",
                bpd_protocol::env::FORM
            )));
        };

        attach::attach(&endpoint, &token)
            .map_err(|error| PySystemExit::new_err(format!("bpd: could not attach: {error}")))?;
        claim(python)?;
        arm(python)?;

        // after the connection, because the hook reports through it, and before
        // the program runs, because a child started by the program's first
        // statement is one bpd has to have seen
        spawns::install(python)?;

        // the other half of that: a forked child inherits this agent armed, and
        // the connection's descriptors, and none of the thread that reads them
        #[cfg(unix)]
        forks::install(python)?;

        // this is the one python frame that belongs to bpd — the `-c` bootstrap
        // the interpreter was entered through — and it is remembered here,
        // before the program has a frame of its own, so that no stack ever
        // reports it. `main` is a native function and the interpreter pushes no
        // frame to call one, so the frame running right now is the bootstrap's
        frames::remember_bootstrap(python)?;

        let outcome = match run::enter(python, form, &target) {
            Ok(()) => Ok(()),
            Err(error) => Err(run::report_uncaught(python, error)),
        };

        // the program is over on every path out of here, including the ones
        // that exit. anything still held has to be named before the interpreter
        // starts finalizing, because a held thread cannot be joined and the
        // process would stop there with nothing having said why
        session::finishing();
        outcome
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

    /// take the agent's own directory back off the debuggee's import path
    ///
    /// the agent is reached by putting its staged directory in front of
    /// `PYTHONPATH`, and **both halves of that are visible to the program**:
    /// the variable is in `os.environ`, and the directory is on `sys.path` —
    /// where under `PYTHONSAFEPATH` it is `sys.path[0]`, the first place every
    /// import looks. neither is there without the debugger, and a directory
    /// searched before everything else is a debugger deciding what the program
    /// imports
    ///
    /// the agent has already been imported, so taking it back off costs
    /// nothing. where it was is read off the module's own `__file__` rather
    /// than taken from the launcher, so what is removed is the directory the
    /// agent really came from
    fn forget_agent_path(python: Python<'_>, inherited: Option<&str>) -> PyResult<()> {
        let file: String = PyModule::import(python, "bpd_agent")?
            .getattr("__file__")?
            .extract()?;
        let staged = std::path::Path::new(&file)
            .parent()
            .unwrap_or_else(|| unreachable!("`{file}` is a file, so it has a directory"))
            .display()
            .to_string();

        let sys = PyModule::import(python, "sys")?;
        let path = sys.getattr("path")?;
        let before = path.len()?;
        path.call_method1("remove", (&staged,))?;
        assert_eq!(
            path.len()? + 1,
            before,
            "removing the staged agent's directory took one entry off `sys.path`"
        );

        let environ = PyModule::import(python, "os")?.getattr("environ")?;
        match inherited {
            Some(original) => environ.set_item("PYTHONPATH", original)?,
            None => {
                environ.call_method1("pop", ("PYTHONPATH", python.None()))?;
            }
        }
        Ok(())
    }

    /// the `major.minor` this artifact was compiled against
    #[pyfunction]
    fn built_for() -> &'static str {
        BUILT_FOR
    }

    /// the `major.minor` of the interpreter that imported this artifact, with a
    /// `t` for a free-threaded build
    ///
    /// the other half of what [`verify_interpreter`] compares, and the half that
    /// is **computed rather than stamped in**. it is exposed because a computed
    /// value needs a test that reaches the same fact by another route, and the
    /// other route is `sysconfig` in a separate process — the expensive answer
    /// the agent deliberately does not ask for inside a debuggee
    #[pyfunction]
    fn running_on(python: Python<'_>) -> PyResult<String> {
        running_version(python)
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
        &events::Callbacks {
            py_start: wrap_pyfunction!(on_py_start, python)?.as_any(),
            line: wrap_pyfunction!(on_line, python)?.as_any(),
            py_return: wrap_pyfunction!(on_py_return, python)?.as_any(),
            py_resume: wrap_pyfunction!(on_py_resume, python)?.as_any(),
            py_unwind: wrap_pyfunction!(on_py_unwind, python)?.as_any(),
            py_throw: wrap_pyfunction!(on_py_throw, python)?.as_any(),
            raised: wrap_pyfunction!(on_raise, python)?.as_any(),
        },
    )?;
    events::watch_globally(
        python,
        events::Global {
            py_start: true,
            ..events::Global::default()
        },
    )
}

/// the `PY_START` callback, called by the interpreter with no python frame in
/// between
///
/// it does not materialise a frame. registering a code object needs the object
/// and its filename, deciding whether this is the program's entry needs the
/// code object itself, and neither needs to know anything about the frame that
/// is about to run — which is the whole reason this is affordable at all
///
/// the offset is part of the signature PEP 669 requires and is always zero for
/// `PY_START`, so there is nothing it could be read for
#[pyfunction]
fn on_py_start<'py>(
    python: Python<'py>,
    code: &Bound<'py, PyAny>,
    _offset: i32,
) -> PyResult<Bound<'py, PyAny>> {
    // the one code object this event is armed *locally* on, and the one place
    // in the agent where deciding needs the frame — the question is which
    // django node is about to render, and the node is only reachable there.
    // it is answered first and returns without disabling, because disabling is
    // per location and would take the hook off for the rest of the process
    if templates::is_render_hook(code.as_ptr() as usize) {
        rendering_a_template_node(python)?;
        return Ok(python.None().into_bound(python));
    }

    let newly_loaded = code::register(code)?;

    if !run::has_stopped_at_entry() && run::is_the_program(python, code)? {
        // the program's own code object is registered before the stop, so a
        // breakpoint set during the entry stop has the whole of the main
        // module — its functions, classes and comprehensions — to bind to.
        // that also means a rebinding pass here would have nothing to say,
        // because the set was resolved with this file already registered
        run::mark_stopped_at_entry();
        session::stop(python, events::thread_ident(python)?, StopReason::Entry)?;
        return Ok(may_forget_a_code_object(python));
    }

    if let Some(loaded) = newly_loaded {
        session::announce_rebinding(breakpoints::rebind(python, &loaded)?);
    }

    // this is one of the three ways a frame is entered, and a step in follows
    // the frame the thread has just entered
    if steps::armed_here() {
        steps::entered_frame(python)?;
    }
    Ok(may_forget_a_code_object(python))
}

/// whether the interpreter may be told never to report this code object again
///
/// `DISABLE` is process wide, so a code object forgotten because *this* thread
/// had no use for it is one another thread's step in would never be offered —
/// and a step in that was never offered the frame it entered behaves exactly
/// like a step over, which is a step landing somewhere other than it claimed
fn may_forget_a_code_object(python: Python<'_>) -> Bound<'_, PyAny> {
    if steps::entering_anywhere() {
        python.None().into_bound(python)
    } else {
        events::disable(python)
    }
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

    let plans = breakpoints::hit(code.as_ptr() as usize, line);
    if plans.is_none() && !steps::armed_anywhere() && !world::parking() && !pause::pausing() {
        // nothing wants this line now, and nothing in the process could want it
        // again before something arms it. `DISABLE` is process wide, so a line
        // forgotten here is one a step being made on another thread would never
        // be offered — which is why a step anywhere is enough to keep it
        return Ok(events::disable(python));
    }

    let file: String = code.getattr("co_filename")?.extract()?;
    let thread = events::thread_ident(python)?;

    let mut stopping = Vec::new();
    let mut failure = None;
    if let Some(plans) = plans {
        let at = conditions::Location {
            file: &file,
            line,
            thread,
        };
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

    // asked whatever the breakpoints decided, because a step that landed here
    // has to be taken off either way — one left armed would go on watching for
    // a frame the thread is already being held in
    let landed = steps::reached_line(python)?;

    if let Some((breakpoint, raised)) = failure {
        steps::cancel(python)?;
        session::stop(
            python,
            thread,
            StopReason::EvaluationFailed {
                breakpoint,
                part: raised.part,
                expression: raised.expression,
                file,
                line,
                error: raised.error,
            },
        )?;
    } else if !stopping.is_empty() {
        // a breakpoint decides the reason even when a step landed on the same
        // line: the thread is held exactly where the step was going to put it,
        // and a breakpoint reported as a step would be one the client never
        // saw fire
        steps::cancel(python)?;
        session::stop(
            python,
            thread,
            StopReason::Breakpoint {
                breakpoints: stopping,
                file,
                line,
            },
        )?;
    } else if let Some(kind) = landed {
        session::stop(python, thread, StopReason::Stepped { kind, file, line })?;
    } else if world::parking() {
        // nothing on this line decided to stop, so a stopped world still has to
        // catch the thread here — otherwise a line that holds a breakpoint
        // whose condition was false would be the one place a thread escapes
        world::park(python, thread);
    } else if pause::pausing() && pause::claim() {
        pause::disarm(python)?;
        session::stop(python, thread, StopReason::Paused { file, line })?;
    }

    // deliberately not `DISABLE`: a breakpoint that fired once still exists,
    // and so does one whose condition was false this time
    Ok(python.None().into_bound(python))
}

/// hold this thread if a breakpoint is bound to the template line about to render
///
/// everything about the hit is decided here, in the agent, the same way a
/// python breakpoint's is: the condition, the hit count and the log message all
/// come from the same [`conditions::Plan`], evaluated against the
/// `Node.render_annotated` frame — where `context` is django's `Context` and
/// `self` is the node
fn rendering_a_template_node(python: Python<'_>) -> PyResult<()> {
    if conditions::evaluating() {
        return Ok(());
    }
    let Some(hit) = templates::rendering(python)? else {
        return Ok(());
    };

    let thread = events::thread_ident(python)?;
    let at = conditions::Location {
        file: &hit.file,
        line: hit.line,
        thread,
    };

    let mut stopping = Vec::new();
    let mut failure = None;
    {
        let _suppressed = conditions::suppress();
        let mut place = conditions::Place::unfetched(python);
        for plan in &hit.plans {
            match plan.fire(python, &mut place, &at)? {
                conditions::Fired::Nothing => {}
                conditions::Fired::Stop => stopping.push(plan.id),
                conditions::Fired::Logged(record) => session::log(record),
                conditions::Fired::Failed(raised) => {
                    failure = Some((plan.id, raised));
                    break;
                }
            }
        }
    }

    if let Some((breakpoint, raised)) = failure {
        return session::stop(
            python,
            thread,
            StopReason::EvaluationFailed {
                breakpoint,
                part: raised.part,
                expression: raised.expression,
                file: hit.file,
                line: hit.line,
                error: raised.error,
            },
        );
    }
    if stopping.is_empty() {
        return Ok(());
    }
    session::stop(
        python,
        thread,
        StopReason::Breakpoint {
            breakpoints: stopping,
            file: hit.file,
            line: hit.line,
        },
    )
}

/// the `PY_RETURN` callback, armed on the code objects a step is following
///
/// a return finishes a frame. the step that was in it moves to the caller and
/// lands at its next line, which is what makes stepping over the last statement
/// of a function land where the call came from
///
/// it is also how a django template becomes visible: `Template.__init__`
/// compiles its nodelist as its last act, so the frame that is returning holds
/// a template bpd can bind breakpoints against
///
/// the returned value is part of the signature PEP 669 requires. reading it
/// would be reading the program's state to decide whether to stop, which is the
/// thing the event path does not do
#[pyfunction]
fn on_py_return<'py>(
    python: Python<'py>,
    code: &Bound<'py, PyAny>,
    _offset: i32,
    _returned: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    if steps::armed_here() {
        steps::left_frame(python)?;
    }

    // a template parsed while a condition of ours is running is not one the
    // program parsed, and registering it would rebind from inside a callback
    // that is already inside a callback
    if !conditions::evaluating()
        && templates::is_init_hook(code.as_ptr() as usize)
        && let Some(loaded) = templates::registered(python)?
    {
        session::announce_rebinding(breakpoints::rebind(python, &loaded)?);
    }
    Ok(python.None().into_bound(python))
}

/// the `PY_RESUME` callback, armed for the program while a step in is in flight
///
/// resuming a generator or a coroutine enters a frame without starting one, so
/// this is the event a step in needs for `next(gen)` and for the second `await`
/// of a coroutine. the offset is part of the signature and is always the point
/// the frame suspended at, which the frame itself already says
#[pyfunction]
fn on_py_resume<'py>(
    python: Python<'py>,
    _code: &Bound<'py, PyAny>,
    _offset: i32,
) -> PyResult<Bound<'py, PyAny>> {
    if steps::armed_here() {
        steps::entered_frame(python)?;
    }
    Ok(python.None().into_bound(python))
}

/// the `PY_THROW` callback, the third way a frame is entered
///
/// `gen.throw()` and `gen.close()` resume a suspended frame with an exception
/// already set, which is neither a start nor a resume. the exception is the
/// one being thrown in, and nothing here decides anything from it
#[pyfunction]
fn on_py_throw<'py>(
    python: Python<'py>,
    _code: &Bound<'py, PyAny>,
    _offset: i32,
    _thrown: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    if steps::armed_here() {
        steps::entered_frame(python)?;
    }
    Ok(python.None().into_bound(python))
}

/// the `PY_UNWIND` callback, armed for the program while anything needs it
///
/// two things do, and they are unrelated: a step whose frame is being left by
/// an exception rather than by a return, and the exception breakpoint that
/// stops where an exception leaves the program. `PY_UNWIND` cannot be a local
/// event — `set_local_events` refuses it — so both pay for it process wide
#[pyfunction]
fn on_py_unwind<'py>(
    python: Python<'py>,
    code: &Bound<'py, PyAny>,
    _offset: i32,
    exception: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    if conditions::evaluating() {
        return Ok(python.None().into_bound(python));
    }
    if steps::armed_here() {
        steps::left_frame(python)?;
    }

    if exceptions::uncaught() {
        let frame = events::current_frame(python)?;
        let caller = frame.getattr("f_back")?;
        // no frame above this one that bpd would ever report, so nothing left
        // that could catch it. this is the first moment that is knowable, and
        // the frames it came through have already been popped — what is left of
        // them is the traceback the exception carries
        //
        // the bootstrap is excluded as a frame in its own right and not only as
        // a caller: the agent reports an exception the program did not catch by
        // raising `SystemExit` out of that frame, and reporting *that* would be
        // bpd stopping the program for a decision bpd had just made
        if !frames::is_bootstrap(&frame) && (caller.is_none() || frames::is_bootstrap(&caller)) {
            session::stop(
                python,
                events::thread_ident(python)?,
                StopReason::Uncaught {
                    error: conditions::capture(python, &PyErr::from_value(exception.clone())),
                    file: code.getattr("co_filename")?.extract()?,
                    line: frame.getattr("f_lineno")?.extract()?,
                },
            )?;
        }
    }
    Ok(python.None().into_bound(python))
}

/// the `RAISE` callback, armed for the program while the exception breakpoint is
///
/// cpython raises this **again in every frame the exception propagates into**,
/// with the same object, so what is reported is the first sighting of it on
/// this thread — the frame it was raised in, with the whole stack still standing
#[pyfunction]
fn on_raise<'py>(
    python: Python<'py>,
    code: &Bound<'py, PyAny>,
    _offset: i32,
    exception: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    if conditions::evaluating()
        || !exceptions::raised()
        || !exceptions::newly_raised(python, exception)
    {
        return Ok(python.None().into_bound(python));
    }

    let frame = events::current_frame(python)?;
    session::stop(
        python,
        events::thread_ident(python)?,
        StopReason::Raised {
            error: conditions::capture(python, &PyErr::from_value(exception.clone())),
            file: code.getattr("co_filename")?.extract()?,
            line: frame.getattr("f_lineno")?.extract()?,
        },
    )?;
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

    Ok(format!(
        "{major}.{minor}{}",
        if free_threaded(python, major, minor)? {
            "t"
        } else {
            ""
        }
    ))
}

/// whether this interpreter is a `Py_GIL_DISABLED` build
///
/// a free-threaded interpreter reports the same `version_info` as the gil build
/// of the same release and is a different abi, so this is half of what
/// identifies the interpreter an agent may be loaded by
///
/// it is read off the **extension suffix**, which is built from `SOABI` and
/// carries the interpreter tag — `cpython-314t-darwin` where a gil build has
/// `cpython-314-darwin`, and `cp314t` where windows has `cp314`. that is the
/// same build flag `sysconfig.get_config_var("Py_GIL_DISABLED")` reports, from a
/// module every interpreter has already imported: asking `sysconfig` instead
/// pulls **twenty-nine** modules into the debuggee that a bare run does not
/// have on 3.14 and later, twenty-five on 3.13 — `re`, `enum`, `collections`,
/// `functools` and `threading` among them.
/// see [launching](../../../docs/development/launching.md)
///
/// `sys._is_gil_enabled()` is not the answer, and neither is `sys.flags.gil`.
/// both report the gil as *on* for a free-threaded build that re-enabled it —
/// which is exactly what importing an extension that has not declared itself
/// free-threading safe does, and a mismatched agent is precisely such an
/// extension. they would answer this wrongly in the one case it is asked for
fn free_threaded(python: Python<'_>, major: u8, minor: u8) -> PyResult<bool> {
    let suffix: String = PyModule::import(python, "_imp")?
        .getattr("extension_suffixes")?
        .call0()?
        .get_item(0)?
        .extract()?;

    let tag = format!("{major}{minor}");
    if suffix.contains(&format!("{tag}t")) {
        return Ok(true);
    }
    if suffix.contains(&tag) {
        return Ok(false);
    }
    Err(PyImportError::new_err(format!(
        "this interpreter's extension suffix is `{suffix}`, which does not \
         carry the `{tag}` tag every cpython build puts in it. that tag is how \
         a free-threaded build is told apart from a gil one, and the two are \
         different abis — so there is nothing to check this agent against and \
         guessing would mean running against a layout it was not compiled for"
    )))
}
