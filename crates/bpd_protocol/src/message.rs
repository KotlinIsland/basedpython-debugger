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
//! a stop is the one message that carries less than the type a front end sees,
//! and it is not a wire copy either: [`bpd_core::Reported`] is what the
//! **debuggee** can know, and `bpd_core::Stop` is that plus the session it
//! arrived on, which only the engine can know. the conversion is
//! `Reported::in_session`, in the core, and it is a struct literal — so a field
//! added to a stop is a compile error there rather than a field that quietly
//! stops crossing
//!
//! there is no request id. one would be a field that is parsed and never read
//! until there are two requests in flight at once, and the first thing to need
//! it is the concurrency that arrives with breakpoints

use std::io::{Read, Write};

use bpd_core::{
    ContextLayer, Detail, Entry, Evaluated, Frame, FrameId, LogRecord, Mode, Omitted, Refusal,
    Reported, Resolved, Scope, SourceBreakpoint, StepKind, ThreadState, Which,
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
        ///
        /// everything about the stop the debuggee can know, which is all of a
        /// [`bpd_core::Stop`] but the session it is of. an agent counts its
        /// stops from one and cannot see another agent doing the same, so the
        /// id that tells two of them apart is the engine's — added as the
        /// report arrives, on the connection it arrived on, which is the only
        /// place that can know
        stop: Reported,
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

    /// whether a forked child of this process becomes a session of its own
    ///
    /// read back off the agent rather than assumed from the request, for the
    /// reason [`Self::ExceptionBreakpointsSet`] is: what is set is what the
    /// process says is set, and a fork handler that never received the setting
    /// would leave a client believing children are being debugged
    DebuggingChildren {
        /// what a fork will do from now on
        on: bool,
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

    /// the program started a child process that could be python
    ///
    /// sent while the program runs and never waited on, for the reason
    /// [`FromAgent::Logged`] is: the child is not blocked while this is written,
    /// and it must not be — reporting a child is not a reason to change what
    /// the program does
    Spawned {
        /// the child, and what the agent could tell about it
        child: bpd_core::Spawn,
    },

    /// this interpreter raises no event for a whole way of starting a child
    ///
    /// sent once, when the program does something that makes such a child
    /// possible. it is the opposite claim to [`FromAgent::Spawned`]: not that a
    /// child exists, but that a silence about one has stopped being evidence
    BlindTo {
        /// what the agent will not be able to see, and on which interpreter
        blindspot: bpd_core::Blindspot,
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

    /// what a template frame's django context holds, layer by layer
    ///
    /// never merged. `django.template.Context` is a stack of dicts and django
    /// resolves a name from the last one backwards, so which layer holds a name
    /// decides what the template renders
    TemplateContext {
        /// which frame it was read from
        frame: FrameId,
        /// the layers, outermost first, in `Context.dicts` order
        layers: Vec<ContextLayer>,
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

    /// what a jump did, and where the frame is now
    ///
    /// the answer to both jumps. where the frame is is read **off the frame**
    /// after the assignment, because no `LINE` event is delivered for the line
    /// a jump moves to — an agent that waited to be told would report the line
    /// after the one it moved to
    Jumped {
        /// what became of it, and what it changed about the frame
        jumped: bpd_core::Jumped,
    },

    /// what replacing a file's code did to the process, or what stopped it
    ///
    /// the whole answer either way. a replacement that was not made carries
    /// **all** of what stood in the way rather than the first thing, because a
    /// client fixing them one at a time is a client asking this seventeen times
    Replaced {
        /// what became of it, and what it changed about the process
        replaced: bpd_core::Replaced,
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

    /// the source map is installed, and it covers this many files
    ///
    /// the answer to [`FromEngine::MapSources`], and it is a count rather than
    /// an acknowledgement with nothing in it: the engine sent a set of files
    /// and a session where the two disagreed about how many arrived would be
    /// one where some locations were mapped and some were not
    SourcesMapped {
        /// how many generated files the agent will map locations of
        files: u32,
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

    /// decide what a forked child of this process does
    ///
    /// a fork copies the agent, the breakpoint table and the control
    /// connection's descriptors into a process with none of the thread that
    /// reads them. with this **off** the child gives the whole session up
    /// before `os.fork()` returns and runs undebugged; with it on the child
    /// gives the inherited connection up and opens one of its own, becoming a
    /// second session of the same debuggee
    ///
    /// it has to be sent **before** the fork, because the handler that acts on
    /// it runs inside `os.fork()` with nothing to ask. the child reads it out
    /// of inherited memory, which is the whole reason a fork needs no
    /// environment channel
    DebugChildren {
        /// whether a forked child reconnects
        on: bool,
    },

    /// replace the whole breakpoint set
    ///
    /// the complete set rather than a delta: a debugger that accumulates edits
    /// has two ideas of what is set, and they diverge
    SetBreakpoints {
        /// every breakpoint that should be armed after this request
        breakpoints: Vec<SourceBreakpoint>,
    },

    /// report locations of this build's generated python as `.by` source
    ///
    /// sent once, at launch, while the debuggee is held at entry and before a
    /// line of the program has run — so every location the agent ever produces
    /// is produced with the map already installed
    ///
    /// **the tables cross and the decision does not.** a
    /// [`bpd_core::MappedFile`] only exists because `bpd` hashed both files it
    /// describes against disk first, out of process, and there is no
    /// constructor that skips that. the agent applies a map it was handed; it
    /// never decides that one is trustworthy, because a debuggee vouching for
    /// the instrument that measures it is not evidence
    ///
    /// nothing about this is visible to the program. it is agent memory —
    /// no module enters `sys.modules`, no path enters `sys.path`, nothing is
    /// written to the environment — and `crates/bpd/tests/launch_parity.rs` is
    /// the guard on that rather than this sentence
    MapSources {
        /// every `.by`/`.py` pair of the build, with the table between them
        files: Vec<bpd_core::MappedFile>,
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

    /// read the django template context of a template frame
    ///
    /// a template frame has no python scopes to read, so this is what
    /// [`FromEngine::Variables`] is for a python one. the answer is the layers
    /// of `Context.dicts`, unmerged
    TemplateContext {
        /// which template frame
        frame: FrameId,
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

    /// move the executing frame to another line of the code it is running
    ///
    /// the thread stays held. only the frame the thread is executing can move,
    /// and the agent refuses any other — cpython accepts a move in a frame that
    /// is suspended in a call, and the frame then runs on with a value stack
    /// that does not match where it is
    SetNextStatement {
        /// which frame
        frame: FrameId,
        /// the line to move to
        line: u32,
    },

    /// re-enter a frame from the top
    ///
    /// the destination is the line of the first instruction of the frame's code
    /// object that carries one, which is why this is a request of its own
    /// rather than a line the engine could work out: the code object is in the
    /// debuggee
    RestartFrame {
        /// which frame
        frame: FrameId,
    },

    /// replace the code the process is running for one file with what is on disk
    ///
    /// the debuggee reads and compiles the file, for the reasons
    /// [`FromEngine::Source`] does — it is the filesystem the interpreter read
    /// it from — and because the code objects the new ones are compared against
    /// are only in the debuggee. compiling runs none of the program
    ReplaceCode {
        /// the file whose code to replace, as the client named it
        file: std::path::PathBuf,
        /// apply it even where a frame is running the code being replaced
        ///
        /// carried to the agent rather than decided here, because the agent is
        /// the only thing that can see the frames — see
        /// `bpd_core::Request::ReplaceCode`
        even_under_a_live_frame: bool,
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
            stop: Reported {
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
            stop: Reported {
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
                            templates_available: false,
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
            stop: Reported {
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
    fn a_spawned_child_round_trips_with_the_evidence_for_its_verdict() {
        let sent = FromAgent::Spawned {
            child: bpd_core::Spawn {
                event: "_posixsubprocess.fork_exec".to_string(),
                executable: Some("/usr/bin/python3.14".to_string()),
                arguments: vec!["/usr/bin/python3.14".to_string(), "-c".to_string()],
                verdict: bpd_core::Verdict::Perhaps {
                    named: "python3.14".to_string(),
                },
                // the agent is the only thing that knows this, so it is on the
                // wire rather than reconstructed by the engine from a setting it
                // asked for and cannot know took
                taking_up: true,
            },
        };

        let mut wire = Vec::new();
        write(&mut wire, &sent).expect("writing to a vec cannot fail");

        let received: Option<FromAgent> =
            read(&mut wire.as_slice(), &mut Vec::new()).expect("the frame is whole");
        assert_eq!(received, Some(sent));
    }

    #[test]
    fn a_blind_spot_round_trips() {
        let sent = FromAgent::BlindTo {
            blindspot: bpd_core::Blindspot::MultiprocessingSpawn {
                interpreter: "3.13".to_string(),
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
    fn a_jump_and_what_it_did_round_trip() {
        let frame = FrameId { stop: 1, depth: 0 };
        let request = FromEngine::SetNextStatement { frame, line: 12 };
        let answer = FromAgent::Jumped {
            jumped: bpd_core::Jumped {
                at: Where {
                    file: "/tmp/program.py".to_string(),
                    line: 12,
                    function: "handler".to_string(),
                },
                outcome: bpd_core::Jump::Moved {
                    from: 15,
                    bound_to_none: vec!["total".to_string()],
                    unannounced: vec![3],
                },
                mode: Mode::NonStop,
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
    fn a_jump_cpython_refused_round_trips_with_cpythons_own_reason() {
        let request = FromEngine::RestartFrame {
            frame: FrameId { stop: 2, depth: 0 },
        };
        let answer = FromAgent::Jumped {
            jumped: bpd_core::Jumped {
                at: Where {
                    file: "/tmp/program.py".to_string(),
                    line: 9,
                    function: "loopy".to_string(),
                },
                outcome: bpd_core::Jump::Refused {
                    wanted: 11,
                    error: PythonError {
                        kind: "ValueError".to_string(),
                        message: "can't jump into the body of a for loop".to_string(),
                        traceback: Vec::new(),
                    },
                },
                mode: Mode::NonStop,
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
    fn a_replacement_and_what_it_changed_round_trip() {
        let request = FromEngine::ReplaceCode {
            // the round trip has to carry it: the agent is the only thing that
            // can see the frames, so a flag lost on the wire is a guarantee
            // traded away by nobody
            even_under_a_live_frame: true,
            file: PathBuf::from("/tmp/victim.py"),
        };
        let answer = FromAgent::Replaced {
            replaced: bpd_core::Replaced {
                file: PathBuf::from("/tmp/victim.py"),
                outcome: bpd_core::Replacement::Applied {
                    still_running: vec![bpd_core::StillRunning {
                        function: "worker".to_string(),
                        frame: bpd_core::LiveFrame::Thread {
                            thread: 12,
                            line: 40,
                            held: Some(3),
                        },
                    }],
                    changed: vec![bpd_core::Rebound {
                        function: "boom".to_string(),
                        was_at: 2,
                        now_at: 5,
                        objects: 2,
                    }],
                    unchanged: vec!["<module>".to_string()],
                    rebound: vec![Resolved {
                        id: 1,
                        binding: Binding::Bound {
                            line: 6,
                            sites: vec![Site {
                                qualname: "boom".to_string(),
                                first_line: 5,
                                offset: 4,
                            }],
                            evaluation: Evaluation::Always,
                        },
                    }],
                },
                mode: Mode::NonStop,
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
    fn a_refused_replacement_round_trips_with_every_reason_it_had() {
        // all of them rather than the first: a client fixing them one at a time
        // is a client asking this seventeen times
        let sent = FromAgent::Replaced {
            replaced: bpd_core::Replaced {
                file: PathBuf::from("/tmp/victim.py"),
                outcome: bpd_core::Replacement::Refused {
                    because: vec![
                        bpd_core::Unreplaceable::TopLevelChanged {
                            file: PathBuf::from("/tmp/victim.py"),
                            differences: vec![bpd_core::Divergence::Defines {
                                added: vec!["helper".to_string()],
                                removed: Vec::new(),
                            }],
                        },
                        bpd_core::Unreplaceable::Running {
                            function: "boom".to_string(),
                            frame: bpd_core::LiveFrame::Thread {
                                thread: 8_482_561_408,
                                line: 3,
                                held: Some(1),
                            },
                        },
                    ],
                },
                mode: Mode::StopTheWorld { native: Vec::new() },
            },
        };

        let mut wire = Vec::new();
        write(&mut wire, &sent).expect("writing to a vec cannot fail");

        let received: Option<FromAgent> =
            read(&mut wire.as_slice(), &mut Vec::new()).expect("the frame is whole");
        assert_eq!(received, Some(sent));
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
