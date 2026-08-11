//! `bpd launch` says when the program it is debugging started another one
//!
//! the engine's own tests
//! (`crates/bpd_engine/tests/spawns.rs`) establish which children are reported
//! and why. what this is about is the last step: that the report reaches the
//! terminal, on the stream a debugger's own words belong on, while the program
//! is still running

use std::process::Command;

use bpd_core::python::Capabilities;
use bpd_test::debuggee::{Fixture, Run};

/// the binary this test run built
const BPD: &str = env!("CARGO_BIN_EXE_bpd");

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// run a fixture under `bpd launch`, from its own directory
fn launched(fixture: &Fixture) -> Run {
    let output = Command::new(BPD)
        .current_dir(fixture.directory())
        .arg("launch")
        .arg("--python")
        .arg(&interpreter().executable)
        .arg(fixture.path())
        .output()
        .expect("the binary was built by the same cargo invocation");

    Run {
        exit_code: output.status.code(),
        success: output.status.success(),
        stdout: String::from_utf8(output.stdout).expect("the program writes utf8"),
        stderr: String::from_utf8(output.stderr).expect("cpython writes utf8 to stderr"),
    }
}

/// a supervisor, in the shape django's reloader has it
///
/// the parent starts a child and then does nothing but wait on its exit code.
/// every breakpoint a user sets is in the code the **child** runs, and `bpd` is
/// attached to the parent
const SUPERVISOR: &str = r#"import subprocess
import sys

finished = subprocess.run([sys.executable, "-c", "print('the child did the work')"])
raise SystemExit(finished.returncode)
"#;

#[test]
fn a_supervisor_that_starts_a_child_says_so_on_the_terminal() {
    let fixture = Fixture::new("supervisor", SUPERVISOR);
    let run = launched(&fixture);

    assert!(
        run.stderr.contains("bpd:"),
        "the notice is bpd's own words and goes on stderr with bpd's prefix, \
         so it is not mistaken for the program's. stderr was:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("not debugging it"),
        "the notice has to say what bpd is not doing, or it is a fact with no \
         consequence attached. stderr was:\n{}",
        run.stderr
    );

    // the program is untouched by any of this: its child ran, its output is its
    // own, and its exit code is its own. reporting a child is not a reason to
    // change what the program does
    assert_eq!(run.stdout, "the child did the work\n");
    assert_eq!(run.exit_code, Some(0));
}

/// the same supervisor, whose child is not python
const ORDINARY: &str = r#"import subprocess

subprocess.run(["/bin/echo", "hello"], check=True)
"#;

#[test]
fn a_program_that_only_runs_ordinary_commands_gets_no_notice() {
    // the notice is only worth having if it is rare. a build script that shells
    // out fifty times would otherwise bury whatever the debugger had to say
    let fixture = Fixture::new("ordinary", ORDINARY);
    let run = launched(&fixture);

    assert!(
        !run.stderr.contains("bpd:"),
        "`/bin/echo` is not a python child, and bpd said:\n{}",
        run.stderr
    );
    assert_eq!(run.stdout, "hello\n");
    assert!(run.success);
}
