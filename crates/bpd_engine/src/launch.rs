//! starting a debuggee with its agent already attached
//!
//! the debuggee is entered through `python -c "import bpd_agent; bpd_agent.main()"`.
//! everything after that — repairing `sys.argv` and `sys.path[0]`, building
//! `__main__`, running the program, reporting its exit — happens in the agent,
//! in rust. a python bootstrap file would be a second place for launch
//! semantics to be subtly wrong, and would leave its own name in `sys.modules`
//!
//! the `-c` form is what makes this possible and is also its one hazard: it sets
//! `sys.path[0]` to the empty string, which is not what a script wants. the
//! agent repairs it before any user code runs, and
//! `crates/bpd/tests/launch_parity.rs` compares the result against a bare
//! interpreter rather than trusting that

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};

use bpd_core::python::Capabilities;
use bpd_protocol::env;
use bpd_protocol::message::{FromAgent, FromEngine, Resolved, SourceBreakpoint, StopReason};

use crate::{Error, Listener, Result, Session, agent};

/// the entry point the interpreter is given
///
/// deliberately the shortest thing that can work: every decision it could
/// contain belongs in the agent, where it is rust and is tested
const BOOTSTRAP: &str = "import bpd_agent; bpd_agent.main()";

/// a running debuggee, stopped at entry with its agent attached
#[derive(Debug)]
pub struct Debuggee {
    child: Child,
    session: Session,
    stopped: Option<StopReason>,
    /// held so the staged agent outlives the debuggee that imported it
    _staged: agent::Staged,
}

/// what a resumed debuggee did next
///
/// deliberately closed, like [`Launched`]: a third outcome is something every
/// caller has to decide about, and a catch-all arm is how a debugger acquires a
/// state nobody handles
#[derive(Debug)]
pub enum Running {
    /// it stopped again
    Stopped {
        /// where, and why
        reason: StopReason,
        /// what loading a file changed about the breakpoint set on the way
        rebound: Vec<Resolved>,
    },

    /// it finished
    Exited {
        /// how it exited
        status: ExitStatus,
        /// what loading a file changed about the breakpoint set on the way
        rebound: Vec<Resolved>,
    },
}

impl Debuggee {
    /// why the debuggee is stopped, or `None` once it has been let go
    pub fn stopped(&self) -> Option<&StopReason> {
        self.stopped.as_ref()
    }

    /// replace the whole breakpoint set, and say how every one of them resolved
    ///
    /// only while the debuggee is stopped. the agent reads the control
    /// connection inside a stop and nowhere else — asking a running program to
    /// bind something would be a request that is answered whenever it next
    /// happens to stop, which is not an answer
    pub fn set_breakpoints(&mut self, breakpoints: Vec<SourceBreakpoint>) -> Result<Vec<Resolved>> {
        const EXPECTED: &str = "the breakpoints to resolve";

        if self.stopped.is_none() {
            return Err(Error::NotStopped { wanted: EXPECTED });
        }

        // every report about a breakpoint, and every stop it causes, names it by
        // this id. two breakpoints sharing one would give the client a single
        // answer for two questions, and it would have no way to tell which
        let mut seen = std::collections::BTreeSet::new();
        for breakpoint in &breakpoints {
            if !seen.insert(breakpoint.id) {
                return Err(Error::DuplicateBreakpointId { id: breakpoint.id });
            }
        }
        self.session
            .send(&FromEngine::SetBreakpoints { breakpoints })?;

        match self.session.next_event()? {
            Some(FromAgent::BreakpointsResolved { resolved }) => Ok(resolved),
            Some(other) => Err(Error::UnexpectedEvent {
                event: format!("{other:?}"),
                expected: EXPECTED,
            }),
            None => Err(Error::AgentGone { expected: EXPECTED }),
        }
    }

    /// let the debuggee run until it stops again or finishes
    ///
    /// the agent can speak while the program runs — loading a module changes
    /// what a breakpoint resolves to — so everything it said on the way is
    /// collected and handed back with the outcome, rather than left in a socket
    /// buffer for someone to find
    pub fn run(&mut self) -> Result<Running> {
        const EXPECTED: &str = "the debuggee to stop or exit";

        if self.stopped.take().is_none() {
            return Err(Error::NotStopped { wanted: EXPECTED });
        }
        self.session.send(&FromEngine::Resume)?;

        let mut rebound = Vec::new();
        loop {
            match self.session.next_event()? {
                Some(FromAgent::Stopped { reason }) => {
                    self.stopped = Some(reason.clone());
                    return Ok(Running::Stopped { reason, rebound });
                }
                Some(FromAgent::BreakpointsResolved { resolved }) => rebound.extend(resolved),
                Some(other) => {
                    return Err(Error::UnexpectedEvent {
                        event: format!("{other:?}"),
                        expected: EXPECTED,
                    });
                }
                None => {
                    let status = self.child.wait().map_err(|source| Error::Spawn {
                        interpreter: PathBuf::from("the debuggee"),
                        source,
                    })?;
                    return Ok(Running::Exited { status, rebound });
                }
            }
        }
    }
}

/// what came of a launch
///
/// deliberately closed. a third outcome would be something every caller has to
/// decide about, and `#[non_exhaustive]` would let them absorb it into a
/// catch-all arm instead — which is how a debugger acquires a state nobody
/// handles
#[derive(Debug)]
pub enum Launched {
    /// the debuggee is attached and held before its first statement
    Stopped(Debuggee),

    /// the debuggee finished before it could be stopped
    ///
    /// a program that cannot be compiled never reaches its first statement, and
    /// neither does one whose interpreter refused to start it. that is not an
    /// engine failure and it is not something to report: the debuggee has
    /// already said what went wrong on its own stderr, in the interpreter's own
    /// words, and its exit code is the answer
    ExitedBeforeStopping(ExitStatus),
}

/// launch a script under the debugger and stop before its first statement
///
/// returns once the agent has reported that it is stopped, so the caller holds a
/// debuggee that has run none of the program yet
pub fn launch(interpreter: &Capabilities, script: &Path, args: &[OsString]) -> Result<Launched> {
    interpreter
        .require_debuggable()
        .map_err(|error| Error::LocateAgent {
            reason: error.to_string(),
        })?;

    let staged = agent::stage()?;
    let listener = Listener::bind()?;
    let endpoint = listener.endpoint()?;

    let mut command = Command::new(&interpreter.executable);
    command
        .arg("-c")
        .arg(BOOTSTRAP)
        .args(args)
        .env(env::ENDPOINT, endpoint.to_string())
        .env(env::TOKEN, listener.token_hex())
        .env(env::TARGET, script)
        .env("PYTHONPATH", staged.python_path());

    let mut child = command.spawn().map_err(|source| Error::Spawn {
        interpreter: interpreter.executable.clone(),
        source,
    })?;

    let session = listener.accept(|| {
        let exited = child.try_wait().map_err(|source| Error::Spawn {
            interpreter: interpreter.executable.clone(),
            source,
        })?;
        Ok(exited.map(|status| status.to_string()))
    });

    let mut session = match session {
        Ok(session) => session,
        Err(error) => {
            // the debuggee is useless without its agent, and leaving it running
            // would strand a process nobody is holding
            let _kill = child.kill();
            let _reap = child.wait();
            return Err(error);
        }
    };

    let reason = match session.expect_stop() {
        Ok(reason) => reason,
        Err(Error::AgentGone { .. }) => {
            let status = child.wait().map_err(|source| Error::Spawn {
                interpreter: interpreter.executable.clone(),
                source,
            })?;
            return Ok(Launched::ExitedBeforeStopping(status));
        }
        Err(error) => return Err(error),
    };

    Ok(Launched::Stopped(Debuggee {
        stopped: Some(reason),
        child,
        session,
        _staged: staged,
    }))
}
