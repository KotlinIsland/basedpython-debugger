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

use std::process::ExitStatus;
use std::time::Duration;

use crate::breakpoint::{LogRecord, Resolved, SourceBreakpoint};
use crate::frame::{Frame, FrameId, Scope};
use crate::stop::{Mode, StepKind, Stop};
use crate::thread::{ThreadState, Which};
use crate::value::{Detail, Entry, Evaluated, Omitted, Value};

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

    /// let every held thread go and wait for what the program does next
    ///
    /// the whole-program "continue": it resumes everything held rather than one
    /// thread, and what it waits for is the program rather than a thread
    Run,

    /// wait for the next thing the program does, resuming nothing
    ///
    /// what a step is followed by. a step lets one thread go and returns, and
    /// where it landed arrives as a stop of its own
    Wait,

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

    /// evaluate a python expression in a frame
    ///
    /// this runs the program's own code, by request. an expression that raises
    /// is answered with the exception
    Evaluate {
        /// which frame it is evaluated in
        frame: FrameId,
        /// the expression, as the client wrote it
        expression: String,
        /// how much of the result to read
        detail: Detail,
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
            Self::Run => "running the program",
            Self::Wait => "waiting for the program",
            Self::Resume { .. } => "resuming a thread",
            Self::Step { .. } => "stepping a thread",
            Self::Pause => "pausing the program",
            Self::Threads { .. } => "the thread census",
            Self::StopTheWorld { .. } => "stopping the world",
            Self::Stack { .. } => "the stack",
            Self::Variables { .. } => "the variables of a scope",
            Self::Evaluate { .. } => "evaluating an expression",
            Self::SetVariable { .. } => "writing a variable",
        }
    }
}

/// what a running debuggee says that is not the answer to a [`Request`]
///
/// a logpoint's record and a pause's acknowledgement both arrive while the
/// program is running, so neither answers anything a client is waiting on. they
/// are handed over as they arrive rather than accumulated: there is no bound on
/// how many records a logpoint produces, and a debugger that buffered a million
/// of them before saying anything would be holding the program's history in its
/// own heap
pub trait Reporting {
    /// a logpoint produced a record
    fn logged(&mut self, record: LogRecord);

    /// a pause is armed, and these threads were running python when it was
    ///
    /// an empty list means the pause is armed and **nothing is going to
    /// arrive** until some thread runs python again: every thread is parked in
    /// a C call, where there is no monitoring event to hold one at
    fn pausing(&mut self, running: Vec<u64>);
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

    /// what an expression did, or what a write left behind
    Evaluated(Evaluated),
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
