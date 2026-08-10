//! why a thread of the debuggee is held, and what the rest of it was doing
//!
//! a stop holds **one thread**. everything here is written from that: a reason
//! says what one thread did, a step moves one thread, and a mode says what the
//! others were doing while an answer was taken

use crate::exception::PythonError;

/// one thread, held
///
/// a stop holds **one thread**, and every other thread in the process goes on
/// running. that is the whole model, and it is the same on a gil-enabled build
/// as on a free-threaded one, because the agent releases the GIL for the
/// duration of a stop rather than letting it freeze the process by accident
///
/// so several of these can be outstanding at once, and each is resumed by
/// naming its thread
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stop {
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
            Self::NonStop => formatter.write_str(
                "non-stop: one thread was held and the rest of the program kept \
                 running, so this is a sample rather than a snapshot",
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
            (Mode::NonStop, vec!["sample", "kept running"]),
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
