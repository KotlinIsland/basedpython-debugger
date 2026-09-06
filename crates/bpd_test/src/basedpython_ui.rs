//! the basedpython-ui runtime, real and stood in for, for the tests that read
//! its trace ring
//!
//! two things live here. the **stand-in** is a package a fixture ships beside
//! itself: a `basedpython_ui.runtime` that writes trace records of exactly the
//! layouts the runtime's own documentation fixes, and nothing else. it is what
//! the agent's reader is tested against slot by slot, because a test that
//! needs the whole framework to prove that a `str` in an `int` slot is refused
//! by name would be a test nobody runs. the **real** runtime is a `by build`
//! output on disk, named by an environment variable, and the one test that
//! runs it proves the layouts the stand-in imitates are the ones the framework
//! really writes
//!
//! there is **no silent skip** of the real one, for the reason
//! [`crate::django::installed`] has none: a test that quietly passed because
//! nobody pointed it at a build would be a test reporting success while
//! proving nothing. the variable being unset fails and says what to set. the
//! test is marked `#[ignore]` so a plain `cargo test` reports it as ignored
//! rather than failing on a checkout with no framework beside it, and it is
//! run on purpose:
//!
//! ```sh
//! BPD_TEST_BASEDPYTHON_UI=/path/to/basedpython-ui/out \
//!     cargo test -p bpd_engine --test recompositions -- --ignored
//! ```

use std::path::{Path, PathBuf};

use crate::debuggee::Fixture;

/// the environment variable naming a `by build` output of basedpython-ui
///
/// the directory holding `basedpython_ui/runtime.py` and `tests/test_runtime.py`
/// — `out` under a checkout of basedpython-ui after `by build`
pub const BUILD_ENV: &str = "BPD_TEST_BASEDPYTHON_UI";

/// a `basedpython_ui.runtime` that keeps the trace ring the protocol
/// describes, and nothing else
///
/// every layout is the runtime's own documentation's, to the slot: the
/// format, the module-level `live_runtimes`, a `Runtime` whose `trace` is a
/// `Trace` or `None`, a `Trace` with `records`, `dropped`, `limit` and
/// `frames`, the halving exactly as the real `Trace.append` does it — after
/// the append that reaches `limit`, dropping the oldest `limit // 2`, so a
/// ring never holds `limit` records when it is read — and the audit event
/// after every append. `append(frame, record)` takes its arguments in the
/// order the real one does. a fixture composes nothing — it appends the
/// records it means to assert on, through `Runtime.record`, so what the test
/// reads back is what the test wrote
pub const STAND_IN_RUNTIME: &str = r#""""a stand-in basedpython_ui.runtime: the trace ring, and nothing else"""
import sys

TRACE_FORMAT = 1
live_runtimes = []


class Trace:
    def __init__(self, limit=4096):
        self.records = []
        self.dropped = 0
        self.limit = limit
        self.frames = 0

    def append(self, frame, record):
        self.records.append(record)
        if len(self.records) >= self.limit:
            half = self.limit // 2
            del self.records[:half]
            self.dropped += half
        self.frames = frame
        sys.audit("basedpython_ui.trace", record)


class Runtime:
    def __init__(self, trace=True, limit=4096):
        self.trace = Trace(limit) if trace else None
        self.frames = 0
        live_runtimes.append(self)

    def dispose(self):
        live_runtimes.remove(self)

    def record(self, record):
        if self.trace is not None:
            self.trace.append(self.frames, record)
"#;

/// a program that writes a few records through the stand-in and stops
///
/// what both front ends' acceptance tests drive: a write, the run it caused
/// and the frame that finished, then a breakpoint — and one more run after it,
/// for a client that turned the stream on at the stop
pub const RECOMPOSING: &str = r#"import threading
from basedpython_ui import runtime as rt

HERE = __file__


def Counter():
    return None


runtime = rt.Runtime()
runtime.frames = 3
code = Counter.__code__
written = ("state", 4401, "state", "set", None, 0, 2, HERE, 8, "count", HERE, 14, threading.get_ident(), False, 1)
runtime.record((2, 3, written))
runtime.record((1, 3, 5, 0, "Counter", code.co_filename, code.co_firstlineno, HERE, 16, None, "self", (written,), (), (), 12345))
runtime.record((3, 3, 1, 0, 15000, 300000))
done = 1  # the breakpoint
runtime.record((1, 4, 5, 0, "Later", code.co_filename, code.co_firstlineno, HERE, 16, None, "self", (("invalidated",),), (), (), 99))
"#;

/// put the stand-in package beside a fixture, importable as `basedpython_ui`
///
/// the fixture's directory is `sys.path[0]` for the script form, so a package
/// beside it is what `import basedpython_ui.runtime` finds — and finds first,
/// ahead of any real one on the machine
pub fn stand_in(fixture: &Fixture) {
    fixture.beside("basedpython_ui/__init__.py", "");
    fixture.beside("basedpython_ui/runtime.py", STAND_IN_RUNTIME);
}

/// the real basedpython-ui build the environment names, checked
///
/// # panics
///
/// when [`BUILD_ENV`] is unset, or names a directory without the runtime and
/// the test harness a fixture imports. either means the test that asked would
/// be asserting against no framework at all, and it fails saying what to set.
/// the test that calls this is `#[ignore]`d and run with `--ignored`, so an
/// unset variable is reached only by somebody who asked for the framework
pub fn build() -> PathBuf {
    let Some(named) = std::env::var_os(BUILD_ENV) else {
        panic!(
            "{BUILD_ENV} is not set, so there is no basedpython-ui build to run \
             the real runtime from and this test would prove nothing\n\n\
             build one and name its output:\n    cd ../basedpython-ui && by build\n    \
             {BUILD_ENV}=/path/to/basedpython-ui/out cargo test -p bpd_engine \
             --test recompositions -- --ignored"
        );
    };
    let build = PathBuf::from(named);
    for wanted in ["basedpython_ui/runtime.py", "tests/test_runtime.py"] {
        assert!(
            build.join(wanted).is_file(),
            "{BUILD_ENV} names {}, which has no {wanted}. it has to name the \
             output of `by build` in a checkout of basedpython-ui — the \
             directory holding the `basedpython_ui` package and its `tests`",
            build.display()
        );
    }
    canonical(&build)
}

/// a path as the interpreter reports it, so a location read back compares
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|error| panic!("could not resolve {}: {error}", path.display()))
}
