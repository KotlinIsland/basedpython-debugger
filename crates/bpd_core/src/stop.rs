//! why a thread of the debuggee is held, and what the rest of it was doing
//!
//! a stop holds **one thread**. everything here is written from that: a reason
//! says what one thread did, a step moves one thread, and a mode says what the
//! others were doing while an answer was taken

use crate::exception::PythonError;
use crate::session::SessionId;

/// a stop as the agent reported it, before it was named
///
/// what crosses the control connection. it is every part of a [`Stop`] the
/// **debuggee** can know, which is all of it but the session: an agent counts
/// its stops from one and cannot see another agent doing the same, so the id
/// that tells two of them apart is added by the engine as the report arrives.
/// [`Self::in_session`] is the one place that happens, which is what makes a
/// stop that nothing named impossible to hold rather than merely unlikely
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Reported {
    /// which stop this is, counting from one in the agent that minted it
    pub stop: u64,
    /// the interpreter's identity for the thread that is held
    pub thread: u64,
    /// why it stopped
    pub reason: StopReason,
    /// what this thread was holding, of the things another thread can wait for
    pub holding: Vec<Holding>,
}

impl Reported {
    /// this stop, named by the session it was reported from
    #[must_use]
    pub fn in_session(self, session: SessionId) -> Stop {
        Stop {
            session,
            stop: self.stop,
            thread: self.thread,
            reason: self.reason,
            holding: self.holding,
        }
    }
}

/// one thread, held
///
/// a stop holds **one thread**, and every other thread in the process goes on
/// running. that is the whole model, and it is the same on a gil-enabled build
/// as on a free-threaded one, because the agent releases the GIL for the
/// duration of a stop rather than letting it freeze the process by accident
///
/// so several of these can be outstanding at once, and each is resumed by
/// naming its thread
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stop {
    /// which session this stop is of
    ///
    /// `stop` counts from one in the agent that minted it, so it names a stop
    /// only within one debugged process. this is what makes the pair unique,
    /// and it is what a request about this stop is addressed to — see
    /// [`crate::Addressed`]
    pub session: SessionId,
    /// which stop this is, counting from one
    ///
    /// the number a [`crate::FrameId`] carries, and the number a request
    /// naming a stop uses. it is minted once per stop and never reused
    pub stop: u64,
    /// the interpreter's identity for the thread that is held, as
    /// `threading.get_ident` reports it
    pub thread: u64,
    /// why it stopped
    pub reason: StopReason,
    /// what this thread was holding, of the things another thread can wait for
    ///
    /// empty means nothing bpd can know about was held — **not** that nothing
    /// was. cpython exposes no owner for a `threading.Lock`, so a lock this
    /// thread took is invisible from here. what is knowable is listed in
    /// [`Holding`], and the way to see the consequence either way is to ask
    /// what the other threads are doing
    pub holding: Vec<Holding>,
}

/// something a held thread holds that other threads can be waiting for
///
/// this is the honest half of the non-stop model. a stop holds one thread and
/// says the rest keep running, which stops being true the moment the held
/// thread is inside something the others need. what cpython makes **knowable**
/// is listed here; everything else is visible only as another thread that is
/// not getting anywhere, which is what [`crate::Request::Threads`] is for
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "holding", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Holding {
    /// the thread is inside the import system
    ///
    /// cpython holds a lock per module for the whole of that module's
    /// execution, so any other thread importing the same module blocks until
    /// this one is resumed — and a thread deep enough in the machinery holds
    /// more than that. this one is knowable because the import machinery runs
    /// in python frames whose filenames name it
    ImportSystem {
        /// the module being imported, when the machinery's own frame says
        ///
        /// `None` when no frame of the walk held a readable name, which is a
        /// statement about what was there rather than about there being no
        /// import
        module: Option<String>,
    },
}

impl std::fmt::Display for Holding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ImportSystem {
                module: Some(module),
            } => write!(
                formatter,
                "the import system, importing `{module}` — another thread \
                 importing it blocks until this one is resumed"
            ),
            Self::ImportSystem { module: None } => formatter.write_str(
                "the import system — another thread importing the same module \
                 blocks until this one is resumed",
            ),
        }
    }
}

/// how the rest of the program was moving while an answer was taken
///
/// every read carries one. a debugger that reported a value without saying
/// whether the program was standing still while it read it is reporting a
/// number and hiding what kind of number it is
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Mode {
    /// one thread was held and every other thread went on running
    ///
    /// the held thread's **stack** is still a snapshot: it is inside a
    /// monitoring callback and cannot return, so its frames cannot go away
    /// underneath the walk. everything the frames point *at* is a **sample** —
    /// another thread can mutate a list between its length being read and its
    /// contents being read, and the answer would then describe a state the
    /// program was never in
    NonStop,

    /// every thread that could be held was held while the answer was taken
    ///
    /// `native` is what keeps this from being a whole-program claim: a thread
    /// parked in a C call has released the GIL and reaches no monitoring event,
    /// so nothing available here can stop it. an empty `native` is the only
    /// case where the answer describes one moment of the whole program
    StopTheWorld {
        /// threads that were running python code when the world was stopped and
        /// never reached a line to be held at, as of that moment
        ///
        /// fixed when the world was stopped rather than recomputed per answer,
        /// so it can name a thread that has parked since. overstating what was
        /// moving is the safe direction to be wrong in
        native: Vec<u64>,
    },
}

impl std::fmt::Display for Mode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // the held thread's own frame chain really is a snapshot, and
            // saying "this is a sample" flatly would be reporting a stack as
            // less than it is. what the rest of the program can move underneath
            // is every *value* reached through those frames
            Self::NonStop => formatter.write_str(
                "non-stop: one thread was held and the rest of the program kept \
                 running. the held thread's own frame chain is a snapshot — it \
                 is inside a monitoring callback and cannot return — and every \
                 value read through it is a sample, because another thread can \
                 change one between two reads",
            ),
            Self::StopTheWorld { native } if native.is_empty() => {
                formatter.write_str("stop-the-world: nothing else in the program was running")
            }
            Self::StopTheWorld { native } => write!(
                formatter,
                "stop-the-world, except for {} thread(s) parked in a C call \
                 that nothing here can stop: {native:?}",
                native.len()
            ),
        }
    }
}

/// why the debuggee stopped
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopReason {
    /// stopped before the first statement of the program, having run nothing
    ///
    /// no user thread exists yet, so this is a stop of the whole program
    Entry,

    /// a thread reached a line a breakpoint is bound to
    ///
    /// this says what **one thread** did, and the thread it did it on is on the
    /// [`Stop`] around it. every other thread in the process is running
    Breakpoint {
        /// every breakpoint that decided to stop here, smallest id first
        ///
        /// more than one is ordinary — a breakpoint moved off a comment can
        /// land on a line another breakpoint already sits on. a breakpoint
        /// bound to the line whose condition was false, or whose hit count was
        /// not reached, is **not** here
        breakpoints: Vec<u32>,
        /// the `co_filename` of the code object that was running
        file: String,
        /// the line it stopped on
        line: u32,
    },

    /// a step the debugger asked for completed
    ///
    /// this says what **one thread** did. every other thread in the process
    /// went on running while it stepped, and a step steps one of them
    Stepped {
        /// the step that was asked for
        kind: StepKind,
        /// the `co_filename` of the code object that is running now
        file: String,
        /// the line it is about to run
        line: u32,
    },

    /// the debugger asked for a thread, and this is the one that arrived
    ///
    /// there is nothing in cpython that suspends a thread, so a pause arms
    /// `LINE` for the whole program and holds the first thread that reaches
    /// one. which thread that is belongs to the operating system — a pause
    /// names the thread it got, and the threads that were running when it was
    /// armed are on the acknowledgement
    Paused {
        /// the `co_filename` of the code object that was running
        file: String,
        /// the line it is about to run
        line: u32,
    },

    /// an exception was raised
    ///
    /// the frame it was raised in is the one that is held, so the stack is the
    /// whole of the program at the moment it went wrong. cpython raises this
    /// event again in **every frame the exception propagates into**, and those
    /// are the same exception rather than new ones — so an exception is
    /// reported once, where it was raised
    Raised {
        /// what was raised
        error: PythonError,
        /// the `co_filename` of the code object that raised it
        file: String,
        /// the line it was raised on
        line: u32,
    },

    /// an exception is leaving the program, and nothing will catch it
    ///
    /// only knowable at unwind time: an exception is caught or not caught by
    /// what happens after it is raised, and a debugger that decided at the
    /// raise would be predicting. so this is reported from the **outermost**
    /// frame, as the exception leaves it — which is also why the held stack is
    /// that one frame and the frames it came through are on the `error`'s own
    /// traceback
    Uncaught {
        /// what is leaving
        error: PythonError,
        /// the `co_filename` of the outermost code object
        file: String,
        /// the line of it the exception is leaving from
        line: u32,
    },

    /// this process is a forked child, and it has just become a session of its
    /// own
    ///
    /// a fork copies the debugged process, and with child debugging on the copy
    /// gives the inherited control connection up and opens one of its own
    /// before `os.fork()` has returned. so the thread that is held is the one
    /// that forked, at the line it forked on, and the child has run nothing of
    /// its own yet — which makes this the child's [`StopReason::Entry`]
    ///
    /// it carries the parent's pid because that is the only thing tying the two
    /// sessions together: the ids the engine mints are its own, and a client
    /// shown two sessions with nothing between them cannot tell which program
    /// made which
    Forked {
        /// the process this one was forked from
        parent: u32,
        /// the `co_filename` of the code object that called `os.fork()`
        file: String,
        /// the line of it the fork returned to
        line: u32,
    },

    /// this process is a child that was **`exec`'d**, and it has just become a
    /// session of its own
    ///
    /// where [`StopReason::Forked`] is a copy of a running process, this is a
    /// fresh interpreter that inherited nothing but the environment — which is
    /// where it found the endpoint and the token. it is held at interpreter
    /// startup, from `site`, before `__main__` exists and before a line of the
    /// program has been compiled
    ///
    /// so there is **no file and no line**, and that is the honest shape rather
    /// than a missing field: the only code running is the four lines of bpd's
    /// own that found the agent, and reporting those as the program's location
    /// would be the debugger pointing at itself. nothing of the program has run,
    /// which makes this the child's [`StopReason::Entry`]
    ///
    /// it carries the parent's pid for the reason [`StopReason::Forked`] does:
    /// the ids the engine mints are its own, and a client shown two sessions
    /// with nothing between them cannot tell which program made which
    Started {
        /// the process that started this one
        parent: u32,
    },

    /// a frame the debugger restarted has been entered again, and is fresh
    ///
    /// a restart forces the old frame to return and rewinds the caller to the
    /// line the call was made from, so this is the interpreter having built a
    /// **new** frame from that call: nothing of it has run, and every local it
    /// has is one this call bound
    ///
    /// held before its first statement, which is where the first `LINE` event
    /// of a frame is delivered — so unlike the entry stop, a jump can be made
    /// from here
    Restarted {
        /// `co_qualname` of what is running again
        function: String,
        /// the `co_filename` of the code object that is running
        file: String,
        /// the line it is about to run
        line: u32,
    },

    /// a frame was forced out for a restart and the restart did not finish
    ///
    /// **which** of [`Abandoned`]'s reasons it was is carried on the stop, and
    /// that list is `non_exhaustive`. an earlier version of this named cpython
    /// refusing the rewind as though it were the only one, and a client reading
    /// it as complete would have been reading a false set: the caller can also
    /// leave before the rewind is made, and the rewind can land somewhere other
    /// than the line it asked for
    ///
    /// the refusal is the half of a restart that cannot be decided in advance.
    /// everything about the caller's line is read off its bytecode before
    /// anything is attempted, but whether cpython will accept a move **to** that
    /// line from wherever the caller got to is cpython's answer and it gives it
    /// at the time
    ///
    /// so the thread is held where the refusal happened rather than let go. the
    /// frame that was forced out is **gone** — it returned — and the call was
    /// not made again. saying so is the whole point of this variant: a restart
    /// that quietly did half of itself is the exact wrongness this project
    /// refuses
    RestartAbandoned {
        /// `co_qualname` of the frame that was forced out and did not come back
        function: String,
        /// the line of the caller the rewind was going to
        wanted: u32,
        /// the `co_filename` of the caller
        file: String,
        /// the line of the caller it is held on
        ///
        /// after a move cpython accepted this is `wanted` — the two differ only
        /// when the move itself was refused, which is
        /// [`Abandoned::Refused`]
        line: u32,
        /// what stopped it
        why: Abandoned,
    },

    /// a breakpoint's condition or log message raised
    ///
    /// the program is held rather than resumed. an expression that raises has
    /// not said "false" — it has said nothing, and carrying on as though it had
    /// answered is the exact quiet wrongness this project refuses. the client
    /// gets the exception, at the line that was about to run
    EvaluationFailed {
        /// the breakpoint whose expression raised
        breakpoint: u32,
        /// whether it was the condition or the log message
        part: Part,
        /// the expression as the client wrote it
        expression: String,
        /// the `co_filename` of the code object that was running
        file: String,
        /// the line it was about to run
        line: u32,
        /// what the interpreter raised
        error: PythonError,
    },
}

/// why a restart did not finish, after the frame had already been forced out
///
/// the two things a restart cannot decide in advance. everything about the
/// caller's line is read off its bytecode before anything is attempted, and
/// neither of these is knowable from bytecode: whether cpython accepts a move
/// **to** that line from wherever the caller got to, and whether the caller
/// gets there at all
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "abandoned", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Abandoned {
    /// cpython refused the move back to the call line
    Refused {
        /// its own refusal, with its reason intact
        error: PythonError,
    },

    /// the caller was left by an exception before it reached a line
    ///
    /// only an exception can do this. the request refuses a call the caller has
    /// no statement after, so a caller that **returns** always runs a line
    /// first — which is the whole reason that refusal exists
    CallerLeft,

    /// the rewind landed at an instruction the analysis had not read
    ///
    /// everything about the caller's line is read before the frame is forced
    /// out, from the offset a jump was **predicted** to land on. cpython picks
    /// that offset by stack depth rather than by offset order, so a line with
    /// more than one copy of its instructions can land somewhere else — and the
    /// span that was checked is then not the span that would run
    ///
    /// so it is read back and compared, and a mismatch stops the restart rather
    /// than resuming into code nobody checked. **nothing is put back**: the
    /// assignment has already succeeded by the time the landing is read, and the
    /// caller is left on the line it was moved to — an earlier version of this
    /// note claimed a put-back, which was residue of a speculative jump that has
    /// since been deleted. the frame that was forced out is gone either way
    LandedElsewhere {
        /// the offset the analysis was built on
        expected: u32,
        /// the offset cpython chose
        landed: u32,
    },
}

impl std::fmt::Display for Abandoned {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused { error } => {
                write!(formatter, "cpython refused to move it back there: {error}")
            }
            Self::CallerLeft => formatter.write_str(
                "an exception left the caller before it reached a line the move \
                 could be made from",
            ),
            Self::LandedElsewhere { expected, landed } => write!(
                formatter,
                "the move back landed at offset {landed} and the line was read \
                 from offset {expected}, so what would have run again is not \
                 what bpd checked. cpython picks the destination of a move by \
                 stack depth rather than by offset order, and a line with more \
                 than one copy of its instructions can land on either"
            ),
        }
    }
}

/// which way a step goes
///
/// a step is **one thread's**. the rest of the program keeps running while it
/// happens, which is the same model a stop has — see [`Stop`]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// run to the next line of this frame, whatever it calls on the way
    ///
    /// a call the line makes is run to its end. a frame that **suspends** is
    /// not left: a `yield` or an `await` hands control away and comes back, so
    /// a step over one lands on the next line of the same frame rather than in
    /// the generator's consumer or in the event loop
    Over,

    /// stop at the first line of the next frame this thread enters
    ///
    /// entering means a function called, a generator or coroutine resumed, or
    /// one thrown into. a line that enters nothing behaves as [`StepKind::Over`]
    In,

    /// run until this frame is finished, and stop at the next line of its caller
    ///
    /// finished, not suspended: a generator that yields is resumed later and is
    /// still the frame the step is in, so a step out of one runs it to its end
    Out,
}

impl std::fmt::Display for StepKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Over => "step over",
            Self::In => "step in",
            Self::Out => "step out",
        })
    }
}

/// which part of a breakpoint an expression belongs to
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Part {
    /// the expression that decides whether to stop
    Condition,
    /// an expression embedded in the log message
    LogMessage,
}

impl std::fmt::Display for Part {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Condition => formatter.write_str("condition"),
            Self::LogMessage => formatter.write_str("log message"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_says_what_was_moving_while_the_answer_was_taken() {
        let cases = [
            (
                Mode::NonStop,
                vec!["sample", "kept running", "frame chain is a snapshot"],
            ),
            (
                Mode::StopTheWorld { native: Vec::new() },
                vec!["nothing else in the program was running"],
            ),
            (
                Mode::StopTheWorld { native: vec![7] },
                vec!["C call", "[7]"],
            ),
        ];

        for (mode, expected) in cases {
            let said = mode.to_string();
            for wanted in expected {
                assert!(said.contains(wanted), "expected {wanted:?} in {said:?}");
            }
        }
    }

    #[test]
    fn non_stop_does_not_call_a_held_threads_own_stack_a_sample() {
        // this sentence is carried on a `stack` answer as well as on a value
        // read, and the frame chain of a held thread really is a snapshot: it
        // is inside a monitoring callback and cannot return. saying "this is a
        // sample" flatly would report a stack as less than it is, and the
        // `stack` tool says the opposite in the same session
        let said = Mode::NonStop.to_string();
        assert!(
            !said.contains("this is a sample"),
            "the whole answer is not a sample, and it said {said:?}"
        );
        assert!(
            said.contains("value read through it is a sample"),
            "what the rest of the program can move underneath is the values, \
             and it said {said:?}"
        );
    }

    #[test]
    fn what_a_held_thread_holds_says_who_it_blocks() {
        let cases = [
            (
                Holding::ImportSystem {
                    module: Some("app.db".to_string()),
                },
                vec!["`app.db`", "blocks until this one is resumed"],
            ),
            (
                Holding::ImportSystem { module: None },
                vec!["the import system", "blocks until this one is resumed"],
            ),
        ];

        for (holding, expected) in cases {
            let said = holding.to_string();
            for wanted in expected {
                assert!(said.contains(wanted), "expected {wanted:?} in {said:?}");
            }
        }
    }
}
