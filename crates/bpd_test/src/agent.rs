//! the built agent, staged for a real interpreter to import
//!
//! the staging itself belongs to the engine — it is how a debuggee gets its
//! agent — so this is a thin wrapper that asks the engine for the agent an
//! interpreter would really be launched with, and remembers which interpreter
//! the build actually matches
//!
//! it asks **per interpreter** rather than once, because that is what a launch
//! does: a `bpd` carries an agent per interpreter tag, and "the agent this test
//! run built" is only one thing in a checkout, where the single artifact cargo
//! made is what every interpreter resolves to

use std::sync::OnceLock;

use bpd_core::python::Capabilities;

use crate::debuggee::Run;

/// the agent, in a directory an interpreter can import from
pub type Staged = bpd_engine::agent::Staged;

/// the agent this test run would launch that interpreter with
///
/// # panics
///
/// if there is no agent for it, which in a checkout means none was built
pub fn staged_for(interpreter: &Capabilities) -> Staged {
    bpd_engine::agent::stage_for(interpreter).unwrap_or_else(|error| {
        panic!(
            "could not stage the agent for `{}`: {error}",
            interpreter.executable.display()
        )
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
        .env("PYTHONPATH", staged_for(interpreter).python_path())
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
/// if no discovered interpreter matches, with what each of them said. one agent
/// build serves one interpreter tag, and a test run with no interpreter for the
/// one this `bpd` carries would prove nothing
pub fn matching_interpreter() -> &'static Capabilities {
    static MATCHING: OnceLock<&'static Capabilities> = OnceLock::new();

    MATCHING.get_or_init(|| {
        let mut refused: Vec<String> = Vec::new();
        for interpreter in crate::discovered().require() {
            // an interpreter this `bpd` carries no agent for is not a match and
            // is not a failure either — it is one of the interpreters on the
            // machine this build was not made for. what it said is kept, since
            // a run where *nothing* matched is a run whose reason is in here
            match bpd_engine::agent::stage_for(interpreter) {
                Ok(_) => {
                    if run(
                        interpreter,
                        "import bpd_agent; bpd_agent.verify_interpreter()",
                    )
                    .success
                    {
                        return interpreter;
                    }
                    refused.push(format!(
                        "    python {}: the agent it resolved to was built for \
                         another interpreter",
                        interpreter.tag()
                    ));
                }
                Err(error) => refused.push(format!("    python {}: {error}", interpreter.tag())),
            }
        }

        panic!(
            "no discovered interpreter matches an agent this bpd carries. one \
             build serves one interpreter tag — build it for one you \
             have:\n    PYO3_PYTHON=python3.14 cargo build -p bpd_agent\n\nwhat \
             each of them said:\n{}",
            refused.join("\n")
        )
    })
}
