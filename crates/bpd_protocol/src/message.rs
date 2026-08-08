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
        /// every breakpoint bound to this line, smallest id first
        ///
        /// more than one is ordinary — a breakpoint moved off a comment can
        /// land on a line another breakpoint already sits on
        breakpoints: Vec<u32>,
        /// the `co_filename` of the code object that was running
        file: String,
        /// the line it stopped on
        line: u32,
        /// the interpreter's identity for the thread that stopped, as
        /// `threading.get_ident` reports it
        thread: u64,
    },
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
    },

    /// nothing will stop, and this is why
    Unbound {
        /// what stood in the way
        reason: Unbound,
    },
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

    /// the file is loaded and has no executable line at or after the one asked for
    NoExecutableLine {
        /// the path as the client gave it
        file: PathBuf,
        /// the line that was asked for
        requested: u32,
        /// the last line of that file the interpreter can stop on, if it has one
        last_executable: Option<u32>,
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
            breakpoints: vec![SourceBreakpoint {
                id: 7,
                file: PathBuf::from("/tmp/program.py"),
                line: 3,
            }],
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
