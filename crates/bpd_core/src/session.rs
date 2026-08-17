//! the capability surface of a debug session, as data
//!
//! a capability used to be a method, and rust cannot enumerate methods. the
//! rule that no capability exists in one adapter and not the other is only
//! checkable against something that *can* be enumerated, so the surface is a
//! [`Request`] and the answers to it are a [`Response`]
//!
//! this is deliberately not the agent's request set. [`Request`] is what a
//! client asks of a session; `bpd_protocol::message::FromEngine` is what the
//! session asks of the agent inside the debuggee, and the two differ where the
//! session does something the agent has no single request for — running the
//! program is a resume followed by a wait. what they share is the vocabulary in
//! this crate, defined once and serialised where it has to be

use std::num::NonZeroU64;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use crate::breakpoint::{LogRecord, Resolved, SourceBreakpoint};
use crate::frame::{Frame, FrameId, Scheduling, Scope};
use crate::jump::Jumped;
use crate::query::{Difference, Snapshot, SnapshotId, StateQuery};
use crate::replace::Replaced;
use crate::script::{Script, Transcript};
use crate::spawn::{Blindspot, Spawn};
use crate::stop::{Mode, StepKind, Stop};
use crate::thread::{ThreadState, Which};
use crate::value::{Detail, Entry, Evaluated, Omitted, Value};

/// which debug session something belongs to
///
/// a session is one control connection to one agent, which is one debugged
/// process. every id the **agent** mints — a stop's number, and the
/// [`crate::FrameId`] and [`crate::SnapshotId`] built on one — counts from one
/// in the process that minted it, so two agents give the same number to
/// different things. this is what tells them apart, and it is minted by the
/// **engine** rather than by an agent because uniqueness is a property of the
/// thing that can see all of them
///
/// it is not a token and grants nothing. it names a session for as long as the
/// engine holds one, and a request naming an id no session has is refused
/// rather than answered from whichever session is nearest
///
/// it does not cross the control connection and is not serialised anywhere. the
/// debuggee has no use for it — a stop is named where it **arrives**, by the
/// engine, which is the only place that can see which connection it arrived on
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(NonZeroU64);

impl SessionId {
    /// the session that number names
    ///
    /// zero is not one of them, which is what keeps a session that was never
    /// minted from being expressible at all
    #[must_use]
    pub const fn new(number: NonZeroU64) -> Self {
        Self(number)
    }

    /// the number, for a front end with one field to carry it in
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "session {}", self.0)
    }
}

/// one session of a debuggee, as a front end lists it
///
/// what makes a second session **learnable**. MCP has no push, so an agent finds
/// out that a program forked into a debugged child by asking; DAP is told, and
/// still has to be able to say what it was told about. the shape is the same
/// either way, in the core, so that the two front ends cannot come to disagree
/// about what a session is
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Joined {
    /// what it is called, and what a request names to reach it
    pub session: SessionId,

    /// whether bpd started this process
    ///
    /// false for a session that arrived on bpd's listener — a debugged fork.
    /// bpd is not its parent, so it has no exit status to read and cannot
    /// terminate it, and a front end that assumed otherwise would offer two
    /// things that are refused
    pub ours: bool,

    /// the stops this session is holding now
    ///
    /// a debugged fork arrives **held**, at the line that forked, so this is
    /// not usually empty
    pub held: Vec<Stop>,

    /// how this session's program ended, or `None` while it is still there
    pub exit: Option<Exit>,
}

/// a request, and the session it is for
///
/// the session is beside the request rather than inside it because it is not
/// something a client asks *about*: every variant of [`Request`] would carry
/// the same field, and a capability that has to be repeated seventeen times is
/// a capability that will one day be repeated sixteen
///
/// naming none is the ordinary case and means the only session there is — see
/// [`only_session`], which refuses rather than picks when there is more than one
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addressed {
    /// the session it is for, or `None` for the only one open
    pub session: Option<SessionId>,
    /// what is being asked
    pub request: Request,
}

impl Addressed {
    /// this request, for whichever session is the only one open
    #[must_use]
    pub const fn unnamed(request: Request) -> Self {
        Self {
            session: None,
            request,
        }
    }

    /// this request, for one named session
    #[must_use]
    pub const fn to(session: SessionId, request: Request) -> Self {
        Self {
            session: Some(session),
            request,
        }
    }

    /// this request, for the session of the stop it is about
    ///
    /// what a front end addresses with. a request that is about a stop belongs
    /// to the session that stop was reported from, and the stop says which —
    /// which is the whole of why [`Stop`] carries one. `known` is the stops the
    /// front end has been told about
    ///
    /// a request that is about the **program** rather than one of its stopped
    /// threads names no session, and so does one naming a stop that `known`
    /// does not hold or holds twice. in each of those the number does not name
    /// one session, and naming none is what makes the engine refuse rather than
    /// this guess
    ///
    /// it lives here rather than in an adapter because both of them have to do
    /// it, and two front ends deciding separately which session a call is for
    /// is how the same call comes to mean two things
    #[must_use]
    pub fn of(request: Request, known: &[Stop]) -> Self {
        let session = request.stop().and_then(|wanted| {
            let mut named = known.iter().filter(|held| held.stop == wanted);
            match (named.next(), named.next()) {
                (Some(only), None) => Some(only.session),
                // two agents both count their stops from one, so two sessions
                // can hold the same number and it then names two stops
                _ => None,
            }
        });
        Self { session, request }
    }
}

/// everything a client can ask of a debug session
///
/// deliberately closed. `#[non_exhaustive]` would let a front end absorb a
/// capability it does not implement into a catch-all arm, and a capability that
/// exists in one adapter and silently not in the other is the exact thing this
/// enum is here to make impossible
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// replace the whole breakpoint set, and say how every one of them resolved
    ///
    /// the complete set rather than a delta: a debugger that accumulates edits
    /// has two ideas of what is set, and they diverge
    SetBreakpoints {
        /// every breakpoint that should be armed after this request
        breakpoints: Vec<SourceBreakpoint>,
    },

    /// stop where an exception is raised, or where one leaves the program
    ///
    /// the whole setting rather than a delta, for the same reason
    SetExceptionBreakpoints {
        /// stop where an exception is raised, whether or not it is caught
        raised: bool,
        /// stop where an exception leaves the outermost frame
        uncaught: bool,
    },

    /// decide whether a forked child of the program becomes a session of its own
    ///
    /// **off by default, and it stays that way.** a fork copies the agent, the
    /// breakpoint table and the control connection's descriptors into a process
    /// with none of the thread that reads them, so a child either gives the
    /// whole session up — which is what off means, and what happens without
    /// this — or opens a connection of its own and is held at the line that
    /// forked
    ///
    /// on means a child **stops**, and something has to be able to resume it. a
    /// front end that cannot address a second session must not turn this on: a
    /// child held by a debugger nothing can reach is a hung program, which is
    /// worse than an undebugged one
    ///
    /// it reaches the child through inherited memory rather than through the
    /// environment, so it changes nothing a program can see about itself — and
    /// it has to be set **before** the fork, because the handler that acts on
    /// it runs inside `os.fork()` with nothing left to ask
    DebugChildren {
        /// whether a forked child reconnects
        on: bool,
    },

    /// let every held thread go and wait for what the program does next
    ///
    /// the whole-program "continue": it resumes everything held rather than one
    /// thread, and what it waits for is the program rather than a thread
    Run {
        /// how long to wait before answering that it is still running
        ///
        /// `None` waits for as long as the program takes, which is what an
        /// event driven front end does — it has an event stream to report a
        /// stop on whenever it arrives. a front end whose answer *is* the stop
        /// has to bound the wait, or a program that never stops is a call that
        /// never returns
        deadline: Option<Duration>,
    },

    /// wait for the next thing the program does, resuming nothing
    ///
    /// what a step is followed by. a step lets one thread go and returns, and
    /// where it landed arrives as a stop of its own
    Wait {
        /// how long to wait before answering that it is still running
        ///
        /// `None` waits for as long as the program takes — see
        /// [`Request::Run`]
        deadline: Option<Duration>,
    },

    /// let held threads go, without waiting for what they do next
    Resume {
        /// which of the held threads to let go
        which: Which,
    },

    /// let one held thread go with a step armed on it
    ///
    /// it names the **stop** rather than the thread, because a step is about
    /// the frame that stop is held in
    Step {
        /// the stop whose thread to step
        stop: u64,
        /// which way
        kind: StepKind,
    },

    /// hold the next thread of the debuggee that reaches a line
    ///
    /// the only request that is made to a program with **nothing held**
    Pause,

    /// what every thread of the debuggee is doing
    ///
    /// the only request that is about threads `bpd` is **not** holding, and
    /// everything it says about one is a sample
    Threads {
        /// how far apart to take the two samples a thread's progress compares
        settle: Duration,
    },

    /// hold every thread that can be held, until the asking stop is resumed
    StopTheWorld {
        /// the stop asking, which is the one whose resume releases the world
        stop: u64,
        /// how long to wait for the other threads to arrive
        settle: Duration,
    },

    /// walk one held thread's frame chain
    Stack {
        /// the stop whose thread to walk
        stop: u64,
        /// how many frames to report, counting from the one that stopped
        ///
        /// `None` is all of them. the answer says how deep the stack really is
        /// either way
        top: Option<u32>,
    },

    /// read one scope of one frame
    Variables {
        /// which frame
        frame: FrameId,
        /// which scope of it
        scope: Scope,
        /// how much of each value to read
        detail: Detail,
    },

    /// read the django template context of a template frame, layer by layer
    ///
    /// a template frame has no python scopes, so [`Request::Variables`] is not
    /// what reads it. what it has is a `django.template.Context`, which is a
    /// **stack of dicts**: the builtins django pushes, the dictionary the render
    /// was given, and one more for every `{% with %}`, `{% for %}` or
    /// `{% include ... with %}` that is open
    ///
    /// the layers are reported as layers rather than merged, because a name in
    /// two of them is exactly what someone debugging a template is usually
    /// looking at, and a merged mapping cannot show it
    TemplateContext {
        /// which template frame
        frame: FrameId,
        /// how much of each value to read
        detail: Detail,
    },

    /// evaluate a python expression in a frame
    ///
    /// this runs the program's own code, by request. an expression that raises
    /// is answered with the exception
    ///
    /// **what the expression means depends on the frame it is evaluated in.**
    /// against a python frame it is python. against a django template frame it
    /// is template syntax, resolved by django's own rules — dictionary key,
    /// then attribute, then list index, with callables invoked and filters
    /// applied — because that is what the same text means where the user is
    /// looking. python in a template frame is reached by naming the python
    /// frame underneath it, which [`crate::FrameKind::Template`] carries
    Evaluate {
        /// which frame it is evaluated in
        frame: FrameId,
        /// the expression, as the client wrote it
        expression: String,
        /// how much of the result to read
        detail: Detail,
    },

    /// start or stop recording where the program goes
    ///
    /// **the one mode that turns off the property the rest of the design rests
    /// on.** a location is normally disabled the first time it is seen — six
    /// callbacks for nine hundred thousand line executions — and a recorder
    /// needs every one of them, which measured at 4× a bare run for the delivery
    /// alone. so it is off by default and asked for by somebody who knows
    ///
    /// it records **where** and never what: a copy of the locals per line costs
    /// five times as much again and is unbounded, and a recorder that
    /// interpolated the values it did not capture would be inventing history
    Record {
        /// whether to record
        on: bool,
    },

    /// the window of where the program has been
    Trail,

    /// what is holding an object, and how
    ///
    /// "why is this still alive". the object is named by an expression, in a
    /// frame, the way [`Self::Evaluate`] names one — there is nothing else a
    /// client could point at, since an object has no id of its own that survives
    /// being asked about
    ///
    /// the answer carries what the walk **cannot** see, always. a walk over the
    /// collector's referent graph is blind to untracked objects and to holders
    /// that are not python objects at all — bpd's own among them — and a list of
    /// holders without that is a different question's answer
    Retainers {
        /// which frame the expression is evaluated in
        frame: FrameId,
        /// the expression naming the object, as the client wrote it
        expression: String,
    },

    /// run a whole investigation against a session, and return what happened
    ///
    /// a tree of debugger steps with its own branching, executed **here** —
    /// only the predicates inside it reach the debuggee, through the machinery
    /// a breakpoint condition already uses. so the program under test is
    /// disturbed by exactly the evaluations that were asked for and nothing else
    ///
    /// the answer is the [`Transcript`], not the final state: a client given
    /// only where a script ended cannot tell **why**, and will guess
    RunScript {
        /// the stop whose thread the script drives
        ///
        /// a script drives **one thread**, for the reason a stop holds one. it
        /// resumes that thread and no other, so a script never lets go of a
        /// thread nobody named
        stop: u64,
        /// the steps, and what they may spend
        script: Script,
    },

    /// describe a stop's state in one call, and keep the answer to compare later
    ///
    /// the declarative form of the tree walk. instead of a stack, then the
    /// scopes of a frame, then the variables of a scope, then the variables
    /// again for each nested object — four or more round trips — the query says
    /// what is wanted and is answered with it
    ///
    /// it is **composed of the same requests** the walk is made of, so the two
    /// cannot disagree about a value. what it removes is the round trips
    ///
    /// the answer is kept, under a content addressed id, so that
    /// [`Request::Diff`] can compare two of them. every query is kept rather
    /// than only the ones a client asks to keep: whether a state is worth
    /// comparing is not knowable when it is read
    Query {
        /// the stop to describe
        stop: u64,
        /// what to describe about it
        query: StateQuery,
    },

    /// what changed between two states this session read
    ///
    /// the difference is the answer. shipping both states and leaving the
    /// comparison to the client is what this exists instead of
    ///
    /// a snapshot does not go stale. it is a reading that was already taken
    /// rather than a promise to take one, so it stays true across any number of
    /// resumes — what ends with its stop is the ability to ask that stop
    /// anything more
    Diff {
        /// the state to compare from
        before: SnapshotId,
        /// the state to compare to
        after: SnapshotId,
    },

    /// move the executing frame to another line of the code it is running
    ///
    /// the program is not resumed by it: the thread is still held, at the line
    /// it was moved to, and what runs next is whatever it is asked to do next.
    /// the code between where it was and where it is now is **not** executed,
    /// and neither is the cleanup of any block the move leaves — see
    /// [`crate::Jumped`]
    ///
    /// only in the frame the thread is executing. a frame below the top is
    /// suspended in a call, and cpython accepts a move in one rather than
    /// refusing it, so the refusal is `bpd`'s
    SetNextStatement {
        /// which frame — the one its thread is executing, or a refusal
        frame: FrameId,
        /// the line of that frame's file to move to
        line: u32,
    },

    /// re-enter a frame from the top
    ///
    /// [`Request::SetNextStatement`] to the line of the first instruction of
    /// the frame's code object, worked out in the debuggee because the code
    /// object is the only thing that knows it
    ///
    /// it re-enters with **what the parameters hold now**, which is not
    /// necessarily what the frame was called with: a parameter the frame has
    /// since assigned to holds the new value, and nothing captured the old one.
    /// capturing them would mean copying every argument of every call in the
    /// process, on the event path, for an operation almost nobody makes
    ///
    /// side effects the frame already performed are not undone. nothing here
    /// can undo them, and a debugger that implied otherwise would be inviting a
    /// belief about the program that is false
    RestartFrame {
        /// which frame — the one its thread is executing, or a refusal
        frame: FrameId,
    },

    /// replace the code the process is running for one file with the code on
    /// disk
    ///
    /// a set of assignments to `function.__code__`, and nothing else. the top
    /// level is **not** re-run, no name is bound or unbound, and no object is
    /// created — so a class's methods come with it, because a method is a
    /// function object in the class dictionary and every instance that already
    /// exists reaches the same one
    ///
    /// it is applicable exactly when every difference between the file on disk
    /// and the code that is running is inside the body of a function that
    /// exists in both and takes the same arguments. anything else is refused
    /// with what blocked it, and **nothing is applied partially** — see
    /// [`crate::Replaced`]
    ReplaceCode {
        /// the file whose code to replace, on the debuggee's own filesystem
        file: PathBuf,

        /// apply it even where a frame is running the code being replaced
        ///
        /// **off by default, and that default is the guarantee.** a replacement
        /// made under a live frame leaves the process running two versions of
        /// one function until that frame returns, and a stack whose frames
        /// behave two different ways is evidence about neither — so the ordinary
        /// answer is a refusal naming every frame that stood in the way
        ///
        /// asking for it turns those refusals into a **report**: the replacement
        /// is applied and every frame still on the old code is named. that is a
        /// weaker guarantee and it is the caller's to want, which is why it is a
        /// field rather than a change of behaviour
        ///
        /// the report is true when it is made and not afterwards. a frame on the
        /// list returns on its own schedule, and nothing tells the client when
        /// one has — so it says which frames were still on the old code at the
        /// moment of the replacement, and a caller reading it as the state of
        /// the process now is reading it wrong. see [`crate::StillRunning`]
        even_under_a_live_frame: bool,
    },

    /// write a variable of a frame, and read back what the frame holds after it
    SetVariable {
        /// which frame
        frame: FrameId,
        /// which scope of it
        scope: Scope,
        /// the name to write
        name: String,
        /// a python expression, evaluated in that frame, for the new value
        value: String,
        /// how much of the value read back to report
        detail: Detail,
    },

    /// what the debugger can **prove** about some of a frame's names
    ///
    /// [`Request::Variables`] answers what a scope holds right now, which is a
    /// statement about a moment. this answers what is true of a name *and* how
    /// far past that moment the answer can be carried — because a client
    /// reasoning about code that has not run yet needs the second half and
    /// cannot derive it. only something holding the object can say whether a
    /// reading of it can go stale, and that is the whole of why this is a
    /// capability of the debugger rather than something a client works out
    ///
    /// the names are named rather than "all of them": a client asking this is
    /// analysing a region of source and knows which names that region mentions,
    /// and every other local in the frame is a read nobody asked for. a name
    /// may be a dotted path, and every segment of one is read out of an
    /// object's own storage or not at all
    ///
    /// **it runs none of the program.** a reading that would need `__bool__`,
    /// `__len__`, a property or a `__getattr__` is not taken and not guessed
    /// at — the name comes back in [`crate::Facts::silent`] naming what would
    /// have run. see [`crate::fact`]
    Facts {
        /// which frame
        frame: FrameId,
        /// the names to prove things about, each a name or a dotted path
        names: Vec<String>,
        /// how much one fact may cost
        limit: crate::fact::Limit,
    },
}

impl Request {
    /// what to call this request in a message about it
    ///
    /// a front end has to name a capability in an error, and a refusal that
    /// said `Step { stop: 3, kind: Over }` would be reporting rust at somebody.
    /// the match is exhaustive and has no catch-all arm, for the reason the
    /// enum is closed
    pub const fn name(&self) -> &'static str {
        match self {
            Self::SetBreakpoints { .. } => "setting the breakpoints",
            Self::SetExceptionBreakpoints { .. } => "setting the exception breakpoints",
            Self::DebugChildren { .. } => "debugging the program's forked children",
            Self::Run { .. } => "running the program",
            Self::Wait { .. } => "waiting for the program",
            Self::Resume { .. } => "resuming a thread",
            Self::Step { .. } => "stepping a thread",
            Self::Pause => "pausing the program",
            Self::Threads { .. } => "the thread census",
            Self::StopTheWorld { .. } => "stopping the world",
            Self::Stack { .. } => "the stack",
            Self::Variables { .. } => "the variables of a scope",
            Self::TemplateContext { .. } => "the template context of a frame",
            Self::Evaluate { .. } => "evaluating an expression",
            Self::RunScript { .. } => "running a debug script",
            Self::Query { .. } => "the state of a stop",
            Self::Diff { .. } => "the difference between two states",
            Self::SetVariable { .. } => "writing a variable",
            Self::Facts { .. } => "what is provable about a frame's names",
            Self::Record { .. } => "recording where the program goes",
            Self::Trail => "where the program has been",
            Self::Retainers { .. } => "what is holding an object",
            Self::ReplaceCode { .. } => "replacing a file's code",
            Self::SetNextStatement { .. } => "setting the next statement",
            Self::RestartFrame { .. } => "restarting a frame",
        }
    }

    /// the stop this request is about, when it is about one
    ///
    /// what [`Addressed::of`] addresses by. a request that is about one held
    /// thread names its stop, directly or through the [`FrameId`] of a frame of
    /// it; one that is about the whole program names none, and neither does
    /// [`Request::Diff`] — it compares two readings that were already taken,
    /// and they are not required to be of the same stop
    ///
    /// the match is exhaustive and has no catch-all arm, for the reason
    /// [`Request::name`] is: a capability added to the enum has to say whether
    /// it is about a stop rather than inherit an answer
    pub const fn stop(&self) -> Option<u64> {
        match self {
            Self::SetBreakpoints { .. }
            | Self::SetExceptionBreakpoints { .. }
            // about the whole process, and about a process that does not exist
            // yet at that
            | Self::DebugChildren { .. }
            // about the whole program rather than one held thread
            | Self::Record { .. }
            | Self::Trail
            | Self::Run { .. }
            | Self::Wait { .. }
            | Self::Resume { .. }
            | Self::Pause
            | Self::Threads { .. }
            // about the process rather than about one held thread. it is
            // answered on a held thread, like everything the agent answers, and
            // which one makes no difference to the answer
            | Self::ReplaceCode { .. }
            | Self::Diff { .. } => None,

            Self::Step { stop, .. }
            | Self::StopTheWorld { stop, .. }
            | Self::Stack { stop, .. }
            | Self::RunScript { stop, .. }
            | Self::Query { stop, .. } => Some(*stop),

            Self::Variables { frame, .. }
            | Self::TemplateContext { frame, .. }
            | Self::Evaluate { frame, .. }
            | Self::SetVariable { frame, .. }
            | Self::SetNextStatement { frame, .. }
            | Self::Facts { frame, .. }
            | Self::Retainers { frame, .. }
            | Self::RestartFrame { frame } => Some(frame.stop),
        }
    }
}

/// what a running debuggee says that is not the answer to a [`Request`]
///
/// a logpoint's record, a pause's acknowledgement and a child process all
/// arrive while the program is running, so none of them answers anything a
/// client is waiting on. they are handed over as they arrive rather than
/// accumulated: there is no bound on how many records a logpoint produces, or
/// on how many children a program starts, and a debugger that buffered a
/// million of either before saying anything would be holding the program's
/// history in its own heap
///
/// there is no default body on any of these, and there is not going to be one.
/// every front end has to say what it does with each, because a front end that
/// silently dropped one would be the only place a fact about the program went
/// missing
pub trait Reporting {
    /// a logpoint produced a record
    fn logged(&mut self, record: LogRecord);

    /// a pause is armed, and these threads were running python when it was
    ///
    /// an empty list means the pause is armed and **nothing is going to
    /// arrive** until some thread runs python again: every thread is parked in
    /// a C call, where there is no monitoring event to hold one at
    fn pausing(&mut self, running: Vec<u64>);

    /// the program started a child process that could be python
    ///
    /// `bpd` debugs one process. a child is not debugged and is not blocked
    /// either — it runs exactly as it would have — so this is the only thing
    /// that stands between a user and a session pointed at a supervisor that
    /// does none of the work. see [`Spawn`]
    fn spawned(&mut self, child: Spawn);

    /// there is a way of starting a child this interpreter does not let `bpd`
    /// see
    ///
    /// its own method rather than a kind of [`Self::spawned`], because it is
    /// the opposite claim: [`Self::spawned`] says a child exists, and this says
    /// that a silence is about to stop being evidence. a front end that
    /// rendered one as the other would be reporting a child that was never
    /// started
    ///
    /// this is what keeps "`bpd` says so when it cannot know" true of a
    /// feature whose normal output is silence. see [`Blindspot`]
    fn blind_to(&mut self, blindspot: Blindspot);

    /// another agent joined this debuggee, and it is a session of its own
    ///
    /// what [`Request::DebugChildren`] produces: a forked child opened a
    /// connection of its own and is **held**, at the line that forked. it is
    /// news rather than an answer — the program forked without asking — and it
    /// is the only thing that tells a front end a second session exists at all
    ///
    /// a front end that ignored one would leave a stopped process nothing can
    /// reach, which is a hung program. that is why there is no default body
    /// here any more than on the others
    fn attached(&mut self, session: SessionId);
}

/// what a session answered a [`Request`] with
///
/// closed for the reason [`Request`] is. two requests can share an answer —
/// a step is a resume with instrumentation, and both are acknowledged by naming
/// the threads that were let go
#[derive(Debug)]
pub enum Response {
    /// how every breakpoint of the requested set resolved
    BreakpointsResolved {
        /// one entry per breakpoint in the request
        resolved: Vec<Resolved>,
    },

    /// what the exception breakpoints are set to now
    ExceptionBreakpoints(ExceptionBreakpoints),

    /// what a forked child of the program will do from now on
    ///
    /// read back off the agent rather than echoed from the request: what is set
    /// is what the process that will fork says is set
    DebuggingChildren {
        /// whether a forked child becomes a session of its own
        on: bool,
    },

    /// what the program did next
    Ran(Running),

    /// the threads that were let go
    Resumed {
        /// the threads that are running again
        threads: Vec<u64>,
    },

    /// a pause is armed, and these threads were running python when it was
    ///
    /// empty means the pause is armed and **nothing is going to arrive** until
    /// some thread runs python again
    Pausing {
        /// the threads that were running python when the pause was armed
        running: Vec<u64>,
    },

    /// what every thread of the debuggee was doing
    Threads(Threads),

    /// what stopping the world managed to stop
    WorldStopped(WorldStopped),

    /// one held thread's stack
    Stack(Stack),

    /// what one scope of one frame holds
    Variables(Variables),

    /// what is provable about a frame's names, and for how long
    Facts(crate::fact::Facts),

    /// what a template frame's django context holds, layer by layer
    TemplateContext(TemplateContext),

    /// what an expression did, or what a write left behind
    Evaluated(Evaluated),

    /// what a jump did, and where the frame is now
    ///
    /// the same answer for both jumps: they differ in where the line comes
    /// from, and a client is told the same things about either — where the
    /// frame is now, what the move bound to `None`, and which breakpoints on
    /// the destination will not fire for this pass
    Jumped(Jumped),

    /// what replacing a file's code did to the process, or what stopped it
    Replaced(Replaced),

    /// what a debug script did, step by step
    Transcript(Transcript),

    /// a stop's state, at the level of detail the query asked for
    State(Snapshot),

    /// whether recording is on, and what the window holds
    Recording {
        /// whether it is recording now
        on: bool,
        /// how many steps the window holds
        held: u64,
        /// how many fell out of it
        dropped: u64,
    },

    /// where the program has been
    Trail(crate::frame::Trail),

    /// what is holding an object, and what the walk could not see
    Retainers(crate::frame::Retainers),

    /// what changed between two of them
    Difference(Difference),
}

/// whether everything a program wrote had arrived by the time it was reported
/// over
///
/// a front end that leaves the debuggee's streams inherited — `bpd launch` —
/// has nothing to answer for: the program wrote to the terminal itself, and the
/// order is the kernel's. one that reads them off a pipe does, because it learns
/// the program exited on a **different** descriptor and the last bytes of the
/// pipe can still be unread when it does. a client told the program is over and
/// then handed another line of its output has been told the run finished before
/// it did
///
/// so the wait for the pipe is bounded and its outcome is carried here rather
/// than assumed. it cannot be unbounded: a **forked child** inherits the write
/// end, so a program whose child outlives it never reaches end-of-file, and a
/// debugger that waited for one would hang at the exit of every program that
/// leaves a daemon behind
/// no `Serialize`: this never goes on a wire. it is engine-to-front-end, in one
/// process, and each front end renders it in its own protocol's shape — a
/// console line for DAP, a boolean field for MCP
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forwarded {
    /// every byte the program wrote is already where it was going
    ///
    /// either the streams were never bpd's to carry, or the pipe reached
    /// end-of-file and what read it is finished
    Everything,

    /// the program is gone and something still holds the stream it wrote to
    ///
    /// what a forked child that outlives its parent looks like from here. it is
    /// not a failure and nothing has been dropped — what is still coming is the
    /// **child's** output, and it keeps being forwarded. what it costs is the
    /// order: a line arriving after this point cannot be said to have been
    /// written before the program ended, so a front end says so rather than
    /// letting a reader assume it
    StillHeldOpen,
}

/// what a resumed debuggee did next
///
/// deliberately closed: a third outcome is something every caller has to decide
/// about, and a catch-all arm is how a debugger acquires a state nobody handles
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
        /// whether what the program wrote had all been forwarded by now
        ///
        /// carried rather than assumed, because a front end that reads the
        /// debuggee's output off a pipe learns the program is over on a
        /// **different** descriptor — the control connection — and the two have
        /// no order between them. see [`Forwarded`]
        output: Forwarded,
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

    /// the debuggee's connection closed, and its exit is **not bpd's to read**
    ///
    /// a session bpd launched ends with [`Self::Exited`], because bpd holds the
    /// child and can wait on it. a session that arrived on bpd's listener from
    /// a process bpd did not start has no child: bpd is not its parent, so it
    /// cannot reap it and never learns what it exited with
    ///
    /// so this says the program is over and says nothing about how, which is
    /// the whole of why it is a variant rather than an [`Self::Exited`] with a
    /// number in it. the number would be invented
    Ended {
        /// what loading a file changed about the breakpoint set on the way
        rebound: Vec<Resolved>,
    },

    /// the deadline passed and the program is **still running**
    ///
    /// only ever the answer to a [`Request::Run`] or [`Request::Wait`] that
    /// carried a deadline. it is not a stop and must never be rendered as one:
    /// no thread is held, nothing was read off the program, and the program is
    /// executing while this is being read
    ///
    /// nothing about the running threads is carried, and that is a limit rather
    /// than an omission. everything the agent answers, it answers on a thread it
    /// is holding — including the thread census — so a program with nothing held
    /// cannot be asked what its threads are doing. arming a [`Request::Pause`]
    /// is what turns one into something that can be asked
    StillRunning {
        /// how long was actually waited before giving up
        ///
        /// the measured wait rather than the deadline that was asked for, so a
        /// caller can tell a deadline that was reached from one that was
        /// overshot
        waited: Duration,
        /// what loading a file changed about the breakpoint set on the way
        rebound: Vec<Resolved>,
    },
}

/// the program's exit, as one number a front end can show
///
/// a signalled child becomes `128 + signal`, which is the number a shell
/// reports for one — so what a client is shown is what a terminal would have
/// shown. the convention lives here rather than in a front end because two
/// front ends choosing their own would make the same exit read as two different
/// numbers
pub fn exit_code(status: ExitStatus) -> i64 {
    if let Some(code) = status.code() {
        return code.into();
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return i64::from(128 + signal);
        }
    }
    unreachable!("an exit status is either a code or a signal, and this was {status}")
}

/// how a session's program ended
///
/// deliberately closed, and deliberately not an `i64` with a sentinel. bpd
/// knows an exit code when it started the process and holds the child; when the
/// process connected to bpd's listener instead, bpd is not its parent and never
/// learns one. those are different facts and a front end has to be able to tell
/// them apart, because "exited with 0" and "over, and what with is unknown"
/// lead a reader to opposite conclusions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// what the program exited with, as one number — see [`exit_code`]
    Code(i64),
    /// the program is over and bpd cannot say what it exited with
    Unknown,
}

impl std::fmt::Display for Exit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Code(code) => write!(formatter, "{code}"),
            Self::Unknown => formatter.write_str("an exit bpd cannot read"),
        }
    }
}

/// the one stop that is held, for a request that is about one thread
///
/// refuses rather than picking when several are held. a debugger that answered
/// about whichever thread came first would be answering a question nobody asked
///
/// this lives here rather than in a front end because a stop holds one thread
/// and several can be held at once — so *every* front end has to decide what a
/// request that names no stop means, and two front ends deciding it separately
/// is how the same call comes to mean two things
///
/// `exit` is how the program ended, when it has. nothing held has causes that
/// need opposite things done about them — the program is running and has to be
/// stopped, or it is over and there is nothing to stop — and a caller that
/// knows which has to say so, because this cannot tell from `held` alone
pub fn only_stop(held: &[Stop], exit: Option<Exit>, wanted: &'static str) -> crate::Result<u64> {
    match held {
        [] => Err(match exit {
            Some(Exit::Code(code)) => crate::Error::ProgramExited { code, wanted },
            Some(Exit::Unknown) => crate::Error::ProgramEnded { wanted },
            None => crate::Error::NotStopped { wanted },
        }),
        [stop] => Ok(stop.stop),
        several => Err(crate::Error::AmbiguousStop {
            wanted,
            held: several.iter().map(|stop| stop.stop).collect(),
        }),
    }
}

/// the one session a request is for, when it does not name one
///
/// the sibling of [`only_stop`], and the same rule one level up: refuse rather
/// than pick. a debugger that answered a request from whichever session came
/// first would be reporting one program's state as another's, which is the
/// worst thing in this project's list of things it will not do
///
/// it lives here for the reason [`only_stop`] does. every front end has to
/// decide what a request naming no session means, and two of them deciding
/// separately is how the same call comes to mean two things
///
/// `open` is every session the engine holds, and `named` is what the request
/// asked for. an id no open session has is refused rather than resolved to the
/// nearest one: it names a session that has ended or one this engine never
/// minted, and both are things the caller has to know
pub fn only_session(
    open: &[SessionId],
    named: Option<SessionId>,
    wanted: &'static str,
) -> crate::Result<SessionId> {
    if let Some(named) = named {
        return if open.contains(&named) {
            Ok(named)
        } else {
            Err(crate::Error::NoSuchSession {
                named,
                open: open.to_vec(),
                wanted,
            })
        };
    }

    match open {
        // whatever holds sessions refuses before it routes among them: a front
        // end with nothing launched has nothing to be asked, and the engine
        // holds a session for as long as a debuggee exists
        [] => unreachable!(
            "{wanted} was routed among no sessions at all, and a debugger \
             holding none has nothing to answer it with"
        ),
        [only] => Ok(*only),
        several => Err(crate::Error::AmbiguousSession {
            wanted,
            open: several.to_vec(),
        }),
    }
}

/// one held thread's stack
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    /// the frames, the one that stopped first
    pub frames: Vec<Frame>,
    /// where the task this stack is inside was created, innermost first
    ///
    /// **a separate list, and never spliced into [`Self::frames`].** the frames
    /// here did not call the ones above — they *scheduled* them, and the real
    /// caller of the running frame is the event loop. presenting one seamless
    /// stack would be a call chain that never happened, which is the exact lie
    /// this project exists not to tell
    ///
    /// empty when the stop is not inside a task, when nothing recorded how that
    /// task was made, or when the program is not running asyncio at all. it is
    /// a record rather than live frames — see [`Scheduling`]
    pub scheduled_by: Vec<Scheduling>,

    /// whether this stack is inside an asyncio task at all
    ///
    /// what stops an empty [`Self::scheduled_by`] meaning two different things.
    /// a stack that is not in a task has nothing to say; a stack that **is** in
    /// one and carries no record is a task bpd did not see created — a route it
    /// does not watch yet — and a client shown the same empty list for both
    /// would read the second as the first
    ///
    /// this is the same rule the blind spot on 3.13 follows: the silence is
    /// announced rather than left to be interpreted
    pub in_a_task: bool,
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

/// a django template context, as the stack of dicts it really is
///
/// never flattened. django resolves a name by walking the layers from the last
/// one backwards and taking the first that holds it, so two layers holding the
/// same name is a shadowing that decides what the template renders — and a
/// merged mapping is a report in which that has already happened invisibly
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateContext {
    /// the layers, outermost first, in `Context.dicts` order
    ///
    /// the order django pushed them, so the **last** one wins a lookup
    pub layers: Vec<ContextLayer>,
    /// how the program was moving while this was taken
    pub mode: Mode,
}

impl TemplateContext {
    /// what a name resolves to, and which layer answers it
    ///
    /// the walk django's own `Context.__getitem__` does: from the last layer
    /// backwards, the first that holds the name. it is here rather than in a
    /// front end because two front ends deciding the direction separately is how
    /// the same context comes to report two different values
    pub fn resolve(&self, name: &str) -> Option<(&ContextLayer, &Value)> {
        self.layers
            .iter()
            .rev()
            .find_map(|layer| layer.get(name).map(|value| (layer, value)))
    }

    /// every name that more than one layer holds, with the layers that hold it
    ///
    /// reported rather than left to be worked out: shadowing is what a layered
    /// context is read for
    pub fn shadowed(&self) -> Vec<Shadowed> {
        let mut names: Vec<&str> = self
            .layers
            .iter()
            .flat_map(|layer| layer.entries.iter().map(|entry| entry.name.as_str()))
            .collect();
        names.sort_unstable();
        names.dedup();

        names
            .into_iter()
            .filter_map(|name| {
                let layers: Vec<u32> = self
                    .layers
                    .iter()
                    .filter(|layer| layer.get(name).is_some())
                    .map(|layer| layer.index)
                    .collect();
                (layers.len() > 1).then(|| Shadowed {
                    name: name.to_string(),
                    layers,
                })
            })
            .collect()
    }
}

/// one dict of a django template context
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextLayer {
    /// how far up the stack it is, counting from zero at the outermost
    ///
    /// the index into `Context.dicts`, so a client can say which layer it means
    /// back to django
    pub index: u32,
    /// what the layer holds
    pub entries: Vec<Entry>,
    /// everything this layer's answer left out, and why
    pub omitted: Vec<Omitted>,
}

impl ContextLayer {
    /// what one name holds in this layer, or `None` when it does not hold it
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| &entry.value)
    }
}

/// a name more than one layer of a template context holds
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shadowed {
    /// the name
    pub name: String,
    /// the layers holding it, outermost first. the **last** is the one that wins
    pub layers: Vec<u32>,
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
    /// how far apart to take the two samples when the client has no way to say
    ///
    /// DAP's `threads` request carries no interval and neither does anything an
    /// agent would naturally ask, so a front end has to supply one. it lives
    /// here rather than in an adapter because [`crate::Progress::Still`] means
    /// "in the same place, this far apart", and two adapters choosing their own
    /// interval would make the same word mean two things
    ///
    /// long enough that a thread going round an ordinary python loop is seen to
    /// move, and short enough that asking for a thread list does not feel like
    /// a pause
    pub const SETTLE: Duration = Duration::from_millis(50);

    /// what one thread was doing, when the census saw it
    pub fn get(&self, thread: u64) -> Option<&ThreadState> {
        self.threads.iter().find(|state| state.thread == thread)
    }
}

/// what the debuggee stops for, of the exceptions it raises
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExceptionBreakpoints {
    /// stopping where an exception is raised, whether or not it is caught
    pub raised: bool,
    /// stopping where an exception leaves the outermost frame
    pub uncaught: bool,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stop::{Reported, StopReason};

    /// the session a stop with no session of its own is put in
    fn first() -> SessionId {
        session(1)
    }

    fn session(number: u64) -> SessionId {
        SessionId::new(NonZeroU64::new(number).expect("a session is numbered from one"))
    }

    fn held_at(stop: u64) -> Stop {
        held_in(first(), stop)
    }

    fn held_in(session: SessionId, stop: u64) -> Stop {
        Reported {
            stop,
            thread: 7,
            reason: StopReason::Entry,
            holding: Vec::new(),
        }
        .in_session(session)
    }

    #[test]
    fn a_request_that_names_no_stop_is_refused_rather_than_answered_about_one_of_several() {
        assert_eq!(
            only_stop(&[held_at(3)], None, "the stack").expect("one is held"),
            3
        );

        let nothing = only_stop(&[], None, "the stack").expect_err("nothing is held");
        let said = nothing.to_string();
        assert!(
            said.contains("the stack"),
            "the refusal has to name what was asked for, and said {said}"
        );
        // an agent told only the cause is left to work out that a debugger with
        // nothing held is a debugger that has to hold something first
        assert!(
            said.contains("breakpoint") && said.contains("pausing it"),
            "the refusal has to name what to do about it, and said {said}"
        );

        // answering from whichever stop came first would be answering about a
        // thread the caller did not name
        let several =
            only_stop(&[held_at(3), held_at(4)], None, "the stack").expect_err("two are held");
        let said = several.to_string();
        assert!(said.contains("[3, 4]"), "said {said}");
        assert!(said.contains("name the stop"), "said {said}");
    }

    #[test]
    fn nothing_held_because_the_program_ended_is_not_the_same_refusal() {
        // "nothing is held" invites holding something. a program that has ended
        // cannot be held at all, and a client told the first would go on pausing
        // a process that is not there
        let over = only_stop(&[], Some(Exit::Code(3)), "the stack").expect_err("nothing is held");
        let said = over.to_string();
        assert!(
            said.contains("exited with 3"),
            "the refusal has to name what became of the program, and said {said}"
        );
        assert!(
            !said.contains("pausing it"),
            "there is nothing left to pause, and it said {said}"
        );

        // and an exit is only ever the reason when nothing is held. a stop that
        // is held is a thread that is still there to answer
        assert_eq!(
            only_stop(&[held_at(3)], Some(Exit::Code(0)), "the stack").expect("one is held"),
            3
        );
    }

    #[test]
    fn a_request_that_names_no_session_is_the_only_one_and_is_refused_when_there_are_two() {
        assert_eq!(
            only_session(&[session(1)], None, "the stack").expect("one is open"),
            session(1)
        );

        // the whole of why this exists. two agents both count their stops from
        // one, so a request that named neither session and was answered from
        // whichever came first would report one program's state as another's
        let several =
            only_session(&[session(1), session(2)], None, "the stack").expect_err("two are open");
        let said = several.to_string();
        assert!(
            said.contains("session 1") && said.contains("session 2"),
            "the refusal has to name what is open, and said {said}"
        );
        assert!(
            said.contains("name the session"),
            "the refusal has to say what to do about it, and said {said}"
        );
    }

    #[test]
    fn a_session_id_that_names_nothing_is_refused_rather_than_resolved_to_the_nearest() {
        assert_eq!(
            only_session(&[session(1), session(2)], Some(session(2)), "the stack")
                .expect("session 2 is open"),
            session(2)
        );

        let gone = only_session(&[session(1)], Some(session(9)), "the stack")
            .expect_err("no session of this debugger is 9");
        let said = gone.to_string();
        assert!(
            said.contains("session 9") && said.contains("session 1"),
            "the refusal has to name what was asked for and what is open, and \
             said {said}"
        );
        assert!(
            said.contains("the stack"),
            "the refusal has to name what was asked for, and said {said}"
        );
    }

    #[test]
    fn a_request_about_a_stop_is_addressed_to_the_session_that_stop_came_from() {
        let known = [held_in(session(2), 1), held_in(session(3), 4)];

        let about_a_stop = Addressed::of(Request::Stack { stop: 4, top: None }, &known);
        assert_eq!(
            about_a_stop.session,
            Some(session(3)),
            "stop 4 is session 3's, and the request about it is session 3's"
        );

        // through a frame id, which is the other way a request names a stop
        let about_a_frame = Addressed::of(
            Request::Variables {
                frame: FrameId { stop: 1, depth: 0 },
                scope: Scope::Local,
                detail: Detail::default(),
            },
            &known,
        );
        assert_eq!(about_a_frame.session, Some(session(2)));

        // a request about the program names none, and the only-session rule is
        // what answers it
        assert_eq!(
            Addressed::of(Request::Pause, &known).session,
            None,
            "a pause is about the program rather than about a held thread"
        );
    }

    #[test]
    fn a_stop_number_two_sessions_both_hold_addresses_neither() {
        // an agent counts its stops from one and cannot see another agent doing
        // the same, so the same number really can name two stops. picking the
        // first would be answering about a program nobody named — naming
        // neither is what makes the engine refuse
        let known = [held_in(session(1), 3), held_in(session(2), 3)];
        assert_eq!(
            Addressed::of(Request::Stack { stop: 3, top: None }, &known).session,
            None
        );

        // and a stop the front end has not been told about names no session
        // either. the refusal for that one is the agent's, which lists the
        // stops it really holds
        assert_eq!(
            Addressed::of(Request::Stack { stop: 8, top: None }, &known).session,
            None
        );
    }

    #[test]
    fn exactly_the_requests_that_are_about_one_held_thread_name_a_stop() {
        // written out rather than derived, because this is the list a new
        // capability has to be added to on purpose. one that is about a stop
        // and says it is not would be addressed to no session, and answered by
        // the only-session rule instead of by the stop it is about
        let about_a_stop: Vec<&str> = crate::parity::surface()
            .iter()
            .filter(|request| request.stop().is_some())
            .map(|request| Request::name(request))
            .collect();
        assert_eq!(
            about_a_stop,
            [
                "stepping a thread",
                "stopping the world",
                "the stack",
                "the variables of a scope",
                "the template context of a frame",
                "evaluating an expression",
                "writing a variable",
                "setting the next statement",
                "restarting a frame",
                "the state of a stop",
                "running a debug script",
            ]
        );

        // and every one of them reports the stop that is in it. the surface
        // builds them all against stop 1
        for request in crate::parity::surface() {
            if let Some(stop) = request.stop() {
                assert_eq!(stop, 1, "`{}` reported stop {stop}", request.name());
            }
        }
    }
}
