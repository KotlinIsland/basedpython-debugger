//! a program that starts a child is a program `bpd` is only half pointed at
//!
//! django's `runserver` is the case this exists for: the reloader starts a
//! child and then does nothing but wait on its exit code, so the process
//! holding the agent never renders a template. nothing is reported *wrongly*
//! when that happens — a template breakpoint is unbound, which is the truth —
//! but a person is left looking at a breakpoint that never fires with no reason
//! given
//!
//! everything here drives a real interpreter, because what is under test is
//! which audit events cpython raises for which way of making a child, and no
//! amount of rust proves that

use bpd_core::python::Capabilities;
use bpd_core::{Running, Spawn, Verdict};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::Fixture;
use bpd_test::reporting::Children;

/// the interpreter the built agent matches, or a failure saying how to get one
fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

fn launch(fixture: &Fixture) -> Debuggee {
    match bpd_engine::launch(
        interpreter(),
        &bpd_engine::Program::Script(fixture.path()),
        &[],
    ) {
        Ok(Launched::Stopped(debuggee)) => debuggee,
        Ok(Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

/// run a program to its end and collect every child it was reported to start
///
/// the program has to **succeed**. that is not incidental: this feature only
/// reports, so a child that did not run exactly as it would have is the one
/// failure it could possibly have introduced
fn children_of(source: &str) -> Vec<Spawn> {
    let fixture = Fixture::new("spawner", source);
    let mut debuggee = launch(&fixture);
    let mut seen = Children::default();

    match debuggee.run(&mut seen).expect("the debuggee was resumed") {
        Running::Exited { status, .. } => assert!(
            status.success(),
            "the program exited with {status}, so whatever it did to its \
             children, bpd changed it"
        ),
        other => panic!("nothing was set, and the program answered with {other:?}"),
    }
    seen.started
}

/// a program that starts one child running its own interpreter
///
/// `subprocess.run` is what django's reloader uses, and the marker file is how
/// the child proves it really ran — a report about a child that was blocked
/// would be a report about a program bpd had changed
const SUBPROCESS: &str = r#"import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).parent

subprocess.run(
    [sys.executable, "-c", f"open({str(HERE / 'child')!r}, 'w').write('ran')"],
    check=True,
)
assert (HERE / "child").read_text() == "ran"
"#;

#[test]
fn a_python_child_is_reported_and_still_runs_exactly_as_it_would_have() {
    let started = children_of(SUBPROCESS);

    assert_eq!(
        started.len(),
        1,
        "one child was started and bpd said {started:?}"
    );
    let child = &started[0];
    assert_eq!(
        child.verdict,
        Verdict::ThisInterpreter,
        "the child was started with `sys.executable`, which is the one thing \
         about a child that can be known for certain"
    );
    assert!(child.verdict.certain());

    // the report has to name the child, or nobody can act on it
    let command = child.command().expect("a started child has a command");
    assert!(
        command.contains("-c"),
        "the report has to say which child it is about, and said {command}"
    );
    assert!(
        child.to_string().contains("not debugging it"),
        "the report has to say what bpd is *not* doing, and said {child}"
    );
}

/// the same program, with a child that is plainly not python
const NOT_PYTHON: &str = r#"import subprocess

subprocess.run(["/bin/echo", "a child that is not python"], check=True)
"#;

#[test]
fn a_child_that_is_not_python_is_not_reported_at_all() {
    // the rule that keeps this feature readable. a debugger that announced a
    // child for every `git`, `ls` and `sh` a build script runs is one whose
    // output nobody reads, and a report nobody reads is the same as no report
    let started = children_of(NOT_PYTHON);
    assert!(
        started.is_empty(),
        "`/bin/echo` is not a python child, and bpd said {started:?}"
    );
}

/// a program that forks without ever exec-ing
const FORKS: &str = r"import os

pid = os.fork()
if pid == 0:
    os._exit(0)
os.waitpid(pid, 0)
";

#[test]
fn a_fork_is_reported_as_a_process_sharing_this_session() {
    let started = children_of(FORKS);

    assert_eq!(started.len(), 1, "one fork, and bpd said {started:?}");
    let child = &started[0];
    assert_eq!(child.verdict, Verdict::ThisProcess);
    assert_eq!(child.event, "os.fork");
    assert_eq!(
        child.command(),
        None,
        "a fork runs no command of its own, and inventing one would be bpd \
         describing a process it had not looked at"
    );

    // a forked child holds the agent's monitoring state *and* the fd of this
    // session's control connection. the report is the only thing standing
    // between that and two processes writing frames into one socket
    let said = child.to_string();
    assert!(said.contains("control connection"), "it said {said}");
}

/// a program whose child comes from `multiprocessing`, not from `subprocess`
///
/// with the `spawn` start method, which is the default on macos and windows.
/// this is the case that decided which audit events are watched: it raises
/// **no** `subprocess.Popen` event and **no** `os.*` event, only
/// `_posixsubprocess.fork_exec`, so a watch list built from the obvious names
/// would miss every one of them
const MULTIPROCESSING: &str = r#"import multiprocessing
import pathlib

HERE = pathlib.Path(__file__).parent


def work():
    (HERE / "worker").write_text("ran")


if __name__ == "__main__":
    process = multiprocessing.get_context("spawn").Process(target=work)
    process.start()
    process.join()
    assert (HERE / "worker").read_text() == "ran"
"#;

#[test]
fn a_multiprocessing_child_is_reported_even_though_it_is_not_a_subprocess() {
    let started = children_of(MULTIPROCESSING);

    assert!(
        !started.is_empty(),
        "`multiprocessing` with the spawn start method starts at least the \
         worker, and bpd reported nothing at all"
    );
    for child in &started {
        assert_eq!(
            child.verdict,
            Verdict::ThisInterpreter,
            "every process `multiprocessing` starts is this interpreter, and \
             bpd said {child}"
        );
    }
}

/// a child reached through a launcher, which is the case bpd cannot be sure of
const LAUNCHER: &str = r#"import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).parent

subprocess.run(
    ["/usr/bin/env", "python3", "-c", f"open({str(HERE / 'child')!r}, 'w').write('ran')"],
    check=True,
)
"#;

#[test]
fn a_child_reached_through_a_launcher_is_reported_as_uncertain_rather_than_guessed_at() {
    let started = children_of(LAUNCHER);

    assert_eq!(started.len(), 1, "one child, and bpd said {started:?}");
    let child = &started[0];
    assert_eq!(
        child.verdict,
        Verdict::Perhaps {
            named: "python3".to_string()
        },
        "the child's program is `env`. that a word of its command is an \
         interpreter is evidence, and calling it a python child would be bpd \
         reporting a thing it did not see"
    );
    assert!(!child.verdict.certain());
    assert!(
        child.to_string().contains("cannot tell"),
        "an uncertain verdict has to say so in the words a person reads, and \
         it said {child}"
    );
}
