//! what the engine and the agent say to each other
//!
//! kept in its own module so a change to the message set cannot change the
//! framing, and encoded as json because a captured session being readable is
//! worth more than the bytes at this frequency — the framing benchmark puts the
//! whole envelope at a few nanoseconds, so the encoding is not where the cost is
//!
//! everything a message carries is defined in `bpd_core`, once. there is no
//! wire copy of a domain type and no conversion between the two: the agent and
//! the engine are built and shipped together and the handshake refuses a
//! mismatch outright, so a second model would buy a stability nobody needs at
//! the price of a seam where a field can be dropped
//!
//! there is no request id. one would be a field that is parsed and never read
//! until there are two requests in flight at once, and the first thing to need
//! it is the concurrency that arrives with breakpoints

use std::io::{Read, Write};

use bpd_core::{
    Detail, Entry, Evaluated, Frame, FrameId, LogRecord, Mode, Omitted, Refusal, Resolved, Scope,
    SourceBreakpoint, StepKind, Stop, ThreadState, Which,
};

use crate::frame::{self, Result};

/// what the agent tells the engine
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FromAgent {
    /// a thread of the debuggee stopped
    ///
    /// sent the moment the thread is held, without waiting for anything. a
    /// second thread reaching a breakpoint while a first is held sends its own
    /// straight away rather than queueing behind the first on the connection —
    /// a thread waiting for the debugger to finish with another thread is a
    /// thread that is not running, and nothing would have said so
    Stopped {
        /// the thread, and why
        stop: Stop,
    },

    /// the threads named were let go
    ///
    /// sent **before** they are woken, so the client never sees a stop from a
    /// resumed thread ahead of the acknowledgement that it was resumed
    Resumed {
        /// the threads that are running again
        threads: Vec<u64>,
    },

    /// a pause is armed, and these threads were running python when it was
    ///
    /// the stop it produces arrives separately, as a
    /// [`bpd_core::StopReason::Paused`] one. `running` is what says whether to
    /// expect it: a thread parked in a C call has released the GIL and executes
    /// no python, so it reaches no line and nothing here can hold it. an empty
    /// `running` means the pause is armed and **nothing is going to arrive**
    /// until some thread runs python again
    Pausing {
        /// the threads that were running python when the pause was armed
        running: Vec<u64>,
    },

    /// what the exception breakpoints are set to now
    ExceptionBreakpointsSet {
        /// stopping where an exception is raised
        raised: bool,
        /// stopping where an exception leaves the outermost frame
        uncaught: bool,
    },

    /// what every thread of the debuggee was doing
    Threads {
        /// one entry per thread the interpreter knows about
        threads: Vec<ThreadState>,
        /// how long, in milliseconds, separated the two samples
        ///
        /// what [`bpd_core::Progress::Still`] means, in the answer, rather
        /// than in the client's memory of what it asked for
        settle_ms: u32,
        /// how the program was moving while this was taken
        mode: Mode,
    },

    /// the world was stopped, as far as it could be
    WorldStopped {
        /// the threads that are held, including the one that asked
        held: Vec<u64>,
        /// the threads that never reached a line to be held at
        ///
        /// a thread parked in a C call has released the GIL and reaches no
        /// monitoring event. it is **running**, and it is reported here rather
        /// than counted among the held — which is the thing a debugger normally
        /// gets wrong about stopping the world
        native: Vec<u64>,
    },

    /// the program has finished and these threads are still held
    ///
    /// the interpreter is about to finalize, which joins the program's
    /// non-daemon threads. a held thread cannot be joined, so the process would
    /// sit there looking like a hang in bpd when it is the debuggee waiting for
    /// a resume that never came. sent so the client can say which threads, and
    /// resume them
    Finishing {
        /// the threads still held as the program ended
        held: Vec<u64>,
    },

    /// how the breakpoint set resolves now
    ///
    /// sent as the answer to [`FromEngine::SetBreakpoints`], and again,
    /// unprompted, whenever loading a file changes the answer — which is how a
    /// breakpoint in a module that was not imported yet stops being unbound
    BreakpointsResolved {
        /// one entry per breakpoint whose binding changed
        resolved: Vec<Resolved>,
    },

    /// a logpoint produced a record
    ///
    /// sent while the program runs and never waited on: a logpoint on a line
    /// executed a million times sends a million of these and blocks for none of
    /// them, which is the whole reason the formatting happens in the agent
    Logged {
        /// what it had to say
        record: LogRecord,
    },

    /// the stack of one held thread
    ///
    /// **only** a held thread's. a running thread's frames are moving, and a
    /// stack read off one would be a picture of a moment that had already
    /// passed. where a running thread is, as a stated sample, is
    /// [`FromAgent::Threads`]
    Stack {
        /// the frames, the one that stopped first
        frames: Vec<Frame>,
        /// how deep the stack is, which is more than `frames` when the request
        /// asked for fewer
        depth: usize,
        /// how the program was moving while this was taken
        ///
        /// a held thread's stack is a snapshot in either mode — it is inside a
        /// callback and cannot return — so what this qualifies is everything
        /// the frames point at
        mode: Mode,
    },

    /// what one scope of one frame holds
    Variables {
        /// which frame it was read from
        frame: FrameId,
        /// which scope of it
        scope: Scope,
        /// the names it holds, in the order the interpreter keeps them
        entries: Vec<Entry>,
        /// names that belong to this scope and hold nothing at this line
        ///
        /// a local before its first assignment. it is not the same as absent,
        /// and it is not `None`
        unbound: Vec<String>,
        /// names that belong to this scope and whose value the frame does not
        /// expose
        ///
        /// a class body's free variables are the case: the code object names
        /// them, and the value lives in a cell that only the function object
        /// holds. they are **not** unbound — they hold something bpd cannot
        /// see, and reporting them as absent would make the scope look smaller
        /// than it is
        unreadable: Vec<String>,
        /// everything this answer left out, and why
        ///
        /// a list rather than one reason: a scope can be read shallower than
        /// the request asked **and** have more names than it asked for, and
        /// reporting whichever came first would leave the other unsaid
        omitted: Vec<Omitted>,
        /// how the program was moving while this was taken
        mode: Mode,
    },

    /// what an expression did, or what a write left behind
    ///
    /// a write answers with the value read back **out of the frame** after it,
    /// rather than with the value that was written: what the frame holds now is
    /// the thing the client asked to be told
    Evaluated {
        /// the outcome
        result: Evaluated,
        /// how the program was moving while this was taken
        mode: Mode,
    },

    /// the source around one frame's current line, or why there is none
    ///
    /// read on the debuggee's own filesystem, which is the one the interpreter
    /// read the file from, and shown only when the file still compiles to the
    /// code object the frame is running
    Source {
        /// the lines, or what stood in the way
        source: bpd_core::Source,
    },

    /// the agent will not answer the request, and this is why
    Refused {
        /// what stood in the way
        reason: Refusal,
    },
}

/// what the engine tells the agent
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FromEngine {
    /// let held threads run again
    ///
    /// resume names the threads it means, because a stop holds one thread and
    /// there can be several held at once. "continue" that quietly meant "every
    /// thread" would be a client resuming threads it had forgotten it had
    Resume {
        /// which of the held threads to let go
        which: Which,
    },

    /// let one held thread go, and hold it again when the step lands
    ///
    /// a step is a resume with instrumentation, so it is acknowledged with
    /// [`FromAgent::Resumed`] naming the thread, and the landing arrives later
    /// as a [`bpd_core::StopReason::Stepped`] stop of its own
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
    /// the only request here that is sent to a program with **nothing held**.
    /// there is nothing in cpython that suspends a thread, so this arms `LINE`
    /// for the whole program and holds the first thread to reach one — which
    /// is why the acknowledgement says which threads were running python, and
    /// therefore whether a stop is going to arrive at all
    Pause,

    /// stop when an exception is raised, or when one is about to leave the
    /// program
    ///
    /// the whole setting rather than a delta, for the reason
    /// [`FromEngine::SetBreakpoints`] is
    SetExceptionBreakpoints {
        /// stop where an exception is raised, whether or not it is caught
        raised: bool,
        /// stop where an exception leaves the outermost frame
        uncaught: bool,
    },

    /// replace the whole breakpoint set
    ///
    /// the complete set rather than a delta: a debugger that accumulates edits
    /// has two ideas of what is set, and they diverge
    SetBreakpoints {
        /// every breakpoint that should be armed after this request
        breakpoints: Vec<SourceBreakpoint>,
    },

    /// what every thread of the debuggee is doing
    ///
    /// the answer to "the other threads are supposed to be running — are they".
    /// it is the only request here that is about a thread bpd is **not**
    /// holding, and everything it says about one is a sample
    Threads {
        /// how long to wait, in milliseconds, before taking the second sample
        ///
        /// a thread is reported as still only when it was in the same place in
        /// both. zero takes both samples together, and then "still" says almost
        /// nothing — which is why the interval comes back in the answer
        settle_ms: u32,
    },

    /// hold every thread that can be held, until the asking stop is resumed
    ///
    /// the explicit mode. non-stop is the default because a live program should
    /// go on living, and a coherent view of a data structure needs the opposite
    ///
    /// it is not free: catching a thread needs an event, so this arms `LINE`
    /// for the whole program and calls `restart_events()`, which undoes every
    /// `DISABLE` in the process. the program pays to re-disable them afterwards
    StopTheWorld {
        /// the stop asking, which is the one whose resume releases the world
        stop: u64,
        /// how long to wait, in milliseconds, for the other threads to arrive
        ///
        /// a thread parked in a C call never will. the answer names the ones
        /// that did not rather than waiting for them
        settle_ms: u32,
    },

    /// walk one held thread's frame chain
    Stack {
        /// the stop whose thread to walk
        stop: u64,
        /// how many frames to report, counting from the one that stopped
        ///
        /// `None` is all of them. the answer says how deep the stack really is
        /// either way, so asking for fewer never hides that there are more
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
    /// this runs the program's own code, by request, in the frame that was
    /// named. an expression that raises is answered with the exception
    Evaluate {
        /// which frame it is evaluated in
        frame: FrameId,
        /// the expression, as the client wrote it
        expression: String,
        /// how much of the result to read
        detail: Detail,
    },

    /// the source around one frame's current line
    ///
    /// the debuggee reads it rather than the engine, for two reasons: the
    /// filesystem the interpreter read the file from is the debuggee's, and a
    /// `co_filename` that is relative is relative to the debuggee's working
    /// directory. it also **compiles** the file, to check that what is on disk
    /// is still the code the frame is running — the file on disk is not evidence
    /// on its own, because it is edited while a program runs
    Source {
        /// which frame
        frame: FrameId,
        /// how many lines either side of that frame's current line
        around: u32,
    },

    /// write a variable of a frame
    ///
    /// the name must already be in that scope of that frame. this is
    /// deliberate, and it is not fussiness: `f_locals` accepts a write of a
    /// name the code object does not have, keeps it, and reads it back — while
    /// the program itself never sees it. a debugger that reported that as a
    /// write performed would be reporting a change the program did not receive
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

/// encode a message and write it as one frame
pub fn write<W: Write, M: serde::Serialize>(writer: &mut W, message: &M) -> Result<()> {
    let encoded = serde_json::to_vec(message).map_err(|source| frame::Error::Undecodable {
        reason: format!("this build could not encode a message it produced: {source}"),
    })?;
    frame::write_frame(writer, &encoded)
}

/// read one frame and decode it
///
/// returns `None` when the peer closed the connection cleanly between frames
pub fn read<R: Read, M: serde::de::DeserializeOwned>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
) -> Result<Option<M>> {
    if !frame::read_frame_into(reader, buffer)? {
        return Ok(None);
    }

    serde_json::from_slice(buffer)
        .map(Some)
        .map_err(|source| frame::Error::Undecodable {
            // the payload itself is not quoted: it can be a whole object graph,
            // and an error that dumps one is an error nobody reads
            reason: format!("the peer sent a message this build does not understand: {source}"),
        })
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::path::PathBuf;

    use bpd_core::{
        Binding, Content, Evaluation, HitCondition, Holding, Part, Progress, PythonError, Site,
        StopReason, TracebackFrame, Unbound, Value, Where,
    };

    use super::*;

    #[test]
    fn an_agent_event_round_trips() {
        let sent = FromAgent::Stopped {
            stop: Stop {
                stop: 1,
                thread: 8_482_561_408,
                reason: StopReason::Entry,
                holding: Vec::new(),
            },
        };

        let mut wire = Vec::new();
        write(&mut wire, &sent).expect("writing to a vec cannot fail");

        let received: Option<FromAgent> =
            read(&mut wire.as_slice(), &mut Vec::new()).expect("the frame is whole");
        assert_eq!(received, Some(sent));
    }

    #[test]
    fn an_engine_request_round_trips() {
        let mut wire = Vec::new();
        let sent = FromEngine::Resume {
            which: Which::Named {
                threads: vec![8_482_561_408],
            },
        };
        write(&mut wire, &sent).expect("writing to a vec cannot fail");

        let received: Option<FromEngine> =
            read(&mut wire.as_slice(), &mut Vec::new()).expect("the frame is whole");
        assert_eq!(received, Some(sent));
    }

    #[test]
    fn a_clean_close_is_not_a_message() {
        let received: Option<FromAgent> =
            read(&mut [].as_slice(), &mut Vec::new()).expect("a clean close is not an error");
        assert_eq!(received, None);
    }

    #[test]
    fn a_breakpoint_stop_round_trips() {
        let sent = FromAgent::Stopped {
            stop: Stop {
                stop: 3,
                thread: 8_482_561_408,
                reason: StopReason::Breakpoint {
                    breakpoints: vec![1, 4],
                    file: "/tmp/program.py".to_string(),
                    line: 12,
                },
                holding: vec![Holding::ImportSystem {
                    module: Some("package.module".to_string()),
                }],
            },
        };

        let mut wire = Vec::new();
        write(&mut wire, &sent).expect("writing to a vec cannot fail");

        let received: Option<FromAgent> =
            read(&mut wire.as_slice(), &mut Vec::new()).expect("the frame is whole");
        assert_eq!(received, Some(sent));
    }

    #[test]
    fn a_resolution_round_trips_in_both_directions() {
        let request = FromEngine::SetBreakpoints {
            breakpoints: vec![
                SourceBreakpoint::at(7, "/tmp/program.py", 3)
                    .when("value == 3")
                    .counting(HitCondition::Every {
                        count: NonZeroU32::new(2).expect("2 is not zero"),
                    })
                    .logging("value is {value}"),
            ],
        };
        let answer = FromAgent::BreakpointsResolved {
            resolved: vec![
                Resolved {
                    id: 7,
                    binding: Binding::Bound {
                        line: 4,
                        sites: vec![Site {
                            qualname: "C.m".to_string(),
                            first_line: 2,
                            offset: 18,
                        }],
                        evaluation: Evaluation::Comparison,
                    },
                },
                Resolved {
                    id: 8,
                    binding: Binding::Unbound {
                        reason: Unbound::NotLoaded {
                            file: PathBuf::from("/tmp/other.py"),
                        },
                    },
                },
            ],
        };

        let mut wire = Vec::new();
        write(&mut wire, &request).expect("writing to a vec cannot fail");
        write(&mut wire, &answer).expect("writing to a vec cannot fail");

        let mut buffer = Vec::new();
        let mut wire = wire.as_slice();
        let received: Option<FromEngine> =
            read(&mut wire, &mut buffer).expect("the frame is whole");
        assert_eq!(received, Some(request));
        let received: Option<FromAgent> = read(&mut wire, &mut buffer).expect("the frame is whole");
        assert_eq!(received, Some(answer));
    }

    #[test]
    fn a_condition_that_raised_round_trips_with_its_traceback() {
        let sent = FromAgent::Stopped {
            stop: Stop {
                stop: 1,
                thread: 8_482_561_408,
                holding: Vec::new(),
                reason: StopReason::EvaluationFailed {
                    breakpoint: 3,
                    part: Part::Condition,
                    expression: "value.missing".to_string(),
                    file: "/tmp/program.py".to_string(),
                    line: 12,
                    error: PythonError {
                        kind: "AttributeError".to_string(),
                        message: "'int' object has no attribute 'missing'".to_string(),
                        traceback: vec![TracebackFrame {
                            file: "<bpd condition of breakpoint 3>".to_string(),
                            line: 1,
                            function: "<module>".to_string(),
                        }],
                    },
                },
            },
        };

        let mut wire = Vec::new();
        write(&mut wire, &sent).expect("writing to a vec cannot fail");

        let received: Option<FromAgent> =
            read(&mut wire.as_slice(), &mut Vec::new()).expect("the frame is whole");
        assert_eq!(received, Some(sent));
    }

    #[test]
    fn a_log_record_round_trips() {
        let sent = FromAgent::Logged {
            record: LogRecord {
                breakpoint: 1,
                file: "/tmp/program.py".to_string(),
                line: 9,
                thread: 8_482_561_408,
                hit: 4,
                message: "value is 4".to_string(),
            },
        };

        let mut wire = Vec::new();
        write(&mut wire, &sent).expect("writing to a vec cannot fail");

        let received: Option<FromAgent> =
            read(&mut wire.as_slice(), &mut Vec::new()).expect("the frame is whole");
        assert_eq!(received, Some(sent));
    }

    #[test]
    fn a_hit_count_of_zero_is_refused_by_the_decoder_rather_than_decoded() {
        // an interval of zero would be a breakpoint that either never fires or
        // divides by zero, and there is no sensible reading of it. it is
        // unrepresentable in rust, so the only way one arrives is over the wire
        let mut wire = Vec::new();
        frame::write_frame(
            &mut wire,
            br#"{"request":"set_breakpoints","breakpoints":[{"id":1,"file":"/tmp/a.py","line":2,"hits":{"hits":"every","count":0}}]}"#,
        )
        .expect("writing to a vec cannot fail");

        let error = read::<_, FromEngine>(&mut wire.as_slice(), &mut Vec::new())
            .expect_err("zero is not a hit interval");
        assert!(
            error.to_string().contains("nonzero"),
            "the refusal has to name what was wrong, and it said {error}"
        );
    }

    #[test]
    fn a_breakpoint_with_nothing_extra_asked_for_is_the_common_case() {
        // the three optional parts default, so a client that only wants a
        // breakpoint writes a breakpoint
        let mut wire = Vec::new();
        frame::write_frame(&mut wire, br#"{"id":1,"file":"/tmp/a.py","line":2}"#)
            .expect("writing to a vec cannot fail");

        let received: Option<SourceBreakpoint> =
            read(&mut wire.as_slice(), &mut Vec::new()).expect("the frame is whole");
        assert_eq!(received, Some(SourceBreakpoint::at(1, "/tmp/a.py", 2)));
    }

    #[test]
    fn a_message_this_build_does_not_understand_is_named_as_such() {
        let mut wire = Vec::new();
        frame::write_frame(&mut wire, br#"{"event":"invented_by_a_newer_agent"}"#)
            .expect("writing to a vec cannot fail");

        let error = read::<_, FromAgent>(&mut wire.as_slice(), &mut Vec::new())
            .expect_err("the tag is not one this build has");
        assert!(
            error.to_string().contains("does not understand"),
            "the refusal must say what happened, got {error}"
        );
    }

    #[test]
    fn a_state_query_and_its_answer_round_trip() {
        let frame = FrameId { stop: 2, depth: 1 };
        let request = FromEngine::Variables {
            frame,
            scope: Scope::Free,
            detail: Detail::default(),
        };
        let answer = FromAgent::Variables {
            frame,
            scope: Scope::Free,
            entries: vec![Entry {
                name: "captured".to_string(),
                value: Value {
                    kind: "list".to_string(),
                    content: Content::Sequence {
                        items: vec![Value {
                            kind: "int".to_string(),
                            content: Content::Int {
                                text: "1".to_string(),
                                omitted: None,
                            },
                        }],
                        length: 4,
                        omitted: Some(Omitted::Children {
                            length: 4,
                            limit: 1,
                        }),
                    },
                },
            }],
            unbound: vec!["later".to_string()],
            unreadable: Vec::new(),
            omitted: vec![Omitted::Shallower { asked: 3, used: 1 }],
            mode: Mode::StopTheWorld {
                native: vec![8_482_561_408],
            },
        };

        let mut wire = Vec::new();
        write(&mut wire, &request).expect("writing to a vec cannot fail");
        write(&mut wire, &answer).expect("writing to a vec cannot fail");

        let mut buffer = Vec::new();
        let mut wire = wire.as_slice();
        let received: Option<FromEngine> =
            read(&mut wire, &mut buffer).expect("the frame is whole");
        assert_eq!(received, Some(request));
        let received: Option<FromAgent> = read(&mut wire, &mut buffer).expect("the frame is whole");
        assert_eq!(received, Some(answer));
    }

    #[test]
    fn an_evaluation_that_raised_is_an_answer_and_round_trips_as_one() {
        let sent = FromAgent::Evaluated {
            mode: Mode::NonStop,
            result: Evaluated::Raised {
                error: PythonError {
                    kind: "ZeroDivisionError".to_string(),
                    message: "division by zero".to_string(),
                    traceback: vec![TracebackFrame {
                        file: "<bpd evaluation>".to_string(),
                        line: 1,
                        function: "<module>".to_string(),
                    }],
                },
            },
        };

        let mut wire = Vec::new();
        write(&mut wire, &sent).expect("writing to a vec cannot fail");

        let received: Option<FromAgent> =
            read(&mut wire.as_slice(), &mut Vec::new()).expect("the frame is whole");
        assert_eq!(received, Some(sent));
    }

    #[test]
    fn a_request_may_leave_the_detail_to_its_defaults() {
        // an agent that only wants to see something writes what it wants to see
        let mut wire = Vec::new();
        frame::write_frame(
            &mut wire,
            br#"{"request":"evaluate","frame":{"stop":1,"depth":0},"expression":"x","detail":{}}"#,
        )
        .expect("writing to a vec cannot fail");

        let received: Option<FromEngine> =
            read(&mut wire.as_slice(), &mut Vec::new()).expect("the frame is whole");
        assert_eq!(
            received,
            Some(FromEngine::Evaluate {
                frame: FrameId { stop: 1, depth: 0 },
                expression: "x".to_string(),
                detail: Detail::default(),
            })
        );
    }

    #[test]
    fn a_thread_census_and_its_request_round_trip() {
        let request = FromEngine::Threads { settle_ms: 50 };
        let answer = FromAgent::Threads {
            threads: vec![
                ThreadState {
                    thread: 1,
                    held: Some(3),
                    at: Some(Where {
                        file: "/tmp/program.py".to_string(),
                        line: 12,
                        function: "handler".to_string(),
                    }),
                    progress: Progress::Held,
                },
                ThreadState {
                    thread: 2,
                    held: None,
                    at: None,
                    progress: Progress::Still,
                },
                ThreadState {
                    thread: 3,
                    held: None,
                    at: Some(Where {
                        file: "/tmp/program.py".to_string(),
                        line: 40,
                        function: "worker".to_string(),
                    }),
                    progress: Progress::Moved,
                },
            ],
            settle_ms: 50,
            mode: Mode::NonStop,
        };

        let mut wire = Vec::new();
        write(&mut wire, &request).expect("writing to a vec cannot fail");
        write(&mut wire, &answer).expect("writing to a vec cannot fail");

        let mut buffer = Vec::new();
        let mut wire = wire.as_slice();
        let received: Option<FromEngine> =
            read(&mut wire, &mut buffer).expect("the frame is whole");
        assert_eq!(received, Some(request));
        let received: Option<FromAgent> = read(&mut wire, &mut buffer).expect("the frame is whole");
        assert_eq!(received, Some(answer));
    }

    #[test]
    fn stopping_the_world_and_what_it_could_not_stop_round_trip() {
        let request = FromEngine::StopTheWorld {
            stop: 2,
            settle_ms: 100,
        };
        let answer = FromAgent::WorldStopped {
            held: vec![2, 9],
            native: vec![11],
        };

        let mut wire = Vec::new();
        write(&mut wire, &request).expect("writing to a vec cannot fail");
        write(&mut wire, &answer).expect("writing to a vec cannot fail");

        let mut buffer = Vec::new();
        let mut wire = wire.as_slice();
        let received: Option<FromEngine> =
            read(&mut wire, &mut buffer).expect("the frame is whole");
        assert_eq!(received, Some(request));
        let received: Option<FromAgent> = read(&mut wire, &mut buffer).expect("the frame is whole");
        assert_eq!(received, Some(answer));
    }

    #[test]
    fn a_resume_is_acknowledged_and_an_unfinished_program_says_what_it_still_holds() {
        for sent in [
            FromAgent::Resumed { threads: vec![4] },
            FromAgent::Finishing { held: vec![4, 7] },
        ] {
            let mut wire = Vec::new();
            write(&mut wire, &sent).expect("writing to a vec cannot fail");

            let received: Option<FromAgent> =
                read(&mut wire.as_slice(), &mut Vec::new()).expect("the frame is whole");
            assert_eq!(received, Some(sent));
        }
    }

    #[test]
    fn a_frame_that_is_not_json_is_refused_rather_than_guessed_at() {
        let mut wire = Vec::new();
        frame::write_frame(&mut wire, b"\xff\xfe not json at all")
            .expect("writing to a vec cannot fail");

        read::<_, FromEngine>(&mut wire.as_slice(), &mut Vec::new())
            .expect_err("a desynchronised stream must not decode");
    }
}
