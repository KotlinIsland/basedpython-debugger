//! `bpd launch` says when the program it is debugging started another one, and
//! debugs it when it was asked to
//!
//! the engine's own tests
//! (`crates/bpd_engine/tests/spawns.rs`, `execs.rs` and `forks.rs`) establish
//! which children are reported, which are debugged and why. what this is about
//! is the last step: that the report reaches the terminal, on the stream a
//! debugger's own words belong on, while the program is still running — and
//! that `--debug-children` **lets a child that arrives held go**, since this
//! command has no ui to hold one in and a child nothing resumes is a hung
//! program

use std::process::Command;

use bpd_core::python::Capabilities;
use bpd_test::debuggee::{Fixture, Run};

/// the binary this test run built
const BPD: &str = env!("CARGO_BIN_EXE_bpd");

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// the watchdog every fixture here whose child could be left held sets
///
/// a child that arrived held and was never resumed waits for a resume no
/// process can send, and holds this test binary's output pipe open while it
/// does. with the pump working it is never reached; without it, a failure is a
/// failure rather than a test run that hangs
const WATCHDOG: &str = "signal.alarm(120)";

/// run a fixture under `bpd launch`, from its own directory
fn launched(fixture: &Fixture) -> Run {
    run_with(fixture, &[])
}

/// the same, with arguments of bpd's own before the program
fn run_with(fixture: &Fixture, flags: &[&str]) -> Run {
    let output = Command::new(BPD)
        .current_dir(fixture.directory())
        .arg("launch")
        .arg("--python")
        .arg(&interpreter().executable)
        .args(flags)
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

/// the same supervisor, with a watchdog in the child
///
/// the child is what `--debug-children` holds, and the parent does nothing but
/// wait on it — so a child that is never let go is a test run that hangs in two
/// processes at once. the alarm turns that into a failure
const SUPERVISOR_WITH_A_WATCHDOG: &str = r#"import subprocess
import sys

finished = subprocess.run(
    [sys.executable, "-c", "import signal; signal.alarm(120); print('the child did the work')"]
)
raise SystemExit(finished.returncode)
"#;

/// the whole of what `--debug-children` has to get right here: a child that
/// arrives **held** is let go
///
/// `bpd launch` sets no breakpoints and has no ui, so there is nothing for a
/// stopped child to wait for — and the parent is blocked in `waitpid` on it, so
/// a child left held is two processes that never end. it is reported and
/// resumed, and the program's own output and exit code are what they would have
/// been
#[test]
fn a_child_that_is_debugged_is_reported_held_and_let_go() {
    assert!(SUPERVISOR_WITH_A_WATCHDOG.contains(WATCHDOG));
    let fixture = Fixture::new("supervisor", SUPERVISOR_WITH_A_WATCHDOG);
    let run = run_with(&fixture, &["--debug-children"]);

    assert!(
        run.stderr.contains("joined"),
        "a session that joined is a **held** process, and a person who is never \
         told has one they cannot see. stderr was:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("is held"),
        "and where it is held, since letting it go without a word would leave a \
         step nobody can see in the program's history. stderr was:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("interpreter startup"),
        "an `exec`'d child is held before a line of its program has been \
         compiled, and that is what the report has to say rather than a line \
         number it does not have. stderr was:\n{}",
        run.stderr
    );

    // the program did what it does. the child ran to its end after it was let
    // go, which is the whole thing this could get wrong
    assert_eq!(run.stdout, "the child did the work\n");
    assert_eq!(run.exit_code, Some(0));
}

/// a program that forks, with the child on a watchdog for the reason above
///
/// the other mechanism, and the one setting covers both: a debugged fork opens
/// a session of its own and arrives held at the line that forked
const A_FORK_TO_DEBUG: &str = r#"import os
import signal
import sys

pid = os.fork()
if pid == 0:
    signal.alarm(120)
    print("the child ran on")
    sys.stdout.flush()
    os._exit(0)

_, status = os.waitpid(pid, 0)
raise SystemExit(os.waitstatus_to_exitcode(status))
"#;

#[test]
fn a_forked_child_that_is_debugged_is_reported_at_the_line_that_forked() {
    assert!(A_FORK_TO_DEBUG.contains(WATCHDOG));
    let fixture = Fixture::new("forker", A_FORK_TO_DEBUG);
    let run = run_with(&fixture, &["--debug-children"]);

    assert!(
        run.stderr.contains("joined"),
        "the forked child opened a session of its own and nothing said so. \
         stderr was:\n{}",
        run.stderr
    );
    let forked = bpd_test::debuggee::line_of(A_FORK_TO_DEBUG, "pid = os.fork()");
    assert!(
        run.stderr
            .contains(&format!("`{}` line {forked}", fixture.path().display())),
        "a fork is held at the line it forked on, in the file the user is \
         looking at, and the report has to name both. stderr was:\n{}",
        run.stderr
    );
    assert!(
        run.stdout.contains("the child ran on"),
        "the child was let go and did its work. stdout was:\n{}",
        run.stdout
    );
    assert_eq!(run.exit_code, Some(0));
}

/// off is the default, and it stays off
///
/// a flag that was on unless asked for would be a debugger that stops processes
/// nobody asked it to stop
#[test]
fn a_child_is_not_debugged_unless_it_was_asked_for() {
    let fixture = Fixture::new("supervisor", SUPERVISOR_WITH_A_WATCHDOG);
    let run = launched(&fixture);

    assert!(
        !run.stderr.contains("joined"),
        "no session can join a debuggee that was never told to debug its \
         children. stderr was:\n{}",
        run.stderr
    );
    assert_eq!(run.stdout, "the child did the work\n");
    assert_eq!(run.exit_code, Some(0));
}
