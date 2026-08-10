//! breakpoints: what the client asked for, and what is actually behind it
//!
//! the rule the whole module serves is that `bpd` never reports a breakpoint as
//! set unless there is a code object and an offset behind it. a request is a
//! [`SourceBreakpoint`], what became of it is a [`Resolved`], and the half of
//! that which says nothing will stop is an [`Unbound`] carrying why

use std::num::NonZeroU32;
use std::path::PathBuf;

use crate::exception::PythonError;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
