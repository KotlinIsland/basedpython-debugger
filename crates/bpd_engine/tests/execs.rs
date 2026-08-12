//! a child that was **`exec`'d** is a fresh interpreter, and it is debugged
//! anyway
//!
//! a fork inherits memory, so a forked child is born holding the endpoint and
//! the token. an `exec` inherits nothing but the environment and the file
//! descriptors: `subprocess`, and `multiprocessing` with the `spawn` and
//! `forkserver` start methods, all reach a python that has never heard of bpd.
//! so the agent has to be *found*, and what finds it is `PYTHONPATH` ending in a
//! directory holding a `sitecustomize`
//!
//! this is django's `runserver`. `django.utils.autoreload.restart_with_reloader`
//! calls `subprocess.run(args)` and the parent then does nothing but wait on the
//! exit code, so the **child** serves every request — and until this worked, a
//! breakpoint in a template was reported unbound, which was true and useless
//!
//! everything here drives a real interpreter and a real `subprocess`, because
//! what is under test is what cpython does at another interpreter's startup

use std::time::Duration;

use bpd_core::python::Capabilities;
use bpd_core::{Binding, Request, Response, Running, SessionId, SourceBreakpoint, StopReason};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};
use bpd_test::reporting::Children;

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// how long a wait on a program that should already be over is given
///
/// it bounds a failure rather than measuring anything: every test here expects
/// something to have happened, and a deadline turns "it never will" into a named
/// failure instead of a run that hangs
const LONG_ENOUGH: Duration = Duration::from_secs(30);

/// how long a wait on a program that is busy with its child is given
const A_MOMENT: Duration = Duration::from_secs(2);

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

fn the_only_session(debuggee: &Debuggee) -> SessionId {
    match debuggee.sessions().as_slice() {
        [only] => *only,
        open => panic!("this debuggee holds {open:?} and the test launched one program"),
    }
}

fn ask(debuggee: &mut Debuggee, at: SessionId, request: Request, seen: &mut Children) -> Response {
    match debuggee.dispatch(bpd_core::Addressed::to(at, request), seen) {
        Ok(answer) => answer,
        Err(error) => panic!("{at} was not answered: {error}"),
    }
}

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

/// wait for what one session's program does next, without resuming anything
///
/// what a session that was left **running** is asked. a `Run` would try to
/// resume a held thread first, and a parent blocked in `waitpid` on a child bpd
/// is holding has none
fn wait_in(debuggee: &mut Debuggee, at: SessionId, seen: &mut Children) -> Running {
    match ask(
        debuggee,
        at,
        Request::Wait {
            deadline: Some(LONG_ENOUGH),
        },
        seen,
    ) {
        Response::Ran(ran) => ran,
        other => panic!("a wait was answered with {other:?}"),
    }
}

/// wait on `at` until the debuggee holds `wanted` sessions
fn until_sessions(debuggee: &mut Debuggee, at: SessionId, wanted: usize, seen: &mut Children) {
    for _ in 0..30 {
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
        "the debuggee holds {:?} and an `exec`'d child was supposed to have \
         joined it. the child is entered through a staged `sitecustomize` on the \
         end of its `PYTHONPATH` — if it did not arrive, either the variables \
         did not reach it or the agent would not import into it, and its own \
         stderr says which",
        debuggee.sessions()
    );
}

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

/// the shape `django.utils.autoreload.restart_with_reloader` has, read in
/// django 6.1
///
/// the parent starts `sys.executable` on a script of its own and then does
/// nothing but wait for the exit code. every line of work is in the child, and
/// so is the only breakpoint this test sets — the parent never calls that
/// function, so a stop on it can only have come from the child
const RELOADER: &str = r#"import os
import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).parent
(HERE / "parent").write_text(str(os.getpid()))

finished = subprocess.run([sys.executable, str(HERE / "worker.py")])
raise SystemExit(finished.returncode)
"#;

/// what the reloader starts, which is where the work is
const WORKER: &str = r#"import pathlib
import signal
import sys

signal.alarm(300)
HERE = pathlib.Path(__file__).parent


def serve():
    (HERE / "served").write_text("the child did the work")


serve()
sys.exit(7)
"#;

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "it is one reloader's whole life — the setting, the child that \
              execs, the session it opens, a breakpoint bound in it and hit, and \
              both processes ending. splitting it would run the same program \
              several times and assert on a different half of one sequence each \
              time"
)]
fn a_child_that_execs_opens_a_session_of_its_own_and_stops_on_a_breakpoint() {
    let fixture = Fixture::new("reloader", RELOADER);
    let worker = fixture.sibling("worker", WORKER);
    let mut debuggee = launch(&fixture);
    let parent = the_only_session(&debuggee);

    // read back off the agent rather than assumed from the request: a setting
    // that never reached the process would leave this waiting for a session
    // that is never going to arrive
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
            "the parent waits on its child, so it does not end until the child \
             is resumed: {other:?}"
        ),
    }
    until_sessions(&mut debuggee, parent, 2, &mut seen);
    let child = the_new_one(&debuggee, &[parent]);

    // reported as it arrived, for the reason a forked child's session is: it is
    // a **held** process, and a front end that is never told has a stopped
    // program it cannot reach
    assert_eq!(
        seen.joined,
        vec![child],
        "the child's session was not reported as it joined"
    );

    // and the child is still *reported* as a child. the two are different
    // claims: one says a process exists, the other says bpd is debugging it
    assert_eq!(seen.started.len(), 1, "{:?}", seen.started);

    let stop = match ask(
        &mut debuggee,
        child,
        Request::Wait {
            deadline: Some(LONG_ENOUGH),
        },
        &mut seen,
    ) {
        Response::Ran(Running::Stopped { stop, .. }) => stop,
        other => panic!("the child was supposed to arrive held: {other:?}"),
    };
    assert_eq!(stop.session, child, "a stop is named where it arrives");

    let StopReason::Started { parent: above } = stop.reason.clone() else {
        panic!(
            "an `exec`'d child's first stop is its start, and was {:?}",
            stop.reason
        )
    };
    let recorded: u32 = std::fs::read_to_string(fixture.directory().join("parent"))
        .expect("the parent wrote its pid before it started the child")
        .trim()
        .parse()
        .expect("a pid is a number");
    assert_eq!(
        above, recorded,
        "the stop has to name the process that started this one"
    );

    // nothing of the program has run, so there are no frames of it to walk. the
    // only python running is bpd's own four lines, and reporting those as the
    // program's stack would be the debugger pointing at itself
    match ask(
        &mut debuggee,
        child,
        Request::Stack {
            stop: stop.stop,
            top: None,
        },
        &mut seen,
    ) {
        Response::Stack(walked) => assert!(
            walked.frames.is_empty(),
            "an `exec`'d child is held before its program is compiled, and the \
             stack held {:?}",
            walked.frames
        ),
        other => panic!("the child's stack was answered with {other:?}"),
    }

    // the whole point: a breakpoint in the code the **child** runs, bound
    // against the child's own interpreter, and hit there
    let line = line_of(WORKER, r#"write_text("the child did the work")"#);
    let resolved = match ask(
        &mut debuggee,
        child,
        Request::SetBreakpoints {
            breakpoints: vec![SourceBreakpoint::at(1, worker.clone(), line)],
        },
        &mut seen,
    ) {
        Response::BreakpointsResolved { resolved } => resolved,
        other => panic!("the child's breakpoints were answered with {other:?}"),
    };

    let hit = match run_in(&mut debuggee, child, &mut seen) {
        Running::Stopped { stop, rebound } => {
            // it binds when the child compiles `worker.py`, not when it is set:
            // at the entry stop the file has not been read yet, which is why a
            // rebinding is announced while the program runs
            assert!(
                matches!(resolved[0].binding, Binding::Bound { .. })
                    || rebound
                        .iter()
                        .any(|late| matches!(late.binding, Binding::Bound { .. })),
                "the breakpoint never bound in the child: {resolved:?} then {rebound:?}"
            );
            stop
        }
        other => panic!(
            "the child was supposed to stop on the breakpoint in `worker.py`: \
             {other:?}"
        ),
    };
    let StopReason::Breakpoint { file, line: at, .. } = hit.reason.clone() else {
        panic!("the child stopped for {:?}", hit.reason)
    };
    assert_eq!(file, worker.display().to_string());
    assert_eq!(at, line);

    // and both programs run to their own ends, with the child's exit code the
    // parent's. a debugged child that could not be let go would leave the
    // parent in `waitpid` for ever
    match run_in(&mut debuggee, child, &mut seen) {
        Running::Ended { .. } => {}
        other => panic!("the child did not end: {other:?}"),
    }
    match wait_in(&mut debuggee, parent, &mut seen) {
        Running::Exited { status, .. } => assert_eq!(
            status.code(),
            Some(7),
            "the parent exits with its child's code, and the child's work has \
             to have happened"
        ),
        other => panic!("the parent did not end: {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(fixture.directory().join("served"))
            .expect("the child did its work"),
        "the child did the work"
    );
}

/// a program that starts a child which is **not** python, and then one that is
///
/// the environment reaches both. `/bin/sh` ignores it because nothing but an
/// interpreter reads `PYTHONPATH`, and the python **grandchild** it starts
/// inherits it and attaches — which is the feature working through a shell
const THROUGH_A_SHELL: &str = r#"import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).parent
subprocess.run(["/bin/echo", "an ordinary child"], check=True, capture_output=True)
subprocess.run(
    "%s %s" % (sys.executable, HERE / "grandchild.py"), shell=True, check=True
)
"#;

const GRANDCHILD: &str = r#"import pathlib
import signal

signal.alarm(300)
pathlib.Path(__file__).parent.joinpath("grandchild").write_text("reached")
"#;

#[cfg(unix)]
#[test]
fn a_child_that_is_not_python_is_inert_and_a_python_grandchild_is_not() {
    let fixture = Fixture::new("shelling", THROUGH_A_SHELL);
    fixture.sibling("grandchild", GRANDCHILD);
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
        other => panic!("the parent waits on its children: {other:?}"),
    }

    // `/bin/echo` inherited the variables and ignored them, so it cannot have
    // opened a session — and the grandchild behind `sh -c` is a python that
    // inherited them and did
    until_sessions(&mut debuggee, parent, 2, &mut seen);
    let grandchild = the_new_one(&debuggee, &[parent]);
    assert_eq!(
        debuggee.sessions().len(),
        2,
        "exactly one session joined: `/bin/echo` is not an interpreter and \
         reads none of this"
    );

    // it arrives **held**, exactly as a child started directly does. reaching a
    // grandchild through `sh -c` is the feature working rather than an
    // exception to it
    match wait_in(&mut debuggee, grandchild, &mut seen) {
        Running::Stopped { stop, .. } => assert!(
            matches!(stop.reason, StopReason::Started { .. }),
            "the grandchild stopped for {:?}",
            stop.reason
        ),
        other => panic!("the grandchild was supposed to arrive held: {other:?}"),
    }
    match run_in(&mut debuggee, grandchild, &mut seen) {
        Running::Ended { .. } => {}
        other => panic!("the grandchild did not end: {other:?}"),
    }
    match wait_in(&mut debuggee, parent, &mut seen) {
        Running::Exited { status, .. } => assert!(status.success(), "{status}"),
        other => panic!("the parent did not end: {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(fixture.directory().join("grandchild"))
            .expect("the grandchild ran"),
        "reached"
    );
}

/// a program that starts a python child, with nothing asked for
const AN_ORDINARY_CHILD: &str = r#"import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).parent
subprocess.run([sys.executable, str(HERE / "worker.py")], check=False)
"#;

#[test]
fn a_child_that_execs_is_not_debugged_unless_it_was_asked_for() {
    // the default, and the reason it is the default: a debugged child **stops**,
    // and a setting that produced stopped processes without being asked for
    // would be a debugger that hangs programs
    let fixture = Fixture::new("ordinary", AN_ORDINARY_CHILD);
    fixture.sibling("worker", WORKER);
    let mut debuggee = launch(&fixture);
    let parent = the_only_session(&debuggee);

    let mut seen = Children::default();
    match run_in(&mut debuggee, parent, &mut seen) {
        Running::Exited { .. } => {}
        other => panic!(
            "the program ran a child that was never asked to be debugged, so \
             nothing should have held it: {other:?}"
        ),
    }
    assert!(
        seen.joined.is_empty(),
        "debugging a child that execs is off unless it is asked for, and {:?} \
         joined",
        seen.joined
    );
    assert_eq!(
        debuggee.sessions().len(),
        1,
        "the debuggee gained a session nobody asked for"
    );
    // the child is still *reported*, which is what the audit hook has always
    // done and is a different claim from debugging it
    assert_eq!(seen.started.len(), 1, "{:?}", seen.started);
}
