//! what the engine and the agent say to each other
//!
//! kept in its own module so a change to the message set cannot change the
//! framing, and encoded as json because a captured session being readable is
//! worth more than the bytes at this frequency — the framing benchmark puts the
//! whole envelope at a few nanoseconds, so the encoding is not where the cost is
//!
//! there is no request id. one would be a field that is parsed and never read
//! until there are two requests in flight at once, and the first thing to need
//! it is the concurrency that arrives with breakpoints

use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::path::PathBuf;

use crate::frame::{self, Result};

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
    /// this says what **one thread** did. it deliberately makes no claim about
    /// the other threads: real stop coordination is not built, and a debugger
    /// that reported threads as held when they were not would be lying about
    /// the one thing it exists to measure
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
        /// the interpreter's identity for the thread that stopped, as
        /// `threading.get_ident` reports it
        thread: u64,
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
        /// the interpreter's identity for the thread that stopped
        thread: u64,
        /// what the interpreter raised
        error: PythonError,
    },
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

/// an exception the interpreter raised, as the agent read it off the object
///
/// read by walking the exception and its traceback rather than by calling
/// `traceback.format_exception`: the agent must not import a module to describe
/// a failure, because the import would run inside a monitoring callback and
/// because a debuggee that imports `traceback` it would not otherwise have
/// imported is a debuggee the debugger changed
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PythonError {
    /// the exception's type, qualified by its module unless it is a builtin
    pub kind: String,
    /// `str(exception)`
    pub message: String,
    /// the frames the exception carries, outermost first
    pub traceback: Vec<TracebackFrame>,
}

impl std::fmt::Display for PythonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(formatter, "{}", self.kind)
        } else {
            write!(formatter, "{}: {}", self.kind, self.message)
        }
    }
}

/// one frame of a traceback
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TracebackFrame {
    /// the `co_filename` of the code that was running
    pub file: String,
    /// the line it was on
    pub line: u32,
    /// `co_qualname`
    pub function: String,
}

/// a breakpoint as the client asked for it
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceBreakpoint {
    /// the client's identity for this breakpoint, echoed in every report about it
    pub id: u32,
    /// the file the user named, spelled however they spelled it
    pub file: PathBuf,
    /// the line the user asked for
    pub line: u32,
    /// a python expression that has to be true before anything happens
    ///
    /// compiled once, when the breakpoint is set, and evaluated in the frame
    /// that reached the line. an expression that does not compile makes the
    /// breakpoint [`Unbound`], because a breakpoint whose condition can never
    /// be answered can never fire
    #[serde(default)]
    pub condition: Option<String>,
    /// how many qualifying hits to wait for
    ///
    /// a hit qualifies when the condition was true, or when there is no
    /// condition. a hit whose condition raised does not count, because it did
    /// not answer
    #[serde(default)]
    pub hits: Option<HitCondition>,
    /// produce a log record instead of stopping
    ///
    /// the text is emitted as it is written, except that `{...}` is a python
    /// expression evaluated in the frame and converted with `str()`. `{{` and
    /// `}}` are a literal brace
    #[serde(default)]
    pub log: Option<String>,
}

impl SourceBreakpoint {
    /// a breakpoint that stops on every pass over the line
    pub fn at(id: u32, file: impl Into<PathBuf>, line: u32) -> Self {
        Self {
            id,
            file: file.into(),
            line,
            condition: None,
            hits: None,
            log: None,
        }
    }

    /// stop only when `condition` is true
    #[must_use]
    pub fn when(mut self, condition: impl Into<String>) -> Self {
        self.condition = Some(condition.into());
        self
    }

    /// stop only on the hits `hits` selects
    #[must_use]
    pub const fn counting(mut self, hits: HitCondition) -> Self {
        self.hits = Some(hits);
        self
    }

    /// log `template` instead of stopping
    #[must_use]
    pub fn logging(mut self, template: impl Into<String>) -> Self {
        self.log = Some(template.into());
        self
    }
}

/// which of a breakpoint's qualifying hits it acts on
///
/// deliberately closed, and deliberately not DAP's `hitCondition` string. that
/// string means different things in different clients, and a debugger that
/// guesses which one a client meant is a debugger that stops on the wrong pass
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "hits", rename_all = "snake_case")]
pub enum HitCondition {
    /// only the nth qualifying hit, and nothing after it
    Exactly {
        /// which hit
        count: NonZeroU32,
    },
    /// the nth qualifying hit and every one after it
    AtLeast {
        /// the first hit that acts
        count: NonZeroU32,
    },
    /// every nth qualifying hit
    Every {
        /// the interval
        count: NonZeroU32,
    },
}

/// one thing a logpoint had to say
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogRecord {
    /// the breakpoint that produced it
    pub breakpoint: u32,
    /// the `co_filename` of the code object that was running
    pub file: String,
    /// the line it was produced on
    pub line: u32,
    /// the interpreter's identity for the thread that produced it
    pub thread: u64,
    /// which qualifying hit of that breakpoint this was, counting from one
    pub hit: u64,
    /// the log message, with every `{...}` replaced by `str()` of its value
    pub message: String,
}

/// what became of one requested breakpoint
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Resolved {
    /// the id the client gave it
    pub id: u32,
    /// whether there is a code object behind it, and where
    pub binding: Binding,
}

/// whether a breakpoint has a code object and an offset behind it
///
/// deliberately closed. there is no third state between "the interpreter will
/// stop here" and "it will not, and here is why", and `#[non_exhaustive]` would
/// invite a client to absorb one into a catch-all arm — which is how a
/// breakpoint ends up displayed as neither set nor refused
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "binding", rename_all = "snake_case")]
pub enum Binding {
    /// the interpreter will stop here
    Bound {
        /// the executable line it sits on
        ///
        /// not necessarily the line that was asked for — a request on a blank
        /// line, a comment or an elided `pass` moves to the next executable
        /// line, and this is where it went
        line: u32,
        /// every code object that holds that line, and where in each
        sites: Vec<Site>,
        /// how the condition will be answered on every hit
        evaluation: Evaluation,
    },

    /// nothing will stop, and this is why
    Unbound {
        /// what stood in the way
        reason: Unbound,
    },
}

/// how a breakpoint's condition is answered
///
/// reported for the same reason [`Site::offset`] is: the client can see what is
/// actually behind its request, rather than being told it worked
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evaluation {
    /// there is no condition, so every hit qualifies
    Always,

    /// `name <op> literal`, compared natively against the frame's fast locals
    ///
    /// the interpreter answers it instead when `name` is not a local of the
    /// frame that hit the line, because resolving a global the way `LOAD_NAME`
    /// does is the interpreter's job and reimplementing it is how a debugger
    /// reads a variable from the wrong scope. the answer is the same either
    /// way, which is what `crates/bpd_engine/tests/conditions.rs` pins
    Comparison,

    /// compiled once when the breakpoint was set, and evaluated by the
    /// interpreter against the frame on every hit
    Expression,
}

/// one code object a breakpoint is armed in
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Site {
    /// `co_qualname` — which function, lambda, comprehension or class body
    pub qualname: String,
    /// `co_firstlineno`, which separates two code objects with the same name
    pub first_line: u32,
    /// the first instrumentable bytecode offset for the bound line
    ///
    /// a line covers several offsets, and a stop anywhere but the first would
    /// land mid-statement
    pub offset: u32,
}

/// why a breakpoint has nothing behind it
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "unbound", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Unbound {
    /// the path does not name a file on disk
    Unresolvable {
        /// the path as the client gave it
        file: PathBuf,
        /// what the filesystem said
        reason: String,
        /// whether the interpreter has code carrying exactly that filename
        ///
        /// true is the shape of a module loaded from a zip archive, a frozen
        /// module, or a string handed to `exec` — the name is real, the file
        /// is not
        loaded_under_that_name: bool,
    },

    /// the path is a file, but nothing the interpreter has loaded comes from it
    NotLoaded {
        /// the path as the client gave it
        file: PathBuf,
    },

    /// the interpreter has run code from the file, but never the file itself
    ///
    /// binding walks down from the code object the file's module compiled to,
    /// so without it only part of the file is visible — and every answer taken
    /// from a partial view is wrong in a way that looks right. so nothing is
    /// answered from one
    PartiallyLoaded {
        /// the path as the client gave it
        file: PathBuf,
    },

    /// the file is loaded and has no executable line at or after the one asked for
    NoExecutableLine {
        /// the path as the client gave it
        file: PathBuf,
        /// the line that was asked for
        requested: u32,
        /// the last line of that file the interpreter can stop on, if it has one
        last_executable: Option<u32>,
    },

    /// the condition does not compile
    ///
    /// nothing about the file is wrong. a breakpoint whose condition cannot be
    /// answered can never fire, so it is refused now — with the interpreter's
    /// own words — rather than at some line the user is waiting on
    ConditionInvalid {
        /// the expression as the client wrote it
        condition: String,
        /// what the interpreter said about it
        error: PythonError,
    },

    /// the log message cannot be used
    LogMessageInvalid {
        /// the template as the client wrote it
        log: String,
        /// the embedded expression at fault, when one of them is
        expression: Option<String>,
        /// what is wrong with it
        reason: String,
    },
}

impl std::fmt::Display for Unbound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unresolvable {
                file,
                reason,
                loaded_under_that_name,
            } => {
                write!(
                    formatter,
                    "`{}` is not a file on disk: {reason}",
                    file.display()
                )?;
                if *loaded_under_that_name {
                    write!(
                        formatter,
                        ". the interpreter has loaded code under exactly that \
                         name, so it came from somewhere that is not the \
                         filesystem — a zip archive, a frozen module, or a \
                         string passed to `exec`. bpd binds breakpoints to \
                         files it can identify on disk, and cannot identify \
                         that one"
                    )?;
                }
                Ok(())
            }
            Self::NotLoaded { file } => write!(
                formatter,
                "the interpreter has not loaded any code from `{}`. it will \
                 bind if that file is imported later",
                file.display()
            ),
            Self::PartiallyLoaded { file } => write!(
                formatter,
                "the interpreter has run code from `{}` but never the file \
                 itself, so bpd has seen only part of it and cannot say where a \
                 breakpoint in it would go. a module first reached from inside \
                 a breakpoint's condition does this, because the interpreter \
                 reports no code object created while a monitoring callback is \
                 running. import it somewhere the program itself runs, or take \
                 the condition that imports it off",
                file.display()
            ),
            Self::NoExecutableLine {
                file,
                requested,
                last_executable,
            } => {
                write!(
                    formatter,
                    "`{}` has no executable line at or after line {requested}",
                    file.display()
                )?;
                match last_executable {
                    Some(last) => write!(formatter, ". the last one is line {last}"),
                    None => write!(formatter, ". it has no executable lines at all"),
                }
            }
            Self::ConditionInvalid { condition, error } => write!(
                formatter,
                "the condition `{condition}` does not compile: {error}. a \
                 breakpoint whose condition cannot be answered can never fire, \
                 so it is not set"
            ),
            Self::LogMessageInvalid {
                log,
                expression,
                reason,
            } => {
                write!(formatter, "the log message `{log}` cannot be used")?;
                match expression {
                    Some(expression) => {
                        write!(formatter, ": `{{{expression}}}` {reason}")
                    }
                    None => write!(formatter, ": {reason}"),
                }
            }
        }
    }
}

/// what the agent tells the engine
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FromAgent {
    /// the debuggee stopped
    Stopped {
        /// why it stopped
        reason: StopReason,
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
}

/// what the engine tells the agent
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FromEngine {
    /// let the debuggee run
    Resume,

    /// replace the whole breakpoint set
    ///
    /// the complete set rather than a delta: a debugger that accumulates edits
    /// has two ideas of what is set, and they diverge
    SetBreakpoints {
        /// every breakpoint that should be armed after this request
        breakpoints: Vec<SourceBreakpoint>,
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
    use super::*;

    #[test]
    fn an_agent_event_round_trips() {
        let sent = FromAgent::Stopped {
            reason: StopReason::Entry,
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
        write(&mut wire, &FromEngine::Resume).expect("writing to a vec cannot fail");

        let received: Option<FromEngine> =
            read(&mut wire.as_slice(), &mut Vec::new()).expect("the frame is whole");
        assert_eq!(received, Some(FromEngine::Resume));
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
            reason: StopReason::Breakpoint {
                breakpoints: vec![1, 4],
                file: "/tmp/program.py".to_string(),
                line: 12,
                thread: 8_482_561_408,
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
    fn every_unbound_reason_says_what_to_do_about_it() {
        let file = PathBuf::from("/tmp/program.py");
        let cases = [
            (
                Unbound::Unresolvable {
                    file: file.clone(),
                    reason: "Not a directory (os error 20)".to_string(),
                    loaded_under_that_name: true,
                },
                "zip archive",
            ),
            (Unbound::NotLoaded { file: file.clone() }, "imported later"),
            (
                Unbound::PartiallyLoaded { file: file.clone() },
                "never the file itself",
            ),
            (
                Unbound::NoExecutableLine {
                    file: file.clone(),
                    requested: 40,
                    last_executable: Some(12),
                },
                "the last one is line 12",
            ),
            (
                Unbound::NoExecutableLine {
                    file,
                    requested: 40,
                    last_executable: None,
                },
                "no executable lines at all",
            ),
        ];

        for (reason, expected) in cases {
            let said = reason.to_string();
            assert!(
                said.contains("/tmp/program.py"),
                "a refusal must name the file, got {said}"
            );
            assert!(said.contains(expected), "expected {expected:?} in {said:?}");
        }
    }

    #[test]
    fn a_refused_condition_or_log_message_quotes_the_thing_that_is_wrong() {
        let cases = [
            (
                Unbound::ConditionInvalid {
                    condition: "value ==".to_string(),
                    error: PythonError {
                        kind: "SyntaxError".to_string(),
                        message: "invalid syntax".to_string(),
                        traceback: Vec::new(),
                    },
                },
                ["value ==", "SyntaxError: invalid syntax", "can never fire"],
            ),
            (
                Unbound::LogMessageInvalid {
                    log: "count is {".to_string(),
                    expression: None,
                    reason: "there is a `{` that is never closed".to_string(),
                },
                ["count is {", "never closed", "cannot be used"],
            ),
            (
                Unbound::LogMessageInvalid {
                    log: "count is {1 +}".to_string(),
                    expression: Some("1 +".to_string()),
                    reason: "does not compile: SyntaxError: invalid syntax".to_string(),
                },
                ["{1 +}", "does not compile", "cannot be used"],
            ),
        ];

        for (reason, expected) in cases {
            let said = reason.to_string();
            for wanted in expected {
                assert!(said.contains(wanted), "expected {wanted:?} in {said:?}");
            }
        }
    }

    #[test]
    fn a_condition_that_raised_round_trips_with_its_traceback() {
        let sent = FromAgent::Stopped {
            reason: StopReason::EvaluationFailed {
                breakpoint: 3,
                part: Part::Condition,
                expression: "value.missing".to_string(),
                file: "/tmp/program.py".to_string(),
                line: 12,
                thread: 8_482_561_408,
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
    fn a_frame_that_is_not_json_is_refused_rather_than_guessed_at() {
        let mut wire = Vec::new();
        frame::write_frame(&mut wire, b"\xff\xfe not json at all")
            .expect("writing to a vec cannot fail");

        read::<_, FromEngine>(&mut wire.as_slice(), &mut Vec::new())
            .expect_err("a desynchronised stream must not decode");
    }
}
