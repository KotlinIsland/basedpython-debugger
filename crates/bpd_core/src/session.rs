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
use crate::query::{Difference, Snapshot, SnapshotId, StateQuery};
use crate::script::{Script, Transcript};
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
            Self::Run { .. } => "running the program",
            Self::Wait { .. } => "waiting for the program",
            Self::Resume { .. } => "resuming a thread",
            Self::Step { .. } => "stepping a thread",
            Self::Pause => "pausing the program",
            Self::Threads { .. } => "the thread census",
            Self::StopTheWorld { .. } => "stopping the world",
            Self::Stack { .. } => "the stack",
            Self::Variables { .. } => "the variables of a scope",
            Self::Evaluate { .. } => "evaluating an expression",
            Self::RunScript { .. } => "running a debug script",
            Self::Query { .. } => "the state of a stop",
            Self::Diff { .. } => "the difference between two states",
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

    /// what a debug script did, step by step
    Transcript(Transcript),

    /// a stop's state, at the level of detail the query asked for
    State(Snapshot),

    /// what changed between two of them
    Difference(Difference),
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
/// `ended` is what the program exited with, when it has. nothing held has two
/// causes that need opposite things done about them — the program is running and
/// has to be stopped, or it is over and there is nothing to stop — and a caller
/// that knows which has to say so, because this cannot tell from `held` alone
pub fn only_stop(held: &[Stop], ended: Option<i64>, wanted: &'static str) -> crate::Result<u64> {
    match held {
        [] => Err(match ended {
            Some(code) => crate::Error::ProgramExited { code, wanted },
            None => crate::Error::NotStopped { wanted },
        }),
        [stop] => Ok(stop.stop),
        several => Err(crate::Error::AmbiguousStop {
            wanted,
            held: several.iter().map(|stop| stop.stop).collect(),
        }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stop::StopReason;

    fn held_at(stop: u64) -> Stop {
        Stop {
            stop,
            thread: 7,
            reason: StopReason::Entry,
            holding: Vec::new(),
        }
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
        let over = only_stop(&[], Some(3), "the stack").expect_err("nothing is held");
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
            only_stop(&[held_at(3)], Some(0), "the stack").expect("one is held"),
            3
        );
    }
}
