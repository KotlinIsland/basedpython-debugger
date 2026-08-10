//! every way a session can refuse to answer, of the reasons that are about the
//! program rather than about `bpd`
//!
//! the split is what the failure *describes*. an interpreter that cannot be
//! debugged, a program with nothing held, a request that names a stop when
//! several are held — all of those describe the thing being debugged, so every
//! front end has to render them and they belong here. a socket that failed, an
//! agent artifact that could not be found, an interval that does not fit the
//! wire — those describe `bpd`'s own machinery, and they live in `bpd_engine`
//!
//! there is deliberately no catch-all string variant and no `NotImplemented`.
//! a variant exists because a real, reachable failure mode exists, and every
//! one of them carries enough context for a user to act on it

use std::path::PathBuf;

use crate::python::{Implementation, PythonVersion};
use crate::refusal::Refusal;

/// the result type used throughout `bpd`
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// a failure that `bpd` reports rather than works around
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// the interpreter could not be executed at all
    #[error("could not run `{path}`")]
    InterpreterLaunch {
        /// the interpreter that was asked for
        path: PathBuf,
        /// the underlying spawn failure
        #[source]
        source: std::io::Error,
    },

    /// the interpreter ran but did not answer the capability probe
    ///
    /// this is a broken or non-conforming interpreter, not an unsupported one
    #[error("`{path}` did not answer the capability probe: {reason}")]
    InterpreterProbe {
        /// the interpreter that was asked for
        path: PathBuf,
        /// what was wrong with the answer
        reason: String,
    },

    /// the interpreter is older than the minimum this debugger supports
    ///
    /// there is no fallback path — see `docs/development/python-support.md`
    #[error(
        "`{path}` is python {found}, and bpd requires at least {minimum}. \
         the event backbone is PEP 669 (`sys.monitoring`), and there is no \
         `sys.settrace` fallback"
    )]
    UnsupportedVersion {
        /// the interpreter that was asked for
        path: PathBuf,
        /// the version it reported
        found: PythonVersion,
        /// the oldest version that can be debugged
        minimum: PythonVersion,
    },

    /// the interpreter is not cpython
    #[error(
        "`{path}` is {found}, and bpd only drives cpython. PEP 669 and PEP 768 \
         are cpython interfaces, and emulating them on another implementation \
         would mean reporting state bpd cannot verify"
    )]
    UnsupportedImplementation {
        /// the interpreter that was asked for
        path: PathBuf,
        /// the implementation it reported
        found: Implementation,
    },

    /// the interpreter claims a supported version but has no `sys.monitoring`
    ///
    /// a patched or stripped build. it cannot be debugged, and pretending
    /// otherwise would mean silently attaching a debugger that never stops
    #[error("`{path}` reports python {found} but has no `sys.monitoring`")]
    MonitoringUnavailable {
        /// the interpreter that was asked for
        path: PathBuf,
        /// the version it reported
        found: PythonVersion,
    },

    /// two breakpoints in one request claimed the same id
    ///
    /// the id is how every later report — a rebinding, a stop — names which
    /// breakpoint it is about. sharing one would mean the client is given one
    /// answer for two questions and cannot tell which it belongs to
    #[error(
        "two breakpoints in the same request both have id {id}. an id names one \
         breakpoint in every report about it, so it has to be unique within a set"
    )]
    DuplicateBreakpointId {
        /// the id that was used twice
        id: u32,
    },

    /// something was asked of a debuggee with no thread held
    ///
    /// the agent answers on a thread it is holding, so a request made to a
    /// program with nothing held would be answered whenever it next happened to
    /// stop. that is not an answer, and waiting for it looks exactly like a hang
    #[error(
        "no thread of the debuggee is held, so it cannot be asked for {wanted}. \
         the agent runs the interpreter's own api on a thread it is holding and \
         at no other time — hold one first, by letting the program run to a \
         breakpoint or by pausing it"
    )]
    NotStopped {
        /// what was asked for
        wanted: &'static str,
    },

    /// something was asked of a program that has ended
    ///
    /// distinct from [`Self::NotStopped`], and the distinction is the whole
    /// reason this variant exists: "nothing is held" invites holding something,
    /// and there is nothing left to hold. a client told the first would go on
    /// pausing a process that is not there
    #[error(
        "the program has exited with {code}, so there is nothing left to ask \
         for {wanted}. every stop it had has ended with it — `launch` another \
         program to debug one again"
    )]
    ProgramExited {
        /// what the program exited with, as [`crate::exit_code`] renders one
        code: i64,
        /// what was asked for
        wanted: &'static str,
    },

    /// a request that is about one stop was made while several were held
    ///
    /// a stop holds one thread and there can be more than one of them at a
    /// time. answering from whichever happened to be first would be answering
    /// about a thread the caller did not name
    #[error(
        "{wanted} is about one held thread and {} are held: {held:?}. name the \
         stop it is about",
        held.len()
    )]
    AmbiguousStop {
        /// what was asked for
        wanted: &'static str,
        /// the stops that are held
        held: Vec<u64>,
    },

    /// a debug script was refused before any of it ran
    ///
    /// examined rather than attempted: a script that cannot be walked, or one
    /// with nowhere to arm its own breakpoint, is answered without touching the
    /// program at all. that is the whole advantage of a step tree over
    /// submitted python — it can be read before it runs
    #[error("this debug script was not run: {reason}")]
    ScriptRefused {
        /// what stood in the way
        reason: crate::script::Refused,
    },

    /// a state was asked for under an id this session never gave out
    ///
    /// the one way a snapshot id fails. it does not go stale — it names a
    /// reading that was already taken rather than a promise to take one — so an
    /// id that resolves to nothing is one this session did not mint, and
    /// answering it from whichever state happened to be nearest would be
    /// comparing something nobody asked about
    #[error(
        "no state of this session is `{id}`. the states it holds are {held:?}. \
         an id is given out by the state query and stays valid for the whole \
         session, so one that resolves to nothing came from somewhere else"
    )]
    NoSuchSnapshot {
        /// the id that was asked for
        id: crate::query::SnapshotId,
        /// the ids this session holds
        held: Vec<String>,
    },

    /// the agent understood the request and would not answer it
    ///
    /// not a failure of `bpd`'s machinery: answering would have meant guessing
    /// what was meant about the program
    #[error("{reason}")]
    Refused {
        /// what stood in the way
        reason: Refusal,
    },
}
