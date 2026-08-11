//! stopping where an exception is raised, and where one leaves the program
//!
//! the whole of this feature is one distinction: an exception that a library
//! raises and catches is not an uncaught exception, and whether it will be
//! caught is not knowable when it is raised. so the two settings answer at
//! different moments, and the tests are about which of them fires when
//!
//! two of the assertions here are about **cpython** rather than about bpd, and
//! are made in a bare interpreter with no agent anywhere near them: that one
//! `raise` produces a raise event in every frame the exception passes through,
//! and that an exception escaping a `threading.Thread`'s target is caught by
//! `threading` itself

use std::ffi::OsString;
use std::path::Path;

use bpd_core::python::Capabilities;
use bpd_core::{Binding, ExceptionBreakpoints, PythonError, Running, SourceBreakpoint, StopReason};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// a library that raises and catches, and an exception two frames deep that the
/// caller catches
///
/// the markers are written with `open` rather than through `pathlib`, because
/// every stop here is counted and a raise inside the library that wrote them
/// would be a stop the test never asked for
const CAUGHT: &str = r#"import pathlib

HERE = str(pathlib.Path(__file__).parent)


def note(name):
    with open(HERE + "/" + name, "w") as handle:
        handle.write("x")


def library():
    try:
        raise KeyError("swallowed by the library")
    except KeyError:
        return "handled"


def deep():
    raise ValueError("from deep")


def middle():
    deep()


def catcher():
    try:
        middle()
    except ValueError:
        return "caught"


def main():
    handled = library()
    note("library_done")
    caught = catcher()
    note("catcher_done")
    return handled, caught


ready = 1
run_it = main()
"#;

/// the same library, and then an exception nothing catches
const ESCAPING: &str = r#"import pathlib

HERE = str(pathlib.Path(__file__).parent)


def note(name):
    with open(HERE + "/" + name, "w") as handle:
        handle.write("x")


def library():
    try:
        raise KeyError("swallowed by the library")
    except KeyError:
        return "handled"


def escaping():
    raise ValueError("nothing catches this")


def main():
    handled = library()
    note("library_done")
    escaping()
    note("never")


ready = 1
run_it = main()
"#;

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

fn launch(fixture: &Fixture) -> Debuggee {
    match bpd_engine::launch(
        interpreter(),
        &bpd_engine::Program::Script(fixture.path()),
        &[] as &[OsString],
    ) {
        Ok(Launched::Stopped(debuggee)) => debuggee,
        Ok(Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

/// run to the module's last statement, and arm the exception breakpoints there
///
/// not at the entry stop, which is before the program's own imports: arming
/// `RAISE` across an import means counting every exception the import system
/// raises at itself, and the thing under test is the program
fn armed(fixture: &Fixture, source: &str, raised: bool, uncaught: bool) -> Debuggee {
    let mut debuggee = launch(fixture);
    let ready = line_of(source, "ready = 1");
    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), ready)])
        .expect("the breakpoint request was answered");
    assert!(matches!(resolved[0].binding, Binding::Bound { .. }));

    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        other => panic!("expected the program to stop before it ran, got {other:?}"),
    }

    let set = debuggee
        .set_exception_breakpoints(raised, uncaught)
        .expect("the exception breakpoints were set");
    assert_eq!(set, ExceptionBreakpoints { raised, uncaught });
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    debuggee
}

/// the next stop, or what the program did instead
fn next_stop(debuggee: &mut Debuggee) -> StopReason {
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop.reason,
        other => panic!("expected a stop, got {other:?}"),
    }
}

fn ran(fixture: &Fixture, name: &str) -> bool {
    fixture.directory().join(name).exists()
}

/// the frames a `PythonError` carries, as `function:line`
fn traceback(error: &PythonError) -> Vec<String> {
    error
        .traceback
        .iter()
        .map(|frame| frame.function.clone())
        .collect()
}

#[test]
fn an_exception_caught_inside_a_library_is_not_an_uncaught_one() {
    let fixture = Fixture::new("escaping", ESCAPING);
    let mut debuggee = armed(&fixture, ESCAPING, false, true);

    // the `KeyError` is raised and caught inside `library`, which is what a
    // library doing its ordinary work looks like. a debugger that decided at
    // the raise would have stopped for it
    let reason = next_stop(&mut debuggee);
    let StopReason::Uncaught { error, file, line } = &reason else {
        panic!("expected an uncaught exception, got {reason:?}")
    };
    assert_eq!(error.kind, "ValueError");
    assert_eq!(error.message, "nothing catches this");
    assert!(
        ran(&fixture, "library_done"),
        "the library raised, caught its own exception and carried on"
    );
    assert!(!ran(&fixture, "never"), "the program did not get past it");

    // it is reported from the outermost frame, because that is the first moment
    // it is knowable, and the frames it came through are on its own traceback
    assert_eq!(Path::new(file), fixture.path());
    assert_eq!(*line, line_of(ESCAPING, "run_it = main()"));
    assert_eq!(traceback(error), ["<module>", "main", "escaping"]);

    // and on being let go it does what it was always going to do. nothing
    // stops again on the way out: the agent reports an exception the program
    // did not catch by raising `SystemExit` out of its own bootstrap frame, and
    // a stop for that would be bpd stopping the program for its own decision
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Exited { status, .. } => assert!(
            !status.success(),
            "an exception nothing catches ends the program"
        ),
        other => panic!("expected the program to end, got {other:?}"),
    }
}

#[test]
fn nothing_is_uncaught_in_a_program_that_catches_everything() {
    let fixture = Fixture::new("caught", CAUGHT);
    let mut debuggee = armed(&fixture, CAUGHT, false, true);

    // two exceptions are raised, one of them through two frames, and both are
    // caught. an uncaught-exception stop for either would be a false statement
    // about the program
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Exited { status, .. } => assert!(status.success()),
        other => panic!("nothing here is uncaught, and it stopped for {other:?}"),
    }
    assert!(ran(&fixture, "catcher_done"));
}

#[test]
fn a_raise_stops_in_the_frame_that_raised_it() {
    let fixture = Fixture::new("caught", CAUGHT);
    let mut debuggee = armed(&fixture, CAUGHT, true, false);

    let reason = next_stop(&mut debuggee);
    let StopReason::Raised { error, file, line } = &reason else {
        panic!("expected a raise, got {reason:?}")
    };
    assert_eq!(error.kind, "KeyError");
    assert_eq!(Path::new(file), fixture.path());
    assert_eq!(*line, line_of(CAUGHT, "raise KeyError"));

    // the point of stopping at the raise rather than at the unwind: the stack
    // that raised it is still standing
    let stack = debuggee.the_stack(None).expect("the stack was answered");
    assert_eq!(
        stack
            .frames
            .iter()
            .map(bpd_core::Frame::name)
            .collect::<Vec<_>>(),
        ["library", "main", "<module>"]
    );
    assert!(
        !ran(&fixture, "library_done"),
        "it is held where the exception was raised, before the library returned"
    );

    debuggee
        .set_exception_breakpoints(false, false)
        .expect("the exception breakpoints were cleared");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Exited { status, .. } => assert!(status.success()),
        other => panic!("nothing is armed any more, and it stopped for {other:?}"),
    }
}

#[test]
fn one_raise_is_one_stop_however_many_frames_it_passes_through() {
    let fixture = Fixture::new("caught", CAUGHT);
    let mut debuggee = armed(&fixture, CAUGHT, true, false);

    // cpython raises the event again in every frame the exception propagates
    // into — three of them for the `ValueError`, which
    // `the_interpreter_raises_an_exception_event_in_every_frame_it_passes_through`
    // measures directly. they are one exception and they are one stop
    let mut raised = Vec::new();
    loop {
        match debuggee
            .run(&mut bpd_test::reporting::Unreported)
            .expect("the debuggee was resumed")
        {
            Running::Stopped { stop, .. } => match stop.reason {
                StopReason::Raised { error, line, .. } => raised.push((error.kind, line)),
                other => panic!("expected a raise, got {other:?}"),
            },
            Running::Exited { status, .. } => {
                assert!(status.success());
                break;
            }
            Running::StillRunning { waited, .. } => unreachable!(
                "this wait carries no deadline and was answered after {waited:?} \
                 with the program still running"
            ),
            other @ Running::Finishing { .. } => {
                panic!("expected a raise or the end, got {other:?}")
            }
        }
    }

    assert_eq!(
        raised,
        vec![
            ("KeyError".to_string(), line_of(CAUGHT, "raise KeyError")),
            (
                "ValueError".to_string(),
                line_of(CAUGHT, "raise ValueError")
            ),
        ],
        "each exception is reported once, in the frame it was raised in"
    );
}

#[test]
fn the_interpreter_raises_an_exception_event_in_every_frame_it_passes_through() {
    // why a raise is reported once rather than once per event. measured in a
    // **bare** interpreter, because it is a statement about PEP 669 rather than
    // about bpd — if cpython ever stops doing it, the deduplication becomes
    // dead code rather than the thing holding the line
    let seen = bpd_test::eval(
        interpreter(),
        "import json, sys\n\
         mon = sys.monitoring\n\
         mon.use_tool_id(0, 'probe')\n\
         seen = []\n\
         def deep():\n\
         \x20   raise ValueError('once')\n\
         def middle():\n\
         \x20   deep()\n\
         def catcher():\n\
         \x20   try:\n\
         \x20       middle()\n\
         \x20   except ValueError:\n\
         \x20       return 'caught'\n\
         def on_raise(code, offset, exception):\n\
         \x20   seen.append((code.co_name, id(exception)))\n\
         mon.register_callback(0, mon.events.RAISE, on_raise)\n\
         mon.set_events(0, mon.events.RAISE)\n\
         catcher()\n\
         mon.set_events(0, 0)\n\
         print(json.dumps([[name, ids == seen[0][1]] for name, ids in seen]))\n",
    );

    let events: Vec<(String, bool)> =
        serde_json::from_str(&seen).expect("the ground truth snippet prints a json list");
    assert_eq!(
        events,
        vec![
            ("deep".to_string(), true),
            ("middle".to_string(), true),
            ("catcher".to_string(), true),
        ],
        "one `raise` statement, one exception object, and an event in every \
         frame it passed through"
    );
}

#[test]
fn an_exception_a_worker_thread_lets_escape_is_caught_by_threading_itself() {
    // the documented limit of "uncaught", measured rather than asserted about
    // bpd: an exception that escapes a thread's target never unwinds out of the
    // thread's outermost frame, because `threading` catches it and hands it to
    // `threading.excepthook`. so it is not uncaught, and bpd does not report it
    // as one
    let seen = bpd_test::eval(
        interpreter(),
        "import json, sys, threading\n\
         unwound = []\n\
         hooked = []\n\
         mon = sys.monitoring\n\
         mon.use_tool_id(0, 'probe')\n\
         def on_unwind(code, offset, exception):\n\
         \x20   if sys._getframe(1).f_back is None:\n\
         \x20       unwound.append(code.co_name)\n\
         mon.register_callback(0, mon.events.PY_UNWIND, on_unwind)\n\
         threading.excepthook = lambda args: hooked.append(args.exc_type.__name__)\n\
         def target():\n\
         \x20   raise ValueError('escapes the thread')\n\
         mon.set_events(0, mon.events.PY_UNWIND)\n\
         thread = threading.Thread(target=target)\n\
         thread.start()\n\
         thread.join()\n\
         mon.set_events(0, 0)\n\
         print(json.dumps([hooked, unwound]))\n",
    );

    let escaped: Vec<Vec<String>> =
        serde_json::from_str(&seen).expect("the ground truth snippet prints two json lists");
    let [hooked, unwound] = escaped.as_slice() else {
        panic!("the ground truth snippet prints exactly two lists, and got {escaped:?}")
    };

    assert_eq!(
        hooked,
        &["ValueError"],
        "`threading` catches it and hands it to its own hook"
    );
    assert!(
        unwound.is_empty(),
        "nothing unwound out of a frame with no caller, so there is no moment \
         at which this exception is leaving the program: {unwound:?}"
    );
}

#[test]
fn clearing_a_code_objects_local_events_undoes_its_disables() {
    // the cheaper instrument a step deliberately does not use. taking a code
    // object's local events to zero and setting them again re-enables every
    // location in it, which `restart_events()` does for the whole process — and
    // on a free-threaded build another thread can run that code object between
    // the two calls and miss a breakpoint. measured so that the choice is a
    // choice rather than an assumption
    let seen = bpd_test::eval(
        interpreter(),
        "import json, sys\n\
         mon = sys.monitoring\n\
         mon.use_tool_id(0, 'probe')\n\
         seen = []\n\
         def target(n):\n\
         \x20   a = n + 1\n\
         \x20   b = a + 1\n\
         \x20   return b\n\
         def on_line(code, line):\n\
         \x20   if code is target.__code__:\n\
         \x20       seen.append(line)\n\
         \x20   return mon.DISABLE\n\
         mon.register_callback(0, mon.events.LINE, on_line)\n\
         mon.set_local_events(0, target.__code__, mon.events.LINE)\n\
         target(1)\n\
         first = len(seen)\n\
         seen.clear()\n\
         mon.set_local_events(0, target.__code__, mon.events.LINE)\n\
         target(1)\n\
         same = len(seen)\n\
         seen.clear()\n\
         mon.set_local_events(0, target.__code__, 0)\n\
         mon.set_local_events(0, target.__code__, mon.events.LINE)\n\
         target(1)\n\
         cleared = len(seen)\n\
         mon.set_local_events(0, target.__code__, 0)\n\
         print(json.dumps([first, same, cleared]))\n",
    );

    let passes: Vec<usize> =
        serde_json::from_str(&seen).expect("the ground truth snippet prints a json list");
    let [first, same, cleared] = passes.as_slice() else {
        panic!("the ground truth snippet prints exactly three counts, and got {passes:?}")
    };

    assert!(*first > 0, "the first pass reports every line");
    assert_eq!(
        *same, 0,
        "setting the same local mask again does not undo a `DISABLE`"
    );
    assert_eq!(
        cleared, first,
        "taking the local events to zero and setting them again does"
    );
}
