//! every way `bpd` can refuse to do something
//!
//! there is deliberately no catch-all string variant and no `NotImplemented`.
//! a variant exists because a real, reachable failure mode exists, and every
//! one of them carries enough context for a user to act on it

use std::path::PathBuf;

use crate::python::{Implementation, PythonVersion};

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
    /// there is no fallback path — see the python support policy in `AGENTS.md`
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
}
