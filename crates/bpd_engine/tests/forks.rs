//! a forked child is a copy of a debuggee that nothing can debug
//!
//! `fork` copies the process and keeps **only the calling thread**. everything
//! the agent set up survives it — the `sys.monitoring` tool id, the breakpoint
//! table, the local events on every armed code object, the file descriptors of
//! the control connection — and the one thing that does not is the thread that
//! reads that connection
//!
//! so without something standing in the way, a forked child is an armed
//! debuggee that can write to the session socket and can never be answered. two
//! processes writing length-prefixed frames into one stream desynchronise it,
//! and a child that stopped would wait for a resume no process can send. that
//! is not a state to report better, it is a state that must not exist
//!
//! what happens instead is that the child stops being a debuggee, and these
//! tests are what says so. everything here drives a real interpreter, because
//! what is under test is what cpython does across `fork`, and no amount of rust
//! proves that

// **the whole file is unix.** every fixture here calls `os.fork()`, which does
// not exist on windows — so without this the windows leg of the matrix compiles
// this, runs it, and fails on an `AttributeError` that has nothing to do with
// the debugger. `execs.rs` and `breakpoints.rs` already gate their fork cases
// this way; this file was the one that did not, and the matrix has never run,
// so nobody had found out
#![cfg(unix)]

//! ## `#![cfg(unix)]`
//!
//! there is no `fork` on windows, so there is nothing here for a test to be
//! about: every fixture in this file calls `os.fork`, and bpd refuses child
//! debugging there by name for the same reason
#![cfg(unix)]

use std::time::Duration;

use bpd_core::python::Capabilities;
use bpd_core::{
    Binding, Content, Detail, Evaluated, Request, Resolved, Response, Running, SessionId,
    SourceBreakpoint, StopReason, Verdict,
};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, Form, line_of};
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

/// the one session a freshly launched debuggee holds
fn the_only_session(debuggee: &Debuggee) -> SessionId {
    match debuggee.sessions().as_slice() {
        [only] => *only,
        open => panic!("this debuggee holds {open:?} and the test launched one program"),
    }
}

/// how long a wait on a program that should already be over is given
///
/// it bounds a failure rather than measuring anything. every test here expects
/// the program to have ended, and a deadline is what turns "it never will" into
/// a named failure instead of a test run that hangs
const LONG_ENOUGH: Duration = Duration::from_secs(20);

/// the watchdog every fixture whose child could be left armed sets
///
/// a child that still holds the agent stops on its breakpoint and waits for a
/// resume that no process can send, and it holds the test binary's own output
/// pipe open while it does. so the fixtures arm `SIGALRM` in the child: with
/// the fix it is never reached, and without it a failure is a failure rather
/// than a hang
const WATCHDOG: &str = "signal.alarm(120)";

/// require that a breakpoint really armed a code object
///
/// every test here inherits the parent's instrumentation into a child, so one
/// that ran against an unbound breakpoint would prove nothing at all
fn armed(resolved: &[Resolved]) {
    match &resolved[0].binding {
        Binding::Bound { .. } => {}
        other => panic!(
            "the breakpoint has to be armed in the parent before the fork, or \
             this test proves nothing about what a child inherits: {other:?}"
        ),
    }
}

/// resume everything and wait for what the program does next, within the
/// deadline
fn run(debuggee: &mut Debuggee, seen: &mut Children) -> Running {
    match debuggee.dispatch(
        bpd_core::Addressed::unnamed(Request::Run {
            deadline: Some(LONG_ENOUGH),
        }),
        seen,
    ) {
        Ok(Response::Ran(ran)) => ran,
        Ok(other) => panic!("a run was answered with {other:?}"),
        Err(error) => panic!("the debuggee was not resumed: {error}"),
    }
}

/// a program that forks and runs a line holding a breakpoint in the child
///
/// the breakpoint goes on `child_only`, which the parent never calls. so every
/// stop this program can produce belongs to the forked child, and a session
/// that is told about one has been told about a process it cannot answer
const BREAKPOINT_IN_THE_CHILD: &str = r#"import os
import pathlib
import signal

HERE = pathlib.Path(__file__).parent


def child_only():
    (HERE / "child").write_text("only the child gets here")


pid = os.fork()
if pid == 0:
    signal.alarm(120)
    child_only()
    os._exit(7)

_, status = os.waitpid(pid, 0)
assert os.waitstatus_to_exitcode(status) == 7, status
assert (HERE / "child").read_text() == "only the child gets here"
"#;

#[test]
fn a_forked_child_never_reports_a_stop_on_the_session_its_parent_owns() {
    assert!(BREAKPOINT_IN_THE_CHILD.contains(WATCHDOG));
    let fixture = Fixture::new("forker", BREAKPOINT_IN_THE_CHILD);
    let mut debuggee = launch(&fixture);

    let line = line_of(BREAKPOINT_IN_THE_CHILD, r#"write_text("only the child"#);
    armed(
        &debuggee
            .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), line)])
            .expect("the breakpoint set was answered"),
    );

    let mut seen = Children::default();
    match run(&mut debuggee, &mut seen) {
        Running::Exited { status, .. } => assert!(
            status.success(),
            "the program exited with {status}. the child reached an armed \
             breakpoint and had to run straight through it"
        ),
        Running::Stopped { stop, .. } => panic!(
            "the engine was told about {:?}. no thread of the process bpd \
             launched can have reached that line — the parent never calls it — \
             so the report came from the forked child, down a socket the child \
             does not own and can never be answered on",
            stop.reason
        ),
        other => panic!("the program did not end: {other:?}"),
    }

    // the fork is still reported. that report is what makes this true rather
    // than merely quiet: a child sharing this session's connection is exactly
    // what one no longer does
    assert_eq!(
        seen.started.len(),
        1,
        "one fork, and bpd said {:?}",
        seen.started
    );
    assert_eq!(seen.started[0].verdict, Verdict::ThisProcess);

    // and nothing joined the debuggee, because nothing asked for one to. this
    // is the default and it is what every test above this line is about: a
    // child that opened a session of its own without being asked would be a
    // process **held** by a debugger nobody pointed at it
    assert!(
        seen.joined.is_empty(),
        "debugging a forked child is off unless it is asked for, and {:?} joined",
        seen.joined
    );
}

/// what a forked child can see of the debugger that was in it a moment ago
///
/// written to a file rather than printed, so the same program can be run bare
/// and under `bpd` and the two records compared byte for byte. the parent's
/// answers are deliberately **not** in it: the parent is being debugged, and it
/// is supposed to be able to tell
const WHAT_THE_CHILD_SEES: &str = r#"import json
import os
import pathlib
import signal
import sys

HERE = pathlib.Path(__file__).parent
DEBUGGER_ID = 0


def child_only():
    return "only the child gets here"


pid = os.fork()
if pid == 0:
    signal.alarm(120)
    monitoring = sys.monitoring
    (HERE / "seen").write_text(
        json.dumps(
            {
                "ran": child_only(),
                "tool": monitoring.get_tool(DEBUGGER_ID),
                "global_events": monitoring.get_events(DEBUGGER_ID),
                "local_events": monitoring.get_local_events(
                    DEBUGGER_ID, child_only.__code__
                ),
                "argv": sys.argv,
                "path0": sys.path[0],
                "name": __name__,
                "file": __file__,
            },
            indent=1,
        )
    )
    os._exit(7)

_, status = os.waitpid(pid, 0)
assert os.waitstatus_to_exitcode(status) == 7, status
"#;

#[test]
fn a_forked_child_sees_exactly_what_it_would_have_seen_without_the_debugger() {
    assert!(WHAT_THE_CHILD_SEES.contains(WATCHDOG));

    // the baseline, from a bare interpreter. the child of a bare run is not a
    // debuggee, and the child of a debugged run has to be indistinguishable
    // from it — same monitoring state, same launch, same exit code
    let bare = Fixture::new("forker", WHAT_THE_CHILD_SEES);
    let ran_bare = bare.run(interpreter(), Form::Script, &[]);
    assert!(
        ran_bare.success,
        "the baseline program failed on its own: {}",
        ran_bare.stderr
    );

    let fixture = Fixture::new("forker", WHAT_THE_CHILD_SEES);
    let mut debuggee = launch(&fixture);

    // a breakpoint on the line the child runs, so what the child inherits is an
    // armed code object rather than an idle tool
    let line = line_of(WHAT_THE_CHILD_SEES, r#"return "only the child"#);
    armed(
        &debuggee
            .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), line)])
            .expect("the breakpoint set was answered"),
    );

    let mut seen = Children::default();
    match run(&mut debuggee, &mut seen) {
        Running::Exited { status, .. } => assert!(status.success(), "{status}"),
        other => panic!("the program did not end: {other:?}"),
    }

    let under_bpd = std::fs::read_to_string(fixture.directory().join("seen"))
        .expect("the child of the debugged run wrote what it saw");
    let bare_seen = std::fs::read_to_string(bare.directory().join("seen"))
        .expect("the child of the bare run wrote what it saw");

    // the fixture's own directory is in two of these, and it is a different
    // temporary directory for each run
    let under_bpd = under_bpd.replace(&fixture.directory().display().to_string(), "<here>");
    let bare_seen = bare_seen.replace(&bare.directory().display().to_string(), "<here>");
    assert_eq!(
        under_bpd, bare_seen,
        "a forked child of a debuggee has to be indistinguishable from a forked \
         child of a bare run. it is not being debugged, and anything it can see \
         of the debugger is the debugger having changed a program it is not \
         debugging"
    );
}

/// a program whose parent goes on to reach a breakpoint of its own
const THE_PARENT_STOPS_AFTER_FORKING: &str = r#"import os
import pathlib

HERE = pathlib.Path(__file__).parent


def parent_only():
    return "only the parent gets here"


pid = os.fork()
if pid == 0:
    (HERE / "child").write_text("ran")
    os._exit(0)

_, status = os.waitpid(pid, 0)
assert os.waitstatus_to_exitcode(status) == 0, status
parent_only()
"#;

#[test]
fn a_fork_leaves_the_parents_session_exactly_as_it_was() {
    let fixture = Fixture::new("forker", THE_PARENT_STOPS_AFTER_FORKING);
    let mut debuggee = launch(&fixture);

    let line = line_of(THE_PARENT_STOPS_AFTER_FORKING, r#"return "only the parent"#);
    armed(
        &debuggee
            .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), line)])
            .expect("the breakpoint set was answered"),
    );

    let mut seen = Children::default();
    let stop = match run(&mut debuggee, &mut seen) {
        Running::Stopped { stop, .. } => stop,
        other => panic!(
            "the parent has a breakpoint on a line it reaches after the fork, \
             and the session answered {other:?}"
        ),
    };
    match &stop.reason {
        StopReason::Breakpoint {
            breakpoints,
            line: at,
            ..
        } => {
            assert_eq!(breakpoints, &[1]);
            assert_eq!(*at, line);
        }
        other => panic!("the parent stopped for {other:?}"),
    }

    // the parent is still the tool. whatever the child gave up it gave up in
    // the child: the two stopped sharing anything the moment they stopped
    // sharing an address space
    let stack = debuggee
        .stack(stop.stop, Some(1))
        .expect("the stack was walked");
    let holder = debuggee
        .evaluate(
            stack.frames[0].id,
            "__import__('sys').monitoring.get_tool(0)",
            Detail::default(),
        )
        .expect("the expression was evaluated");
    match holder {
        Evaluated::Value { value } => assert_eq!(
            value.content,
            Content::Str {
                text: "bpd".to_string(),
                characters: 3,
                omitted: None,
            },
            "the parent still holds the debugger tool id after its child gave \
             one up"
        ),
        Evaluated::Raised { error } => panic!("reading the tool id raised {error}"),
    }

    match run(&mut debuggee, &mut seen) {
        Running::Exited { status, .. } => assert!(status.success(), "{status}"),
        other => panic!("the program did not end: {other:?}"),
    }
    assert_eq!(seen.started.len(), 1, "{:?}", seen.started);
}

/// a program that forks on one thread while another is held at a breakpoint
///
/// the reader of the control connection is taken off the process for every
/// fork and put back afterwards, so between the two there is a window in which
/// nothing is reading it. this is that window, opened over and over on purpose:
/// the worker is held for the whole of it, and every request the session makes
/// of the held thread has to cross one
///
/// the loop is released by the test rather than counted out, so the forking and
/// the asking really do overlap. the count is a backstop — a program that
/// forked for ever would hold the test binary's own output pipe open
const A_FORK_WHILE_A_THREAD_IS_HELD: &str = r#"import os
import pathlib
import signal
import threading

HERE = pathlib.Path(__file__).parent


def held_here():
    return "the worker is held"


started = threading.Event()


def worker():
    started.wait()
    held_here()


thread = threading.Thread(target=worker)
thread.start()
started.set()

for _ in range(2000):
    if (HERE / "release").exists():
        break
    pid = os.fork()
    if pid == 0:
        signal.alarm(120)
        os._exit(0)
    os.waitpid(pid, 0)

thread.join()
"#;

#[test]
fn a_thread_held_at_a_breakpoint_is_still_answered_while_another_thread_forks() {
    assert!(A_FORK_WHILE_A_THREAD_IS_HELD.contains(WATCHDOG));
    let fixture = Fixture::new("forker", A_FORK_WHILE_A_THREAD_IS_HELD);
    let mut debuggee = launch(&fixture);

    let line = line_of(
        A_FORK_WHILE_A_THREAD_IS_HELD,
        r#"return "the worker is held"#,
    );
    armed(
        &debuggee
            .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), line)])
            .expect("the breakpoint set was answered"),
    );

    let mut seen = Children::default();
    let stop = match run(&mut debuggee, &mut seen) {
        Running::Stopped { stop, .. } => stop,
        other => panic!("the worker has a breakpoint and the session answered {other:?}"),
    };

    // asked while the other thread is forking. a request that arrived in a
    // window and was dropped would hang here, and one that desynchronised the
    // stream would come back as a frame this build cannot decode
    for round in 0..20 {
        let stack = debuggee
            .stack(stop.stop, Some(1))
            .expect("the held thread walked its own stack");
        assert_eq!(stack.frames[0].line, line, "round {round}");

        let answer = debuggee
            .evaluate(stack.frames[0].id, "1 + 1", Detail::default())
            .expect("the held thread evaluated an expression");
        match answer {
            Evaluated::Value { value } => assert_eq!(
                value.content,
                Content::Int {
                    text: "2".to_string(),
                    omitted: None,
                },
                "round {round}"
            ),
            Evaluated::Raised { error } => panic!("`1 + 1` raised {error} in round {round}"),
        }
    }

    std::fs::write(fixture.directory().join("release"), "go")
        .expect("the forking thread was let go of");

    // the resume is the last thing to cross a window, and a lost one would show
    // as a program that never ended
    match run(&mut debuggee, &mut seen) {
        Running::Exited { status, .. } => assert!(status.success(), "{status}"),
        Running::StillRunning { .. } => panic!(
            "the worker was resumed and the program did not end. the resume was \
             sent while another thread was forking, and nothing delivered it"
        ),
        other => panic!("the program did not end: {other:?}"),
    }

    assert!(
        !seen.started.is_empty(),
        "the program has to have forked while the worker was held, or this \
         asked twenty questions of a session nothing was interfering with"
    );
}

/// a program whose forked child outlives it
///
/// the shape of every worker pool and every reloader: the parent's job is over
/// and the children go on. a socket is closed when the **last** descriptor
/// referring to it is, so a child that keeps its inherited copies keeps the
/// session open long after the process bpd launched has gone
const THE_CHILD_OUTLIVES_THE_PARENT: &str = r#"import os
import pathlib
import time

HERE = pathlib.Path(__file__).parent

pid = os.fork()
if pid == 0:
    # released by the test the moment it knows what the session did. the count
    # is a backstop and nothing else — a child left waiting for ever would hold
    # the test binary's own output pipe open
    for _ in range(1200):
        if (HERE / "release").exists():
            break
        time.sleep(0.1)
    os._exit(0)
"#;

#[test]
fn a_child_that_outlives_its_parent_does_not_hold_the_session_open() {
    let fixture = Fixture::new("forker", THE_CHILD_OUTLIVES_THE_PARENT);
    let mut debuggee = launch(&fixture);

    let mut seen = Children::default();
    let ran = run(&mut debuggee, &mut seen);
    std::fs::write(fixture.directory().join("release"), "go").expect("the child was let go of");

    match ran {
        Running::Exited { status, .. } => assert!(status.success(), "{status}"),
        Running::StillRunning { .. } => panic!(
            "the process bpd launched has exited and the session did not end. \
             its forked child is holding an inherited copy of the control \
             connection, and a socket closes when the last descriptor on it \
             does — so the engine is waiting on a connection whose only \
             remaining owner is a process it is not debugging"
        ),
        other => panic!("the program did not end: {other:?}"),
    }
}

/// a program that forks on its main thread while other threads are stopping
///
/// what this puts in the way of every fork is a **program thread inside the
/// agent's own state**: three workers going round a breakpoint as fast as the
/// session resumes them, each of them taking the stop registry to register
/// itself. on a gil build `os.fork()` holds the GIL and so do they, so they
/// cannot overlap; on a free-threaded build there is no GIL and they do, which
/// is the build this is written for
///
/// the fork is on the **main** thread on purpose. `fork` keeps only the calling
/// thread, so a child forked from a worker would have no frame of the agent's
/// entry point left to return through — this way the child leaves the loop,
/// runs off the end of the program and goes out through `session::finishing`,
/// which reads the stop registry it inherited. a child that inherited a locked
/// one instead of a replaced one waits there for ever, and its parent waits in
/// `waitpid` for it
const FORKING_WHILE_THREADS_STOP: &str = r#"import os
import pathlib
import signal
import threading
import time
import warnings

HERE = pathlib.Path(__file__).parent
PARENT = os.getpid()

# `os.fork()` with more than one thread on the process is a DeprecationWarning
# since 3.12, and this forks hundreds of times on purpose
warnings.simplefilter("ignore", DeprecationWarning)

release = threading.Event()


def stops_here(round):
    return round


def stopper():
    round = 0
    while not release.is_set():
        stops_here(round)
        round += 1


workers = [threading.Thread(target=stopper) for _ in range(3)]
for worker in workers:
    worker.start()

# the loop forks until the **session** lets it go, rather than a fixed number of
# times. a bound raced the debugger: on a runner where a fork costs less than a
# stop round trip, two hundred forks finished before the session had driven two
# hundred stops, the program ended, and the guard below refused a run in which
# nothing was wrong. the bound that is left is a watchdog — reaching it stops
# forking and waits, so a test that never releases is a slow failure and not a
# fork bomb
# a floor **and** a release, because either alone depends on which of the two
# machines is faster: two hundred forks finished before the session had driven
# two hundred stops on one runner, and on another the release arrived after a
# single fork. so it forks at least this many, and then until it is let go
forks = 0
while forks < 50 or not (HERE / "release").exists():
    if forks >= 2000:
        time.sleep(0.01)
        continue
    pid = os.fork()
    if pid == 0:
        signal.alarm(120)
        break
    forks += 1
    _, status = os.waitpid(pid, 0)
    assert os.waitstatus_to_exitcode(status) == 0, (
        "a forked child did not run to the end of the program: %r" % (status,)
    )

if os.getpid() == PARENT:
    (HERE / "forks").write_text(str(forks))
    release.set()
    for worker in workers:
        worker.join()
"#;

/// how many stops the session drives before it lets the forking loop go
///
/// enough that the forking and the registering really do overlap, and a bound
/// rather than a measurement: the loop is released by the test, so what this
/// number decides is how long the two are in each other's way, not how long the
/// test takes
///
/// the program forks until it is released rather than a fixed number of times,
/// which is what makes that true on a machine other than this one. it used to
/// fork two hundred times and stop, and on a runner where a fork costs less
/// than a stop round trip it finished first — the program ended after 42 stops
/// and the guard below refused a run in which nothing had gone wrong
const ROUNDS: usize = 200;

#[test]
fn a_child_forked_while_threads_are_registering_stops_runs_to_the_end() {
    assert!(FORKING_WHILE_THREADS_STOP.contains(WATCHDOG));
    let fixture = Fixture::new("forker", FORKING_WHILE_THREADS_STOP);
    let mut debuggee = launch(&fixture);

    let line = line_of(FORKING_WHILE_THREADS_STOP, "    return round");
    armed(
        &debuggee
            .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), line)])
            .expect("the breakpoint set was answered"),
    );

    let mut seen = Children::default();
    let mut rounds = 0;
    let status = loop {
        match run(&mut debuggee, &mut seen) {
            Running::Stopped { .. } => {
                rounds += 1;
                if rounds == ROUNDS {
                    std::fs::write(fixture.directory().join("release"), "go")
                        .expect("the forking loop was let go of");
                }
            }
            Running::Exited { status, .. } => break status,
            Running::StillRunning { .. } => panic!(
                "the program did not end within {LONG_ENOUGH:?}, after {rounds} \
                 stops. a forked child that inherited a locked stop registry \
                 waits for ever in the agent's exit path, and its parent waits \
                 for it in `waitpid`"
            ),
            // bpd launched this program and holds its child, so it is bpd that
            // reads the exit
            Running::Ended { .. } => unreachable!(
                "the program bpd launched ended without an exit status, and bpd \
                 holds its child"
            ),
            finishing @ Running::Finishing { .. } => {
                panic!("the program did not end: {finishing:?}")
            }
        }
    };
    assert!(
        status.success(),
        "the program exited with {status}. every child it forked has to have \
         run to the end of the program and exited zero"
    );
    assert!(
        rounds >= ROUNDS,
        "the session drove {rounds} stops, and the forking loop is only \
         released after {ROUNDS} — so the forking and the registering did not \
         overlap and this proved nothing"
    );

    let forked: usize = std::fs::read_to_string(fixture.directory().join("forks"))
        .expect("the program wrote how many times it forked")
        .trim()
        .parse()
        .expect("the program wrote a count");
    assert!(
        forked > 1,
        "the program forked {forked} time(s), which is not a fork under load"
    );
    assert_eq!(
        seen.started.len(),
        forked,
        "bpd has to have reported every one of the {forked} forks: {:?}",
        seen.started.len()
    );
}

/// a fork inside a fork
///
/// the handler is inherited by the child along with everything else, so it runs
/// again in the grandchild — against a process that has already given the
/// session up and closed the descriptors. giving them up twice has to mean the
/// same as giving them up once
const A_FORK_OF_A_FORK: &str = r#"import json
import os
import pathlib
import signal
import sys

HERE = pathlib.Path(__file__).parent
DEBUGGER_ID = 0


def anywhere_below():
    return "a line every generation runs"


pid = os.fork()
if pid == 0:
    signal.alarm(120)
    grandchild = os.fork()
    if grandchild == 0:
        monitoring = sys.monitoring
        (HERE / "seen").write_text(
            json.dumps(
                {
                    "ran": anywhere_below(),
                    "tool": monitoring.get_tool(DEBUGGER_ID),
                    "global_events": monitoring.get_events(DEBUGGER_ID),
                    "local_events": monitoring.get_local_events(
                        DEBUGGER_ID, anywhere_below.__code__
                    ),
                },
                indent=1,
            )
        )
        os._exit(3)
    _, below = os.waitpid(grandchild, 0)
    os._exit(os.waitstatus_to_exitcode(below))

_, status = os.waitpid(pid, 0)
assert os.waitstatus_to_exitcode(status) == 3, status
"#;

#[test]
fn a_fork_of_a_fork_gives_up_a_session_that_was_already_given_up() {
    assert!(A_FORK_OF_A_FORK.contains(WATCHDOG));
    let fixture = Fixture::new("forker", A_FORK_OF_A_FORK);
    let mut debuggee = launch(&fixture);

    let line = line_of(A_FORK_OF_A_FORK, r#"return "a line every generation"#);
    armed(
        &debuggee
            .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), line)])
            .expect("the breakpoint set was answered"),
    );

    let mut seen = Children::default();
    match run(&mut debuggee, &mut seen) {
        Running::Exited { status, .. } => assert!(status.success(), "{status}"),
        Running::Stopped { stop, .. } => panic!(
            "the engine was told about {:?}, from a generation of the program \
             that gave the debugger up before it existed",
            stop.reason
        ),
        other => panic!("the program did not end: {other:?}"),
    }

    let seen_below: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fixture.directory().join("seen"))
            .expect("the grandchild wrote what it saw"),
    )
    .expect("the grandchild wrote json");
    assert_eq!(
        seen_below["tool"],
        serde_json::Value::Null,
        "the grandchild of a debuggee holds no tool id: {seen_below}"
    );
    assert_eq!(seen_below["global_events"], 0, "{seen_below}");
    assert_eq!(seen_below["local_events"], 0, "{seen_below}");

    // the second fork is the child's, and only the process that attached
    // reports — so one report for two forks, which is the stated limit rather
    // than a report gone missing
    assert_eq!(
        seen.started.len(),
        1,
        "the process bpd attached to forked once, and bpd said {:?}",
        seen.started
    );
    assert_eq!(seen.started[0].verdict, Verdict::ThisProcess);
}

// ---- a fork that **is** debugged -------------------------------------------
//
// everything above is what a fork does when nothing asked for one to be
// debugged, which is the default and is what a program gets unless a front end
// says otherwise. what follows is the other half: with `debug_children` on, the
// child gives the inherited connection up and opens one of its own, and is held
// at the line that forked
//
// nothing goes through the environment and nothing is written to disk. a fork
// inherits memory, so the endpoint and the token the child presents are the
// ones its parent was given — which is why `crates/bpd/tests/launch_parity.rs`
// is untouched by any of this

/// how long a wait that is only looking for a session to arrive is given
///
/// short, because it is polled: the listener is looked at from inside a wait,
/// so this is how long one turn of that spends on a program that is not talking
const A_MOMENT: Duration = Duration::from_secs(2);

/// ask one session for something, and require it to be answered
fn ask(debuggee: &mut Debuggee, at: SessionId, request: Request, seen: &mut Children) -> Response {
    match debuggee.dispatch(bpd_core::Addressed::to(at, request), seen) {
        Ok(answer) => answer,
        Err(error) => panic!("{at} was not answered: {error}"),
    }
}

/// resume one session and wait for what its program does next
fn run_in(debuggee: &mut Debuggee, at: SessionId, seen: &mut Children) -> Running {
    match ask(
        debuggee,
        at,
        Request::Run {
            deadline: Some(LONG_ENOUGH),
        },
        seen,
    ) {
        Response::Ran(ran) => ran,
        other => panic!("a run was answered with {other:?}"),
    }
}

/// wait on one session until the debuggee holds `wanted` sessions
///
/// the listener is looked at from inside a wait, which is what makes a session
/// that arrives while the program runs arrive at all. so this drives a wait on
/// a session that has nothing to say and reads the count back
fn until_sessions(debuggee: &mut Debuggee, at: SessionId, wanted: usize, seen: &mut Children) {
    for _ in 0..15 {
        if debuggee.sessions().len() >= wanted {
            return;
        }
        match ask(
            debuggee,
            at,
            Request::Wait {
                deadline: Some(A_MOMENT),
            },
            seen,
        ) {
            Response::Ran(Running::StillRunning { .. }) => {}
            Response::Ran(other) => panic!(
                "the session being waited on was supposed to be busy with its \
                 child, and answered {other:?}"
            ),
            other => panic!("a wait was answered with {other:?}"),
        }
    }
    panic!(
        "the debuggee holds {:?} and a forked child was supposed to have joined \
         it",
        debuggee.sessions()
    );
}

/// the one session a debuggee holds that is not one of `known`
fn the_new_one(debuggee: &Debuggee, known: &[SessionId]) -> SessionId {
    let open = debuggee.sessions();
    let mut fresh = open.iter().filter(|id| !known.contains(id));
    match (fresh.next(), fresh.next()) {
        (Some(only), None) => *only,
        _ => panic!(
            "one session was supposed to have joined {known:?}, and the debuggee holds {open:?}"
        ),
    }
}

/// a program that forks once, and whose child does something the test can see
///
/// the parent writes its own pid **before** the fork, so that the child — which
/// is held before it runs anything of its own — already has it on disk. that is
/// what `StopReason::Forked`'s `parent` is checked against, rather than against
/// a number bpd could have invented
const A_FORK_TO_DEBUG: &str = r#"import os
import pathlib
import signal

HERE = pathlib.Path(__file__).parent
(HERE / "parent").write_text(str(os.getpid()))

pid = os.fork()
if pid == 0:
    signal.alarm(120)
    (HERE / "child").write_text(str(os.getpid()))
    os._exit(7)

_, status = os.waitpid(pid, 0)
assert os.waitstatus_to_exitcode(status) == 7, status
"#;

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "it is one program's whole life with a debugged child in it — the \
              setting, the fork, the session that joins, where the child is \
              held, and both processes ending. splitting it would launch the \
              same program several times and assert on a different half of one \
              sequence each time"
)]
fn a_forked_child_opens_a_session_of_its_own_and_is_held_where_it_forked() {
    assert!(A_FORK_TO_DEBUG.contains(WATCHDOG));
    let fixture = Fixture::new("forker", A_FORK_TO_DEBUG);
    let mut debuggee = launch(&fixture);
    let parent = the_only_session(&debuggee);

    // read back off the agent rather than assumed from the request: a setting
    // the fork handler never received would leave this test waiting for a
    // session that is never going to arrive
    assert!(
        debuggee
            .debug_children(true)
            .expect("the debuggee took the setting"),
        "the agent has to say the setting took"
    );

    let mut seen = Children::default();
    match ask(
        &mut debuggee,
        parent,
        Request::Run {
            deadline: Some(A_MOMENT),
        },
        &mut seen,
    ) {
        Response::Ran(Running::StillRunning { .. }) => {}
        other => panic!(
            "the parent forks and then waits on its child, so it does not end \
             until the child is resumed: {other:?}"
        ),
    }
    until_sessions(&mut debuggee, parent, 2, &mut seen);
    let child = the_new_one(&debuggee, &[parent]);

    // it was **reported** as it arrived. a session that joined is a held
    // process, so a front end that is never told has a stopped program it
    // cannot reach — which is the whole reason this is opt-in
    assert_eq!(
        seen.joined,
        vec![child],
        "the child's session was not reported as it joined"
    );

    // and the fork itself is still reported, exactly as it is with this off.
    // the two are different claims: one says a child exists, and the other says
    // bpd is now debugging it
    assert_eq!(seen.started.len(), 1, "{:?}", seen.started);
    assert_eq!(seen.started[0].verdict, Verdict::ThisProcess);

    // and the first has to agree with the second. the child's session joined
    // during the same run, so a report that said bpd was not taking the child
    // up would be the debugger contradicting itself one line later — which is
    // the one thing a person has no way to resolve
    assert!(
        seen.started[0].taking_up,
        "a session joined for this fork and the report of it said: {}",
        seen.started[0]
    );

    // now the thing this feature is: the child is **held**, at the line that
    // forked, before it has run anything of its own
    let stop = match ask(
        &mut debuggee,
        child,
        Request::Wait {
            deadline: Some(LONG_ENOUGH),
        },
        &mut seen,
    ) {
        Response::Ran(Running::Stopped { stop, .. }) => stop,
        other => panic!("the child was supposed to be held at the fork: {other:?}"),
    };
    assert_eq!(stop.session, child, "a stop is named where it arrives");

    let StopReason::Forked {
        parent: above,
        file,
        line,
    } = stop.reason.clone()
    else {
        panic!(
            "the child's first stop is its fork, and was {:?}",
            stop.reason
        )
    };
    assert_eq!(
        file,
        fixture.path().display().to_string(),
        "the child is held in the program's own file"
    );
    assert_eq!(
        line,
        line_of(A_FORK_TO_DEBUG, "pid = os.fork()"),
        "the child is held at the line that forked"
    );
    let recorded: u32 = std::fs::read_to_string(fixture.directory().join("parent"))
        .expect("the parent wrote its pid before it forked")
        .trim()
        .parse()
        .expect("a pid is a number");
    assert_eq!(
        above, recorded,
        "the stop has to name the process this one was forked from"
    );

    // the child's own stack is there to walk, which is what makes the stop
    // worth having: `after_in_child` runs with the frame chain of the
    // `os.fork()` caller intact
    let walked = match ask(
        &mut debuggee,
        child,
        Request::Stack {
            stop: stop.stop,
            top: None,
        },
        &mut seen,
    ) {
        Response::Stack(walked) => walked,
        other => panic!("the child's stack was answered with {other:?}"),
    };
    assert_eq!(walked.frames[0].line, line, "{walked:?}");

    // the stop counter is **inherited**, so the child's first stop is not 1.
    // that is the point of it living outside the table it numbers: a child that
    // started again at one would reissue, immediately, numbers its parent has
    // already reported — and the parent's entry stop is 1
    assert!(
        stop.stop > 1,
        "the child carried on the parent's stop numbering, and its first stop \
         was {}",
        stop.stop
    );

    // and it runs on when it is let go. bpd is not its parent, so what it
    // exited with is not bpd's to read — it ends, and says nothing about how
    match run_in(&mut debuggee, child, &mut seen) {
        Running::Ended { .. } => {}
        Running::Exited { status, .. } => {
            panic!("bpd did not start that process and cannot have read {status} off it")
        }
        other => panic!("the child did not end: {other:?}"),
    }
    let below: u32 = std::fs::read_to_string(fixture.directory().join("child"))
        .expect("the child ran on after it was resumed")
        .trim()
        .parse()
        .expect("a pid is a number");
    assert_ne!(
        below, recorded,
        "the process that ran on is the child rather than the parent"
    );

    // and the parent, which was let go before the child joined and has been in
    // `waitpid` ever since, finishes. it is waited for rather than resumed:
    // nothing of it is held, and a resume of a running program is refused
    match ask(
        &mut debuggee,
        parent,
        Request::Wait {
            deadline: Some(LONG_ENOUGH),
        },
        &mut seen,
    ) {
        Response::Ran(Running::Exited { status, .. }) => assert!(status.success(), "{status}"),
        other => panic!("the parent did not end: {other:?}"),
    }
}

/// a program whose child forks again
///
/// the handler is inherited by every generation, and a child that opened a
/// session of its own is a debuggee like any other — so its own fork is a third
/// session rather than a process that quietly stopped being debugged
const A_DEBUGGED_FORK_OF_A_FORK: &str = r#"import os
import pathlib
import signal

HERE = pathlib.Path(__file__).parent

pid = os.fork()
if pid == 0:
    signal.alarm(120)
    inner = os.fork()
    if inner == 0:
        signal.alarm(120)
        (HERE / "grandchild").write_text("here")
        os._exit(9)
    _, below = os.waitpid(inner, 0)
    os._exit(os.waitstatus_to_exitcode(below))

_, status = os.waitpid(pid, 0)
assert os.waitstatus_to_exitcode(status) == 9, status
"#;

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "three generations of one program, in the order they happen. what \
              is under test is the sequence itself, and it cannot be cut in \
              half without launching it twice"
)]
fn a_fork_of_a_fork_is_a_third_session_rather_than_a_process_that_slipped_out() {
    assert!(A_DEBUGGED_FORK_OF_A_FORK.contains(WATCHDOG));
    let fixture = Fixture::new("forker", A_DEBUGGED_FORK_OF_A_FORK);
    let mut debuggee = launch(&fixture);
    let parent = the_only_session(&debuggee);
    assert!(
        debuggee
            .debug_children(true)
            .expect("the debuggee took the setting")
    );

    let mut seen = Children::default();
    match ask(
        &mut debuggee,
        parent,
        Request::Run {
            deadline: Some(A_MOMENT),
        },
        &mut seen,
    ) {
        Response::Ran(Running::StillRunning { .. }) => {}
        other => panic!("the parent waits on its child: {other:?}"),
    }
    until_sessions(&mut debuggee, parent, 2, &mut seen);
    let child = the_new_one(&debuggee, &[parent]);

    // the child is held at its own fork line, and the setting reached it
    // through inherited memory rather than through anything it was told
    match ask(
        &mut debuggee,
        child,
        Request::Wait {
            deadline: Some(LONG_ENOUGH),
        },
        &mut seen,
    ) {
        Response::Ran(Running::Stopped { stop, .. }) => {
            assert!(
                matches!(stop.reason, StopReason::Forked { .. }),
                "{:?}",
                stop.reason
            );
        }
        other => panic!("the child was supposed to be held at the fork: {other:?}"),
    }

    // letting it go makes it fork in its turn, and the grandchild joins
    match ask(
        &mut debuggee,
        child,
        Request::Run {
            deadline: Some(A_MOMENT),
        },
        &mut seen,
    ) {
        Response::Ran(Running::StillRunning { .. }) => {}
        other => panic!("the child waits on its own child: {other:?}"),
    }
    until_sessions(&mut debuggee, child, 3, &mut seen);
    let grandchild = the_new_one(&debuggee, &[parent, child]);
    assert_eq!(
        seen.joined,
        vec![child, grandchild],
        "every generation that joined has to be reported as it arrives"
    );

    match ask(
        &mut debuggee,
        grandchild,
        Request::Wait {
            deadline: Some(LONG_ENOUGH),
        },
        &mut seen,
    ) {
        Response::Ran(Running::Stopped { stop, .. }) => {
            let StopReason::Forked { line, .. } = stop.reason.clone() else {
                panic!(
                    "the grandchild's first stop is its fork, and was {:?}",
                    stop.reason
                )
            };
            assert_eq!(
                line,
                line_of(A_DEBUGGED_FORK_OF_A_FORK, "inner = os.fork()"),
                "the grandchild is held at the line **its** parent forked on"
            );
        }
        other => panic!("the grandchild was supposed to be held at the fork: {other:?}"),
    }

    // and the three of them end from the bottom up, which is the order the
    // program itself waits in. the child is **not** resumed again: it was let
    // go above and has been sitting in `waitpid` ever since, so what it needs
    // is to be waited for rather than resumed
    match run_in(&mut debuggee, grandchild, &mut seen) {
        Running::Ended { .. } => {}
        other => panic!("the grandchild did not end: {other:?}"),
    }
    match ask(
        &mut debuggee,
        child,
        Request::Wait {
            deadline: Some(LONG_ENOUGH),
        },
        &mut seen,
    ) {
        Response::Ran(Running::Ended { .. }) => {}
        other => panic!("the child did not end: {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(fixture.directory().join("grandchild"))
            .expect("the grandchild ran on after it was resumed")
            .trim(),
        "here"
    );
    match ask(
        &mut debuggee,
        parent,
        Request::Wait {
            deadline: Some(LONG_ENOUGH),
        },
        &mut seen,
    ) {
        Response::Ran(Running::Exited { status, .. }) => assert!(status.success(), "{status}"),
        other => panic!("the parent did not end: {other:?}"),
    }
}
