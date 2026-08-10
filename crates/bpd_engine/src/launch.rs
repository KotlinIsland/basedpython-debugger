//! starting a debuggee with its agent already attached, and answering the
//! session's requests against it
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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bpd_core::python::Capabilities;
use bpd_core::{
    Detail, Evaluated, ExceptionBreakpoints, FrameId, LogRecord, Reporting, Request, Resolved,
    Response, Running, Scope, SourceBreakpoint, Stack, StepKind, Stop, Threads, Variables, Which,
    WorldStopped,
};
use bpd_protocol::env;
use bpd_protocol::message::{FromAgent, FromEngine};

use crate::{Error, Interrupt, Listener, Result, Session, agent};

/// a [`Reporting`] sink that can only take log records
///
/// what the ergonomic methods on [`Debuggee`] hand to [`Debuggee::dispatch`].
/// a pause acknowledgement cannot reach one: [`Debuggee::pause`] waits for its
/// own, and the other way to arm one is [`Interrupt::deliver`], whose
/// acknowledgement arrives at whatever sink the wait was given
struct OnlyLogs<F>(F);

impl<F: FnMut(LogRecord)> Reporting for OnlyLogs<F> {
    fn logged(&mut self, record: LogRecord) {
        (self.0)(record);
    }

    fn pausing(&mut self, running: Vec<u64>) {
        unreachable!(
            "a pause was acknowledged to a caller that cannot have armed one, \
             naming {running:?} as running python"
        )
    }
}

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
    /// shared with every [`Interrupt`], which can end a program the session is
    /// waiting on
    child: Arc<Mutex<Child>>,
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
    pub fn requests_sent(&self) -> u64 {
        self.session.requests_sent()
    }

    /// a handle that reaches this debuggee while the session is waiting on it
    ///
    /// the only way to arm a pause or end a program from a front end that is
    /// blocked in [`Request::Wait`], which is what an event driven front end
    /// spends most of a session doing
    pub fn interrupt(&self) -> Interrupt {
        Interrupt::new(self.session.writer(), Arc::clone(&self.child))
    }

    /// answer one [`Request`] against this debuggee
    ///
    /// the capability surface is the enum, and this is the one place it is
    /// answered. the ergonomic methods below build a request and come through
    /// here, so there is a single implementation of every capability rather
    /// than one per front end — which is what makes the adapters translations
    /// rather than second opinions
    ///
    /// the match is exhaustive and has no catch-all arm. a capability added to
    /// [`Request`] is a compile error here rather than a request nothing
    /// answers
    ///
    /// `reporting` is a parameter of the call rather than a field of the
    /// request because what it takes is not an answer to anything: a logpoint
    /// fires while the program runs, and so does the acknowledgement of a pause
    /// armed on an [`Interrupt`]
    pub fn dispatch(
        &mut self,
        request: Request,
        reporting: &mut dyn Reporting,
    ) -> Result<Response> {
        match request {
            Request::SetBreakpoints { breakpoints } => Ok(Response::BreakpointsResolved {
                resolved: self.resolve_breakpoints(breakpoints, reporting)?,
            }),
            Request::SetExceptionBreakpoints { raised, uncaught } => Ok(
                Response::ExceptionBreakpoints(self.arm_exceptions(raised, uncaught, reporting)?),
            ),
            Request::Run { deadline } => {
                // the deadline bounds the wait rather than the whole request:
                // the resume is answered on a thread that is already held, so
                // it cannot be what a program with nothing to say delays
                self.let_go(Which::All, reporting)?;
                Ok(Response::Ran(self.wait_for(deadline, reporting)?))
            }
            Request::Wait { deadline } => Ok(Response::Ran(self.wait_for(deadline, reporting)?)),
            Request::Resume { which } => Ok(Response::Resumed {
                threads: self.let_go(which, reporting)?,
            }),
            Request::Step { stop, kind } => Ok(Response::Resumed {
                threads: self.step_thread(stop, kind, reporting)?,
            }),
            Request::Pause => Ok(Response::Pausing {
                running: self.arm_pause(reporting)?,
            }),
            Request::Threads { settle } => Ok(Response::Threads(self.census(settle, reporting)?)),
            Request::StopTheWorld { stop, settle } => Ok(Response::WorldStopped(
                self.stop_world(stop, settle, reporting)?,
            )),
            Request::Stack { stop, top } => {
                Ok(Response::Stack(self.walk_stack(stop, top, reporting)?))
            }
            Request::Variables {
                frame,
                scope,
                detail,
            } => Ok(Response::Variables(
                self.read_scope(frame, scope, detail, reporting)?,
            )),
            Request::Evaluate {
                frame,
                expression,
                detail,
            } => Ok(Response::Evaluated(self.evaluate_in(
                frame,
                &expression,
                detail,
                reporting,
            )?)),
            Request::SetVariable {
                frame,
                scope,
                name,
                value,
                detail,
            } => Ok(Response::Evaluated(self.write_variable(
                frame, scope, &name, &value, detail, reporting,
            )?)),
        }
    }

    /// dispatch a request that cannot deliver a log record
    ///
    /// a logpoint that fires while one of these is in flight is kept in
    /// `pending_logs` for the next [`Self::wait`] rather than handed over here,
    /// so there is nothing for a sink to receive
    fn ask_for(&mut self, request: Request) -> Result<Response> {
        self.dispatch(request, &mut OnlyLogs(drop))
    }

    /// the one stop that is held, for a request that is about one thread
    ///
    /// the rule itself is [`bpd_core::only_stop`], because every front end has
    /// to apply it and two of them applying their own would make a request that
    /// names no stop mean two things
    fn only(&self, wanted: &'static str) -> Result<u64> {
        Ok(bpd_core::only_stop(&self.held, wanted)?)
    }

    /// replace the whole breakpoint set, and say how every one of them resolved
    ///
    /// only while a thread is held. the agent answers on a thread it is holding
    /// and nowhere else — asking a running program to bind something would be a
    /// request that is answered whenever it next happens to stop, which is not
    /// an answer
    pub fn set_breakpoints(&mut self, breakpoints: Vec<SourceBreakpoint>) -> Result<Vec<Resolved>> {
        match self.ask_for(Request::SetBreakpoints { breakpoints })? {
            Response::BreakpointsResolved { resolved } => Ok(resolved),
            other => unreachable!("a breakpoint set was answered with {other:?}"),
        }
    }

    fn resolve_breakpoints(
        &mut self,
        breakpoints: Vec<SourceBreakpoint>,
        reporting: &mut dyn Reporting,
    ) -> Result<Vec<Resolved>> {
        const EXPECTED: &str = "the breakpoints to resolve";

        // every report about a breakpoint, and every stop it causes, names it by
        // this id. two breakpoints sharing one would give the client a single
        // answer for two questions, and it would have no way to tell which
        let mut seen = std::collections::BTreeSet::new();
        for breakpoint in &breakpoints {
            if !seen.insert(breakpoint.id) {
                return Err(bpd_core::Error::DuplicateBreakpointId { id: breakpoint.id }.into());
            }
        }

        let request = FromEngine::SetBreakpoints { breakpoints };
        match self.ask(&request, EXPECTED, reporting)? {
            FromAgent::BreakpointsResolved { resolved } => Ok(resolved),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// stop where an exception is raised, or where one leaves the program
    ///
    /// the whole setting, not a delta, for the reason the breakpoint set is.
    /// the answer is what is armed now, read back off the agent rather than
    /// assumed from what was asked for
    pub fn set_exception_breakpoints(
        &mut self,
        raised: bool,
        uncaught: bool,
    ) -> Result<ExceptionBreakpoints> {
        match self.ask_for(Request::SetExceptionBreakpoints { raised, uncaught })? {
            Response::ExceptionBreakpoints(armed) => Ok(armed),
            other => unreachable!("an exception breakpoint set was answered with {other:?}"),
        }
    }

    fn arm_exceptions(
        &mut self,
        raised: bool,
        uncaught: bool,
        reporting: &mut dyn Reporting,
    ) -> Result<ExceptionBreakpoints> {
        const EXPECTED: &str = "the exception breakpoints to be set";

        let request = FromEngine::SetExceptionBreakpoints { raised, uncaught };
        match self.ask(&request, EXPECTED, reporting)? {
            FromAgent::ExceptionBreakpointsSet { raised, uncaught } => {
                Ok(ExceptionBreakpoints { raised, uncaught })
            }
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// let one held thread go with a step armed on it
    ///
    /// a step is a resume of that one thread — every other thread in the
    /// program goes on running while it happens, and this returns as soon as
    /// the thread has been let go. where it landed arrives from [`Self::wait`],
    /// as a stop of its own
    pub fn step(&mut self, stop: u64, kind: StepKind) -> Result<Vec<u64>> {
        match self.ask_for(Request::Step { stop, kind })? {
            Response::Resumed { threads } => Ok(threads),
            other => unreachable!("a step was answered with {other:?}"),
        }
    }

    /// step the only held thread
    pub fn the_step(&mut self, kind: StepKind) -> Result<Vec<u64>> {
        let stop = self.only("a step")?;
        self.step(stop, kind)
    }

    fn step_thread(
        &mut self,
        stop: u64,
        kind: StepKind,
        reporting: &mut dyn Reporting,
    ) -> Result<Vec<u64>> {
        const EXPECTED: &str = "the thread to be stepped";

        match self.ask(&FromEngine::Step { stop, kind }, EXPECTED, reporting)? {
            FromAgent::Resumed { threads } => {
                self.held.retain(|stop| !threads.contains(&stop.thread));
                Ok(threads)
            }
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// hold the next thread of the debuggee that reaches a line
    ///
    /// the one request that is made to a program with **nothing held**, and the
    /// one that cannot say in advance which thread it will get: nothing in
    /// cpython suspends a thread, so what this does is arm `LINE` for the whole
    /// program and hold whichever thread arrives first
    ///
    /// what comes back is the threads that were running python when it was
    /// armed. an empty list means the pause is armed and nothing is going to
    /// reach it until some thread runs python again — every one of them is
    /// parked in a C call, where no monitoring event exists
    pub fn pause(&mut self) -> Result<Vec<u64>> {
        match self.ask_for(Request::Pause)? {
            Response::Pausing { running } => Ok(running),
            other => unreachable!("a pause was answered with {other:?}"),
        }
    }

    fn arm_pause(&mut self, reporting: &mut dyn Reporting) -> Result<Vec<u64>> {
        const EXPECTED: &str = "a pause to be armed";

        match self.send_and_wait(&FromEngine::Pause, EXPECTED, reporting)? {
            FromAgent::Pausing { running } => Ok(running),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// walk one held thread's frame chain
    ///
    /// `top` bounds how many frames come back, counting from the one that
    /// stopped. the answer says how deep the stack really is either way
    pub fn stack(&mut self, stop: u64, top: Option<u32>) -> Result<Stack> {
        match self.ask_for(Request::Stack { stop, top })? {
            Response::Stack(stack) => Ok(stack),
            other => unreachable!("a stack walk was answered with {other:?}"),
        }
    }

    /// walk the frame chain of the only held thread
    pub fn the_stack(&mut self, top: Option<u32>) -> Result<Stack> {
        let stop = self.only("the stack")?;
        self.stack(stop, top)
    }

    fn walk_stack(
        &mut self,
        stop: u64,
        top: Option<u32>,
        reporting: &mut dyn Reporting,
    ) -> Result<Stack> {
        const EXPECTED: &str = "the stack";

        match self.ask(&FromEngine::Stack { stop, top }, EXPECTED, reporting)? {
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

    /// read one scope of one frame
    pub fn variables(&mut self, frame: FrameId, scope: Scope, detail: Detail) -> Result<Variables> {
        match self.ask_for(Request::Variables {
            frame,
            scope,
            detail,
        })? {
            Response::Variables(variables) => Ok(variables),
            other => unreachable!("a scope read was answered with {other:?}"),
        }
    }

    fn read_scope(
        &mut self,
        frame: FrameId,
        scope: Scope,
        detail: Detail,
        reporting: &mut dyn Reporting,
    ) -> Result<Variables> {
        const EXPECTED: &str = "the variables of a scope";

        let request = FromEngine::Variables {
            frame,
            scope,
            detail,
        };
        match self.ask(&request, EXPECTED, reporting)? {
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
        match self.ask_for(Request::Evaluate {
            frame,
            expression: expression.to_string(),
            detail,
        })? {
            Response::Evaluated(result) => Ok(result),
            other => unreachable!("an evaluation was answered with {other:?}"),
        }
    }

    fn evaluate_in(
        &mut self,
        frame: FrameId,
        expression: &str,
        detail: Detail,
        reporting: &mut dyn Reporting,
    ) -> Result<Evaluated> {
        const EXPECTED: &str = "the value of an expression";

        let request = FromEngine::Evaluate {
            frame,
            expression: expression.to_string(),
            detail,
        };
        match self.ask(&request, EXPECTED, reporting)? {
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
        match self.ask_for(Request::SetVariable {
            frame,
            scope,
            name: name.to_string(),
            value: value.to_string(),
            detail,
        })? {
            Response::Evaluated(result) => Ok(result),
            other => unreachable!("a variable write was answered with {other:?}"),
        }
    }

    fn write_variable(
        &mut self,
        frame: FrameId,
        scope: Scope,
        name: &str,
        value: &str,
        detail: Detail,
        reporting: &mut dyn Reporting,
    ) -> Result<Evaluated> {
        const EXPECTED: &str = "the value a variable was set to";

        let request = FromEngine::SetVariable {
            frame,
            scope,
            name: name.to_string(),
            value: value.to_string(),
            detail,
        };
        match self.ask(&request, EXPECTED, reporting)? {
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
        match self.ask_for(Request::Threads { settle })? {
            Response::Threads(threads) => Ok(threads),
            other => unreachable!("a thread census was answered with {other:?}"),
        }
    }

    fn census(&mut self, settle: Duration, reporting: &mut dyn Reporting) -> Result<Threads> {
        const EXPECTED: &str = "what the threads are doing";

        let settle_ms =
            u32::try_from(settle.as_millis()).map_err(|_| Error::SettleTooLong { settle })?;
        match self.ask(&FromEngine::Threads { settle_ms }, EXPECTED, reporting)? {
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
        match self.ask_for(Request::StopTheWorld { stop, settle })? {
            Response::WorldStopped(stopped) => Ok(stopped),
            other => unreachable!("stopping the world was answered with {other:?}"),
        }
    }

    fn stop_world(
        &mut self,
        stop: u64,
        settle: Duration,
        reporting: &mut dyn Reporting,
    ) -> Result<WorldStopped> {
        const EXPECTED: &str = "the world to stop";

        let settle_ms =
            u32::try_from(settle.as_millis()).map_err(|_| Error::SettleTooLong { settle })?;
        let request = FromEngine::StopTheWorld { stop, settle_ms };
        match self.ask(&request, EXPECTED, reporting)? {
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
    fn ask(
        &mut self,
        request: &FromEngine,
        expected: &'static str,
        reporting: &mut dyn Reporting,
    ) -> Result<FromAgent> {
        if self.held.is_empty() {
            return Err(bpd_core::Error::NotStopped { wanted: expected }.into());
        }
        self.send_and_wait(request, expected, reporting)
    }

    /// send one request and wait for the answer, of a program that may be
    /// running
    ///
    /// only a pause reaches this without a held thread. everything else is
    /// answered on a thread the agent is already holding, and asks through
    /// [`Self::ask`] so that a request made to a running program is refused
    /// here rather than waited on for ever
    fn send_and_wait(
        &mut self,
        request: &FromEngine,
        expected: &'static str,
        reporting: &mut dyn Reporting,
    ) -> Result<FromAgent> {
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
                // an interrupt can arm a pause at any moment, including while
                // an answer is on its way. it is not the answer to this
                // request, and it is not something to drop either: an empty
                // `running` is how a client learns that nothing is coming
                Some(FromAgent::Pausing { running }) if !matches!(request, FromEngine::Pause) => {
                    reporting.pausing(running);
                }
                Some(FromAgent::Refused { reason }) => {
                    return Err(bpd_core::Error::Refused { reason }.into());
                }
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
        self.resumed(Which::Named {
            threads: threads.to_vec(),
        })
    }

    /// let every held thread go, without waiting for what they do next
    pub fn resume_all(&mut self) -> Result<Vec<u64>> {
        self.resumed(Which::All)
    }

    fn resumed(&mut self, which: Which) -> Result<Vec<u64>> {
        match self.ask_for(Request::Resume { which })? {
            Response::Resumed { threads } => Ok(threads),
            other => unreachable!("a resume was answered with {other:?}"),
        }
    }

    fn let_go(&mut self, which: Which, reporting: &mut dyn Reporting) -> Result<Vec<u64>> {
        const EXPECTED: &str = "the threads to be resumed";

        match self.ask(&FromEngine::Resume { which }, EXPECTED, reporting)? {
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
    pub fn wait(&mut self, on_log: impl FnMut(LogRecord)) -> Result<Running> {
        match self.dispatch(Request::Wait { deadline: None }, &mut OnlyLogs(on_log))? {
            Response::Ran(running) => Ok(running),
            other => unreachable!("a wait was answered with {other:?}"),
        }
    }

    /// let every held thread go and wait for what the debuggee does next
    ///
    /// the whole-program "continue". it resumes everything held rather than one
    /// thread, and what it waits for is the program, not a particular thread
    pub fn run(&mut self, on_log: impl FnMut(LogRecord)) -> Result<Running> {
        match self.dispatch(Request::Run { deadline: None }, &mut OnlyLogs(on_log))? {
            Response::Ran(running) => Ok(running),
            other => unreachable!("a run was answered with {other:?}"),
        }
    }

    /// wait for the next thing the debuggee does, giving up after `deadline`
    ///
    /// giving up leaves nothing outstanding on the wire. a wait is not a request
    /// the agent is answering — it is the engine reading the connection — so
    /// stopping reading it costs nothing and whatever the program says next is
    /// still there for the next wait
    fn wait_for(
        &mut self,
        deadline: Option<Duration>,
        reporting: &mut dyn Reporting,
    ) -> Result<Running> {
        for record in self.pending_logs.drain(..) {
            reporting.logged(record);
        }
        let mut rebound = std::mem::take(&mut self.pending_rebinds);

        let started = Instant::now();
        let until = deadline.map(|deadline| started + deadline);

        loop {
            if let Some(until) = until
                && !self.session.readable_by(until)?
            {
                // the rebindings gathered so far go out with the timeout. a
                // breakpoint that bound while the program ran is a fact about
                // the program, and losing it because a deadline passed would
                // leave a client believing a breakpoint is still unbound
                return Ok(Running::StillRunning {
                    waited: started.elapsed(),
                    rebound,
                });
            }

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
                Some(FromAgent::Logged { record }) => reporting.logged(record),
                // the acknowledgement of a pause armed on an `Interrupt`. it
                // arrives here because here is where the reading end is, and
                // its `running` is what says whether a stop is coming at all
                Some(FromAgent::Pausing { running }) => reporting.pausing(running),
                Some(other) => {
                    return Err(Error::UnexpectedEvent {
                        event: format!("{other:?}"),
                        expected: "the debuggee to stop or exit",
                    });
                }
                None => {
                    let status = self
                        .child
                        .lock()
                        .expect(
                            "nothing panics holding the debuggee: every path \
                             through it is a kill or a wait",
                        )
                        .wait()
                        .map_err(|source| Error::Spawn {
                            interpreter: PathBuf::from("the debuggee"),
                            source,
                        })?;
                    return Ok(Running::Exited { status, rebound });
                }
            }
        }
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
/// debuggee that has run none of the program yet. the debuggee's own stdout and
/// stderr are bpd's, untouched — which is what makes a run under bpd
/// indistinguishable from a bare one
pub fn launch(interpreter: &Capabilities, script: &Path, args: &[OsString]) -> Result<Launched> {
    start(interpreter, script, args, None)
}

/// launch with the debuggee's own output in pipes rather than on bpd's streams
///
/// what a front end whose own stdout is a **protocol** needs: one `print` from
/// the program in the middle of a message and every message after it is
/// unreadable
///
/// `on_spawn` is handed the two pipes the moment the process exists and before
/// anything waits on it. that ordering is the whole of the contract: a pipe
/// nobody is reading fills up, and a process whose pipe is full stops — so a
/// launcher that waited first and handed the pipes over afterwards would hang
/// on any program that says more than a pipe buffer holds
pub fn launch_piped(
    interpreter: &Capabilities,
    script: &Path,
    args: &[OsString],
    on_spawn: impl FnOnce(std::process::ChildStdout, std::process::ChildStderr) + 'static,
) -> Result<Launched> {
    start(interpreter, script, args, Some(Box::new(on_spawn)))
}

/// what to do with the debuggee's own output the moment it exists
type OnSpawn = Box<dyn FnOnce(std::process::ChildStdout, std::process::ChildStderr)>;

fn start(
    interpreter: &Capabilities,
    script: &Path,
    args: &[OsString],
    piped: Option<OnSpawn>,
) -> Result<Launched> {
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

    if piped.is_some() {
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
    }

    let mut child = command.spawn().map_err(|source| Error::Spawn {
        interpreter: interpreter.executable.clone(),
        source,
    })?;

    if let Some(hand_over) = piped {
        let stdout = child
            .stdout
            .take()
            .expect("the command asked for a stdout pipe and cargo has not taken it");
        let stderr = child
            .stderr
            .take()
            .expect("the command asked for a stderr pipe and cargo has not taken it");
        hand_over(stdout, stderr);
    }

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
        child: Arc::new(Mutex::new(child)),
        session,
        pending_logs: Vec::new(),
        pending_rebinds: Vec::new(),
        _staged: staged,
    }))
}
