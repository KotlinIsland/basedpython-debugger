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
use bpd_core::{Running, Verdict};
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

/// whether this interpreter raises `_posixsubprocess.fork_exec` as an audit
/// event
///
/// it became one in **3.14**. below that the whole event is silent, which
/// changes both which events bpd watches and what it is able to see at all —
/// so the tests that turn on it ask this rather than assuming one answer
///
/// this is the only version branch in the file, and it is a fact about cpython
/// rather than a capability bpd chooses to have
fn raises_fork_exec() -> bool {
    let version = interpreter().version;
    (version.major, version.minor) >= (3, 14)
}

/// run a program to its end and collect everything bpd said about its children
///
/// the program has to **succeed**. that is not incidental: this feature only
/// reports, so a child that did not run exactly as it would have is the one
/// failure it could possibly have introduced
fn children_of(source: &str) -> Children {
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
    seen
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
    let started = children_of(SUBPROCESS).started;

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
    let started = children_of(NOT_PYTHON).started;
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
    let started = children_of(FORKS).started;

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

    // a forked child inherits the agent's monitoring state *and* both
    // descriptors of this session's control connection, and gives all of it up
    // before it runs a line — see `forks.rs`. the report is what says so, and a
    // fork reported without it reads as a child bpd has simply lost sight of
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
    if !raises_fork_exec() {
        // below 3.14 there is no event to see one by, which the test below is
        // about. this one would be asserting that bpd sees something cpython
        // never told it
        return;
    }
    let seen = children_of(MULTIPROCESSING);

    assert!(
        !seen.started.is_empty(),
        "`multiprocessing` with the spawn start method starts at least the \
         worker, and bpd reported nothing at all"
    );
    assert!(
        seen.unseen.is_empty(),
        "this interpreter can see one, and bpd said it could not: {:?}",
        seen.unseen
    );
    for child in &seen.started {
        assert_eq!(
            child.verdict,
            Verdict::ThisInterpreter,
            "every process `multiprocessing` starts is this interpreter, and \
             bpd said {child}"
        );
    }
}

#[test]
fn an_interpreter_that_cannot_see_a_multiprocessing_child_says_so_rather_than_nothing() {
    // the rule this whole feature would otherwise break. below 3.14, a
    // `multiprocessing` spawn child raises **no audit event of any name** —
    // measured by recording every event raised while one starts — so bpd
    // reports nothing about it and a user would read that silence as "no child"
    //
    // silence is the one thing this feature must never do, so the silence is
    // announced. this test is the thing that fails if it stops being
    if raises_fork_exec() {
        return;
    }
    let seen = children_of(MULTIPROCESSING);

    assert!(
        seen.started.is_empty(),
        "this interpreter raises no event for a `multiprocessing` spawn child, \
         and bpd claimed to have seen {:?}",
        seen.started
    );
    assert_eq!(
        seen.unseen.len(),
        1,
        "one blind spot, said once however many modules import \
         `multiprocessing`, and bpd said {:?}",
        seen.unseen
    );

    let said = seen.unseen[0].to_string();
    assert!(
        said.contains("silence here does not mean there was none"),
        "the announcement is worth having only if it says what the silence has \
         stopped meaning, and it said {said}"
    );
    assert!(
        said.contains("3.14"),
        "it has to name the release where this is visible, and it said {said}"
    );
}

/// a child started down `subprocess`'s `posix_spawn` path
///
/// `close_fds=False` is what lets `subprocess` take it, and taking it raises
/// `os.posix_spawn` **beside** `subprocess.Popen` for the one child. on 3.13
/// both of those are watched — `subprocess.Popen` because it is the only event
/// an ordinary child raises there — so this is the pair the deduplication
/// exists for
const CLOSE_FDS_OFF: &str = r#"import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).parent

subprocess.run(
    [sys.executable, "-c", f"open({str(HERE / 'child')!r}, 'w').write('ran')"],
    check=True,
    close_fds=False,
)
assert (HERE / "child").read_text() == "ran"
"#;

#[test]
fn a_child_started_the_posix_spawn_way_is_reported_once() {
    // this is the proof of the deduplication rather than an assumption about
    // it. on 3.13 the child raises two watched events and on 3.14 it raises one
    // watched event of a different name — and a user is told about one child
    // either way, because there was one child
    let started = children_of(CLOSE_FDS_OFF).started;

    assert_eq!(
        started.len(),
        1,
        "one child was started, and bpd reported {started:?}. two reports for \
         one child is the debugger describing a process that does not exist"
    );
    assert_eq!(started[0].verdict, Verdict::ThisInterpreter);
}

/// a child started by calling `os.posix_spawn` directly, with no `subprocess`
///
/// the deduplication above keys on the previous watched event on the thread,
/// and on 3.13 a `subprocess` child raises `subprocess.Popen` first — so a rule
/// that suppressed **every** `os.posix_spawn` would still report that child,
/// through the other event, and look correct. this is the case that tells the
/// two apart: there is no `subprocess.Popen` in front of it, and the report has
/// to come from `os.posix_spawn` itself
const DIRECT_POSIX_SPAWN: &str = r#"import os
import pathlib
import sys

HERE = pathlib.Path(__file__).parent

pid = os.posix_spawn(
    sys.executable,
    [sys.executable, "-c", f"open({str(HERE / 'child')!r}, 'w').write('ran')"],
    os.environ,
)
os.waitpid(pid, 0)
assert (HERE / "child").read_text() == "ran"
"#;

#[test]
fn a_child_spawned_without_subprocess_is_reported_by_the_event_that_started_it() {
    let started = children_of(DIRECT_POSIX_SPAWN).started;

    assert_eq!(
        started.len(),
        1,
        "one child was started with no `subprocess` anywhere near it, and bpd \
         reported {started:?}. losing it would be the deduplication reaching \
         past the pair it is for"
    );
    assert_eq!(started[0].verdict, Verdict::ThisInterpreter);
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
    let started = children_of(LAUNCHER).started;

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
