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
use bpd_protocol::message::{
    Detail, Entry, Evaluated, Frame, FrameId, FromAgent, FromEngine, LogRecord, Omitted, Resolved,
    Scope, SourceBreakpoint, StopReason, Value,
};

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
    /// log records that arrived while the engine was waiting for an answer
    ///
    /// a thread that reaches a logpoint sends its record without waiting, so
    /// one can be in the socket ahead of the reply to a request. it is kept for
    /// the next `run` rather than dropped, because a log record the client
    /// never sees is a line of the program's history that silently went missing
    pending_logs: Vec<LogRecord>,
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

    /// how many requests the engine has sent this debuggee's agent
    ///
    /// the agent reads the control connection only inside a stop, so this is
    /// also the number of times the debuggee has waited for the debugger
    pub const fn requests_sent(&self) -> u64 {
        self.session.requests_sent()
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

        loop {
            match self.session.next_event()? {
                Some(FromAgent::BreakpointsResolved { resolved }) => return Ok(resolved),
                Some(FromAgent::Logged { record }) => self.pending_logs.push(record),
                Some(other) => {
                    return Err(Error::UnexpectedEvent {
                        event: format!("{other:?}"),
                        expected: EXPECTED,
                    });
                }
                None => return Err(Error::AgentGone { expected: EXPECTED }),
            }
        }
    }

    /// walk the stopped thread's frame chain
    ///
    /// `top` bounds how many frames come back, counting from the one that
    /// stopped. the answer says how deep the stack really is either way
    pub fn stack(&mut self, top: Option<u32>) -> Result<Stack> {
        match self.ask(&FromEngine::Stack { top }, "the stack")? {
            FromAgent::Stack { frames, depth } => Ok(Stack { frames, depth }),
            other => Err(unexpected(&other, "the stack")),
        }
    }

    /// read one scope of one frame
    pub fn variables(&mut self, frame: FrameId, scope: Scope, detail: Detail) -> Result<Variables> {
        const EXPECTED: &str = "the variables of a scope";

        let request = FromEngine::Variables {
            frame,
            scope,
            detail,
        };
        match self.ask(&request, EXPECTED)? {
            FromAgent::Variables {
                entries,
                unbound,
                unreadable,
                omitted,
                ..
            } => Ok(Variables {
                entries,
                unbound,
                unreadable,
                omitted,
            }),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// evaluate a python expression in a frame
    ///
    /// an expression that raises is answered with the exception, which is the
    /// answer — not an error of the engine's
    pub fn evaluate(
        &mut self,
        frame: FrameId,
        expression: &str,
        detail: Detail,
    ) -> Result<Evaluated> {
        const EXPECTED: &str = "the value of an expression";

        let request = FromEngine::Evaluate {
            frame,
            expression: expression.to_string(),
            detail,
        };
        match self.ask(&request, EXPECTED)? {
            FromAgent::Evaluated { result } => Ok(result),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// write a variable of a frame, and read back what the frame holds after it
    pub fn set_variable(
        &mut self,
        frame: FrameId,
        scope: Scope,
        name: &str,
        value: &str,
        detail: Detail,
    ) -> Result<Evaluated> {
        const EXPECTED: &str = "the value a variable was set to";

        let request = FromEngine::SetVariable {
            frame,
            scope,
            name: name.to_string(),
            value: value.to_string(),
            detail,
        };
        match self.ask(&request, EXPECTED)? {
            FromAgent::Evaluated { result } => Ok(result),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// send one request and wait for the answer to it
    ///
    /// a logpoint's record can be in the socket ahead of an answer, because a
    /// thread that reaches one sends it without waiting. it is kept for the
    /// next `run` rather than dropped, for the same reason
    /// [`Self::set_breakpoints`] keeps one
    fn ask(&mut self, request: &FromEngine, expected: &'static str) -> Result<FromAgent> {
        if self.stopped.is_none() {
            return Err(Error::NotStopped { wanted: expected });
        }
        self.session.send(request)?;

        loop {
            match self.session.next_event()? {
                Some(FromAgent::Logged { record }) => self.pending_logs.push(record),
                Some(FromAgent::Refused { reason }) => return Err(Error::Refused { reason }),
                Some(answer) => return Ok(answer),
                None => return Err(Error::AgentGone { expected }),
            }
        }
    }

    /// let the debuggee run until it stops again or finishes
    ///
    /// the agent can speak while the program runs — loading a module changes
    /// what a breakpoint resolves to, and a logpoint has something to say every
    /// time it is reached — so nothing it said is left in a socket buffer for
    /// someone to find
    ///
    /// log records go to `on_log` as they arrive rather than into the result.
    /// there is no bound on how many a logpoint produces, and a debugger that
    /// accumulated a million of them before saying anything would be holding
    /// the program's history hostage to its own memory
    pub fn run(&mut self, mut on_log: impl FnMut(LogRecord)) -> Result<Running> {
        const EXPECTED: &str = "the debuggee to stop or exit";

        if self.stopped.take().is_none() {
            return Err(Error::NotStopped { wanted: EXPECTED });
        }
        for record in self.pending_logs.drain(..) {
            on_log(record);
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
                Some(FromAgent::Logged { record }) => on_log(record),
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

/// the stopped thread's stack
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    /// the frames, the one that stopped first
    pub frames: Vec<Frame>,
    /// how deep the stack is, which is more than `frames` when fewer were asked
    /// for
    pub depth: usize,
}

/// what one scope of one frame holds
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variables {
    /// the names it holds
    pub entries: Vec<Entry>,
    /// names of the scope that hold nothing at this line
    pub unbound: Vec<String>,
    /// names of the scope whose value the frame does not expose
    pub unreadable: Vec<String>,
    /// everything the answer left out, and why
    pub omitted: Vec<Omitted>,
}

impl Variables {
    /// what one name holds, or `None` when the scope does not hold it
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| &entry.value)
    }

    /// the names, in the order the interpreter keeps them
    pub fn names(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect()
    }
}

/// the agent answered a request with something else entirely
fn unexpected(event: &FromAgent, expected: &'static str) -> Error {
    Error::UnexpectedEvent {
        event: format!("{event:?}"),
        expected,
    }
}

/// what came of a launch
///
/// deliberately closed. a third outcome would be something every caller has to
/// decide about, and `#[non_exhaustive]` would let them absorb it into a
/// catch-all arm instead — which is how a debugger acquires a state nobody
/// handles
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "one variant holds a running process and the other holds its exit \
              status, and boxing the first would put an allocation on the path \
              every launch takes to make a value that is moved once smaller"
)]
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
        pending_logs: Vec::new(),
        _staged: staged,
    }))
}
