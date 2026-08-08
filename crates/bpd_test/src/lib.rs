//! test support: finding and running the real interpreters `bpd` is tested
//! against
//!
//! the contract says that anything touching interpreter behaviour needs a test
//! that spawns a real interpreter, because a unit test over a mocked frame
//! proves nothing about cpython. this crate is what those tests use to find one
//!
//! the policy it enforces is **no silent skip**. a test that quietly passes
//! because no interpreter was installed is a test that reports success while
//! proving nothing, which is the exact failure mode this project exists to
//! avoid. when there is nothing to test against, [`Interpreters::require`]
//! fails and says how to fix it

// a test support crate that cannot say which interpreters it found is useless
// the first time a matrix test fails on someone else's machine
#![allow(clippy::print_stderr)]

pub mod agent;
pub mod alloc;
pub mod debuggee;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use bpd_core::python::{Capabilities, MINIMUM_SUPPORTED};

/// command names probed when the environment does not name interpreters itself
///
/// ordered most specific first, so the deduplication below keeps `python3.14`
/// rather than whichever `python3` happens to point at the same binary
const CANDIDATES: &[&str] = &[
    "python3.13",
    "python3.13t",
    "python3.14",
    "python3.14t",
    "python3.15",
    "python3.15t",
    "python3",
    "python",
];

/// names interpreters explicitly, delimited the way `PATH` is on the platform
///
/// this is how CI pins the matrix, and how anyone with interpreters in an
/// unusual place points the suite at them
pub const INTERPRETERS_ENV: &str = "BPD_TEST_PYTHONS";

/// the interpreters available to this test run
#[derive(Debug)]
pub struct Interpreters {
    found: Vec<Capabilities>,
}

impl Interpreters {
    /// every interpreter that answered the probe, whatever version it is
    ///
    /// includes interpreters `bpd` refuses to debug — a test that asserts on a
    /// refusal needs one
    pub fn all(&self) -> &[Capabilities] {
        &self.found
    }

    /// the interpreters `bpd` can actually drive
    pub fn supported(&self) -> Vec<&Capabilities> {
        self.found
            .iter()
            .filter(|capabilities| capabilities.require_debuggable().is_ok())
            .collect()
    }

    /// the interpreters `bpd` can drive, or a failure naming how to get one
    ///
    /// call this at the top of any test whose result would otherwise be
    /// meaningless on a machine with no supported interpreter
    pub fn require(&self) -> Vec<&Capabilities> {
        let supported = self.supported();
        assert!(
            !supported.is_empty(),
            "no supported python interpreter was found, so this test would \
             prove nothing\n\n\
             bpd requires cpython {MINIMUM_SUPPORTED} or newer. what answered \
             the probe:\n{}\n\
             install one:\n    uv python install 3.13 3.14\n\
             or name them explicitly:\n    {INTERPRETERS_ENV}=/path/to/python3.14 cargo test",
            self.describe()
        );
        supported
    }

    fn describe(&self) -> String {
        if self.found.is_empty() {
            return "    (nothing — no candidate command answered)\n".to_string();
        }
        let mut described = String::new();
        for capabilities in &self.found {
            writeln!(
                described,
                "    {} -> {} ({})",
                capabilities.interpreter.display(),
                capabilities.version,
                capabilities.implementation
            )
            .expect("writing to a string cannot fail");
        }
        described
    }
}

/// the interpreters on this machine, probed once per process
pub fn discovered() -> &'static Interpreters {
    static DISCOVERED: OnceLock<Interpreters> = OnceLock::new();
    DISCOVERED.get_or_init(|| {
        let interpreters = discover();
        eprintln!(
            "bpd_test: {} interpreter(s) discovered\n{}",
            interpreters.found.len(),
            interpreters.describe()
        );
        interpreters
    })
}

fn discover() -> Interpreters {
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();

    for candidate in candidates() {
        // a candidate name that is simply not installed is not a failure — it
        // is the normal case for most of the list. the loud failure belongs at
        // `require`, where an empty *result* means the test cannot prove
        // anything
        let Ok(capabilities) = Capabilities::probe(&candidate) else {
            continue;
        };
        if seen.insert(capabilities.executable.clone()) {
            found.push(capabilities);
        }
    }

    Interpreters { found }
}

fn candidates() -> Vec<PathBuf> {
    candidates_from(std::env::var_os(INTERPRETERS_ENV))
}

/// split out from [`candidates`] so it is testable without mutating the
/// process environment, which is unsafe in edition 2024 and racy under a
/// threaded test runner either way
fn candidates_from(named: Option<std::ffi::OsString>) -> Vec<PathBuf> {
    match named {
        Some(named) => std::env::split_paths(&named).collect(),
        None => CANDIDATES.iter().map(PathBuf::from).collect(),
    }
}

/// run a snippet in an interpreter and return its trimmed stdout
///
/// this exists so a test can obtain ground truth by a **different route** than
/// the capability probe. asserting that the probe agrees with itself proves
/// nothing
///
/// # panics
///
/// if the interpreter cannot be run, exits non-zero, or writes non-utf8. all
/// three mean the test's ground truth is unavailable, and continuing would
/// assert against a value that was never established
pub fn eval(interpreter: &Capabilities, code: &str) -> String {
    let output = Command::new(&interpreter.executable)
        .args(["-I", "-c", code])
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "could not run `{}`: {error}",
                interpreter.executable.display()
            )
        });

    assert!(
        output.status.success(),
        "`{}` exited with {} running the ground truth snippet:\n{code}\nstderr:\n{}",
        interpreter.executable.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout)
        .expect("cpython writes utf8 to stdout for the snippets used here")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_candidate_list_is_ordered_most_specific_first() {
        // deduplication keeps the first name that resolves to a given binary,
        // so a bare `python3` must never precede a versioned name
        let bare = CANDIDATES
            .iter()
            .position(|name| *name == "python3")
            .expect("`python3` is a candidate");
        let versioned = CANDIDATES
            .iter()
            .position(|name| name.starts_with("python3."))
            .expect("there is at least one versioned candidate");
        assert!(versioned < bare);
    }

    #[test]
    fn an_explicit_list_replaces_the_candidates() {
        // joined the platform's own way, so the test means the same thing on
        // windows, where the delimiter is `;`
        let named = std::env::join_paths(["/one/python", "/two/python"])
            .expect("neither path contains the delimiter");

        assert_eq!(
            candidates_from(Some(named)),
            vec![PathBuf::from("/one/python"), PathBuf::from("/two/python")]
        );
    }

    #[test]
    fn no_explicit_list_falls_back_to_the_candidate_names() {
        assert_eq!(candidates_from(None).len(), CANDIDATES.len());
    }

    #[test]
    fn discovery_finds_something_supported_on_this_machine() {
        assert!(!discovered().require().is_empty());
    }
}
