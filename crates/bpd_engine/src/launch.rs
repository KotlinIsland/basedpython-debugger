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
use bpd_protocol::message::{FromEngine, StopReason};

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
    stopped: StopReason,
    /// held so the staged agent outlives the debuggee that imported it
    _staged: agent::Staged,
}

impl Debuggee {
    /// the control connection to the agent
    pub fn session(&mut self) -> &mut Session {
        &mut self.session
    }

    /// why the debuggee is stopped
    pub fn stopped(&self) -> StopReason {
        self.stopped
    }

    /// let the debuggee run, and wait for it to finish
    pub fn resume_to_exit(mut self) -> Result<ExitStatus> {
        self.session.send(&FromEngine::Resume)?;
        self.child.wait().map_err(|source| Error::Spawn {
            interpreter: PathBuf::from("the debuggee"),
            source,
        })
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
        stopped: reason,
        child,
        session,
        _staged: staged,
    }))
}
