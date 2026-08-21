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

mod armed;
mod attach;
mod breakpoints;
mod bytecode;
mod cells;
// the `sitecustomize` a child is entered through is portable, but what decides
// whether children are debugged at all is `debugChildren` — and that is refused
// where there is no `fork`, because half a feature reported as the whole of one
// is the thing this project does not ship
#[cfg(unix)]
mod children;
mod code;
mod conditions;
mod events;
mod exceptions;
mod facts;
mod files;
// `fork` is posix, and so is `os.register_at_fork`. there is nothing on windows
// for this to be the answer to
#[cfg(unix)]
mod forks;
mod frames;
mod inplace;
mod interpframe;
mod linetable;
mod pause;
mod replace;
mod restarts;
mod retainers;
mod run;
mod session;
mod source;
mod sources;
mod spawns;
mod steps;
mod stops;
mod tasks;
mod templates;
mod threads;
mod trail;
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

/// the oldest interpreter `build.rs` will compile this against
///
/// stamped so a test can put it beside `bpd_core::python::MINIMUM_SUPPORTED`. a
/// build script cannot depend on a workspace crate, so the number is written in
/// two places and a test is what keeps them one
#[cfg(test)]
const BUILD_MINIMUM: &str = env!("BPD_AGENT_MINIMUM");

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
        BUILT_FOR, DEBUGGER_TOOL_ID, arm, attach, frames, monitoring, run, running_version,
        session, spawns,
    };
    #[cfg(unix)]
    use super::{children, forks};
    use bpd_protocol::env::Form;
    use pyo3::exceptions::PySystemExit;
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

        // what an `exec`'d child would have to be told, kept now because the
        // loop below is the last moment the launcher's variables are readable.
        // nothing is put back into the environment unless `debugChildren` asks
        // for it
        #[cfg(unix)]
        children::remember(
            python,
            &endpoint,
            &required_env(bpd_protocol::env::CHILD_TOKEN)?,
            &required_env(bpd_protocol::env::SITECUSTOMIZE)?,
        )?;

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

    /// the whole of an `exec`'d child's entry point
    ///
    /// called from the staged `sitecustomize`, at interpreter startup, before
    /// `__main__` exists. everything it does is in [`children`]; what is here
    /// is the name the four lines of python reach it by
    ///
    /// it is **not** [`main`], and the two are not variants of one another: a
    /// launched program is entered through `-c` with a target to run, and a
    /// child is a program the interpreter is already about to run on its own.
    /// the one thing they share is what a debuggee is, and that is
    /// [`children::entered`]'s to call rather than to repeat
    #[cfg(unix)]
    #[pyfunction]
    fn child_main(python: Python<'_>) -> PyResult<()> {
        children::entered(python)
    }

    /// read a variable the launcher is contracted to have set
    fn required_env(name: &str) -> PyResult<String> {
        std::env::var(name).map_err(|_| {
            PySystemExit::new_err(format!(
                "bpd: `{name}` is not set. `bpd_agent.main()` is the entry point the launcher uses \
                 and is not meant to be called by hand"
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
        super::verify(python)
    }

    /// claim the debugger tool id, or report who already holds it
    #[pyfunction]
    fn claim(python: Python<'_>) -> PyResult<()> {
        super::claim(python)
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

/// fail unless this interpreter is the one the agent was compiled against
///
/// a mismatched minor version sometimes fails to import outright and sometimes
/// loads and then reads the wrong offsets, which is far worse. the check is
/// cheap and it happens before anything is instrumented
///
/// it is at the crate root rather than in the module because **two** entry
/// points need it and there is one interpreter to be right about: a launched
/// program, whose interpreter bpd chose, and an `exec`'d child, whose
/// interpreter the *program* chose and which is the one this is really for
fn verify(python: Python<'_>) -> PyResult<()> {
    let running = running_version(python)?;
    if running == BUILT_FOR {
        return Ok(());
    }
    Err(PyImportError::new_err(format!(
        "this bpd agent was built for python {BUILT_FOR} and is being imported \
         by python {running}. the agent is not abi3 — it reads interpreter \
         state whose layout changes between releases — so the build has to \
         match the interpreter exactly"
    )))
}

/// claim the debugger tool id, or report who already holds it
fn claim(python: Python<'_>) -> PyResult<()> {
    let holder: Option<String> = monitoring(python)?
        .call_method1("get_tool", (DEBUGGER_TOOL_ID,))?
        .extract()?;
    if let Some(holder) = holder {
        return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
            "`sys.monitoring` tool id {DEBUGGER_TOOL_ID} is already held by \
             `{holder}`. bpd claims that id or none: tools are not \
             interchangeable, and taking a different one would mean debugging \
             with another tool's event semantics. stop `{holder}` and try again"
        )));
    }

    monitoring(python)?.call_method1("use_tool_id", (DEBUGGER_TOOL_ID, TOOL_NAME))?;
    Ok(())
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

    // `asyncio/tasks.py`'s module body is where `create_task` comes from, and
    // this is the moment it runs — before any task exists, and without asking
    // the import machinery anything
    if !conditions::evaluating()
        && tasks::notice(code)?
        && let Some(hook) = tasks::hook(python)
    {
        session::refresh_code(python, hook.bind(python))?;
    }

    // this is one of the three ways a frame is entered, and a step in follows
    // the frame the thread has just entered
    if steps::armed_here() {
        steps::entered_frame(python)?;
    }
    // and it is the way the frame a restart is waiting for comes into being
    if restarts::armed_here() {
        restarts::entered_frame(python)?;
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
    if armed::entering_anywhere() {
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

    // where the program went, before anything decides whether to stop. it is
    // the one mode that turns off the `DISABLE` this whole design rests on, so
    // it is asked for rather than assumed — see `crate::trail`
    if trail::recording() {
        trail::went(python, code, line, events::thread_ident(python)?);
    }

    // a restart takes this line before anything else on it decides anything —
    // see `restart_took_the_line`
    if restart_took_the_line(python)? {
        return Ok(python.None().into_bound(python));
    }

    let plans = breakpoints::hit(code.as_ptr() as usize, line);
    if plans.is_none()
        && !armed::armed_anywhere()
        && !world::parking()
        && !pause::pausing()
        && !trail::recording()
    {
        // nothing wants this line now, and nothing in the process could want it
        // again before something arms it. `DISABLE` is process wide, so a line
        // forgotten here is one a step being made on another thread would never
        // be offered — which is why a step anywhere is enough to keep it
        return Ok(events::disable(python));
    }

    // where this is **reported**, which for a basedpython build is the `.by`
    // line the generated one came from. `line` itself stays the generated line
    // everywhere it decides anything: it is what the breakpoint table is keyed
    // by and what the interpreter offered
    let at = sources::locate(code.getattr("co_filename")?.extract()?, line);
    let (file, reported) = (at.file, at.line);
    let thread = events::thread_ident(python)?;

    let Fired {
        stopping,
        acted,
        failure,
    } = fire_the_breakpoints(python, plans, &file, reported, thread)?;

    // before the stop is reported, so a client told the program stopped can
    // already see that the breakpoint waiting on this one is armed. the other
    // order reports a stop whose consequence arrives after it
    session::announce_rebinding(breakpoints::arm_after(python, &acted)?);

    // asked whatever the breakpoints decided, because a step that landed here
    // has to be taken off either way — one left armed would go on watching for
    // a frame the thread is already being held in
    let landed = steps::reached_line(python)?;
    // the same question for a restart, and asked whatever the breakpoints
    // decided for the same reason: one left armed would go on watching for a
    // frame the thread is already being held in
    let restarted = restarts::reached_line(python)?;

    if let Some((breakpoint, raised)) = failure {
        left_armed_here(python)?;
        session::stop(
            python,
            thread,
            StopReason::EvaluationFailed {
                breakpoint,
                part: raised.part,
                expression: raised.expression,
                file,
                line: reported,
                error: raised.error,
            },
        )?;
    } else if !stopping.is_empty() {
        // a breakpoint decides the reason even when a step or a restart landed
        // on the same line: the thread is held exactly where either was going
        // to put it, and a breakpoint reported as one of them would be one the
        // client never saw fire
        left_armed_here(python)?;
        session::stop(
            python,
            thread,
            StopReason::Breakpoint {
                breakpoints: stopping,
                file,
                line: reported,
            },
        )?;
    } else if let Some(reason) = restarted {
        // only the step: `restarts::reached_line` has already taken the restart
        // off, which is how it produced this reason at all
        steps::cancel(python)?;
        session::stop(python, thread, reason)?;
    } else if let Some(kind) = landed {
        session::stop(
            python,
            thread,
            StopReason::Stepped {
                kind,
                file,
                line: reported,
            },
        )?;
    } else if world::parking() {
        // nothing on this line decided to stop, so a stopped world still has to
        // catch the thread here — otherwise a line that holds a breakpoint
        // whose condition was false would be the one place a thread escapes
        left_armed_here(python)?;
        world::park(python, thread);
    } else if pause::pausing() && pause::claim() {
        pause::disarm(python)?;
        left_armed_here(python)?;
        session::stop(
            python,
            thread,
            StopReason::Paused {
                file,
                line: reported,
            },
        )?;
    }

    // deliberately not `DISABLE`: a breakpoint that fired once still exists,
    // and so does one whose condition was false this time
    Ok(python.None().into_bound(python))
}

/// take off whatever this thread had armed, because something is about to hold it
///
/// every path that holds a thread calls this, and none of them may skip it. an
/// operation left armed goes on watching for a frame the thread is already being
/// held in — and worse, it keeps `armed::armed_anywhere()` true, which is what
/// stops the interpreter being told to forget a location. one leaked that way
/// costs the whole process its `DISABLE` for the rest of the run
fn left_armed_here(python: Python<'_>) -> PyResult<()> {
    steps::cancel(python)?;
    restarts::cancel(python)
}

/// whether a restart took this line, so that nothing else on it decides anything
///
/// asked **before** the breakpoints, and that ordering is the point. a rewind
/// happens at the line event and the line then does not run at all, so a
/// breakpoint that fired there would be reporting the program at a line it never
/// executed. an abandoned restart holds the thread where it is, which is also
/// the end of this line. either way there is nothing left for it to decide
fn restart_took_the_line(python: Python<'_>) -> PyResult<bool> {
    match restarts::rewinding(python)? {
        restarts::Rewind::NotMine => Ok(false),
        restarts::Rewind::Rewound => Ok(true),
        restarts::Rewind::Abandoned(reason) => {
            session::stop(python, events::thread_ident(python)?, reason)?;
            Ok(true)
        }
    }
}

/// what the breakpoints bound to one line decided about it
struct Fired {
    /// the breakpoints that decided to hold the thread here
    stopping: Vec<u32>,
    /// the breakpoints that **acted**, which is what arms anything waiting on
    /// them
    ///
    /// a condition that was false, and a hit the count has not reached yet, are
    /// not acts: "after the request came through" means the breakpoint did its
    /// thing, not that the interpreter passed the line
    acted: Vec<u32>,
    /// the breakpoint whose expression raised, and what it raised
    failure: Option<(u32, conditions::Raised)>,
}

/// run every breakpoint bound to this line, and say what they decided
fn fire_the_breakpoints(
    python: Python<'_>,
    plans: Option<Vec<std::sync::Arc<conditions::Plan>>>,
    file: &str,
    reported: u32,
    thread: u64,
) -> PyResult<Fired> {
    let mut fired = Fired {
        stopping: Vec::new(),
        acted: Vec::new(),
        failure: None,
    };
    let Some(plans) = plans else {
        return Ok(fired);
    };

    let at = conditions::Location {
        file,
        line: reported,
        thread,
    };
    // held across every expression of every breakpoint on this line, so a
    // condition that calls a function with a breakpoint in it runs to an answer
    // rather than stopping inside itself
    let _suppressed = conditions::suppress();
    let mut place = conditions::Place::unfetched(python);

    for plan in &plans {
        match plan.fire(python, &mut place, &at)? {
            conditions::Fired::Nothing => {}
            conditions::Fired::Stop => {
                fired.acted.push(plan.id);
                fired.stopping.push(plan.id);
            }
            conditions::Fired::Logged(record) => {
                fired.acted.push(plan.id);
                session::log(record);
            }
            // the remaining breakpoints on this line are left alone: the program
            // is about to be held here anyway, and a log record produced during
            // a hit the client is being told is broken would be a record nobody
            // can trust
            conditions::Fired::Failed(raised) => {
                fired.failure = Some((plan.id, raised));
                break;
            }
        }
    }
    Ok(fired)
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
    // the breakpoints that **acted** here, which is what arms anything waiting
    // for them. a condition that was false, and a hit the count has not reached
    // yet, are not acts: "after the request came through" means the breakpoint
    // did its thing, not that the interpreter passed the line
    let mut acted: Vec<u32> = Vec::new();
    let mut failure = None;
    {
        let _suppressed = conditions::suppress();
        let mut place = conditions::Place::unfetched(python);
        for plan in &hit.plans {
            match plan.fire(python, &mut place, &at)? {
                conditions::Fired::Nothing => {}
                conditions::Fired::Stop => {
                    acted.push(plan.id);
                    stopping.push(plan.id);
                }
                conditions::Fired::Logged(record) => {
                    acted.push(plan.id);
                    session::log(record);
                }
                conditions::Fired::Failed(raised) => {
                    failure = Some((plan.id, raised));
                    break;
                }
            }
        }
    }

    // the same rule as a python line: a template breakpoint that acted arms
    // whatever was waiting for it. it is not a different kind of hit
    session::announce_rebinding(breakpoints::arm_after(python, &acted)?);

    if let Some((breakpoint, raised)) = failure {
        left_armed_here(python)?;
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
    left_armed_here(python)?;
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
    returned: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    if steps::armed_here() {
        steps::left_frame(python)?;
    }

    // a task made while a condition of ours is running was made by bpd rather
    // than by the program, and recording it would file a stack of ours under it
    if !conditions::evaluating() && tasks::is_hook(code.as_ptr() as usize) {
        tasks::record(python, returned)?;
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
    // a restart whose caller is being left by an exception can never complete,
    // and the frame it forced out has already gone. the thread is held here
    // saying so rather than carrying on as though nothing had been asked
    if restarts::armed_here()
        && let Some(reason) = restarts::left_frame(python)?
    {
        session::stop(python, events::thread_ident(python)?, reason)?;
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
            left_armed_here(python)?;
            session::stop(
                python,
                events::thread_ident(python)?,
                uncaught(python, code, &frame, exception)?,
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
    let at = at_of(code, &frame)?;
    left_armed_here(python)?;
    session::stop(
        python,
        events::thread_ident(python)?,
        StopReason::Raised {
            error: conditions::capture(python, &PyErr::from_value(exception.clone())),
            file: at.file,
            line: at.line,
        },
    )?;
    Ok(python.None().into_bound(python))
}

/// the stop an exception leaving the outermost frame reports
///
/// a function of its own so that the two exception stops read the location the
/// same way. they are the only stops whose file comes from the code object and
/// whose line comes from the frame, and the pair has to be mapped together
fn uncaught(
    python: Python<'_>,
    code: &Bound<'_, PyAny>,
    frame: &Bound<'_, PyAny>,
    exception: &Bound<'_, PyAny>,
) -> PyResult<StopReason> {
    let at = at_of(code, frame)?;
    Ok(StopReason::Uncaught {
        error: conditions::capture(python, &PyErr::from_value(exception.clone())),
        file: at.file,
        line: at.line,
    })
}

/// where a running frame is, as a client should be told it
fn at_of(code: &Bound<'_, PyAny>, frame: &Bound<'_, PyAny>) -> PyResult<sources::Reported> {
    Ok(sources::locate(
        code.getattr("co_filename")?.extract()?,
        frame.getattr("f_lineno")?.extract()?,
    ))
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

/// the running interpreter's tag
///
/// spelled by [`bpd_core::python::InterpreterTag`], which is also what `bpd`
/// names an agent directory with — the check here and the selection out there
/// have to be about the same thing or one of them is answering a different
/// question
fn running_version(python: Python<'_>) -> PyResult<String> {
    let info = PyModule::import(python, "sys")?.getattr("version_info")?;
    let major: u8 = info.getattr("major")?.extract()?;
    let minor: u8 = info.getattr("minor")?.extract()?;

    Ok(
        bpd_core::python::InterpreterTag::new(major, minor, free_threaded(python, major, minor)?)
            .to_string(),
    )
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

#[cfg(test)]
mod tests {
    use super::{BUILD_MINIMUM, BUILT_FOR};
    use bpd_core::python::{InterpreterTag, MINIMUM_SUPPORTED};

    /// `bpd` finds an agent by naming a directory after the interpreter's tag,
    /// and the agent refuses an interpreter whose tag is not the one stamped
    /// here. those are two different guarantees and they are only comparable
    /// because they are said in the same words — a stamp in any other spelling
    /// would be an agent no launcher could ever select
    #[test]
    fn the_stamped_tag_is_the_one_bpd_selects_by() {
        let tag = InterpreterTag::parse(BUILT_FOR)
            .unwrap_or_else(|| panic!("`{BUILT_FOR}` was stamped by build.rs and is not a tag"));

        assert_eq!(tag.to_string(), BUILT_FOR);
    }

    #[test]
    fn the_agent_build_minimum_matches_the_support_policy() {
        assert_eq!(
            BUILD_MINIMUM,
            format!("{}.{}", MINIMUM_SUPPORTED.major, MINIMUM_SUPPORTED.minor),
            "build.rs refuses below one interpreter and the support policy \
             names another, so an interpreter bpd will not drive can still \
             build an agent"
        );
    }
}
