//! a debug script: a tree of debugger steps submitted in one call, and the
//! transcript of what happened at every one of them
//!
//! ## why it is data rather than text
//!
//! the steps are a schema-validated tree, submitted as data. an MCP tool takes
//! JSON Schema input already, so a tree of steps needs **no parser, no grammar
//! and no syntax errors**, and the schema is itself the documentation. the
//! predicates inside it *are* python, evaluated in a chosen frame through the
//! machinery a breakpoint condition already uses — that is the half python is
//! good at
//!
//! ## what the transcript claims
//!
//! the **transcript is the return value**, not the final state: an agent given
//! only where a script ended cannot tell why, and will guess. so every record
//! says which step of the submitted tree it came from, where the held thread
//! was when it ran, and — for a branch — which way it went
//!
//! three rules keep one from lying, and each is enforced here or in the engine
//! that walks this:
//!
//! - **a budget is mandatory**, on three axes. exhausting one ends the script
//!   and the transcript says so, naming which bound bit and where
//! - **a step that fails halts the script.** there is no carrying on past one:
//!   the steps after it would run somewhere the script did not intend, and the
//!   record would describe an investigation that did not happen
//! - **no step reports a location the program was not at.** an [`At`] is built
//!   from a [`Stop`] the agent reported and from nothing else
//!
//! ## what is deliberately not in a transcript
//!
//! **no measured duration.** the same script over the same program state has to
//! produce the same transcript, so that an agent can re-run one to confirm a
//! reading rather than trusting its memory of it. a wall clock reading would
//! differ between two runs and make every transcript unequal to every other

use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;
use std::time::Duration;

use crate::breakpoint::{Binding, HitCondition, Resolved, Unbound};
use crate::exception::PythonError;
use crate::frame::Frame;
use crate::stop::{Mode, StepKind, Stop, StopReason};
use crate::value::{Detail, Evaluated};

/// a program of debugger steps, and what it may spend running them
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    /// the steps, in the order they run
    pub steps: Vec<Step>,
    /// what the whole script may spend
    ///
    /// no default, and no way to leave it out. a script without one is a
    /// session that can hang, and the reason submitted python was rejected for
    /// this job is that a budget is needed regardless
    pub budget: Budget,
}

/// what a script may spend before it is stopped and labelled partial
///
/// three axes, and all three bite. the **byte** budget is usually the first: an
/// object graph read inside a loop spends it long before fifty steps have run
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// how many steps may run
    ///
    /// one per record, including the test of an `if` and each test of a `while`
    pub steps: NonZeroU32,

    /// how long the whole script may take, in milliseconds
    ///
    /// this is also the deadline every control step waits under: a script that
    /// is waiting for a program which never stops is spending exactly this
    pub wall_ms: NonZeroU64,

    /// how many bytes of transcript may be recorded
    ///
    /// a byte is a byte of the json one record serialises to. the unit has to
    /// be something, both front ends render json, and json is what spends the
    /// context window this bound exists to protect
    ///
    /// checked after each record, so one record can carry the total past it —
    /// by at most one value read, whose own bound is its `detail.budget`.
    /// [`Transcript::bytes`] says what was really recorded either way, so the
    /// overshoot is never silent
    pub bytes: NonZeroU32,
}

impl Budget {
    /// how long the whole script may take
    pub const fn wall(&self) -> Duration {
        Duration::from_millis(self.wall_ms.get())
    }
}

/// one step of a script
///
/// deliberately closed. a front end that could absorb an unknown step into a
/// catch-all arm would be a front end that ran a script it did not understand
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    /// step the script's thread to the next line of its frame
    StepOver,

    /// step the script's thread into the next frame it enters
    StepIn,

    /// run the script's thread's frame to its end
    StepOut,

    /// let the script's thread go until it stops again
    ///
    /// only that thread. a script drives the one thread its starting stop
    /// holds, and resuming the others would be the script letting go of threads
    /// nobody named
    Continue,

    /// run until the script's thread reaches a source location
    ///
    /// the engine arms a breakpoint of its own, runs, and takes it back off —
    /// which is why this is a step of a script rather than a tool of a front
    /// end. an adapter that armed a breakpoint of its own would be making a
    /// decision about the program, and under a deadline it could not take it
    /// off again, because a one-shot breakpoint cannot be removed from a
    /// program that is running
    RunTo {
        /// the file, as a path
        file: PathBuf,
        /// the line to run to
        line: u32,
        /// a python expression that has to be true for it to count
        ///
        /// the breakpoint condition machinery, unchanged — which is how *run to
        /// the call with a negative amount* is written
        #[serde(default)]
        condition: Option<String>,
        /// which of the qualifying hits to stop on
        #[serde(default)]
        hits: Option<HitCondition>,
    },

    /// evaluate a python expression and record what it produced
    ///
    /// this runs the program's own code, by request, exactly as `evaluate`
    /// does. an expression that raises **halts the script**: carrying on past
    /// one would record an investigation that did not happen
    Eval {
        /// the expression, as python
        expression: String,
        /// which frame of the stop the script is at, with 0 the frame it is in
        #[serde(default)]
        frame: u32,
        /// how much of the result to read
        #[serde(default)]
        detail: Detail,
    },

    /// record the script's thread's frame chain
    Stack {
        /// how many frames, counting from the one that stopped
        #[serde(default)]
        top: Option<u32>,
    },

    /// record a note of the script's own
    ///
    /// nothing reaches the debuggee: it is text the script wrote, so that a
    /// transcript of fifty records says what the script thought it was doing.
    /// recording a *value* is [`Step::Eval`], which is a different thing and
    /// costs the program an evaluation
    Log {
        /// the note
        note: String,
    },

    /// run one of two blocks, according to a python predicate
    If {
        /// what decides
        predicate: Predicate,
        /// what runs when it is true
        #[serde(default)]
        then: Vec<Step>,
        /// what runs when it is false
        #[serde(default)]
        otherwise: Vec<Step>,
    },

    /// run a block while a python predicate is true, at most `limit` times
    ///
    /// the limit is not optional and cannot be zero. a loop without a bound is
    /// a hung session, and a script that cannot be shown to terminate is
    /// refused at submission rather than discovered at runtime — which is what
    /// this field makes structural
    While {
        /// what decides
        predicate: Predicate,
        /// the most passes of the body there may be
        limit: NonZeroU32,
        /// what runs on each pass
        #[serde(default)]
        body: Vec<Step>,
    },

    /// end the script here, with a reason
    Finish {
        /// why the script stopped here, in the script's own words
        because: String,
    },
}

impl Step {
    /// what to call this step in a message about it
    pub const fn name(&self) -> &'static str {
        match self {
            Self::StepOver => "step_over",
            Self::StepIn => "step_in",
            Self::StepOut => "step_out",
            Self::Continue => "continue",
            Self::RunTo { .. } => "run_to",
            Self::Eval { .. } => "eval",
            Self::Stack { .. } => "stack",
            Self::Log { .. } => "log",
            Self::If { .. } => "if",
            Self::While { .. } => "while",
            Self::Finish { .. } => "finish",
        }
    }

    /// the most records this step can produce
    ///
    /// saturating, because nested loops multiply and a number that wrapped
    /// would understate a bound rather than overstate it
    fn at_most(&self) -> u64 {
        match self {
            Self::If {
                then, otherwise, ..
            } => 1u64.saturating_add(at_most(then).max(at_most(otherwise))),
            // one test per pass, plus the body, plus the record that says the
            // limit was reached
            Self::While { limit, body, .. } => u64::from(limit.get())
                .saturating_mul(1u64.saturating_add(at_most(body)))
                .saturating_add(1),
            _ => 1,
        }
    }

    /// how deep the tree under this step goes, counting this step as one
    fn depth(&self) -> u32 {
        match self {
            Self::If {
                then, otherwise, ..
            } => 1 + depth(then).max(depth(otherwise)),
            Self::While { body, .. } => 1 + depth(body),
            _ => 1,
        }
    }
}

/// the most records a block of steps can produce
fn at_most(steps: &[Step]) -> u64 {
    steps
        .iter()
        .fold(0u64, |total, step| total.saturating_add(step.at_most()))
}

/// how deep a block of steps nests
fn depth(steps: &[Step]) -> u32 {
    steps.iter().map(Step::depth).max().unwrap_or(0)
}

/// what decides a branch, as python
///
/// it has to produce a **`bool`**. anything else halts the script naming the
/// type it produced, because truth-testing an arbitrary object means running
/// the program's own `__bool__` or `__len__` and deciding a branch on the
/// result — and re-deriving cpython's truthiness in rust would be a second
/// implementation of a rule cpython owns. writing the comparison down
/// (`x is not None`, `len(items) > 0`) puts it in the transcript, where a
/// reader can see what was actually asked
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Predicate {
    /// the expression, as python
    pub expression: String,
    /// which frame of the stop the script is at, with 0 the frame it is in
    #[serde(default)]
    pub frame: u32,
}

/// how deep a step tree may nest
///
/// the executor walks the tree recursively, so a tree deep enough would exhaust
/// the engine's own stack. it is refused at submission with the depth in it
/// rather than found at a stack overflow, which reports nothing at all
pub const MAX_DEPTH: u32 = 32;

/// why a script was refused before any of it ran
///
/// separate from [`Halted`], which is a step that failed while running. these
/// are answered without touching the program at all
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "refused", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Refused {
    /// the tree nests deeper than the executor will walk
    TooDeep {
        /// how deep it goes
        depth: u32,
        /// how deep it may go
        limit: u32,
    },

    /// there is no breakpoint id left for a `run_to` to arm its own under
    ///
    /// the script's breakpoint has to be distinguishable from the client's in
    /// every report about it, so it takes an id no breakpoint of the set has
    NoBreakpointIdLeft {
        /// the highest id the breakpoint set already uses
        highest: u32,
    },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooDeep { depth, limit } => write!(
                formatter,
                "this script nests {depth} blocks deep and bpd walks at most \
                 {limit}. flatten it, or submit it as two scripts"
            ),
            Self::NoBreakpointIdLeft { highest } => write!(
                formatter,
                "a `run_to` arms a breakpoint of its own, under an id no \
                 breakpoint of the set uses, and the set already uses {highest} \
                 — the largest there is. set the breakpoints again with smaller \
                 ids"
            ),
        }
    }
}

impl Script {
    /// the most records this script can produce
    ///
    /// computable because every loop carries a bound, which is the whole reason
    /// a step tree is submitted rather than python. it is what lets a script be
    /// answered with "this runs at most 40 steps" before it runs
    pub fn at_most(&self) -> u64 {
        at_most(&self.steps)
    }

    /// how deep the tree nests
    pub fn depth(&self) -> u32 {
        depth(&self.steps)
    }

    /// refuse a script that cannot be walked, before any of it runs
    ///
    /// # errors
    ///
    /// when the tree nests deeper than [`MAX_DEPTH`]
    pub fn examine(&self) -> Result<(), Refused> {
        let depth = self.depth();
        if depth > MAX_DEPTH {
            return Err(Refused::TooDeep {
                depth,
                limit: MAX_DEPTH,
            });
        }
        Ok(())
    }
}

/// where a held thread was, and why it is held there
///
/// built from a [`Stop`] the agent reported and from nothing else, which is
/// what makes "no step reports a location the program was not at" structural
/// rather than a habit
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct At {
    /// the stop that holds the thread
    pub stop: u64,
    /// the interpreter's identity for the thread
    pub thread: u64,
    /// the file and line it is at
    ///
    /// `None` at the entry stop and nowhere else: the program is held before
    /// its first statement, so there is no line it is at yet
    pub place: Option<Place>,
    /// why it is held here
    pub why: StopReason,
}

impl At {
    /// where a stop is
    pub fn of(stop: &Stop) -> Self {
        Self {
            stop: stop.stop,
            thread: stop.thread,
            place: place_of(&stop.reason),
            why: stop.reason.clone(),
        }
    }
}

/// a file and a line of it
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Place {
    /// the `co_filename` of the code object that is running
    pub file: String,
    /// the line
    pub line: u32,
}

/// the line a stop reason names, when it names one
fn place_of(reason: &StopReason) -> Option<Place> {
    let (file, line) = match reason {
        StopReason::Entry => return None,
        StopReason::Breakpoint { file, line, .. }
        | StopReason::Stepped { file, line, .. }
        | StopReason::Paused { file, line }
        | StopReason::Raised { file, line, .. }
        | StopReason::Uncaught { file, line, .. }
        | StopReason::EvaluationFailed { file, line, .. } => (file, line),
    };
    Some(Place {
        file: file.clone(),
        line: *line,
    })
}

/// what a script did, step by step
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Transcript {
    /// the most records this script could have produced, computed before it ran
    pub at_most: u64,
    /// how many bytes of record were made
    ///
    /// always, rather than only when the byte budget bit — a transcript that
    /// ended for another reason at the same moment would otherwise have gone
    /// over its bound without saying so
    pub bytes: u64,
    /// what happened, in the order it happened
    pub records: Vec<Record>,
    /// what loading a file changed about the breakpoint set while the script ran
    ///
    /// it is a fact about the program that the script learned, and a client
    /// never told would go on believing a breakpoint of its own is unbound
    pub rebound: Vec<Resolved>,
    /// how it ended
    pub outcome: Outcome,
}

impl Transcript {
    /// whether this transcript is a **partial** record of the script
    ///
    /// true when a budget ran out. the steps after the one it stopped at did
    /// not run, and nothing here says anything about them
    pub const fn partial(&self) -> bool {
        matches!(self.outcome, Outcome::Exhausted { .. })
    }
}

/// one thing a script did
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Record {
    /// which step of the submitted tree this was
    ///
    /// the position in the tree, counting from one, with a branch named on the
    /// way in: `3` is the third step, `3.then.1` is the first step of its
    /// `then` block, and `4.body.2` is the second step of a `while` body. a
    /// loop's body records the same path on every pass, and the test before
    /// each pass says which pass it is
    pub step: String,
    /// where the held thread was when this step ran
    pub at: At,
    /// what the step did
    pub did: Did,
}

/// what one step did
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "did", rename_all = "snake_case")]
#[non_exhaustive]
#[expect(
    clippy::large_enum_variant,
    reason = "a `run_to` says more about itself than a `log` does, and that is \
              the vocabulary rather than an oversight. boxing a field of a \
              report type to even them up would put an indirection in the thing \
              a reader reads, to save bytes on a value that is built once per \
              step and moved once into a vector"
)]
pub enum Did {
    /// a step of the script's thread
    Stepped {
        /// which way
        kind: StepKind,
        /// what came of letting the thread go
        landed: Landed,
    },

    /// the script's thread was let go until it stopped again
    Continued {
        /// what came of letting the thread go
        landed: Landed,
    },

    /// the script armed a breakpoint of its own and ran to it
    RanTo {
        /// the file it was armed on
        file: PathBuf,
        /// the line it was armed on
        line: u32,
        /// the id it was armed under, which is how the stop names it
        ///
        /// an id no breakpoint of the client's set uses, so a stop that names
        /// it is unambiguously this step's
        armed_as: u32,
        /// where that breakpoint bound, or why it did not
        binding: Binding,
        /// what came of letting the thread go, when there was a breakpoint to
        /// run to at all
        landed: Option<Landed>,
        /// what became of the script's own breakpoint
        disarmed: Disarmed,
    },

    /// an expression was evaluated in a frame
    Evaluated {
        /// the expression, as the script wrote it
        expression: String,
        /// which frame it was evaluated in
        frame: u32,
        /// what it produced, or what it raised
        result: Evaluated,
    },

    /// the thread's frame chain was walked
    Walked {
        /// the frames, the one that stopped first
        frames: Vec<Frame>,
        /// how deep the stack is
        depth: usize,
        /// how the program was moving while it was taken
        mode: Mode,
    },

    /// the script recorded a note of its own
    Logged {
        /// the note, as the script wrote it
        note: String,
    },

    /// a predicate decided which block of an `if` ran
    Branched {
        /// the predicate, as the script wrote it
        expression: String,
        /// which frame it was evaluated in
        frame: u32,
        /// what it answered
        answered: Answered,
    },

    /// a predicate decided whether another pass of a `while` body ran
    Tested {
        /// the predicate, as the script wrote it
        expression: String,
        /// which frame it was evaluated in
        frame: u32,
        /// which pass this test was for, counting from one
        pass: u32,
        /// what it answered
        answered: Answered,
    },

    /// a loop ran its whole allowance of passes and its predicate was still
    /// true
    ///
    /// the loop did not finish what it was for, so the script stops here rather
    /// than running the steps after it somewhere they did not expect
    Bounded {
        /// the allowance
        limit: NonZeroU32,
    },

    /// the script ended itself
    Finished {
        /// why, in the script's own words
        because: String,
    },

    /// the session refused the request this step is made of
    ///
    /// a frame the stack does not have, a stop that has ended, a thread that is
    /// not held. every step has a record even when it did nothing, because a
    /// step missing from a transcript is a step a reader would assume ran
    Refused {
        /// what the step was trying to do
        doing: String,
        /// what the session said
        reason: String,
    },
}

/// what came of letting the script's thread go
///
/// deliberately closed: every one of these is something a caller has to decide
/// about, and a catch-all arm is how a script acquires a state nobody handles
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "landed", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Landed {
    /// the thread stopped, and this is where
    Stopped {
        /// where it stopped
        to: At,
    },

    /// it stopped, and not for the reason this step asked for
    ///
    /// a breakpoint of the client's during a `run_to`, or an exception during a
    /// step. it is a real stop and it is not where the step was going, so the
    /// steps after it would run somewhere the script did not intend
    Elsewhere {
        /// where it stopped
        to: At,
    },

    /// another thread stopped and the one this step was about did not
    ///
    /// a stop holds one thread and the rest of the program keeps running, so a
    /// second thread can reach a breakpoint while the first is still going.
    /// this step's own thread is **still running**, and there is nothing
    /// truthful to say about where it is
    OtherThread {
        /// where the other thread stopped
        to: At,
        /// the thread this step was about, which is still running
        expected: u64,
    },

    /// the program exited
    Exited {
        /// the exit code, with a signalled process reported as `128 + signal`
        exit_code: i64,
    },

    /// the program is over and its exit is not bpd's to read
    ///
    /// separate from [`Self::Exited`] and carrying no number, because there is
    /// none to carry: bpd did not start that process and never learns what it
    /// exited with. see [`crate::Running::Ended`]
    Ended,

    /// the program ran to its end with threads still held
    ///
    /// it cannot exit: the interpreter finalizes by joining the program's
    /// non-daemon threads, and a held one cannot be joined
    Finishing {
        /// the threads still held
        threads: Vec<u64>,
    },

    /// the script's wall clock budget passed and the program is still running
    ///
    /// not a stop, and it carries no location of any kind. everything the agent
    /// inside the debuggee answers, it answers on a thread it is holding, so a
    /// program with nothing held cannot be asked where it is
    StillRunning,
}

/// what a predicate answered
///
/// deliberately closed. there is no fourth thing an expression can do — it
/// produced a `bool`, it produced something else, or it raised — and a
/// catch-all arm here would be a branch taken on an answer nobody read
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "answered", rename_all = "snake_case")]
pub enum Answered {
    /// it produced a `bool`
    Value {
        /// which one
        value: bool,
    },

    /// it raised
    Raised {
        /// what it raised
        error: PythonError,
    },

    /// it produced something that is not a `bool`
    NotABool {
        /// `type()` of what it produced
        kind: String,
    },
}

/// what became of the breakpoint a `run_to` armed of its own
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "disarmed", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Disarmed {
    /// it was taken back off, and the client's breakpoint set is what it was
    Removed,

    /// nothing was armed: the location did not bind, so there was nothing to
    /// take off
    NothingArmed,

    /// the program ended, so there is nothing left for it to be armed in
    ProgramEnded,

    /// the program was still running, so bpd held a thread to take it off
    ///
    /// a one-shot breakpoint cannot be removed from a program that is running,
    /// and leaving one armed would leave the program stopping at a place nobody
    /// asked about. so a pause is armed — which holds the next thread that
    /// reaches a line — and this is where it landed
    PausedToRemove {
        /// where the pause landed
        at: At,
    },

    /// it is **still armed**, and so is the pause bpd armed to take it off
    ///
    /// the program was running, and no thread reached a line to be held at, so
    /// there was nothing to take it off on. this is the one case a `run_to`
    /// leaves something behind, and it says so rather than leaving it to be
    /// found
    StillArmed {
        /// the file it is armed on
        file: PathBuf,
        /// the line it is armed on
        line: u32,
        /// the id it is armed under
        id: u32,
        /// the threads that were running python when the pause was armed
        ///
        /// empty means nothing was going to reach it: every thread was parked
        /// in a C call, where there is no monitoring event to hold one at
        running: Vec<u64>,
    },
}

impl std::fmt::Display for Disarmed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Removed => formatter.write_str(
                "the script's own breakpoint was taken back off, and the \
                 breakpoint set is what the client last asked for",
            ),
            Self::NothingArmed => formatter.write_str(
                "the location did not bind, so there was no breakpoint of the \
                 script's to take off",
            ),
            Self::ProgramEnded => formatter.write_str(
                "the program ended, so there is nothing left for the script's own \
                 breakpoint to be armed in",
            ),
            Self::PausedToRemove { at } => write!(
                formatter,
                "the program was still running, so bpd paused it to take its own \
                 breakpoint back off — thread {} is now held at stop {}",
                at.thread, at.stop
            ),
            Self::StillArmed {
                file,
                line,
                id,
                running,
            } => write!(
                formatter,
                "the script's own breakpoint is **still armed** on `{}` line \
                 {line}, as id {id}, and so is the pause bpd armed to take it \
                 off. the program was running and no thread reached a line in \
                 time — {}. wait for the program to stop, then set the \
                 breakpoints again to take id {id} off",
                file.display(),
                if running.is_empty() {
                    "every thread was parked in a C call, where there is no \
                     monitoring event to hold one at"
                        .to_string()
                } else {
                    format!(
                        "{} thread(s) were running python: {running:?}",
                        running.len()
                    )
                }
            ),
        }
    }
}

/// how a script ended
///
/// deliberately closed, for the reason every other outcome in `bpd` is: a
/// fifth ending is something every reader has to decide about
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// every step ran and the script reached its end
    Ran,

    /// a `finish` step ended it
    Finished {
        /// which step of the tree
        at: String,
        /// why, in the script's own words
        because: String,
    },

    /// a step could not be completed, and the script stopped there
    ///
    /// the steps after it did not run. carrying on past a failure would produce
    /// a record of an investigation that did not happen
    Halted {
        /// which step of the tree
        at: String,
        /// what stood in the way
        why: Halted,
    },

    /// a budget ran out, so this transcript is **partial**
    Exhausted {
        /// the step of the tree that was running, or about to
        at: String,
        /// which bound bit
        bound: Bound,
        /// how many records were made before it did
        made: u32,
    },
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ran => formatter.write_str("every step of the script ran"),
            Self::Finished { at, because } => {
                write!(formatter, "the script ended itself at step {at}: {because}")
            }
            Self::Halted { at, why } => write!(
                formatter,
                "the script halted at step {at}: {why}. the steps after it did \
                 not run"
            ),
            Self::Exhausted { at, bound, made } => write!(
                formatter,
                "this transcript is **partial**: {bound}. it stopped at step \
                 {at} with {made} record(s) made, and the steps after it did not \
                 run"
            ),
        }
    }
}

/// which budget ran out
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "bound", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Bound {
    /// the script ran as many steps as it was allowed
    Steps {
        /// what it was allowed
        limit: NonZeroU32,
    },

    /// the script took as long as it was allowed
    Wall {
        /// what it was allowed, in milliseconds
        limit_ms: NonZeroU64,
    },

    /// the transcript is as long as it was allowed to be
    Bytes {
        /// what it was allowed
        limit: NonZeroU32,
        /// how many were recorded, which one record can carry past the limit
        recorded: u64,
    },
}

impl std::fmt::Display for Bound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Steps { limit } => write!(
                formatter,
                "the step budget of {limit} ran out. submit it again with a \
                 larger `steps`, or with fewer passes of the loops in it"
            ),
            Self::Wall { limit_ms } => write!(
                formatter,
                "the wall clock budget of {limit_ms}ms ran out. it is also the \
                 deadline every control step waits under, so a program that does \
                 not stop spends the whole of it: submit it again with a larger \
                 `wall_ms`"
            ),
            Self::Bytes { limit, recorded } => write!(
                formatter,
                "the transcript budget of {limit} bytes ran out at {recorded}. a \
                 value read inside a loop is what usually spends it: submit it \
                 again with a larger `bytes`, or with a smaller `detail` on the \
                 reads"
            ),
        }
    }
}

/// why a script stopped at a step rather than running the rest
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "halted", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Halted {
    /// an expression raised
    Raised {
        /// the expression, as the script wrote it
        expression: String,
        /// what it raised
        error: PythonError,
    },

    /// a predicate produced something that is not a `bool`
    NotABool {
        /// the expression, as the script wrote it
        expression: String,
        /// `type()` of what it produced
        kind: String,
    },

    /// a `run_to` named a location nothing will stop at
    Unbound {
        /// what stood in the way
        reason: Unbound,
    },

    /// the thread stopped somewhere the step did not ask for
    Elsewhere {
        /// where it stopped
        to: At,
    },

    /// another thread stopped and the one the step was about did not
    OtherThread {
        /// where the other thread stopped
        to: At,
        /// the thread the step was about, which is still running
        expected: u64,
    },

    /// the program exited
    Exited {
        /// the exit code
        exit_code: i64,
    },

    /// the program is over and its exit is not bpd's to read
    Ended,

    /// the program ran to its end with threads still held
    Finishing {
        /// the threads still held
        threads: Vec<u64>,
    },

    /// a loop ran its whole allowance of passes and its predicate was still
    /// true
    Bounded {
        /// the allowance
        limit: NonZeroU32,
    },

    /// the session refused the request the step is made of
    Refused {
        /// what it said
        reason: String,
    },
}

impl std::fmt::Display for Halted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Raised { expression, error } => write!(
                formatter,
                "`{expression}` raised {error}. an expression that raised has \
                 not answered, and a script that carried on would be recording \
                 an investigation that did not happen"
            ),
            Self::NotABool { expression, kind } => write!(
                formatter,
                "the predicate `{expression}` produced a `{kind}` rather than a \
                 `bool`. truth-testing it would mean running the program's own \
                 `__bool__` or `__len__` and branching on the result — write the \
                 comparison down instead, as `x is not None` or `len(x) > 0`"
            ),
            Self::Unbound { reason } => write!(formatter, "{reason}"),
            Self::Elsewhere { to } => write!(
                formatter,
                "the thread stopped at stop {} for a reason this step did not \
                 ask for, so it is not where the script was going and the steps \
                 after it would run somewhere they did not expect",
                to.stop
            ),
            Self::OtherThread { to, expected } => write!(
                formatter,
                "thread {} stopped and thread {expected} — the one this step was \
                 about — is still running. a stop holds one thread and leaves \
                 the rest going, and there is nothing truthful to say about \
                 where a running one is",
                to.thread
            ),
            Self::Exited { exit_code } => write!(
                formatter,
                "the program exited with {exit_code}, so there is nothing left \
                 to run the rest of the script against"
            ),
            Self::Ended => formatter.write_str(
                "the program is over, so there is nothing left to run the rest \
                 of the script against. bpd did not start that process and is \
                 not its parent, so what it exited with is not bpd's to read",
            ),
            Self::Finishing { threads } => write!(
                formatter,
                "the program ran to its end with {} thread(s) still held: \
                 {threads:?}. it cannot exit until they are resumed, and the \
                 thread this script was driving has no more program to run",
                threads.len()
            ),
            Self::Bounded { limit } => write!(
                formatter,
                "the loop ran its allowance of {limit} pass(es) and its \
                 predicate was still true, so it did not finish what it was for. \
                 submit it again with a larger `limit`, or with a predicate that \
                 goes false"
            ),
            Self::Refused { reason } => write!(formatter, "{reason}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> Budget {
        Budget {
            steps: NonZeroU32::new(40).expect("40 is not zero"),
            wall_ms: NonZeroU64::new(1000).expect("1000 is not zero"),
            bytes: NonZeroU32::new(8192).expect("8192 is not zero"),
        }
    }

    /// a stop as an agent reports one, named by the session it arrived on
    fn reported(stop: u64, reason: StopReason) -> Stop {
        crate::stop::Reported {
            stop,
            thread: 7,
            reason,
            holding: Vec::new(),
        }
        .in_session(crate::SessionId::new(
            NonZeroU64::new(1).expect("1 is not zero"),
        ))
    }

    fn predicate() -> Predicate {
        Predicate {
            expression: "x > 1".to_string(),
            frame: 0,
        }
    }

    #[test]
    fn a_script_says_how_many_steps_it_can_run_before_it_runs() {
        // the claim the whole shape exists for: every loop carries a bound, so
        // a submitted tree can be answered with "this runs at most n steps"
        // without running it — which arbitrary python cannot be
        let script = Script {
            steps: vec![
                Step::Log {
                    note: "starting".to_string(),
                },
                Step::While {
                    predicate: predicate(),
                    limit: NonZeroU32::new(3).expect("3 is not zero"),
                    body: vec![Step::StepOver, Step::StepOver],
                },
            ],
            budget: budget(),
        };

        // one log, then three passes of (one test plus two steps), plus the
        // record that says the limit was reached
        assert_eq!(script.at_most(), 1 + 3 * 3 + 1);
    }

    #[test]
    fn a_tree_too_deep_to_walk_is_refused_before_any_of_it_runs() {
        let mut steps = vec![Step::StepOver];
        for _ in 0..MAX_DEPTH {
            steps = vec![Step::While {
                predicate: predicate(),
                limit: NonZeroU32::new(1).expect("1 is not zero"),
                body: steps,
            }];
        }
        let script = Script {
            steps,
            budget: budget(),
        };

        let refused = script.examine().expect_err("it nests deeper than the walk");
        let said = refused.to_string();
        assert!(said.contains("flatten it"), "said {said}");

        // and one exactly at the limit is walked
        let shallower = Script {
            steps: vec![Step::StepOver],
            budget: budget(),
        };
        shallower.examine().expect("one step nests one deep");
    }

    #[test]
    fn a_loop_limit_of_zero_cannot_be_written_down_at_all() {
        // the bound is not a validation rule that could be forgotten. a `while`
        // carries a `NonZeroU32`, so a script with an unbounded loop does not
        // deserialise
        let refused = serde_json::from_value::<Step>(serde_json::json!({
            "step": "while",
            "predicate": { "expression": "x" },
            "limit": 0,
            "body": [],
        }))
        .expect_err("a loop that may run zero times is not a loop");
        assert!(
            refused.to_string().contains("zero"),
            "said {refused}, which has to name the field's own rule"
        );
    }

    #[test]
    fn a_budget_cannot_be_left_out_of_a_script() {
        // a script without one is a session that can hang, which is the whole
        // reason a step tree exists rather than submitted python
        let refused = serde_json::from_value::<Script>(serde_json::json!({
            "steps": [ { "step": "step_over" } ],
        }))
        .expect_err("a script without a budget is not a script");
        assert!(refused.to_string().contains("budget"), "said {refused}");
    }

    #[test]
    fn every_place_a_transcript_reports_comes_from_a_stop_reason() {
        // there is no other constructor. a location bpd invented would have to
        // be written here, in the open
        let entry = At::of(&reported(1, StopReason::Entry));
        assert_eq!(
            entry.place, None,
            "the program has run nothing at entry, so there is no line it is at"
        );

        let stepped = At::of(&reported(
            2,
            StopReason::Stepped {
                kind: StepKind::Over,
                file: "/tmp/a.py".to_string(),
                line: 12,
            },
        ));
        assert_eq!(
            stepped.place,
            Some(Place {
                file: "/tmp/a.py".to_string(),
                line: 12,
            })
        );
    }

    #[test]
    fn every_way_a_script_can_end_badly_says_what_to_do_about_it() {
        let cases: Vec<(Halted, &str)> = vec![
            (
                Halted::NotABool {
                    expression: "items".to_string(),
                    kind: "list".to_string(),
                },
                "len(x) > 0",
            ),
            (
                Halted::Bounded {
                    limit: NonZeroU32::new(4).expect("4 is not zero"),
                },
                "larger `limit`",
            ),
            (
                Halted::Exited { exit_code: 3 },
                "nothing left to run the rest",
            ),
            (
                Halted::OtherThread {
                    to: At {
                        stop: 4,
                        thread: 9,
                        place: None,
                        why: StopReason::Entry,
                    },
                    expected: 7,
                },
                "still running",
            ),
        ];

        for (halted, expected) in cases {
            let said = halted.to_string();
            assert!(said.contains(expected), "expected {expected:?} in {said:?}");
        }
    }

    #[test]
    fn a_budget_that_ran_out_says_which_one_and_what_to_raise() {
        let cases: Vec<(Bound, &str)> = vec![
            (
                Bound::Steps {
                    limit: NonZeroU32::new(20).expect("20 is not zero"),
                },
                "larger `steps`",
            ),
            (
                Bound::Wall {
                    limit_ms: NonZeroU64::new(500).expect("500 is not zero"),
                },
                "larger `wall_ms`",
            ),
            (
                Bound::Bytes {
                    limit: NonZeroU32::new(64).expect("64 is not zero"),
                    recorded: 900,
                },
                "larger `bytes`",
            ),
        ];

        for (bound, expected) in cases {
            let said = bound.to_string();
            assert!(said.contains(expected), "expected {expected:?} in {said:?}");
        }
    }

    #[test]
    fn a_transcript_that_ran_out_of_budget_says_it_is_partial() {
        let exhausted = Transcript {
            at_most: 9,
            bytes: 400,
            records: Vec::new(),
            rebound: Vec::new(),
            outcome: Outcome::Exhausted {
                at: "2.body.1".to_string(),
                bound: Bound::Steps {
                    limit: NonZeroU32::new(3).expect("3 is not zero"),
                },
                made: 3,
            },
        };
        assert!(exhausted.partial());
        assert!(
            exhausted.outcome.to_string().contains("**partial**"),
            "an agent cannot see an elision a person would: {}",
            exhausted.outcome
        );

        let whole = Transcript {
            at_most: 1,
            bytes: 0,
            records: Vec::new(),
            rebound: Vec::new(),
            outcome: Outcome::Ran,
        };
        assert!(!whole.partial());
    }
}
