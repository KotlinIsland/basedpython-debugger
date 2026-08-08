//! the built agent, staged for a real interpreter to import
//!
//! the staging itself belongs to the engine — it is how a debuggee gets its
//! agent — so this is a thin wrapper that does it once per test process and
//! remembers which interpreter the build actually matches

use std::sync::OnceLock;

use bpd_core::python::Capabilities;

use crate::debuggee::Run;

/// the agent, in a directory an interpreter can import from
pub type Staged = bpd_engine::agent::Staged;

/// the agent this test run built, staged once per process
pub fn staged() -> &'static Staged {
    static STAGED: OnceLock<Staged> = OnceLock::new();
    STAGED.get_or_init(|| {
        bpd_engine::agent::stage()
            .unwrap_or_else(|error| panic!("could not stage the agent: {error}"))
    })
}

/// run a snippet with the agent importable
///
/// deliberately not `-I`, which implies `-E` and would discard the `PYTHONPATH`
/// that makes the agent importable in the first place
///
/// # panics
///
/// if the interpreter cannot be spawned, or writes non-utf8
pub fn run(interpreter: &Capabilities, code: &str) -> Run {
    let output = std::process::Command::new(&interpreter.executable)
        .env("PYTHONPATH", staged().python_path())
        .args(["-c", code])
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "could not run `{}`: {error}",
                interpreter.executable.display()
            )
        });

    Run {
        exit_code: output.status.code(),
        success: output.status.success(),
        stdout: String::from_utf8(output.stdout).expect("the snippet writes utf8"),
        stderr: String::from_utf8(output.stderr).expect("cpython writes utf8 to stderr"),
    }
}

/// the interpreter the built agent was actually compiled for
///
/// selected by `verify_interpreter`, **not** by whether the import succeeds.
/// those are not the same question: on unix an extension module is a shared
/// object whose cpython symbols resolve at load time, so a 3.13 interpreter
/// imports a 3.14 build without complaint and then runs it against a layout it
/// was not compiled for. that is the whole reason the explicit check exists
///
/// # panics
///
/// if no discovered interpreter matches. the agent is built for one
/// `major.minor` at a time, and a test run with no interpreter for it would
/// prove nothing
pub fn matching_interpreter() -> &'static Capabilities {
    static MATCHING: OnceLock<&'static Capabilities> = OnceLock::new();

    MATCHING.get_or_init(|| {
        let supported = crate::discovered().require();
        let matching = supported.iter().copied().find(|interpreter| {
            run(
                interpreter,
                "import bpd_agent; bpd_agent.verify_interpreter()",
            )
            .success
        });

        match matching {
            Some(interpreter) => interpreter,
            None => panic!(
                "no discovered interpreter matches the built agent. it is \
                 compiled for one `major.minor` at a time — build it for one \
                 you have:\n    PYO3_PYTHON=python3.14 cargo build -p bpd_agent"
            ),
        }
    })
}
