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
use std::time::Duration;

use bpd_core::python::Capabilities;
use bpd_protocol::env;
use bpd_protocol::message::{
    Detail, Entry, Evaluated, Frame, FrameId, FromAgent, FromEngine, LogRecord, Mode, Omitted,
    Resolved, Scope, SourceBreakpoint, Stop, ThreadState, Value, Which,
};

use crate::{Error, Listener, Result, Session, agent};

/// the entry point the interpreter is given
///
/// deliberately the shortest thing that can work: every decision it could
/// contain belongs in the agent, where it is rust and is tested
const BOOTSTRAP: &str = "import bpd_agent; bpd_agent.main()";

/// a running debuggee, with at least one of its threads held
///
/// a stop holds **one thread**, so more than one can be held at a time and a
/// request that is about a thread says which one. the ones that are about the
/// process — the breakpoint set, the thread census — do not
#[derive(Debug)]
pub struct Debuggee {
    child: Child,
    session: Session,
    /// the stops held now, in the order the agent reported them
    held: Vec<Stop>,
    /// log records that arrived while the engine was waiting for an answer
    ///
    /// a thread that reaches a logpoint sends its record without waiting, so
    /// one can be in the socket ahead of the reply to a request. it is kept for
    /// the next `run` rather than dropped, because a log record the client
    /// never sees is a line of the program's history that silently went missing
    pending_logs: Vec<LogRecord>,
    /// rebindings that arrived while the engine was waiting for an answer
    ///
    /// loading a file changes what a breakpoint resolves to, and the agent says
    /// so while the program runs. kept for the next wait rather than dropped,
    /// for the same reason a log record is
    pending_rebinds: Vec<Resolved>,
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
    /// a thread stopped
    Stopped {
        /// which thread, where, and why
        stop: Stop,
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

    /// the program ran to its end with threads still held
    ///
    /// it cannot exit: the interpreter finalizes by joining the program's
    /// non-daemon threads, and a held one cannot be joined. resuming the named
    /// threads is what lets it finish, and until then the process is sitting
    /// there — which is a fact rather than the hang it would otherwise look
    /// like
    Finishing {
        /// the threads still held as the program ended
        threads: Vec<u64>,
        /// what loading a file changed about the breakpoint set on the way
        rebound: Vec<Resolved>,
    },
}

impl Debuggee {
    /// the stops held right now, in the order the agent reported them
    ///
    /// more than one is ordinary: a stop holds one thread, so a second thread
    /// reaching a breakpoint while a first is held reports its own straight
    /// away rather than waiting for the first to be resumed
    pub fn held(&self) -> &[Stop] {
        &self.held
    }

    /// how many requests the engine has sent this debuggee's agent
    ///
    /// the agent answers on a thread it is holding, so this is also the number
    /// of times the debuggee has waited for the debugger
    pub const fn requests_sent(&self) -> u64 {
        self.session.requests_sent()
    }

    /// the one stop that is held, for a request that is about one thread
    ///
    /// refuses rather than picking when several are held. a debugger that
    /// answered about whichever thread came first would be answering a question
    /// nobody asked
    fn only(&self, wanted: &'static str) -> Result<u64> {
        match self.held.as_slice() {
            [] => Err(Error::NotStopped { wanted }),
            [stop] => Ok(stop.stop),
            several => Err(Error::AmbiguousStop {
                wanted,
                held: several.iter().map(|stop| stop.stop).collect(),
            }),
        }
    }

    /// replace the whole breakpoint set, and say how every one of them resolved
    ///
    /// only while a thread is held. the agent answers on a thread it is holding
    /// and nowhere else — asking a running program to bind something would be a
    /// request that is answered whenever it next happens to stop, which is not
    /// an answer
    pub fn set_breakpoints(&mut self, breakpoints: Vec<SourceBreakpoint>) -> Result<Vec<Resolved>> {
        const EXPECTED: &str = "the breakpoints to resolve";

        // every report about a breakpoint, and every stop it causes, names it by
        // this id. two breakpoints sharing one would give the client a single
        // answer for two questions, and it would have no way to tell which
        let mut seen = std::collections::BTreeSet::new();
        for breakpoint in &breakpoints {
            if !seen.insert(breakpoint.id) {
                return Err(Error::DuplicateBreakpointId { id: breakpoint.id });
            }
        }

        let request = FromEngine::SetBreakpoints { breakpoints };
        match self.ask(&request, EXPECTED)? {
            FromAgent::BreakpointsResolved { resolved } => Ok(resolved),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// walk one held thread's frame chain
    ///
    /// `top` bounds how many frames come back, counting from the one that
    /// stopped. the answer says how deep the stack really is either way
    pub fn stack(&mut self, stop: u64, top: Option<u32>) -> Result<Stack> {
        const EXPECTED: &str = "the stack";

        match self.ask(&FromEngine::Stack { stop, top }, EXPECTED)? {
            FromAgent::Stack {
                frames,
                depth,
                mode,
            } => Ok(Stack {
                frames,
                depth,
                mode,
            }),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// walk the frame chain of the only held thread
    pub fn the_stack(&mut self, top: Option<u32>) -> Result<Stack> {
        let stop = self.only("the stack")?;
        self.stack(stop, top)
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
                mode,
                ..
            } => Ok(Variables {
                entries,
                unbound,
                unreadable,
                omitted,
                mode,
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
            FromAgent::Evaluated { result, .. } => Ok(result),
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
            FromAgent::Evaluated { result, .. } => Ok(result),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// what every thread of the debuggee is doing
    ///
    /// the answer to "the other threads are supposed to be running — are they".
    /// everything it says about a thread bpd is not holding is a sample, and
    /// `settle` is how far apart the two samples it compares were taken
    pub fn threads(&mut self, settle: Duration) -> Result<Threads> {
        const EXPECTED: &str = "what the threads are doing";

        let settle_ms =
            u32::try_from(settle.as_millis()).map_err(|_| Error::SettleTooLong { settle })?;
        match self.ask(&FromEngine::Threads { settle_ms }, EXPECTED)? {
            FromAgent::Threads {
                threads,
                settle_ms,
                mode,
            } => Ok(Threads {
                threads,
                settle: Duration::from_millis(settle_ms.into()),
                mode,
            }),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// hold every thread that can be held, until `stop` is resumed
    ///
    /// the explicit mode. the answer names the threads it could not hold, which
    /// are parked in a C call and are still running — a whole-program snapshot
    /// is only what came back with that list empty
    pub fn stop_the_world(&mut self, stop: u64, settle: Duration) -> Result<WorldStopped> {
        const EXPECTED: &str = "the world to stop";

        let settle_ms =
            u32::try_from(settle.as_millis()).map_err(|_| Error::SettleTooLong { settle })?;
        let request = FromEngine::StopTheWorld { stop, settle_ms };
        match self.ask(&request, EXPECTED)? {
            FromAgent::WorldStopped { held, native } => Ok(WorldStopped { held, native }),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// send one request and wait for the answer to it
    ///
    /// a logpoint's record, a rebinding and another thread's stop can all be in
    /// the socket ahead of an answer, because none of them waits for anything.
    /// each is kept for the next wait rather than dropped: a log record the
    /// client never sees is a line of the program's history that silently went
    /// missing, and a stop that went missing is a thread the client thinks is
    /// running
    fn ask(&mut self, request: &FromEngine, expected: &'static str) -> Result<FromAgent> {
        if self.held.is_empty() {
            return Err(Error::NotStopped { wanted: expected });
        }
        self.session.send(request)?;

        loop {
            match self.session.next_event()? {
                Some(FromAgent::Logged { record }) => self.pending_logs.push(record),
                Some(FromAgent::BreakpointsResolved { resolved })
                    if !matches!(request, FromEngine::SetBreakpoints { .. }) =>
                {
                    self.pending_rebinds.extend(resolved);
                }
                Some(FromAgent::Stopped { stop }) => self.held.push(stop),
                Some(FromAgent::Refused { reason }) => return Err(Error::Refused { reason }),
                Some(answer) => return Ok(answer),
                None => return Err(Error::AgentGone { expected }),
            }
        }
    }

    /// let named threads go, without waiting for what they do next
    ///
    /// naming a thread that is not held is refused rather than ignored: the
    /// client would otherwise believe it had released something it had not, and
    /// the next thing it waited for would never come
    pub fn resume(&mut self, threads: &[u64]) -> Result<Vec<u64>> {
        self.let_go(Which::Named {
            threads: threads.to_vec(),
        })
    }

    /// let every held thread go, without waiting for what they do next
    pub fn resume_all(&mut self) -> Result<Vec<u64>> {
        self.let_go(Which::All)
    }

    fn let_go(&mut self, which: Which) -> Result<Vec<u64>> {
        const EXPECTED: &str = "the threads to be resumed";

        match self.ask(&FromEngine::Resume { which }, EXPECTED)? {
            FromAgent::Resumed { threads } => {
                self.held.retain(|stop| !threads.contains(&stop.thread));
                Ok(threads)
            }
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// wait for the next thing the debuggee does
    ///
    /// log records go to `on_log` as they arrive rather than into the result.
    /// there is no bound on how many a logpoint produces, and a debugger that
    /// accumulated a million of them before saying anything would be holding
    /// the program's history hostage to its own memory
    pub fn wait(&mut self, mut on_log: impl FnMut(LogRecord)) -> Result<Running> {
        for record in self.pending_logs.drain(..) {
            on_log(record);
        }
        let mut rebound = std::mem::take(&mut self.pending_rebinds);

        loop {
            match self.session.next_event()? {
                Some(FromAgent::Stopped { stop }) => {
                    self.held.push(stop.clone());
                    return Ok(Running::Stopped { stop, rebound });
                }
                Some(FromAgent::Finishing { held }) => {
                    return Ok(Running::Finishing {
                        threads: held,
                        rebound,
                    });
                }
                Some(FromAgent::BreakpointsResolved { resolved }) => rebound.extend(resolved),
                Some(FromAgent::Logged { record }) => on_log(record),
                Some(other) => {
                    return Err(Error::UnexpectedEvent {
                        event: format!("{other:?}"),
                        expected: "the debuggee to stop or exit",
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

    /// let every held thread go and wait for what the debuggee does next
    ///
    /// the whole-program "continue". it resumes everything held rather than one
    /// thread, and what it waits for is the program, not a particular thread
    pub fn run(&mut self, mut on_log: impl FnMut(LogRecord)) -> Result<Running> {
        self.resume_all()?;
        self.wait(&mut on_log)
    }
}

/// one held thread's stack
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    /// the frames, the one that stopped first
    pub frames: Vec<Frame>,
    /// how deep the stack is, which is more than `frames` when fewer were asked
    /// for
    pub depth: usize,
    /// how the program was moving while this was taken
    pub mode: Mode,
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
    /// how the program was moving while this was taken
    pub mode: Mode,
}

/// what every thread of the debuggee was doing, as a sample
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Threads {
    /// one entry per thread the interpreter knows about
    pub threads: Vec<ThreadState>,
    /// how far apart the two samples were taken
    pub settle: Duration,
    /// how the program was moving while this was taken
    pub mode: Mode,
}

impl Threads {
    /// what one thread was doing, when the census saw it
    pub fn get(&self, thread: u64) -> Option<&ThreadState> {
        self.threads.iter().find(|state| state.thread == thread)
    }
}

/// what stopping the world managed to stop
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldStopped {
    /// the threads that are held
    pub held: Vec<u64>,
    /// the threads parked in a C call, which are **running**
    ///
    /// nothing available here can stop one: it has released the GIL and
    /// executes no python, so it reaches no monitoring event. an answer taken
    /// with this list non-empty is not a whole-program snapshot, and says so
    pub native: Vec<u64>,
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

    let stop = match session.expect_stop() {
        Ok(stop) => stop,
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
        held: vec![stop],
        child,
        session,
        pending_logs: Vec::new(),
        pending_rebinds: Vec::new(),
        _staged: staged,
    }))
}
