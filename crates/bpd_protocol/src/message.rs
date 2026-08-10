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
    /// the number a [`FrameId`] carries, and the number a request naming a stop
    /// uses. it is minted once per stop and never reused
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
/// not getting anywhere, which is what [`FromEngine::Threads`] is for
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

/// which frame of the stopped thread's stack something is asked about
///
/// an id is minted at a stop and names the stop it was minted at, so a client
/// that holds one across a resume finds out rather than reading a frame that is
/// no longer the one it meant. DAP's opaque handle cannot do that: it looks the
/// same before and after, and the debugger has to guess which the client meant
///
/// `depth` counts from the frame that stopped, so `0` is always where the
/// program is now
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct FrameId {
    /// which stop this id belongs to, counting from one
    pub stop: u64,
    /// how far down the stack, with the frame that stopped at zero
    pub depth: u32,
}

impl std::fmt::Display for FrameId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "frame {} of stop {}", self.depth, self.stop)
    }
}

/// one frame of the stopped thread's stack
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Frame {
    /// how to ask about this frame, for as long as this stop lasts
    pub id: FrameId,
    /// the `co_filename` of the code it is running
    pub file: String,
    /// the line it is on now, as `f_lineno` reports it
    pub line: u32,
    /// `co_qualname`
    pub function: String,
    /// `co_firstlineno`, which separates two code objects with the same name
    pub first_line: u32,
}

/// where a name lives, which is not a detail a debugger may round off
///
/// python resolves a name in a function by which of these it is, decided at
/// compile time. merging them into one "variables" mapping — which is what
/// `f_locals` itself does — means a report that cannot distinguish a captured
/// variable from a global of the same name, and "a variable read from the wrong
/// scope" is the thing this project exists not to do
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// the frame's own locals — `co_varnames`
    ///
    /// for a module or a class body there are none of these in the code object,
    /// and the frame's namespace mapping is the local scope instead
    Local,

    /// locals of this frame that a nested function captures — `co_cellvars`
    ///
    /// an argument that a closure captures is in this scope **and** in
    /// [`Scope::Local`], because cpython says it is both
    Cell,

    /// variables this frame captures from an enclosing one — `co_freevars`
    ///
    /// the value lives in the enclosing frame's cell. it is not a local of this
    /// frame and it is not a global
    Free,

    /// the module namespace the frame's code was compiled into — `f_globals`
    Global,
}

impl std::fmt::Display for Scope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Local => "local",
            Self::Cell => "cell",
            Self::Free => "free",
            Self::Global => "global",
        })
    }
}

/// how much of a value to read, and what the debugger may run to read it
///
/// every field is a bound the answer is held to, and every bound that bites is
/// named in the answer. there is no setting here that makes a value quietly
/// incomplete
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Detail {
    /// how many levels of container or object to open
    ///
    /// zero reports a value's type and size and opens nothing
    #[serde(default = "Detail::depth")]
    pub depth: u32,

    /// how many children of one container to read
    #[serde(default = "Detail::children")]
    pub children: u32,

    /// how many characters of one string, or bytes of one `bytes`, to read
    #[serde(default = "Detail::text")]
    pub text: u32,

    /// the byte budget for the whole answer
    ///
    /// spent on the text a value carries, its type name, and a fixed cost per
    /// value for the envelope around it. when it runs out the answer says so at
    /// the point it ran out, rather than being quietly shorter than it looks
    #[serde(default = "Detail::budget")]
    pub budget: u32,

    /// read an object's instance dictionary
    ///
    /// on by default, because it is **storage**: for an ordinary object it is a
    /// slot read that runs nothing, and it never reaches `__getattr__`, a
    /// property or any other descriptor. a type is free to make `__dict__` its
    /// own code, and then this runs that code — which is why it can be turned
    /// off for a program full of proxies or mocks
    #[serde(default = "Detail::attributes")]
    pub attributes: bool,

    /// call `__repr__` on a value that has no structural representation
    ///
    /// off by default, because it is **behaviour**: `__repr__` is arbitrary user
    /// code that can hang, mutate the program, or reach the network. bpd cannot
    /// interrupt it once it has started, so it is never called unless the
    /// request asked for it
    #[serde(default)]
    pub repr: bool,
}

impl Detail {
    /// the default depth
    const fn depth() -> u32 {
        3
    }
    /// the default number of children per container
    const fn children() -> u32 {
        100
    }
    /// the default number of characters of one string
    const fn text() -> u32 {
        1024
    }
    /// the default byte budget for one answer
    ///
    /// this is a starting point rather than a settled answer: the budget is
    /// spending an agent's context window, and what it is worth cannot be known
    /// until there is an agent surface to measure it against
    const fn budget() -> u32 {
        8192
    }
    /// whether an object's instance dictionary is read by default
    const fn attributes() -> bool {
        true
    }
}

impl Default for Detail {
    fn default() -> Self {
        Self {
            depth: Self::depth(),
            children: Self::children(),
            text: Self::text(),
            budget: Self::budget(),
            attributes: Self::attributes(),
            repr: false,
        }
    }
}

/// a value as the debugger read it
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Value {
    /// `type(value)`, qualified by its module unless it is a builtin
    ///
    /// always present, and always the value's real type: a `defaultdict` reads
    /// as a mapping and says it is a `collections.defaultdict`
    pub kind: String,
    /// what it is
    pub content: Content,
}

/// what a value turned out to be
///
/// the structural forms are read through cpython's concrete C interface — the
/// object's own storage — so an overridden `__getitem__` or `__iter__` cannot
/// change what is reported, and reading one runs no python
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "content", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Content {
    /// `None`
    None,

    /// a `bool`
    Bool {
        /// which one
        value: bool,
    },

    /// an integer, in decimal
    ///
    /// text rather than a number because a python `int` has no width, and a
    /// json number that silently became a float would be a different value.
    /// text is **never** cut: half of a number is a different number, so an
    /// integer too long for the budget is left out entirely and says so
    Int {
        /// the digits, or empty when `omitted` says why they are not here
        text: String,
        /// why the digits are not here
        omitted: Option<Omitted>,
    },

    /// a float, as `float.__repr__` writes it
    ///
    /// python's own text, so `inf`, `nan` and `-0.0` survive — a json number
    /// cannot carry the first two at all
    Float {
        /// the repr
        text: String,
    },

    /// a string, as itself rather than as a repr
    Str {
        /// the characters, cut to the request's limit
        text: String,
        /// how many characters the whole string has
        characters: usize,
        /// why they are not all here
        omitted: Option<Omitted>,
    },

    /// `bytes` or a `bytearray`, in lowercase hex
    Bytes {
        /// the bytes, in hex, cut to the request's limit
        hex: String,
        /// how many bytes the whole value has
        length: usize,
        /// why they are not all here
        omitted: Option<Omitted>,
    },

    /// a list, a tuple or a set
    Sequence {
        /// the items, in order for a list or a tuple and in iteration order for
        /// a set
        items: Vec<Value>,
        /// how many items the whole value has
        length: usize,
        /// why they are not all here
        omitted: Option<Omitted>,
    },

    /// a mapping, as pairs rather than as names
    ///
    /// a key can be any object, so it is a value in its own right. a mapping
    /// reported as `name: value` would be a lie about every dict that is not
    /// keyed by strings
    Mapping {
        /// the entries, in iteration order
        entries: Vec<Pair>,
        /// how many entries the whole mapping has
        length: usize,
        /// why they are not all here
        omitted: Option<Omitted>,
    },

    /// an object, read from its instance dictionary
    Object {
        /// the attributes it stores
        attributes: Vec<Entry>,
        /// why they are not all here, or why there are none
        omitted: Option<Omitted>,
    },

    /// what `__repr__` said, because the request asked for it
    ///
    /// labelled, so nothing can mistake user code's opinion of a value for the
    /// value
    Repr {
        /// the text, cut to the request's limit
        text: String,
        /// how many characters it produced
        characters: usize,
        /// why they are not all here
        omitted: Option<Omitted>,
    },

    /// nothing was read, and this is why
    ///
    /// a cycle, or a budget that ran out before this value was reached
    Unread {
        /// what stopped it
        omitted: Omitted,
    },
}

/// one named thing: a variable, or an attribute
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// the name
    pub name: String,
    /// what it holds
    pub value: Value,
}

/// one entry of a mapping
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Pair {
    /// the key, which is a value like any other
    pub key: Value,
    /// what it maps to
    pub value: Value,
}

/// what is not in an answer, and why
///
/// every one of these is a statement that something exists and is not here. an
/// answer that was cut and did not say so is worse for an agent than for a
/// person, who would at least see the ellipsis
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "omitted", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Omitted {
    /// there is more text than the request asked to see
    Text {
        /// how long the whole thing is
        characters: usize,
        /// what the request allowed
        limit: u32,
    },

    /// there are more children than the request asked to see
    Children {
        /// how many there are
        length: usize,
        /// what the request allowed
        limit: u32,
    },

    /// the depth ran out here
    Depth {
        /// the depth that was applied
        ///
        /// the request's `depth`, unless [`Omitted::Shallower`] says the budget
        /// could not fit it
        limit: u32,
    },

    /// the request's depth did not fit the budget, so less of it was read
    ///
    /// a set of variables is read at the deepest whole level the budget allows,
    /// rather than at the level asked for until it runs out. spending the whole
    /// budget on whichever variable came first is honest and useless: every
    /// module namespace begins with `__builtins__`, and an answer that opened
    /// that and nothing else would be a true statement about the wrong thing
    Shallower {
        /// the depth the request asked for
        asked: u32,
        /// the depth that fitted
        used: u32,
    },

    /// the answer's byte budget ran out here
    Budget {
        /// what the request allowed
        limit: u32,
    },

    /// this object is already open further up the same answer
    ///
    /// a structure that points back at itself terminates here and says where it
    /// came round to, rather than stopping silently — which would look exactly
    /// like a structure that ended
    Cycle {
        /// where in this answer it was already opened
        path: String,
    },

    /// the type keeps no instance dictionary
    ///
    /// a `__slots__` class, or a type implemented in C. what it holds is only
    /// reachable by running its own code
    NoAttributes,

    /// the request did not ask for an object's attributes
    AttributesNotRequested,

    /// reading the instance dictionary raised
    ///
    /// which means the type made `__dict__` its own code, and that code failed
    AttributesRaised {
        /// what it raised
        error: PythonError,
    },

    /// the namespace is not a dictionary
    ///
    /// a class body whose metaclass prepared its own mapping — what `enum` does
    /// — has one. reading it means calling that mapping's own code, which is
    /// the program, so it is named instead of run
    NotADictionary,

    /// the string holds code points that cannot be encoded as utf-8
    ///
    /// lone surrogates, which is what `surrogateescape` produces for a
    /// filename the filesystem encoding could not decode. json cannot carry
    /// them and neither can rust, so they are replaced rather than dropped
    Unencodable,

    /// entries of an object's dictionary whose keys are not names
    ///
    /// reachable by writing into `__dict__` directly. they are not attributes,
    /// nothing can read them by name, and they are not silently dropped either
    NotNames {
        /// how many there are
        count: usize,
    },
}

impl std::fmt::Display for Omitted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text { characters, limit } => write!(
                formatter,
                "{characters} characters, of which the request allowed {limit}. \
                 ask again with a larger `text`"
            ),
            Self::Children { length, limit } => write!(
                formatter,
                "{length} children, of which the request allowed {limit}. ask \
                 again with a larger `children`"
            ),
            Self::Depth { limit } => write!(
                formatter,
                "the depth of {limit} that was applied ran out here. ask again \
                 with a larger `depth`"
            ),
            Self::Shallower { asked, used } => write!(
                formatter,
                "the request asked for a depth of {asked} and the byte budget \
                 fitted {used}, so every value here was read to {used}. ask \
                 again with a larger `budget`, or for one value rather than a \
                 whole scope"
            ),
            Self::Budget { limit } => write!(
                formatter,
                "the request's byte budget of {limit} ran out here. ask again \
                 with a larger `budget`, or for less of the graph"
            ),
            Self::Cycle { path } => write!(
                formatter,
                "this is the same object as `{path}`, which is already open \
                 above it. the structure points back at itself"
            ),
            Self::NoAttributes => formatter.write_str(
                "the type keeps no instance dictionary — it uses `__slots__`, or \
                 it is implemented in C — so what it holds cannot be read \
                 without running its own code. ask again with `repr`",
            ),
            Self::AttributesNotRequested => formatter.write_str(
                "the request asked for no attributes, so the object was not \
                 opened. ask again with `attributes`",
            ),
            Self::AttributesRaised { error } => write!(
                formatter,
                "reading the instance dictionary raised {error}, so the type \
                 made `__dict__` its own code and that code failed"
            ),
            Self::NotADictionary => formatter.write_str(
                "the namespace is not a `dict` — a class body whose metaclass \
                 prepared its own mapping has one — so reading it would mean \
                 running that mapping's own code",
            ),
            Self::Unencodable => formatter.write_str(
                "the string holds code points that cannot be encoded as utf-8 — \
                 lone surrogates, which is what `surrogateescape` produces for \
                 an undecodable filename — and they are replaced here with \
                 U+FFFD",
            ),
            Self::NotNames { count } => write!(
                formatter,
                "{count} entries of the instance dictionary have keys that are \
                 not names, so they are not attributes and nothing can read \
                 them by name"
            ),
        }
    }
}

/// what an expression did
///
/// an expression that raised has an answer, and the answer is the exception.
/// reporting `None` for it would be the debugger inventing a value the program
/// never produced
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "evaluated", rename_all = "snake_case")]
pub enum Evaluated {
    /// it produced a value
    Value {
        /// what it produced
        value: Value,
    },
    /// it raised
    Raised {
        /// what it raised
        error: PythonError,
    },
}

/// a request the agent will not answer, and why
///
/// separate from an expression that raised: that is an answer. this is the
/// agent refusing to guess what was meant
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "refused", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Refusal {
    /// the frame id was minted at a stop that is no longer held
    StaleFrame {
        /// what was asked about
        frame: FrameId,
        /// the stops that are held now
        held: Vec<u64>,
    },

    /// the stopped thread's stack is not that deep
    NoSuchFrame {
        /// what was asked about
        frame: FrameId,
        /// how many frames there are
        depth: usize,
    },

    /// that scope of that frame holds no such name
    NoSuchVariable {
        /// which frame
        frame: FrameId,
        /// which scope it was asked for in
        scope: Scope,
        /// the name
        name: String,
        /// the scopes of that frame that do hold it
        elsewhere: Vec<Scope>,
    },

    /// the name is in that scope and the frame does not expose it
    ///
    /// the read says the same thing, in `unreadable`. a write is refused
    /// outright: putting it in the frame's namespace mapping would leave a
    /// value the compiled code never reads and report a change the program did
    /// not receive
    UnreadableVariable {
        /// which frame
        frame: FrameId,
        /// which scope it is in
        scope: Scope,
        /// the name
        name: String,
    },

    /// no thread is held under that stop number
    ///
    /// several threads can be held at once, so a request that names a stop
    /// names one of them. a stop that has been resumed is gone, and answering
    /// from whichever stop happened to be nearest would be answering a
    /// different question
    NoSuchStop {
        /// the stop that was asked about
        stop: u64,
        /// the stops that are held now
        held: Vec<u64>,
    },

    /// that thread is not one this agent is holding
    ///
    /// resuming a thread that is running is not a no-op to report quietly: the
    /// client believes it is holding something it is not, and the next thing it
    /// waits for will never come
    ThreadNotHeld {
        /// the thread that was named
        thread: u64,
        /// the threads that are held now
        held: Vec<u64>,
    },

    /// the request needs a held thread and there is none
    ///
    /// the agent runs the interpreter's own api to answer this, and it can only
    /// do that on a thread it is holding. asking a program with nothing held
    /// would be a request answered whenever it next happened to stop
    NothingHeld {
        /// what was asked for
        wanted: String,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleFrame { frame, held } => write!(
                formatter,
                "{frame} belongs to a stop that has ended — the stops held now \
                 are {held:?}. a frame id is valid for one stop, because the \
                 frame it named has run on since. ask for the stack again"
            ),
            Self::NoSuchFrame { frame, depth } => write!(
                formatter,
                "{frame} does not exist: the stopped thread's stack is {depth} \
                 frames deep"
            ),
            Self::NoSuchVariable {
                frame,
                scope,
                name,
                elsewhere,
            } => {
                write!(formatter, "`{name}` is not in the {scope} scope of {frame}")?;
                if elsewhere.is_empty() {
                    formatter.write_str(
                        ". it is not in any scope of that frame. writing it \
                         would be accepted by `f_locals` and the program would \
                         never see it, because compiled code reads the fast \
                         locals the compiler gave it and nothing else",
                    )
                } else {
                    formatter.write_str(". it is in the ")?;
                    for (index, scope) in elsewhere.iter().enumerate() {
                        if index > 0 {
                            formatter.write_str(" and ")?;
                        }
                        write!(formatter, "{scope}")?;
                    }
                    formatter.write_str(" scope of it — ask for it there")
                }
            }
            Self::UnreadableVariable { frame, scope, name } => write!(
                formatter,
                "`{name}` is in the {scope} scope of {frame} and that frame does \
                 not expose it: the value lives in a cell only the function \
                 object holds, which is how a class body sees a variable of the \
                 function around it. writing it into the frame's namespace would \
                 leave a value the compiled code never reads"
            ),
            Self::NoSuchStop { stop, held } => write!(
                formatter,
                "stop {stop} is not held — the stops held now are {held:?}. a \
                 stop ends when its thread is resumed, and the thread has run \
                 on since"
            ),
            Self::ThreadNotHeld { thread, held } => write!(
                formatter,
                "thread {thread} is not held — the threads held now are \
                 {held:?}. a stop holds one thread and leaves the rest running, \
                 so a thread bpd never stopped is one it cannot resume"
            ),
            Self::NothingHeld { wanted } => write!(
                formatter,
                "no thread is held, so there is nothing to answer {wanted} on. \
                 the agent runs the interpreter's own api on a thread it is \
                 holding and at no other time"
            ),
        }
    }
}

/// where a thread was when it was sampled
///
/// the innermost python frame, which for a thread inside a C call is the frame
/// that made the call rather than the call itself — the interpreter has no
/// frame for one, and inventing a location for it would be the debugger
/// describing something it cannot see
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Where {
    /// the `co_filename` of the code it is running
    pub file: String,
    /// the line it is on, as `f_lineno` reports it
    pub line: u32,
    /// `co_qualname`
    pub function: String,
}

impl std::fmt::Display for Where {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}:{} in {}",
            self.file, self.line, self.function
        )
    }
}

/// whether a thread was seen to get anywhere between two samples
///
/// this is the general half of the lock problem, and it is deliberately not
/// called a diagnosis. cpython exposes no owner for a lock, so bpd cannot say
/// "thread 7 is waiting for a lock thread 3 holds". what it can say is that
/// thread 7 was in the same place twice, a stated interval apart, which is the
/// symptom the user is actually looking at when they think bpd has hung
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Progress {
    /// bpd is holding this thread, so it did not move because bpd stopped it
    Held,
    /// it was somewhere else in the second sample
    Moved,
    /// it was in the same place in both samples
    ///
    /// not proof of anything on its own: a thread blocked in `sock.recv` and a
    /// thread piled up behind a lock the held thread took look identical from
    /// here. it is where to look, not what is wrong
    Still,
}

/// one thread of the debuggee, as of a sample
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadState {
    /// the interpreter's identity for it
    pub thread: u64,
    /// the stop holding it, when bpd is holding it
    pub held: Option<u64>,
    /// where it was, or `None` when it has no python frame of the program's
    ///
    /// the agent's own bootstrap frame is not a location: it is the `-c` the
    /// interpreter was entered through, and reporting it would put a frame of
    /// bpd's in front of a user
    pub at: Option<Where>,
    /// whether it was seen to get anywhere
    pub progress: Progress,
}

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
    /// the stop it produces arrives separately, as a [`StopReason::Paused`] one.
    /// `running` is what says whether to expect it: a thread parked in a C call
    /// has released the GIL and executes no python, so it reaches no line and
    /// nothing here can hold it. an empty `running` means the pause is armed
    /// and **nothing is going to arrive** until some thread runs python again
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
        /// what [`Progress::Still`] means, in the answer, rather than in the
        /// client's memory of what it asked for
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

    /// the agent will not answer the request, and this is why
    Refused {
        /// what stood in the way
        reason: Refusal,
    },
}

/// which held threads a resume is about
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "which", rename_all = "snake_case")]
pub enum Which {
    /// every thread that is held right now
    ///
    /// resolved by the agent at the moment the request arrives, so a thread
    /// that stopped while this was in flight is included. that is deliberate:
    /// the alternative is a client that asked for everything and got a program
    /// with one thread still held and nothing saying so
    All,
    /// exactly these, by the interpreter's thread identity
    ///
    /// naming a thread that is not held is refused, not ignored
    Named {
        /// the threads to let go
        threads: Vec<u64>,
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
    /// as a [`StopReason::Stepped`] stop of its own
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
    fn every_omission_says_what_is_missing_and_how_to_ask_for_it() {
        let cases = [
            (
                Omitted::Text {
                    characters: 4000,
                    limit: 100,
                },
                ["4000", "`text`"],
            ),
            (
                Omitted::Children {
                    length: 900,
                    limit: 10,
                },
                ["900", "`children`"],
            ),
            (Omitted::Depth { limit: 2 }, ["depth of 2", "`depth`"]),
            (
                Omitted::Shallower { asked: 3, used: 1 },
                ["asked for a depth of 3", "fitted 1"],
            ),
            (Omitted::Budget { limit: 64 }, ["budget of 64", "`budget`"]),
            (
                Omitted::Cycle {
                    path: "node.next".to_string(),
                },
                ["node.next", "points back at itself"],
            ),
            (Omitted::NoAttributes, ["__slots__", "`repr`"]),
            (
                Omitted::AttributesNotRequested,
                ["no attributes", "`attributes`"],
            ),
            (Omitted::NotNames { count: 2 }, ["2 entries", "not names"]),
            (Omitted::Unencodable, ["surrogate", "U+FFFD"]),
            (Omitted::NotADictionary, ["metaclass", "not a `dict`"]),
        ];

        for (omission, expected) in cases {
            let said = omission.to_string();
            for wanted in expected {
                assert!(said.contains(wanted), "expected {wanted:?} in {said:?}");
            }
        }
    }

    #[test]
    fn a_refusal_names_the_frame_and_what_to_do_instead() {
        let frame = FrameId { stop: 1, depth: 2 };
        let cases = [
            (
                Refusal::StaleFrame {
                    frame,
                    held: vec![4],
                },
                vec!["frame 2 of stop 1", "[4]", "ask for the stack again"],
            ),
            (
                Refusal::NoSuchFrame { frame, depth: 2 },
                vec!["frame 2 of stop 1", "2 frames deep"],
            ),
            (
                Refusal::NoSuchVariable {
                    frame,
                    scope: Scope::Local,
                    name: "total".to_string(),
                    elsewhere: vec![Scope::Free, Scope::Global],
                },
                vec!["`total`", "local scope", "free and global"],
            ),
            (
                Refusal::NoSuchVariable {
                    frame,
                    scope: Scope::Local,
                    name: "typo".to_string(),
                    elsewhere: Vec::new(),
                },
                vec!["`typo`", "the program would never see it"],
            ),
            (
                Refusal::UnreadableVariable {
                    frame,
                    scope: Scope::Free,
                    name: "captured".to_string(),
                },
                vec!["`captured`", "free scope", "class body"],
            ),
            (
                Refusal::NoSuchStop {
                    stop: 2,
                    held: vec![5, 6],
                },
                vec!["stop 2 is not held", "[5, 6]"],
            ),
            (
                Refusal::ThreadNotHeld {
                    thread: 11,
                    held: vec![12],
                },
                vec!["thread 11 is not held", "[12]"],
            ),
            (
                Refusal::NothingHeld {
                    wanted: "the breakpoints to resolve".to_string(),
                },
                vec!["no thread is held", "the breakpoints to resolve"],
            ),
        ];

        for (refusal, expected) in cases {
            let said = refusal.to_string();
            for wanted in expected {
                assert!(said.contains(wanted), "expected {wanted:?} in {said:?}");
            }
        }
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

    #[test]
    fn a_frame_that_is_not_json_is_refused_rather_than_guessed_at() {
        let mut wire = Vec::new();
        frame::write_frame(&mut wire, b"\xff\xfe not json at all")
            .expect("writing to a vec cannot fail");

        read::<_, FromEngine>(&mut wire.as_slice(), &mut Vec::new())
            .expect_err("a desynchronised stream must not decode");
    }
}
