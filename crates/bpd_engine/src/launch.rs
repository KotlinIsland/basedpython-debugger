//! starting a debuggee with its agent already attached, and answering the
//! session's requests against it
//!
//! the debuggee is entered through `python -c "import bpd_agent; bpd_agent.main()"`.
//! everything after that — repairing `sys.argv` and `sys.path[0]`, building
//! `__main__`, running the program, reporting its exit — happens in the agent,
//! in rust. a python bootstrap file would be a second place for launch
//! semantics to be subtly wrong, and would leave its own name in `sys.modules`
//!
//! the `-c` form is what makes this possible and is also its one hazard: it
//! sets `sys.path[0]` to the empty string and `sys.argv[0]` to `-c`, which is
//! what a command wants and is wrong for the other two. the agent repairs what
//! the requested form needs before any user code runs, and
//! `crates/bpd/tests/launch_parity.rs` compares the result against a bare
//! interpreter of each form rather than trusting that
//!
//! which form is requested reaches the agent in the environment, beside the
//! target, because `-c` leaves no room for anything structured

mod query;
mod script;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bpd_core::python::Capabilities;
use bpd_core::{
    Addressed, Blindspot, Detail, Difference, Evaluated, ExceptionBreakpoints, Exit, Forwarded,
    FrameId, Joined, Jumped, LogRecord, Replaced, Reporting, Request, Resolved, Response, Running,
    Scope, Script, SessionId, Snapshot, SnapshotId, SourceBreakpoint, Spawn, Stack, StateQuery,
    StepKind, Stop, TemplateContext, Threads, Transcript, Variables, Which, WorldStopped,
};
use bpd_protocol::env;
use bpd_protocol::message::{FromAgent, FromEngine};

use crate::{Error, Interrupt, Listener, Result, Session, agent, mapping};

/// the [`Reporting`] sink for a request whose answer is what is waited for
///
/// what [`Debuggee::ask_for`] hands to [`Debuggee::dispatch`]. a log record or a
/// child that arrives while one of those is in flight is kept by
/// [`Debuggee::send_and_wait`] for the next wait, so almost nothing reaches
/// here — the exception is a debug script, whose steps do their own waiting
///
/// a child is **collected** rather than dropped, and the caller puts it back on
/// the debuggee's queue. a program that starts a child while a script is
/// running has started a child, and a report that went missing because of when
/// it arrived would be the debugger losing a fact about the program
struct Aside {
    /// the children seen while the request was in flight
    spawned: Vec<Spawn>,
    /// the blind spots announced while it was
    blind: Vec<Blindspot>,
    /// the sessions that joined while it was
    joined: Vec<SessionId>,
}

impl Aside {
    const fn new() -> Self {
        Self {
            spawned: Vec::new(),
            blind: Vec::new(),
            joined: Vec::new(),
        }
    }
}

impl Reporting for Aside {
    fn logged(&mut self, _record: LogRecord) {
        // a debug script's own wait drains the queue into whatever sink it was
        // given, and this is that sink. the records are the script's to report
        // and it reports them in its transcript
    }

    fn pausing(&mut self, running: Vec<u64>) {
        unreachable!(
            "a pause was acknowledged to a caller that cannot have armed one, \
             naming {running:?} as running python"
        )
    }

    fn spawned(&mut self, child: Spawn) {
        self.spawned.push(child);
    }

    fn blind_to(&mut self, blindspot: Blindspot) {
        self.blind.push(blindspot);
    }

    fn attached(&mut self, session: SessionId) {
        self.joined.push(session);
    }
}

/// how long a wait sits on one connection before it looks at the listener
///
/// the slice bounds how long a connection waits in the backlog and nothing
/// else. it does not delay anything the program says: the wait is a peek with a
/// deadline, and a peek returns the instant a byte arrives
const LISTEN_SLICE: Duration = Duration::from_millis(5);

/// the entry point the interpreter is given
///
/// deliberately the shortest thing that can work: every decision it could
/// contain belongs in the agent, where it is rust and is tested
const BOOTSTRAP: &str = "import bpd_agent; bpd_agent.main()";

/// one attached agent, and everything that is about that one process
///
/// a stop holds **one thread**, so more than one can be held at a time and a
/// request that is about a thread says which one. the ones that are about the
/// process — the breakpoint set, the thread census — do not
///
/// every field here is per-process and none of it is shareable. two debuggees
/// have two breakpoint sets, two stop numbering spaces and two exits, so what
/// [`Debuggee`] holds is a collection of these rather than one of them with
/// extras bolted on
#[derive(Debug)]
struct Attached {
    /// shared with every [`Interrupt`], which can end a program the session is
    /// waiting on
    ///
    /// `None` when bpd did not start this process. a session that arrived on
    /// the retained listener is a program bpd is **not** the parent of: there
    /// is nothing to signal, and nothing to reap for an exit status. every
    /// place that would otherwise assume a child says what it does instead
    child: Option<Arc<Mutex<Child>>>,
    session: Session,
    /// the stops held now, in the order the agent reported them
    held: Vec<Stop>,
    /// the breakpoint set the client last asked for
    ///
    /// [`Request::SetBreakpoints`] replaces the whole set, so this is what is
    /// armed. it is kept because a `run_to` inside a debug script has to arm a
    /// breakpoint of its own **and put the set back**, and there is no delta
    /// request to do either with
    armed: Vec<SourceBreakpoint>,
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
    /// children that were started while the engine was waiting for an answer
    ///
    /// a program starts a child without asking the debugger, so one can be in
    /// the socket ahead of the reply to a request. kept for the next wait for
    /// the reason a log record is: a session pointed at a process that does
    /// none of the work is the thing this report exists to prevent, and one
    /// that went missing because of when it arrived would prevent nothing
    pending_spawns: Vec<Spawn>,
    /// blind spots announced while the engine was waiting for an answer
    ///
    /// kept for the reason the others are. this one is the least droppable of
    /// the three: it is the message that stops a silence being read as "there
    /// was no child"
    pending_blind: Vec<Blindspot>,
    /// sessions that joined the debuggee while the engine was waiting
    ///
    /// a debugged child connects while the program runs, so one can arrive
    /// while a request is in flight. it is the least droppable of all of them:
    /// a child that joined is a process **held** at the line that forked, and a
    /// front end that never learns of it has a stopped program nothing can
    /// resume
    pending_joined: Vec<SessionId>,
    /// the verified source map this session's breakpoints go through
    ///
    /// `None` is the ordinary case — a program written in python has nothing to
    /// map. it is per session rather than on the debuggee because a session is
    /// what holds a breakpoint set, and the two have to be replaced together
    map: Option<Arc<bpd_core::SourceMap>>,
    /// whether this session's agent has been handed the map's tables
    ///
    /// the agent reports every location of the build as the `.by` line it came
    /// from, and it can only do that once it has them. a session that joined —
    /// an `exec`'d child, which is a fresh interpreter — has not been sent them
    /// with the launch, so this is what stops a second send and what makes the
    /// first one happen before the child runs anything
    mapped: bool,
    /// where each translated breakpoint went, by the id the client gave it
    ///
    /// the record [`mapping::restore`] reads to put an answer back into `.by`
    /// terms. replaced whole every time the set is, because a breakpoint set is
    /// replaced whole and a stale entry here would map an answer through a
    /// translation nobody made
    translated: std::collections::BTreeMap<u32, mapping::Translated>,
    /// every state a query has read, under the id it was given out as
    ///
    /// nothing evicts one. a snapshot is a reading that was already taken rather
    /// than a promise to take one, so it does not go stale when the program runs
    /// on — and an id that resolved earlier in a session and not later would be
    /// the stale handle problem this exists to avoid
    snapshots: Vec<Snapshot>,
}

/// every debuggee this engine holds, and the door the next one comes through
///
/// one session is the ordinary case and two is what a program that starts
/// another one produces. a request that names none is for the only session
/// there is — refused rather than picked when there is more than one, which is
/// [`bpd_core::only_session`]'s rule and not this type's
#[derive(Debug)]
pub struct Debuggee {
    /// the verified source map every session of this debuggee maps through
    ///
    /// held here as well as on each [`Attached`] because a session that joins
    /// later — a debugged fork — is running the same build out of the same
    /// directory, and a child whose `.by` breakpoints stopped resolving would be
    /// a gap nobody asked for
    map: Option<Arc<bpd_core::SourceMap>>,
    /// where an agent connects, kept open for the life of the debuggee
    ///
    /// it used to be a local of [`start`] and closed when that returned, which
    /// made the first agent the only one there could ever be. keeping it open
    /// is what makes a second connection possible at all
    ///
    /// a connection arriving on it is **not** assumed to be anything. the
    /// session token is the only evidence there is, the handshake is where it
    /// is checked, and a peer that cannot present it is closed and not counted
    listener: Listener,
    /// the debuggees, in the order their agents attached
    attached: Vec<Attached>,
    /// what is carrying this program's output somewhere, if anything is
    ///
    /// held per **debuggee** and not per session, because the pipe is: a
    /// debugged fork inherits the write end its parent was given, so what a
    /// child prints comes out of the same descriptor and is read by the same
    /// thread. whichever session is reported over, it is this that says whether
    /// what was written has been carried
    ///
    /// `None` when there is no pipe of bpd's at all — a launch that left the
    /// streams inherited, or a program bpd did not start
    forwarders: Option<Forwarders>,
}

impl Debuggee {
    /// the stops held right now, across every session, in the order they
    /// arrived
    ///
    /// more than one is ordinary: a stop holds one thread, so a second thread
    /// reaching a breakpoint while a first is held reports its own straight
    /// away rather than waiting for the first to be resumed. every one of them
    /// carries the session it arrived on, because two agents both count their
    /// stops from one
    pub fn held(&self) -> Vec<Stop> {
        self.attached
            .iter()
            .flat_map(|attached| attached.held.iter().cloned())
            .collect()
    }

    /// every session this debuggee holds, in the order their agents attached
    ///
    /// one is the ordinary case. every stop reports which session it is of, and
    /// a request may name one — see [`bpd_core::Addressed`]
    pub fn sessions(&self) -> Vec<SessionId> {
        self.attached
            .iter()
            .map(|attached| attached.session.id())
            .collect()
    }

    /// every session this debuggee holds, with what a front end has to know
    /// about each
    ///
    /// what makes a second session **learnable** rather than merely present.
    /// [`Self::sessions`] is the ids alone, which is what routing needs; this is
    /// what a client is shown — including whether bpd started the process, which
    /// decides whether it can be terminated or its exit read
    pub fn joined(&self) -> Vec<Joined> {
        self.attached
            .iter()
            .map(|attached| Joined {
                session: attached.session.id(),
                ours: attached.child.is_some(),
                held: attached.held.clone(),
                exit: attached.exited(),
            })
            .collect()
    }

    /// how one session's program ended, or `None` while it is still there
    ///
    /// naming none means the only session there is, which is every request's
    /// rule. it is `None` with more than one open and none named, for the reason
    /// [`Self::exited`] is
    pub fn exit_of(&self, session: Option<SessionId>) -> Option<Exit> {
        let id = bpd_core::only_session(&self.sessions(), session, "the exit").ok()?;
        self.at(id).exited()
    }

    /// how many requests the engine has sent the only session's agent
    ///
    /// the agent answers on a thread it is holding, so this is also the number
    /// of times the debuggee has waited for the debugger
    ///
    /// # errors
    ///
    /// when there is more than one session, because the number is a statement
    /// about one connection
    pub fn requests_sent(&self) -> Result<u64> {
        Ok(self
            .the_one("how many requests were sent")?
            .session
            .requests_sent())
    }

    /// where another agent connects to join this debuggee, and with what
    ///
    /// the listener the first agent attached on, kept open. a second session
    /// uses the **parent's** listener and token rather than one of its own:
    /// there is one lifetime to get right instead of two, and a peer that
    /// reached this port with this token is as authenticated as the first one
    /// was
    pub const fn listener(&self) -> &Listener {
        &self.listener
    }

    /// a handle that reaches one session's program while it is waiting on it
    ///
    /// the only way to arm a pause or end a program from a front end that is
    /// blocked in [`Request::Wait`], which is what an event driven front end
    /// spends most of a session doing
    ///
    /// naming none means the only session there is, which is
    /// [`bpd_core::Addressed`]'s rule and the same one every request follows
    ///
    /// # errors
    ///
    /// when the id names no session of this debuggee, and when it names none
    /// and there is more than one. an interrupt reaches one process, and which
    /// one is not something to guess at
    pub fn interrupt(&self, session: Option<SessionId>) -> Result<Interrupt> {
        let id = bpd_core::only_session(&self.sessions(), session, "an interrupt")?;
        Ok(self.at(id).interrupt())
    }

    /// how the only session's program ended, or `None` while it is still there
    ///
    /// a front end needs this to tell the two shapes of "nothing is held" apart
    /// — a program that is running and has to be stopped, and one that is over
    /// and cannot be
    ///
    /// `None` when there is more than one session as well, because then there
    /// is no "the program" for it to be about. every request that names no
    /// session is refused by [`bpd_core::only_session`] before this is
    /// consulted, so what it improves is a message rather than a decision
    pub fn exited(&self) -> Option<Exit> {
        self.the_one("the exit").ok()?.exited()
    }

    /// the one session there is, for something that is about a whole process
    fn the_one(&self, wanted: &'static str) -> Result<&Attached> {
        let id = bpd_core::only_session(&self.sessions(), None, wanted)?;
        Ok(self.at(id))
    }

    /// where a session lives in the list
    ///
    /// the id came from [`bpd_core::only_session`] over [`Self::sessions`], so
    /// it is one of them
    fn at(&self, id: SessionId) -> &Attached {
        self.attached
            .iter()
            .find(|attached| attached.session.id() == id)
            .unwrap_or_else(|| unreachable!("{id} was resolved against the open sessions"))
    }

    fn index_of(&self, id: SessionId) -> usize {
        self.attached
            .iter()
            .position(|attached| attached.session.id() == id)
            .unwrap_or_else(|| unreachable!("{id} was resolved against the open sessions"))
    }

    /// the requests that read state without moving the program
    ///
    /// lifted out of [`Self::dispatch`] as a group rather than one at a time,
    /// because what they share is the reason they can be: none of them resumes
    /// anything, so none of them can produce a stop the caller has to handle
    fn reading(
        &mut self,
        at: usize,
        request: Request,
        reporting: &mut dyn Reporting,
    ) -> Result<Response> {
        match request {
            Request::RunScript { stop, script } => Ok(Response::Transcript(
                self.execute(at, stop, &script, reporting)?,
            )),
            Request::Query { stop, query } => Ok(Response::State(
                self.attached[at].describe(stop, &query, reporting)?,
            )),
            // nothing of the program is touched: both states were read when
            // they were read, and the difference between them is data over data
            Request::Diff { before, after } => Ok(Response::Difference(
                self.attached[at].compare(&before, &after)?,
            )),
            other => {
                unreachable!("`reading` is only ever handed one of its three, and got {other:?}")
            }
        }
    }

    /// start or stop recording where the program goes
    fn recording(
        &mut self,
        at: usize,
        on: bool,
        depth: bpd_core::Depth,
        reporting: &mut dyn Reporting,
    ) -> Result<Response> {
        let (on, held, dropped) = self.attached[at].record_trail(on, depth, reporting)?;
        Ok(Response::Recording { on, held, dropped })
    }

    /// the window of where the program has been
    fn taken(&mut self, at: usize, reporting: &mut dyn Reporting) -> Result<Response> {
        Ok(Response::Trail(self.attached[at].taken_trail(reporting)?))
    }

    /// what is holding an object, and answer with it
    ///
    /// lifted out of [`Self::dispatch`] for the reason the replacement arm was:
    /// the match is already at the length where one more request stops being
    /// readable
    fn holds(
        &mut self,
        at: usize,
        frame: FrameId,
        expression: String,
        reporting: &mut dyn Reporting,
    ) -> Result<Response> {
        let found = self.attached[at].what_holds(frame, expression, reporting)?;
        Ok(Response::Retainers(found))
    }

    /// replace a file's code, and answer with what became of it
    ///
    /// lifted out of [`Self::dispatch`] rather than written inline: the arm
    /// carries two things a client asked for and the match is already at the
    /// length where one more of those stops being readable
    fn answer_a_replacement(
        &mut self,
        at: usize,
        file: PathBuf,
        even_under_a_live_frame: bool,
        reporting: &mut dyn Reporting,
    ) -> Result<Response> {
        let replaced =
            self.attached[at].replace_the_code(file, even_under_a_live_frame, reporting)?;
        Ok(Response::Replaced(replaced))
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
    ///
    /// what the request is addressed to is resolved first: a request naming a
    /// session this debuggee does not hold is refused rather than answered from
    /// whichever session is nearest, and one that names none is for the only
    /// session there is — refused when there is more than one
    pub fn dispatch(
        &mut self,
        asked: Addressed,
        reporting: &mut dyn Reporting,
    ) -> Result<Response> {
        let Addressed { session, request } = asked;
        let id = bpd_core::only_session(&self.sessions(), session, request.name())?;
        let at = self.index_of(id);

        match request {
            Request::SetBreakpoints { breakpoints } => {
                let resolved =
                    self.attached[at].resolve_breakpoints(breakpoints.clone(), reporting)?;
                // only once it was accepted. a set that was refused is not what
                // is armed, and a `run_to` that put *that* back would be
                // arming something nobody asked for
                self.attached[at].armed = breakpoints;
                Ok(Response::BreakpointsResolved { resolved })
            }
            Request::SetExceptionBreakpoints { raised, uncaught } => {
                Ok(Response::ExceptionBreakpoints(
                    self.attached[at].arm_exceptions(raised, uncaught, reporting)?,
                ))
            }
            Request::DebugChildren { on } => Ok(Response::DebuggingChildren {
                on: self.attached[at].debug_children(on, reporting)?,
            }),
            Request::Run { deadline } => {
                // the deadline bounds the wait rather than the whole request:
                // the resume is answered on a thread that is already held, so
                // it cannot be what a program with nothing to say delays
                self.attached[at].let_go(Which::All, reporting)?;
                Ok(Response::Ran(self.wait_for(at, deadline, reporting)?))
            }
            Request::Wait { deadline } => {
                Ok(Response::Ran(self.wait_for(at, deadline, reporting)?))
            }
            Request::Resume { which } => Ok(Response::Resumed {
                threads: self.attached[at].let_go(which, reporting)?,
            }),
            Request::Step { stop, kind } => Ok(Response::Resumed {
                threads: self.attached[at].step_thread(stop, kind, reporting)?,
            }),
            Request::Pause => Ok(Response::Pausing {
                running: self.attached[at].arm_pause(reporting)?,
            }),
            Request::Threads { settle } => Ok(Response::Threads(
                self.attached[at].census(settle, reporting)?,
            )),
            Request::StopTheWorld { stop, settle } => Ok(Response::WorldStopped(
                self.attached[at].stop_world(stop, settle, reporting)?,
            )),
            Request::Stack { stop, top } => Ok(Response::Stack(
                self.attached[at].walk_stack(stop, top, reporting)?,
            )),
            Request::Variables {
                frame,
                scope,
                detail,
            } => Ok(Response::Variables(
                self.attached[at].read_scope(frame, scope, detail, reporting)?,
            )),
            Request::Facts {
                frame,
                names,
                limit,
            } => Ok(Response::Facts(
                self.attached[at].prove_facts(frame, names, limit, reporting)?,
            )),
            Request::TemplateContext { frame, detail } => Ok(Response::TemplateContext(
                self.attached[at].read_template_context(frame, detail, reporting)?,
            )),
            Request::Evaluate {
                frame,
                expression,
                detail,
            } => Ok(Response::Evaluated(self.attached[at].evaluate_in(
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
            } => Ok(Response::Evaluated(self.attached[at].write_variable(
                frame, scope, &name, &value, detail, reporting,
            )?)),
            Request::SetNextStatement { frame, line } => Ok(Response::Jumped(
                self.attached[at]
                    .move_frame(&FromEngine::SetNextStatement { frame, line }, reporting)?,
            )),
            Request::RestartFrame { frame } => Ok(Response::Jumped(
                self.attached[at].move_frame(&FromEngine::RestartFrame { frame }, reporting)?,
            )),
            Request::ReplaceCode {
                file,
                even_under_a_live_frame,
            } => self.answer_a_replacement(at, file, even_under_a_live_frame, reporting),
            Request::Record { on, depth } => self.recording(at, on, depth, reporting),
            Request::Trail => self.taken(at, reporting),
            Request::Retainers { frame, expression } => {
                self.holds(at, frame, expression, reporting)
            }
            // the three that read state rather than moving the program, lifted
            // out together: this match is at the length where one more arm stops
            // being readable, and these three share a shape
            Request::RunScript { .. } | Request::Query { .. } | Request::Diff { .. } => {
                self.reading(at, request, reporting)
            }
        }
    }

    /// describe one stop's state in one call, and keep the answer
    pub fn query(&mut self, stop: u64, query: StateQuery) -> Result<Snapshot> {
        match self.ask_for(Request::Query { stop, query })? {
            Response::State(snapshot) => Ok(snapshot),
            other => unreachable!("a state query was answered with {other:?}"),
        }
    }

    /// describe the only held stop's state
    pub fn the_query(&mut self, query: StateQuery) -> Result<Snapshot> {
        let stop = self.only("the state of a stop")?;
        self.query(stop, query)
    }

    /// what changed between two states this session read
    pub fn diff(&mut self, before: &SnapshotId, after: &SnapshotId) -> Result<Difference> {
        match self.ask_for(Request::Diff {
            before: before.clone(),
            after: after.clone(),
        })? {
            Response::Difference(difference) => Ok(difference),
            other => unreachable!("a difference was answered with {other:?}"),
        }
    }

    /// run a whole debug script against the thread `stop` holds
    ///
    /// the transcript is the answer, and it is the answer whatever became of
    /// the script — a caller given only where one ended cannot tell why, and
    /// will guess
    pub fn run_script(&mut self, stop: u64, script: Script) -> Result<Transcript> {
        match self.ask_for(Request::RunScript { stop, script })? {
            Response::Transcript(transcript) => Ok(transcript),
            other => unreachable!("a debug script was answered with {other:?}"),
        }
    }

    /// run a debug script against the only held thread
    pub fn the_script(&mut self, script: Script) -> Result<Transcript> {
        let stop = self.only("a debug script")?;
        self.run_script(stop, script)
    }

    /// dispatch a request whose answer is the only thing the caller waits for
    ///
    /// a logpoint that fires while one of these is in flight is kept in
    /// `pending_logs` for the next [`Self::wait`] rather than handed over here,
    /// and so is a child. what a debug script's own waiting reaches is
    /// [`Aside`], and the children it collected go back on the queue here
    fn ask_for(&mut self, request: Request) -> Result<Response> {
        // the aside goes back on the session the request was answered against,
        // which is the only session there is — an unaddressed request is
        // refused before it reaches an agent when there is more than one
        let id = bpd_core::only_session(&self.sessions(), None, request.name())?;
        let at = self.index_of(id);
        let mut aside = Aside::new();
        let answer = self.dispatch(Addressed::to(id, request), &mut aside);
        self.attached[at].pending_spawns.extend(aside.spawned);
        self.attached[at].pending_blind.extend(aside.blind);
        self.attached[at].pending_joined.extend(aside.joined);
        answer
    }

    /// the one stop that is held, for a request that is about one thread
    ///
    /// the rule itself is [`bpd_core::only_stop`], because every front end has
    /// to apply it and two of them applying their own would make a request that
    /// names no stop mean two things
    fn only(&self, wanted: &'static str) -> Result<u64> {
        self.the_one(wanted)?.only(wanted)
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

    /// map this debuggee's `.by` breakpoints through `map`
    ///
    /// installed at launch, before anything is armed. every session this
    /// debuggee holds now and every one that joins later maps through it,
    /// because a debugged fork is running the same build out of the same
    /// directory
    ///
    /// the map has already been verified — [`bpd_core::SourceMap::load`] is the
    /// only way to have one, and it checks both digests of every entry against
    /// the files on disk before it returns. so what this installs is a map that
    /// was true a moment ago rather than one that claims to be
    ///
    /// it is installed in **two** places, and they are not the same job. here,
    /// so a `.by` breakpoint becomes a generated line before the agent sees one
    /// — that translation has to happen before the program has run, and in DAP
    /// before it has been launched. and in the **agent**, so that every
    /// location the debuggee reports comes back as the `.by` line it came from.
    /// a location leaves through about thirty fields and translating them on
    /// the way out means finding every one of them; missing one reports two
    /// different files for a single location
    ///
    /// the agent is sent the tables and never the decision. it is handed a map
    /// that was already checked and applies it, because a debuggee vouching for
    /// the instrument that measures it is not evidence
    fn map_sources(&mut self, map: bpd_core::SourceMap) -> Result<()> {
        let armed: usize = self
            .attached
            .iter()
            .map(|session| session.armed.len())
            .sum();
        assert_eq!(
            armed, 0,
            "a source map is installed at launch, and breakpoints had already \
             been resolved without it — they would disagree with everything \
             resolved after"
        );
        let map = Arc::new(map);
        for session in &mut self.attached {
            session.map = Some(Arc::clone(&map));
            session.map_debuggee()?;
        }
        self.map = Some(map);
        Ok(())
    }

    /// decide whether a forked child of the program becomes a session of its own
    ///
    /// off by default. what comes back is what the agent says is set, and it is
    /// the debuggee's own memory that carries it into a child — see
    /// [`bpd_core::Request::DebugChildren`]
    pub fn debug_children(&mut self, on: bool) -> Result<bool> {
        match self.ask_for(Request::DebugChildren { on })? {
            Response::DebuggingChildren { on } => Ok(on),
            other => unreachable!("debugging children was answered with {other:?}"),
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

    /// what is provable about a frame's names, and for how long
    pub fn facts(
        &mut self,
        frame: FrameId,
        names: &[&str],
        limit: bpd_core::Limit,
    ) -> Result<bpd_core::Facts> {
        match self.ask_for(Request::Facts {
            frame,
            names: names.iter().map(|name| (*name).to_string()).collect(),
            limit,
        })? {
            Response::Facts(facts) => Ok(facts),
            other => unreachable!("a fact request was answered with {other:?}"),
        }
    }

    /// read a template frame's django context, layer by layer
    ///
    /// what [`Self::variables`] is for a python frame. a django template frame
    /// has no python scopes — the interpreter has no frame for it at all — and
    /// asking for one is refused with the python frame that does answer
    pub fn template_context(&mut self, frame: FrameId, detail: Detail) -> Result<TemplateContext> {
        match self.ask_for(Request::TemplateContext { frame, detail })? {
            Response::TemplateContext(context) => Ok(context),
            other => unreachable!("a template context was answered with {other:?}"),
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

    /// move the executing frame to another line of the code it is running
    ///
    /// the thread stays held, at the line it moved to. the program is not
    /// resumed by it and the lines between are not executed
    pub fn set_next_statement(&mut self, frame: FrameId, line: u32) -> Result<Jumped> {
        match self.ask_for(Request::SetNextStatement { frame, line })? {
            Response::Jumped(jumped) => Ok(jumped),
            other => unreachable!("a jump was answered with {other:?}"),
        }
    }

    /// re-enter a frame from the top, with what its parameters hold now
    pub fn restart_frame(&mut self, frame: FrameId) -> Result<Jumped> {
        match self.ask_for(Request::RestartFrame { frame })? {
            Response::Jumped(jumped) => Ok(jumped),
            other => unreachable!("a restart was answered with {other:?}"),
        }
    }

    /// start or stop recording where the program goes
    ///
    /// **the one mode that turns off what makes the rest of this fast.** a line
    /// is normally watched once and disabled; a recorder needs every execution
    /// of it, which measured at about 4× a bare run
    ///
    /// # errors
    ///
    /// when the session cannot be reached
    pub fn record(&mut self, on: bool, depth: bpd_core::Depth) -> Result<(bool, u64, u64)> {
        match self.ask_for(Request::Record { on, depth })? {
            Response::Recording { on, held, dropped } => Ok((on, held, dropped)),
            other => unreachable!("a recording was answered with {other:?}"),
        }
    }

    /// where the program has been, over the window
    ///
    /// # errors
    ///
    /// when the session cannot be reached
    pub fn trail(&mut self) -> Result<bpd_core::Trail> {
        match self.ask_for(Request::Trail)? {
            Response::Trail(trail) => Ok(trail),
            other => unreachable!("a trail was answered with {other:?}"),
        }
    }

    /// what is holding the object an expression names, and how
    ///
    /// # errors
    ///
    /// when the session cannot be reached, or the frame is not a python one
    pub fn what_holds(
        &mut self,
        frame: FrameId,
        expression: impl Into<String>,
    ) -> Result<bpd_core::Retainers> {
        match self.ask_for(Request::Retainers {
            frame,
            expression: expression.into(),
        })? {
            Response::Retainers(found) => Ok(found),
            other => unreachable!("a retainer walk was answered with {other:?}"),
        }
    }

    /// replace the code the process is running for one file with what is on
    /// disk, refusing under a live frame
    ///
    /// # errors
    ///
    /// when the session cannot be reached, or the agent answers with something
    /// else
    pub fn replace_code(&mut self, file: impl Into<PathBuf>) -> Result<Replaced> {
        self.replacing(file, false)
    }

    /// the same, applied even where a frame is running the code
    ///
    /// **a weaker guarantee, and its own method rather than a flag.** a `true`
    /// at a call site says nothing about what it buys, and what it buys is that
    /// the process runs two versions of one function until those frames return.
    /// the name is long because reaching for this should not be casual
    ///
    /// what comes back is applied, with every frame that will finish on the old
    /// code named — and that list is true when it is made and not afterwards.
    /// see [`bpd_core::StillRunning`]
    ///
    /// # errors
    ///
    /// when the session cannot be reached, or the agent answers with something
    /// else
    pub fn replace_code_even_under_a_live_frame(
        &mut self,
        file: impl Into<PathBuf>,
    ) -> Result<Replaced> {
        self.replacing(file, true)
    }

    /// what both of them are
    fn replacing(
        &mut self,
        file: impl Into<PathBuf>,
        even_under_a_live_frame: bool,
    ) -> Result<Replaced> {
        match self.ask_for(Request::ReplaceCode {
            file: file.into(),
            even_under_a_live_frame,
        })? {
            Response::Replaced(replaced) => Ok(replaced),
            other => unreachable!("a code replacement was answered with {other:?}"),
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

    /// wait for the next thing the debuggee does
    ///
    /// what the program says while it runs goes to `reporting` as it arrives
    /// rather than into the result. there is no bound on how many records a
    /// logpoint produces or how many children a program starts, and a debugger
    /// that accumulated a million of either before saying anything would be
    /// holding the program's history hostage to its own memory
    ///
    /// it is a [`Reporting`] rather than a closure over log records because
    /// there is more than one kind of thing a running program says, and a
    /// caller that could only receive one of them would be a caller the rest
    /// went missing at
    pub fn wait(&mut self, reporting: &mut dyn Reporting) -> Result<Running> {
        match self.dispatch(
            Addressed::unnamed(Request::Wait { deadline: None }),
            reporting,
        )? {
            Response::Ran(running) => Ok(running),
            other => unreachable!("a wait was answered with {other:?}"),
        }
    }

    /// let every held thread go and wait for what the debuggee does next
    ///
    /// the whole-program "continue". it resumes everything held rather than one
    /// thread, and what it waits for is the program, not a particular thread
    pub fn run(&mut self, reporting: &mut dyn Reporting) -> Result<Running> {
        match self.dispatch(
            Addressed::unnamed(Request::Run { deadline: None }),
            reporting,
        )? {
            Response::Ran(running) => Ok(running),
            other => unreachable!("a run was answered with {other:?}"),
        }
    }

    /// wait for the next thing the session at `at` does, giving up after
    /// `deadline`
    ///
    /// giving up leaves nothing outstanding on the wire. a wait is not a request
    /// the agent is answering — it is the engine reading the connection — so
    /// stopping reading it costs nothing and whatever the program says next is
    /// still there for the next wait
    ///
    /// the **listener is watched alongside the connection**, which is why this
    /// is a poll rather than a blocking read. a wait is where a session spends
    /// almost all of its time, so a wait that blocked on one socket would be a
    /// door that is only answered between requests
    ///
    /// the *other* sessions' connections are deliberately not read here. this
    /// wait is addressed to one of them, and a stop that arrived on another has
    /// nowhere in the answer to go — [`Running::Stopped`] names one stop of one
    /// session. nothing is lost by not reading it: the bytes stay in the
    /// kernel's receive buffer until a wait addressed to that session reads
    /// them, exactly as they do while the engine is answering a request
    fn wait_for(
        &mut self,
        at: usize,
        deadline: Option<Duration>,
        reporting: &mut dyn Reporting,
    ) -> Result<Running> {
        for record in self.attached[at].pending_logs.drain(..) {
            reporting.logged(record);
        }
        for child in self.attached[at].pending_spawns.drain(..) {
            reporting.spawned(child);
        }
        for blindspot in self.attached[at].pending_blind.drain(..) {
            reporting.blind_to(blindspot);
        }
        for joined in self.attached[at].pending_joined.drain(..) {
            reporting.attached(joined);
        }
        let mut rebound = std::mem::take(&mut self.attached[at].pending_rebinds);

        let started = Instant::now();
        let until = deadline.map(|deadline| started + deadline);

        loop {
            if let Some(arrived) = self.listener.arrived()? {
                // said as it happens rather than left to be discovered. an
                // agent that connects here is a **held** process — a debugged
                // fork is stopped at the line that forked — so a front end that
                // is never told has a stopped program it cannot reach
                reporting.attached(arrived.id());
                let mut joined = Attached::connected(arrived);
                joined.map = self.map.as_ref().map(Arc::clone);
                self.attached.push(joined);
            }

            // the wait is sliced so that the listener is looked at in between,
            // and the slice is what bounds how long a connection sits in the
            // backlog. it does not delay a stop: the peek returns the moment a
            // byte arrives, so a session with something to say is answered as
            // soon as it says it
            let slice = until.map_or_else(
                || Instant::now() + LISTEN_SLICE,
                |until| until.min(Instant::now() + LISTEN_SLICE),
            );
            if !self.attached[at].session.readable_by(slice)? {
                if until.is_some_and(|until| Instant::now() >= until) {
                    // the rebindings gathered so far go out with the timeout. a
                    // breakpoint that bound while the program ran is a fact
                    // about the program, and losing it because a deadline
                    // passed would leave a client believing one is still
                    // unbound
                    return Ok(Running::StillRunning {
                        waited: started.elapsed(),
                        rebound,
                    });
                }
                continue;
            }

            match self.attached[at].session.next_event()? {
                Some(FromAgent::Stopped { stop }) => {
                    // named here, where the connection it arrived on is known.
                    // the agent counts its stops from one and cannot see
                    // another agent doing the same
                    let stop = stop.in_session(self.attached[at].session.id());
                    self.attached[at].held.push(stop.clone());
                    return Ok(Running::Stopped { stop, rebound });
                }
                Some(FromAgent::Finishing { held }) => {
                    return Ok(Running::Finishing {
                        threads: held,
                        rebound,
                    });
                }
                Some(FromAgent::BreakpointsResolved { resolved }) => {
                    rebound.extend(self.attached[at].restore(resolved));
                }
                Some(FromAgent::Logged { record }) => reporting.logged(record),
                // the program started a child. it is already running — the
                // agent reports and does not block — so this is news rather
                // than a decision anybody is waiting to make
                Some(FromAgent::Spawned { child }) => reporting.spawned(child),
                // and the message that says a silence about one has stopped
                // being evidence
                Some(FromAgent::BlindTo { blindspot }) => reporting.blind_to(blindspot),
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
                // the connection closed, which is the program being over. what
                // it exited with is a different question, and only the process
                // that started it can answer
                None => {
                    let Some(child) = self.attached[at].child.as_ref() else {
                        return Ok(Running::Ended { rebound });
                    };
                    let status = child
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
                    // the program is reaped, and what it printed may still be
                    // in a pipe: this arm was reached because the **control
                    // connection** closed, and the descriptors carrying its
                    // output are different ones with no order against that. so
                    // the forwarding is waited for here, before the exit is
                    // reported, rather than left to race the report
                    let output = self
                        .forwarders
                        .as_mut()
                        .map_or(Forwarded::Everything, Forwarders::drained);
                    return Ok(Running::Exited {
                        status,
                        rebound,
                        output,
                    });
                }
            }
        }
    }
}

impl Attached {
    /// a session that arrived on the retained listener
    ///
    /// no child, because bpd did not start it. everything else starts where a
    /// launched session's does: nothing held, nothing armed, nothing pending
    const fn connected(session: Session) -> Self {
        Self {
            child: None,
            session,
            held: Vec::new(),
            armed: Vec::new(),
            pending_logs: Vec::new(),
            pending_rebinds: Vec::new(),
            pending_spawns: Vec::new(),
            pending_blind: Vec::new(),
            pending_joined: Vec::new(),
            map: None,
            mapped: false,
            translated: std::collections::BTreeMap::new(),
            snapshots: Vec::new(),
        }
    }

    /// a handle that reaches this process while the session is waiting on it
    fn interrupt(&self) -> Interrupt {
        Interrupt::new(
            self.session.id(),
            self.session.writer(),
            self.child.as_ref().map(Arc::clone),
        )
    }

    /// how this session's program ended, or `None` while it is still running
    ///
    /// a failure to read the child's status is reported as no exit: it is not
    /// evidence that the program ended, and claiming one would be the debugger
    /// inventing a state
    fn exited(&self) -> Option<Exit> {
        self.exit_code().ok().flatten()
    }

    /// the one stop this session holds, for a request that is about one thread
    fn only(&self, wanted: &'static str) -> Result<u64> {
        Ok(bpd_core::only_stop(&self.held, self.exit_code()?, wanted)?)
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

        // a `.by` breakpoint is translated into the generated python before
        // the agent sees it, and the answer is translated back before anybody
        // else does. the agent never learns a source map exists
        let sent = mapping::send(self.map.as_deref(), breakpoints);
        let breakpoints = sent.breakpoints;
        // replaced whole, like the set it describes. a translation left over
        // from the last set would map an answer through a route this one never
        // took
        self.translated = sent.translated;

        let request = FromEngine::SetBreakpoints { breakpoints };
        match self.ask(&request, EXPECTED, reporting)? {
            FromAgent::BreakpointsResolved { resolved } => {
                let mut answers = self.restore(resolved);
                // the ones the map refused never went to the agent, so they are
                // put back here. a client asked about every breakpoint in the
                // set and is owed an answer about every one of them, in the
                // order it asked
                answers.extend(sent.refused);
                Ok(mapping::reorder(&sent.order, answers))
            }
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// hand this session's agent the tables it reports locations through
    ///
    /// sent while the debuggee is held at entry, before a line of the program
    /// has run, so there is no location it could have produced without the map.
    /// the answer is the count, and it is compared: an agent that installed
    /// fewer files than were sent would map some of the build's locations and
    /// not others, which is the inconsistency this whole layer is about
    ///
    /// what crosses is [`bpd_core::MappedFile`], which cannot be built without
    /// [`bpd_core::SourceMap::load`] having hashed both files it describes
    /// against disk first. the debuggee applies a map; it never decides one is
    /// trustworthy
    fn map_debuggee(&mut self) -> Result<()> {
        const EXPECTED: &str = "the source map to be installed";

        let files = self
            .map
            .as_ref()
            .unwrap_or_else(|| unreachable!("the map is installed before it is sent"))
            .files();
        let sent = u32::try_from(files.len()).unwrap_or(u32::MAX);

        let mut aside = Aside::new();
        // `send_and_wait` rather than `ask`, which is what puts this request
        // ahead of every other one: `ask` is where a session that has not been
        // sent the map yet is sent it, and going back through it here would be
        // this request waiting for itself
        let answer = self.send_and_wait(&FromEngine::MapSources { files }, EXPECTED, &mut aside)?;
        self.pending_spawns.extend(aside.spawned);
        self.pending_blind.extend(aside.blind);
        self.pending_joined.extend(aside.joined);

        match answer {
            FromAgent::SourcesMapped { files } if files == sent => {
                self.mapped = true;
                Ok(())
            }
            FromAgent::SourcesMapped { files } => unreachable!(
                "{sent} mapped files were sent to the agent and it installed \
                 {files}. the engine and the agent are built and shipped \
                 together, and the handshake refuses a mismatch"
            ),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// every answer about a translated breakpoint, back in `.by` terms
    ///
    /// applied to every `Resolved` that leaves this session — the answer to a
    /// set and the rebindings that arrive unprompted — because a client that
    /// was handed one raw would be reading a line of a file it never wrote
    fn restore(&self, resolved: Vec<Resolved>) -> Vec<Resolved> {
        mapping::restore(self.map.as_deref(), &self.translated, resolved)
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

    /// decide what a forked child of this session's program does
    ///
    /// the answer is what the **agent** says is set, not what was asked for. a
    /// setting the fork handler never received would leave a client waiting for
    /// child sessions that are never going to arrive, and on a platform with no
    /// `fork` there is nothing for it to be true of at all — which the agent
    /// refuses by name
    fn debug_children(&mut self, on: bool, reporting: &mut dyn Reporting) -> Result<bool> {
        const EXPECTED: &str = "what a forked child will do";

        match self.ask(&FromEngine::DebugChildren { on }, EXPECTED, reporting)? {
            FromAgent::DebuggingChildren { on } => Ok(on),
            other => Err(unexpected(&other, EXPECTED)),
        }
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

    fn arm_pause(&mut self, reporting: &mut dyn Reporting) -> Result<Vec<u64>> {
        const EXPECTED: &str = "a pause to be armed";

        match self.send_and_wait(&FromEngine::Pause, EXPECTED, reporting)? {
            FromAgent::Pausing { running } => Ok(running),
            other => Err(unexpected(&other, EXPECTED)),
        }
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
                scheduled_by,
                in_a_task,
                scheduling_cut,
                depth,
                mode,
            } => Ok(Stack {
                frames,
                scheduled_by,
                in_a_task,
                scheduling_cut,
                depth,
                mode,
            }),
            other => Err(unexpected(&other, EXPECTED)),
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

    /// what is provable about a frame's names, and for how long
    fn prove_facts(
        &mut self,
        frame: FrameId,
        names: Vec<String>,
        limit: bpd_core::Limit,
        reporting: &mut dyn Reporting,
    ) -> Result<bpd_core::Facts> {
        const EXPECTED: &str = "what is provable about a frame's names";

        let request = FromEngine::Facts {
            frame,
            names,
            limit,
        };
        match self.ask(&request, EXPECTED, reporting)? {
            FromAgent::Facts {
                proved,
                silent,
                mode,
                ..
            } => Ok(bpd_core::Facts {
                proved,
                silent,
                mode,
            }),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    fn read_template_context(
        &mut self,
        frame: FrameId,
        detail: Detail,
        reporting: &mut dyn Reporting,
    ) -> Result<TemplateContext> {
        const EXPECTED: &str = "the template context of a frame";

        let request = FromEngine::TemplateContext { frame, detail };
        match self.ask(&request, EXPECTED, reporting)? {
            FromAgent::TemplateContext { layers, mode, .. } => Ok(TemplateContext { layers, mode }),
            other => Err(unexpected(&other, EXPECTED)),
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

    fn replace_the_code(
        &mut self,
        file: PathBuf,
        even_under_a_live_frame: bool,
        reporting: &mut dyn Reporting,
    ) -> Result<Replaced> {
        const EXPECTED: &str = "what replacing a file's code did";

        match self.ask(
            &FromEngine::ReplaceCode {
                file,
                even_under_a_live_frame,
            },
            EXPECTED,
            reporting,
        )? {
            FromAgent::Replaced { replaced } => Ok(replaced),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// start or stop recording, and say what the window holds
    ///
    /// # errors
    ///
    /// when the session cannot be reached, or the agent answers with something
    /// else
    fn record_trail(
        &mut self,
        on: bool,
        depth: bpd_core::Depth,
        reporting: &mut dyn Reporting,
    ) -> Result<(bool, u64, u64)> {
        const EXPECTED: &str = "whether recording is on";

        match self.ask(&FromEngine::Record { on, depth }, EXPECTED, reporting)? {
            FromAgent::Recording { on, held, dropped } => Ok((on, held, dropped)),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// the window of where the program has been
    ///
    /// # errors
    ///
    /// when the session cannot be reached, or the agent answers with something
    /// else
    fn taken_trail(&mut self, reporting: &mut dyn Reporting) -> Result<bpd_core::Trail> {
        const EXPECTED: &str = "where the program has been";

        match self.ask(&FromEngine::Trail, EXPECTED, reporting)? {
            FromAgent::Trailed { trail } => Ok(trail),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// what is holding the object an expression names
    ///
    /// # errors
    ///
    /// when the session cannot be reached, or the agent answers with something
    /// else
    fn what_holds(
        &mut self,
        frame: FrameId,
        expression: String,
        reporting: &mut dyn Reporting,
    ) -> Result<bpd_core::Retainers> {
        const EXPECTED: &str = "what is holding an object";

        match self.ask(
            &FromEngine::Retainers { frame, expression },
            EXPECTED,
            reporting,
        )? {
            FromAgent::Retaining { retainers } => Ok(retainers),
            // a refusal never reaches here: `ask` turns one into an error for
            // every request, so a template frame is refused by name there
            other => Err(unexpected(&other, EXPECTED)),
        }
    }

    /// the request half both jumps share
    ///
    /// one function rather than two, because the two differ only in which
    /// message goes down the connection: everything about the answer — where
    /// the frame is now, what the move bound to `None`, which breakpoints on
    /// the destination will not fire — is the same claim either way
    fn move_frame(
        &mut self,
        request: &FromEngine,
        reporting: &mut dyn Reporting,
    ) -> Result<Jumped> {
        const EXPECTED: &str = "the frame to move";

        match self.ask(request, EXPECTED, reporting)? {
            FromAgent::Jumped { jumped } => Ok(jumped),
            other => Err(unexpected(&other, EXPECTED)),
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
            // "nothing is held" invites holding something, and a program that
            // has ended cannot be held at all. which of the two this is comes
            // from the child, when there is one
            return Err(match self.exit_code()? {
                Some(Exit::Code(code)) => bpd_core::Error::ProgramExited {
                    code,
                    wanted: expected,
                },
                Some(Exit::Unknown) => bpd_core::Error::ProgramEnded { wanted: expected },
                None => bpd_core::Error::NotStopped { wanted: expected },
            }
            .into());
        }
        // before anything else this session is ever asked, and once. the
        // launched session is sent the map at launch and this is what covers a
        // session that **joined** later — an `exec`'d child is a fresh
        // interpreter with a fresh agent, and a fork inherits the tables in
        // memory along with everything else. nothing of a joined session's
        // program can have run before the client resumed it, and a resume comes
        // through here
        if self.map.is_some() && !self.mapped {
            self.map_debuggee()?;
        }
        self.send_and_wait(request, expected, reporting)
    }

    /// how this session's program ended, or `None` while it is still running
    ///
    /// `try_wait` reaps a child that has ended and remembers the status, so the
    /// `wait` on the exit path afterwards still answers with it
    ///
    /// with **no child** there is nothing to reap and no status anywhere, so
    /// the two states this can be in are read off the connection instead: it is
    /// open, or the agent hung up and the program is over. either way the
    /// number is not bpd's to give, and [`Exit::Unknown`] is how that is said
    /// rather than a zero nobody measured
    fn exit_code(&self) -> Result<Option<Exit>> {
        let Some(child) = self.child.as_ref() else {
            return Ok(self.session.hung_up()?.then_some(Exit::Unknown));
        };
        let ended = child
            .lock()
            .expect(
                "nothing panics holding the debuggee: every path through it is a kill or a wait",
            )
            .try_wait()
            .map_err(|source| Error::Spawn {
                interpreter: PathBuf::from("the debuggee"),
                source,
            })?;
        Ok(ended.map(bpd_core::exit_code).map(Exit::Code))
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
                Some(FromAgent::Spawned { child }) => self.pending_spawns.push(child),
                Some(FromAgent::BlindTo { blindspot }) => self.pending_blind.push(blindspot),
                Some(FromAgent::BreakpointsResolved { resolved })
                    if !matches!(request, FromEngine::SetBreakpoints { .. }) =>
                {
                    let restored = self.restore(resolved);
                    self.pending_rebinds.extend(restored);
                }
                Some(FromAgent::Stopped { stop }) => {
                    self.held.push(stop.in_session(self.session.id()));
                }
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

/// what the debuggee is asked to run, and how the interpreter should enter it
///
/// the three forms are not variations of one another. `sys.argv[0]`,
/// `sys.path[0]` and `__main__` differ between them, and a launcher that treats
/// one as a special case of another gets at least one of them wrong — so the
/// choice is a closed enum a caller has to make, not a path with two optional
/// flags beside it
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Program {
    /// `python <path>`, with the path exactly as it was typed
    Script(PathBuf),
    /// `python -m <module>`
    Module(String),
    /// `python -c <source>`
    Command(String),
}

impl Program {
    /// how the interpreter is entered
    fn form(&self) -> env::Form {
        match self {
            Self::Script(_) => env::Form::Script,
            Self::Module(_) => env::Form::Module,
            Self::Command(_) => env::Form::Command,
        }
    }

    /// what the agent is handed, which the form says how to read
    fn target(&self) -> &std::ffi::OsStr {
        match self {
            Self::Script(path) => path.as_os_str(),
            Self::Module(module) => module.as_ref(),
            Self::Command(source) => source.as_ref(),
        }
    }
}

/// launch a program under the debugger and stop before its first statement
///
/// returns once the agent has reported that it is stopped, so the caller holds a
/// debuggee that has run none of the program yet. all three of the debuggee's
/// own standard streams are bpd's, untouched — which is what makes a run under
/// bpd indistinguishable from a bare one, down to `isatty()`
pub fn launch(
    interpreter: &Capabilities,
    program: &Program,
    args: &[OsString],
) -> Result<Launched> {
    start(interpreter, program, args, Start::Here(None))
}

/// launch with the debuggee's own output in pipes rather than on bpd's streams
///
/// what a front end whose own stdout is a **protocol** needs: one `print` from
/// the program in the middle of a message and every message after it is
/// unreadable
///
/// the debuggee's **stdin is `/dev/null`** here, and that is part of the same
/// decision rather than a separate one: a front end that has taken the
/// program's output over has taken bpd's streams for itself, and there is no
/// bare run left to inherit from. `input()` raises `EOFError`, exactly as it
/// does under `python program.py < /dev/null`
///
/// `on_spawn` is handed the two pipes the moment the process exists and before
/// anything waits on it. that ordering is the whole of the contract: a pipe
/// nobody is reading fills up, and a process whose pipe is full stops — so a
/// launcher that waited first and handed the pipes over afterwards would hang
/// on any program that says more than a pipe buffer holds
///
/// what it hands back is what is reading them, and that is not a formality: the
/// engine waits for it before reporting the program over, so that a client is
/// never told a program has finished while a line it printed is still in a pipe.
/// see [`Forwarders`]
pub fn launch_piped(
    interpreter: &Capabilities,
    program: &Program,
    args: &[OsString],
    on_spawn: impl FnOnce(std::process::ChildStdout, std::process::ChildStderr) -> Forwarders + 'static,
) -> Result<Launched> {
    start(
        interpreter,
        program,
        args,
        Start::Here(Some(Box::new(on_spawn))),
    )
}

/// hand the launch to somebody else, and wait for the agent it starts
///
/// what DAP's `runInTerminal` needs. everything up to the spawn is the same —
/// the interpreter is probed, the map is loaded, the agent is staged, the
/// listener is bound and the environment is written — and then `start_it` is
/// given the [`Invocation`] instead of a `Command` being spawned. the agent
/// connects **back** exactly as it does from a process bpd started, which is
/// why this is a different last step rather than a different launch
///
/// the debuggee is then **not bpd's child**, and everything that follows from
/// that is already the rule for a session that arrived on the retained
/// listener: there is no exit status to read, so the program being over is
/// [`Running::Ended`], and ending it is refused by name
///
/// `start_it` returning an error is the client saying it did not start the
/// program, and nothing is waited for afterwards — a wait for an agent that was
/// never launched is a timeout with a cause nobody would find
pub fn launch_in_terminal(
    interpreter: &Capabilities,
    program: &Program,
    args: &[OsString],
    start_it: impl FnOnce(
        &Invocation,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>,
) -> Result<Launched> {
    start(
        interpreter,
        program,
        args,
        Start::Elsewhere(Box::new(start_it)),
    )
}

/// what to do with the debuggee's own output the moment it exists
///
/// it hands back what is doing the reading, because the engine has to be able
/// to **wait** for it: see [`Forwarders`]
type OnSpawn = Box<dyn FnOnce(std::process::ChildStdout, std::process::ChildStderr) -> Forwarders>;

/// the threads carrying a debuggee's output somewhere, and the wait for them
///
/// a front end that pipes the debuggee reads the two streams on threads of its
/// own, and those threads are the only thing that knows whether the last of what
/// the program wrote has been carried. the engine learns the program is over on
/// the **control connection**, which is a different descriptor with no order
/// against them — so without this, `bpd` reports a program finished while a line
/// it printed is still in a pipe
///
/// [`Self::drained`] is what closes that, and it is deliberately **bounded**.
/// each thread ends at end-of-file, and end-of-file needs every write end
/// closed: a forked child inherits one, so a program that leaves a child running
/// never reaches it. an unbounded wait here would turn "the program exited" into
/// a hang for exactly the programs this debugger exists to follow
#[derive(Debug)]
pub struct Forwarders(Vec<std::thread::JoinHandle<()>>);

/// how long the debuggee's own output is waited for once the program is gone
///
/// end-of-file has already been reached in the ordinary case — the process that
/// held the write end is dead — so this is not a delay a program pays. it is
/// what stops a **child** holding the stream open from turning an exit into a
/// hang, and it is generous because the only cost of it being generous is paid
/// by a program that has already ended
const OUTPUT_PATIENCE: Duration = Duration::from_secs(2);

/// how often the wait above looks again
///
/// `JoinHandle` has no join-with-deadline, so this polls `is_finished` and then
/// joins one that has already ended — which cannot block
const OUTPUT_POLL: Duration = Duration::from_millis(2);

impl Forwarders {
    /// the threads carrying the debuggee's output
    pub fn on(threads: Vec<std::thread::JoinHandle<()>>) -> Self {
        Self(threads)
    }

    /// nothing is carrying it, because it was never bpd's to carry
    ///
    /// what an inherited stream is: the program wrote to the terminal itself,
    /// so there is no pipe to drain and the order is the kernel's
    pub fn inherited() -> Self {
        Self(Vec::new())
    }

    /// wait for what is left of the program's output, for as long as
    /// [`OUTPUT_PATIENCE`]
    ///
    /// a thread that panicked is joined and its panic dropped rather than
    /// resumed: this is called on the way to reporting a program's exit, and
    /// turning a forwarder's panic into a panic here would lose the exit status
    /// the caller is waiting for. what it costs is carried out —
    /// [`Forwarded::StillHeldOpen`] is what a caller sees, because a thread that
    /// died is one whose stream is no longer being read
    fn drained(&mut self) -> Forwarded {
        let deadline = Instant::now() + OUTPUT_PATIENCE;
        while self.0.iter().any(|thread| !thread.is_finished()) {
            if Instant::now() >= deadline {
                return Forwarded::StillHeldOpen;
            }
            std::thread::sleep(OUTPUT_POLL);
        }

        // joined first and judged after, rather than `any` over the joins.
        // `any` short-circuits, so the first thread that panicked would leave
        // the rest dropped un-joined — a thread whose panic nobody collected.
        // they have all finished by here, so none of these can block
        let joined: Vec<_> = self
            .0
            .drain(..)
            .map(std::thread::JoinHandle::join)
            .collect();
        if joined.iter().any(Result::is_err) {
            return Forwarded::StillHeldOpen;
        }
        Forwarded::Everything
    }
}

/// what somebody else is asked to run, and what it needs around it
type StartIt<'a> = Box<
    dyn FnOnce(&Invocation) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>
        + 'a,
>;

/// who starts the interpreter
enum Start<'a> {
    /// bpd does, and this is what to do with its output when it exists
    ///
    /// `None` leaves all three of the debuggee's standard streams inherited,
    /// which is what makes a run under `bpd launch` indistinguishable from a
    /// bare one
    Here(Option<OnSpawn>),

    /// somebody else does, and this is how they are asked
    Elsewhere(StartIt<'a>),
}

/// the command line that starts a debuggee, for something else to run
///
/// every part of what [`start`] would have spawned, and nothing that is bpd's
/// own: the whole argument vector with the interpreter first, the environment
/// entries the agent reads, and the directory a spawn would have inherited.
/// a caller that dropped one of them would be starting a different program from
/// the one bpd prepared — a missing `PYTHONPATH` is an interpreter that cannot
/// import the agent, and a missing token is a connection the listener refuses
///
/// the values are [`OsString`]s because a path is. rendering them for a
/// protocol that carries text is the front end's, and so is refusing one that
/// will not render
#[derive(Debug, Clone)]
pub struct Invocation {
    /// the whole command line, the interpreter first
    pub arguments: Vec<OsString>,

    /// the variables to set, **in addition to** whatever the client already has
    ///
    /// bpd's own environment is where these were going to be written, so a
    /// terminal that starts with the client's environment and adds these is the
    /// same environment a spawn would have produced. every one of them is taken
    /// back out by the agent before any user code runs
    pub env: Vec<(String, OsString)>,

    /// the directory to run it in, which is the one bpd is in
    pub directory: PathBuf,
}

fn start(
    interpreter: &Capabilities,
    program: &Program,
    args: &[OsString],
    how: Start<'_>,
) -> Result<Launched> {
    interpreter
        .require_debuggable()
        .map_err(|error| Error::LocateAgent {
            reason: error.to_string(),
        })?;

    // before anything is started, for the reason an unsupported interpreter is
    // refused before anything is started: a program that ran and then could not
    // be debugged is a program that ran. `crates/bpd/tests/launch_refusal.rs` is
    // where that rule is checked for the interpreter, and this is the same rule
    let map = match build_directory(program) {
        Some(directory) => Some(
            bpd_core::SourceMap::load(&directory).map_err(|source| Error::SourceMap { source })?,
        ),
        None => None,
    };

    let staged = agent::stage_for(interpreter)?;
    // staged at launch and not when child debugging is asked for, because the
    // ask arrives while the debuggee is held at entry and a staging failure
    // there would be a refusal in the middle of a session rather than at the
    // one moment nothing has happened yet
    let child_hook = agent::stage_child_hook()?;
    let listener = Listener::bind()?;
    let endpoint = listener.endpoint()?;

    let mut arguments = vec![
        OsString::from(&interpreter.executable),
        OsString::from("-c"),
        OsString::from(BOOTSTRAP),
    ];
    arguments.extend(args.iter().cloned());

    let mut environment = vec![
        (
            env::ENDPOINT.to_string(),
            OsString::from(endpoint.to_string()),
        ),
        (env::TOKEN.to_string(), OsString::from(listener.token_hex())),
        (env::TARGET.to_string(), program.target().to_os_string()),
        (
            env::FORM.to_string(),
            OsString::from(program.form().as_str()),
        ),
        // both are taken back out of the environment before any user code runs,
        // exactly like the four above. what puts the child's pair back — under
        // the names a child reads — is `debugChildren`, and nothing else
        (
            env::CHILD_TOKEN.to_string(),
            OsString::from(listener.child_token_hex()),
        ),
        (
            env::SITECUSTOMIZE.to_string(),
            OsString::from(child_hook.python_path()),
        ),
    ];

    // the agent is imported by putting its staged directory in front of
    // whatever `PYTHONPATH` this process inherited. **in front of** and not
    // instead of: overwriting it would take away search path the program was
    // given, which is a debugger changing what the program imports. the
    // original goes along so the agent can put it back
    let mut import_path = OsString::from(staged.python_path());
    if let Some(inherited) = std::env::var_os("PYTHONPATH") {
        import_path.push(if cfg!(windows) { ";" } else { ":" });
        import_path.push(&inherited);
        environment.push((env::PYTHON_PATH.to_string(), inherited));
    }
    environment.push(("PYTHONPATH".to_string(), import_path));

    // somebody else runs it, and the wait for the agent is the same wait. what
    // is different is everything that follows from bpd not being the parent,
    // and every one of those is already the rule for a session that arrived on
    // this listener rather than being launched on it
    let piped = match how {
        Start::Here(piped) => piped,
        Start::Elsewhere(start_it) => {
            let invocation = Invocation {
                arguments,
                env: environment,
                // the directory a spawn would have inherited. a client that ran
                // the command somewhere else would run a different program for
                // a relative path, and resolve a different `sys.path[0]`
                directory: std::env::current_dir().map_err(|source| Error::Spawn {
                    interpreter: interpreter.executable.clone(),
                    source,
                })?,
            };
            start_it(&invocation).map_err(|source| Error::NotStarted { source })?;
            return attached_in_terminal(listener, map);
        }
    };

    spawned_here(interpreter, &arguments, &environment, piped, listener, map)
}

/// spawn the interpreter here, and wait for the agent in the child bpd holds
///
/// the ordinary half of a launch, and the one that has a **child**: an exit
/// status to read, a process to poll while the agent is being waited for, and
/// something to kill when it never announces itself
fn spawned_here(
    interpreter: &Capabilities,
    arguments: &[OsString],
    environment: &[(String, OsString)],
    piped: Option<OnSpawn>,
    listener: Listener,
    map: Option<bpd_core::SourceMap>,
) -> Result<Launched> {
    let mut command = Command::new(&interpreter.executable);
    command.args(&arguments[1..]);
    for (name, value) in environment {
        command.env(name, value);
    }

    if piped.is_some() {
        command
            // stdin goes with the other two, and it goes to `/dev/null` rather
            // than staying inherited. this process's stdin is either the
            // **protocol** — `bpd dap` on stdio, `bpd mcp` — or it belongs to
            // whatever spawned bpd, and neither is the debuggee's to read.
            // measured, before this: `input()` in a debuggee under `bpd dap`
            // handed the program `'Content-Length: 68\r'`, and the request
            // those bytes were the header of was answered by nothing
            //
            // nothing carries a client's keystrokes down a pipe to a debuggee,
            // and DAP's answer for a program that needs them is a terminal the
            // client owns — `launch_in_terminal`, which is a different last
            // step rather than a channel bolted onto this one. so what is left
            // here is the empty stream `python program.py < /dev/null` has.
            // `input()` there raises `EOFError` at the line that asked, which is
            // an outcome with a name rather than a hang
            //
            // **null and not a closed descriptor.** cpython sets `sys.stdin` to
            // `None` when descriptor 0 is not open, and then `input()` raises
            // `RuntimeError: input(): lost sys.stdin` — and descriptor 0 would
            // be handed to the next file the program opened
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
    }

    let mut child = command.spawn().map_err(|source| Error::Spawn {
        interpreter: interpreter.executable.clone(),
        source,
    })?;

    let mut forwarders = None;
    if let Some(hand_over) = piped {
        let stdout = child
            .stdout
            .take()
            .expect("the command asked for a stdout pipe and cargo has not taken it");
        let stderr = child
            .stderr
            .take()
            .expect("the command asked for a stderr pipe and cargo has not taken it");
        forwarders = Some(hand_over(stdout, stderr));
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
            // the same rule as a program that exits after running: what it said
            // is waited for before it is reported over. it matters more here
            // than anywhere, because what a program that never started wrote is
            // the **interpreter's own words about why** — a `SyntaxError`, a
            // missing module — and a caller handed the outcome without them has
            // nothing to tell anybody
            //
            // the verdict has nowhere to go and is not carried: a program that
            // never ran a line of its own had nothing to fork, so the only
            // thing that could hold these streams open is gone with it
            if let Some(forwarders) = forwarders.as_mut() {
                forwarders.drained();
            }
            return Ok(Launched::ExitedBeforeStopping(status));
        }
        Err(error) => return Err(error),
    };

    // the listener goes on the debuggee rather than out of scope here. it used
    // to be dropped with this frame, which closed the socket and made the first
    // agent the only one that could ever attach
    let mut debuggee = Debuggee {
        listener,
        map: None,
        attached: vec![Attached {
            held: vec![stop],
            child: Some(Arc::new(Mutex::new(child))),
            ..Attached::connected(session)
        }],
        forwarders,
    };
    if let Some(map) = map {
        debuggee.map_sources(map)?;
    }
    Ok(Launched::Stopped(debuggee))
}

/// wait for the agent of a debuggee **somebody else** started
///
/// the same wait as a launch's, minus the one thing bpd no longer has: a child
/// to poll. so a debuggee that never starts at all cannot be told from one that
/// is slow, and what is left is the deadline — which is why the refusal names
/// the terminal rather than the agent. the process bpd cannot see is the
/// process whose output the person can
fn attached_in_terminal(listener: Listener, map: Option<bpd_core::SourceMap>) -> Result<Launched> {
    let mut session = listener.accept(|| Ok(None)).map_err(|error| match error {
        Error::AttachTimeout { timeout } => Error::NoAgentFromTerminal { timeout },
        other => other,
    })?;

    // there is no status to read here and no `ExitedBeforeStopping` to report.
    // a program that does not compile takes this path — the agent connects and
    // then fails to build the program — and what it said is in the terminal the
    // client opened, in the interpreter's own words
    let stop = match session.expect_stop() {
        Ok(stop) => stop,
        Err(Error::AgentGone { .. }) => return Err(Error::EndedInTerminal),
        Err(error) => return Err(error),
    };

    let mut debuggee = Debuggee {
        listener,
        map: None,
        attached: vec![Attached {
            held: vec![stop],
            ..Attached::connected(session)
        }],
        // somebody else started this program in a terminal they own, so its
        // output never passed through bpd and there is no pipe to drain
        forwarders: None,
    };
    if let Some(map) = map {
        debuggee.map_sources(map)?;
    }
    Ok(Launched::Stopped(debuggee))
}

/// the basedpython build directory this program runs out of, if it does
///
/// **how bpd finds the map, and it is found rather than configured.** `by run`
/// transpiles into one directory and writes `_by_sourcemap.py` into it beside
/// the python it generated and the runner shim it starts — so a program running
/// out of a directory that holds that file is running that build, by
/// construction, and there is nothing for a user to point at and get wrong
///
/// the alternative was a launch option, and it would have been a launch option
/// three front ends each had to grow. finding it here means the command line, a
/// DAP client and an MCP client all debug `.by` on the same terms without any of
/// them being taught what basedpython is
///
/// the rule is one sentence: **the directory the program is in, or the current
/// directory for a form that names no file**. what makes that safe rather than a
/// guess is that finding the file is where the guessing stops — the map is
/// verified against both files it describes before a single line comes out of
/// it, and one that cannot be is a refusal at launch
fn build_directory(program: &Program) -> Option<PathBuf> {
    let directory = match program {
        Program::Script(path) => path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())?,
        Program::Module(_) | Program::Command(_) => std::env::current_dir().ok()?,
    };
    directory
        .join(bpd_core::source_map::MAP_FILENAME)
        .is_file()
        .then_some(directory)
}
