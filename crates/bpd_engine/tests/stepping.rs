//! a step lands where it says it landed, on the thread it was asked about
//!
//! "it stepped" is as easy to claim wrongly as "it stopped", and the ways of
//! getting it wrong all look right in a single-call test: a step that followed
//! a *code object* rather than a frame steps into a recursive call and into the
//! second generator built from the same function, and a step that treated a
//! `yield` as a return lands in the event loop
//!
//! so nothing here takes the agent's word for where the program got to. the
//! fixtures write a marker file from inside the calls that a step is or is not
//! supposed to enter, and every landing is checked against which of them exist

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use bpd_core::python::Capabilities;
use bpd_core::{
    Binding, Detail, Evaluated, LogRecord, Running, SourceBreakpoint, StepKind, Stop, StopReason,
};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// how long a test waits for a side effect it expects to happen
const PATIENCE: Duration = Duration::from_secs(30);

/// one function calling another, a recursion, two generators of the same
/// function, a coroutine awaited twice, a raise caught by the caller, and an
/// inlined comprehension
///
/// every call a step might enter writes a marker as it goes, so a test can say
/// whether the program really went in there rather than asking the debugger
const PROGRAM: &str = r#"import asyncio
import pathlib

HERE = str(pathlib.Path(__file__).parent)


def note(name):
    with open(HERE + "/" + name, "w") as handle:
        handle.write("x")


def helper(value):
    inside = value * 2
    note("helper_ran")
    return inside


def recurse(n):
    if n == 0:
        return 0
    deeper = recurse(n - 1)
    return deeper + n


def counter(tag):
    step_one = tag
    yield step_one
    step_two = tag
    yield step_two


def two_generators():
    left = counter("left")
    right = counter("right")
    started = next(left)
    other = next(right)
    resumed = next(left)
    return started, other, resumed


def raiser():
    note("raiser_ran")
    raise ValueError("from raiser")


def catching():
    try:
        raiser()
    except ValueError:
        caught = 1
    return caught


async def leaf(tag):
    marker = tag
    await asyncio.sleep(0)
    return marker


async def both():
    first = await leaf("first")
    second = await leaf("second")
    return first, second


def squares(items):
    squared = [
        item * item
        for item in items
    ]
    return squared


def main():
    stop_here = 1
    doubled = helper(4)
    after_helper = doubled + 1
    recursed = recurse(2)
    generated = two_generators()
    caught = catching()
    awaited = asyncio.run(both())
    computed = squares([1, 2])
    note("main_finished")
    return after_helper, recursed, generated, caught, awaited, computed


started = 1
main()
"#;

/// a function reached twice, so the second visit finds its own lines disabled
///
/// the first pass over it returns `DISABLE` for every line that is not the
/// breakpoint, and PEP 669 has no per-location undo. a step that did not
/// restart the process's events would never be offered `middle` again
const TWICE: &str = r"def twice(n):
    entered = n
    middle = entered + 1
    tail = middle + 1
    return tail


twice(1)
twice(2)
";

/// two threads inside the **same** function, so a step armed on one of them is
/// offered lines the other is running
const SHARED: &str = r#"import pathlib, threading, time

HERE = pathlib.Path(__file__).parent


def touch(name):
    (HERE / name).write_text("x")


def announce(name):
    (HERE / ("ident_" + name)).write_text(str(threading.get_ident()))


def wait_for(name):
    path = HERE / name
    deadline = time.monotonic() + 120
    while not path.exists():
        if time.monotonic() > deadline:
            raise SystemExit("the test never created " + name)
        time.sleep(0.002)


def gate(tag):
    if tag == "main":
        touch("gating")
        wait_for("go")
    return tag


def shared(tag):
    a = tag
    b = gate(tag)
    c = b
    return c


def spin():
    announce("spin")
    touch("spin_started")
    while not (HERE / "stop").exists():
        shared("worker")
        if (HERE / "gating").exists():
            touch("spun_while_gated")
    touch("spin_done")


announce("main")
thread = threading.Thread(target=spin)
thread.start()
wait_for("spin_started")
result = shared("main")
touch("finished")
thread.join()
"#;

/// a program that does nothing but go round a loop until a file appears
const SPINNING: &str = r#"import pathlib, threading

HERE = pathlib.Path(__file__).parent
(HERE / "ident_main").write_text(str(threading.get_ident()))
(HERE / "running").write_text("x")
while not (HERE / "stop").exists():
    spun = 1
(HERE / "finished").write_text("x")
"#;

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// no test here sets a logpoint, so a record would be one the agent invented
#[expect(
    clippy::needless_pass_by_value,
    reason = "it stands in for a `FnMut(LogRecord)` sink, which is handed the \
              record to own"
)]
fn unlogged(record: LogRecord) {
    panic!("no logpoint was set, and the agent sent {record:?}")
}

fn launch(fixture: &Fixture) -> Debuggee {
    match bpd_engine::launch(interpreter(), &fixture.path(), &[] as &[OsString]) {
        Ok(Launched::Stopped(debuggee)) => debuggee,
        Ok(Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

/// stop the program on `line`, and take the breakpoint back off again
///
/// every step test starts from a stop and then wants nothing else to interfere:
/// a breakpoint left set would fire again inside the call a step runs, and the
/// test would be measuring the breakpoint rather than the step
fn held_at(debuggee: &mut Debuggee, file: &Path, line: u32) -> Stop {
    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, file, line)])
        .expect("the breakpoint request was answered");
    match &resolved[0].binding {
        Binding::Bound { line: bound, .. } => assert_eq!(
            *bound, line,
            "the fixture line has to be executable, or the test is about a \
             different line than it says"
        ),
        Binding::Unbound { reason } => panic!("the breakpoint did not bind: {reason}"),
    }

    let stop = match debuggee.run(unlogged).expect("the debuggee was resumed") {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected a breakpoint stop, got {other:?}"),
    };
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    stop
}

/// step the only held thread and require that it landed
fn stepped(debuggee: &mut Debuggee, kind: StepKind) -> Stop {
    debuggee.the_step(kind).expect("the thread was stepped");
    match debuggee.wait(unlogged).expect("the debuggee was waited on") {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected a {kind} to land, got {other:?}"),
    }
}

/// where a step landed, or what the stop turned out to be instead
fn landing(stop: &Stop) -> (StepKind, &str, u32) {
    match &stop.reason {
        StopReason::Stepped { kind, file, line } => (*kind, file.as_str(), *line),
        other => panic!("expected a step to have landed, got {other:?}"),
    }
}

/// `co_qualname` of the frame the held thread is in
fn function(debuggee: &mut Debuggee) -> String {
    let stack = debuggee.the_stack(Some(1)).expect("the stack was answered");
    stack.frames[0].function.clone()
}

/// what an expression is worth in the frame that is held
fn value_of(debuggee: &mut Debuggee, expression: &str) -> String {
    let frame = debuggee
        .the_stack(Some(1))
        .expect("the stack was answered")
        .frames[0]
        .id;
    match debuggee
        .evaluate(frame, expression, Detail::default())
        .expect("the expression was evaluated")
    {
        Evaluated::Value { value } => format!("{:?}", value.content),
        Evaluated::Raised { error } => panic!("`{expression}` raised {error}"),
    }
}

/// resume everything and require that the program finishes successfully
fn to_exit(debuggee: &mut Debuggee) {
    match debuggee.run(unlogged).expect("the debuggee was resumed") {
        Running::Exited { status, .. } => {
            assert!(status.success(), "the program exited with {status}");
        }
        other => panic!("expected the program to finish, got {other:?}"),
    }
}

fn tell(fixture: &Fixture, name: &str) {
    std::fs::write(fixture.directory().join(name), "x")
        .unwrap_or_else(|error| panic!("could not write {name}: {error}"));
}

fn ran(fixture: &Fixture, name: &str) -> bool {
    fixture.directory().join(name).exists()
}

/// wait until the program produces a side effect, or say what never happened
fn expect(fixture: &Fixture, name: &str) {
    let deadline = Instant::now() + PATIENCE;
    while !ran(fixture, name) {
        assert!(
            Instant::now() < deadline,
            "the program never wrote `{name}`, so whatever was supposed to \
             produce it was not running"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// the identity a thread of the program announced for itself
fn ident(fixture: &Fixture, name: &str) -> u64 {
    let path = fixture.directory().join(format!("ident_{name}"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("`{}` was never written: {error}", path.display()))
        .trim()
        .parse()
        .expect("a thread identity is a number")
}

#[test]
fn a_step_over_a_call_runs_it_and_lands_on_the_next_line() {
    let fixture = Fixture::new("program", PROGRAM);
    let calling = line_of(PROGRAM, "doubled = helper(4)");
    let next = line_of(PROGRAM, "after_helper = doubled + 1");

    let mut debuggee = launch(&fixture);
    held_at(&mut debuggee, &fixture.path(), calling);
    assert!(
        !ran(&fixture, "helper_ran"),
        "the program is held before the call, so the call has not happened"
    );

    let stop = stepped(&mut debuggee, StepKind::Over);
    let (kind, file, line) = landing(&stop);
    assert_eq!(kind, StepKind::Over);
    assert_eq!(Path::new(file), fixture.path());
    assert_eq!(line, next);
    assert_eq!(function(&mut debuggee), "main");

    // the whole difference between over and in: the call was run, and the
    // program is not inside it
    assert!(
        ran(&fixture, "helper_ran"),
        "a step over runs the call it steps over"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_step_in_stops_on_the_first_line_of_the_call() {
    let fixture = Fixture::new("program", PROGRAM);
    let calling = line_of(PROGRAM, "doubled = helper(4)");
    let first = line_of(PROGRAM, "inside = value * 2");

    let mut debuggee = launch(&fixture);
    held_at(&mut debuggee, &fixture.path(), calling);

    let stop = stepped(&mut debuggee, StepKind::In);
    let (kind, _, line) = landing(&stop);
    assert_eq!(kind, StepKind::In);
    assert_eq!(line, first);
    assert_eq!(function(&mut debuggee), "helper");

    // the proof that it stopped at the *first* line of the callee rather than
    // somewhere inside it: the marker on the line below has not been written
    assert!(
        !ran(&fixture, "helper_ran"),
        "a step in stops before the callee has run anything"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_step_in_on_a_line_that_calls_nothing_is_a_step_over() {
    let fixture = Fixture::new("program", PROGRAM);
    let plain = line_of(PROGRAM, "stop_here = 1");
    let next = line_of(PROGRAM, "doubled = helper(4)");

    let mut debuggee = launch(&fixture);
    held_at(&mut debuggee, &fixture.path(), plain);

    let stop = stepped(&mut debuggee, StepKind::In);
    assert_eq!(landing(&stop).2, next);
    assert_eq!(function(&mut debuggee), "main");

    to_exit(&mut debuggee);
}

#[test]
fn a_step_out_finishes_the_frame_and_lands_in_the_caller() {
    let fixture = Fixture::new("program", PROGRAM);
    let inside = line_of(PROGRAM, "inside = value * 2");
    let after = line_of(PROGRAM, "after_helper = doubled + 1");

    let mut debuggee = launch(&fixture);
    held_at(&mut debuggee, &fixture.path(), inside);
    assert!(!ran(&fixture, "helper_ran"));

    let stop = stepped(&mut debuggee, StepKind::Out);
    let (kind, _, line) = landing(&stop);
    assert_eq!(kind, StepKind::Out);
    assert_eq!(line, after);
    assert_eq!(function(&mut debuggee), "main");

    // out means the frame was finished, not abandoned: the rest of it ran
    assert!(
        ran(&fixture, "helper_ran"),
        "a step out runs the frame it steps out of to its end"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_step_over_a_recursive_call_stays_in_the_frame_it_was_made_in() {
    let fixture = Fixture::new("program", PROGRAM);
    let call = line_of(PROGRAM, "deeper = recurse(n - 1)");
    let next = line_of(PROGRAM, "return deeper + n");

    let mut debuggee = launch(&fixture);
    held_at(&mut debuggee, &fixture.path(), call);
    assert_eq!(
        value_of(&mut debuggee, "n"),
        value_of(&mut debuggee, "2"),
        "the first hit is the outermost call"
    );

    // the recursion re-enters the same code object, so every line of it is a
    // line the step is watching. a step that followed the code object rather
    // than the frame lands on `if n == 0` one level down
    let stop = stepped(&mut debuggee, StepKind::Over);
    assert_eq!(landing(&stop).2, next);
    assert_eq!(
        value_of(&mut debuggee, "n"),
        value_of(&mut debuggee, "2"),
        "it landed in the frame the step was made in, not in a deeper one"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_step_over_a_yield_lands_on_the_next_line_of_the_same_generator() {
    let fixture = Fixture::new("program", PROGRAM);
    let first = line_of(PROGRAM, "step_one = tag");
    let yielding = line_of(PROGRAM, "yield step_one");
    let second = line_of(PROGRAM, "step_two = tag");

    let mut debuggee = launch(&fixture);
    held_at(&mut debuggee, &fixture.path(), first);
    assert_eq!(
        value_of(&mut debuggee, "tag"),
        value_of(&mut debuggee, "'left'"),
        "the first generator to run is the one built first"
    );

    assert_eq!(landing(&stepped(&mut debuggee, StepKind::Over)).2, yielding);

    // the generator is suspended here, the consumer runs on, and a **second**
    // generator of the same function runs both of its first two lines before
    // this one is resumed. a yield is a suspension rather than a return, so the
    // step follows this frame across it
    let stop = stepped(&mut debuggee, StepKind::Over);
    assert_eq!(landing(&stop).2, second);
    assert_eq!(
        value_of(&mut debuggee, "tag"),
        value_of(&mut debuggee, "'left'"),
        "it landed in the generator it was stepping in, not in the other one"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_coroutine_awaited_from_two_places_steps_into_the_right_one() {
    let fixture = Fixture::new("program", PROGRAM);
    let awaiting = line_of(PROGRAM, "second = await leaf(\"second\")");
    let first = line_of(PROGRAM, "marker = tag");

    let mut debuggee = launch(&fixture);
    held_at(&mut debuggee, &fixture.path(), awaiting);

    // the first `leaf` frame has been freed by now, and cpython hands its
    // address straight back to this one — which is why the step holds the frame
    // rather than its address
    let stop = stepped(&mut debuggee, StepKind::In);
    assert_eq!(landing(&stop).2, first);
    assert_eq!(function(&mut debuggee), "leaf");
    assert_eq!(
        value_of(&mut debuggee, "tag"),
        value_of(&mut debuggee, "'second'"),
        "the step went into the coroutine the line it was on awaited"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_step_over_a_call_that_raises_lands_where_the_caller_catches_it() {
    let fixture = Fixture::new("program", PROGRAM);
    let calling = line_of(PROGRAM, "        raiser()");
    // the `except` line, not the body of it: matching the handler is executable
    // and has a line of its own, so it is the first line this frame reaches
    // after the call it made raised
    let handler = line_of(PROGRAM, "    except ValueError:");

    let mut debuggee = launch(&fixture);
    held_at(&mut debuggee, &fixture.path(), calling);
    assert!(!ran(&fixture, "raiser_ran"));

    // the callee is left by an exception rather than by a return, and this
    // frame is not left at all — it catches it. the step lands on the first
    // line the frame reaches afterwards, which is the handler
    let stop = stepped(&mut debuggee, StepKind::Over);
    assert_eq!(landing(&stop).2, handler);
    assert_eq!(function(&mut debuggee), "catching");
    assert!(ran(&fixture, "raiser_ran"), "the call really was made");

    to_exit(&mut debuggee);
}

#[test]
fn stepping_through_an_inlined_comprehension_stays_in_the_enclosing_function() {
    let fixture = Fixture::new("program", PROGRAM);
    let opening = line_of(PROGRAM, "squared = [");

    let mut debuggee = launch(&fixture);
    held_at(&mut debuggee, &fixture.path(), opening);

    // list, dict and set comprehensions have been inlined into the function
    // that contains them since cpython 3.12 and PEP 709, so there is no code
    // object of the comprehension's to step into and no `<listcomp>` frame to
    // land in. stepping through one walks the enclosing function's own lines
    let mut walked = Vec::new();
    loop {
        let stop = stepped(&mut debuggee, StepKind::Over);
        let held = function(&mut debuggee);
        if held != "squares" {
            assert_eq!(held, "main", "it left `squares` by returning to `main`");
            break;
        }
        walked.push(landing(&stop).2);
        assert!(
            walked.len() < 20,
            "a comprehension over two items does not take twenty steps: {walked:?}"
        );
    }

    assert!(
        walked.len() > 1,
        "the comprehension has lines of its own to step through, and got {walked:?}"
    );
    to_exit(&mut debuggee);
}

#[test]
fn a_step_is_offered_a_line_an_earlier_pass_disabled() {
    let fixture = Fixture::new("twice", TWICE);
    let entry = line_of(TWICE, "entered = n");
    let middle = line_of(TWICE, "middle = entered + 1");

    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), entry)])
        .expect("the breakpoint request was answered");
    assert!(matches!(resolved[0].binding, Binding::Bound { .. }));

    // the first call runs `middle`, `tail` and `return` with a breakpoint armed
    // in this code object, so each of them tells the interpreter never to offer
    // it again. the second call is where a step has to be offered `middle`
    // anyway, which needs the process-wide restart because PEP 669 has no
    // per-location undo
    for expected in [1, 2] {
        match debuggee.run(unlogged).expect("the debuggee was resumed") {
            Running::Stopped { stop, .. } => match &stop.reason {
                StopReason::Breakpoint { line, .. } => assert_eq!(*line, entry),
                other => panic!("expected a breakpoint stop, got {other:?}"),
            },
            other => panic!("expected call {expected} to stop, got {other:?}"),
        }
        if expected == 1 {
            continue;
        }

        // the breakpoint set is deliberately left alone. changing it restarts
        // the process's events on its own, which would undo the disables this
        // test is about and leave it proving nothing
        let stop = stepped(&mut debuggee, StepKind::Over);
        assert_eq!(
            landing(&stop).2,
            middle,
            "the line after the breakpoint was disabled by the first call, and \
             a step that was never offered it lands in the caller instead"
        );
        assert_eq!(function(&mut debuggee), "twice");
    }

    to_exit(&mut debuggee);
}

#[test]
fn a_breakpoint_reached_while_stepping_is_reported_as_a_breakpoint() {
    let fixture = Fixture::new("program", PROGRAM);
    let calling = line_of(PROGRAM, "doubled = helper(4)");
    let inside = line_of(PROGRAM, "inside = value * 2");

    let mut debuggee = launch(&fixture);
    held_at(&mut debuggee, &fixture.path(), calling);

    // a breakpoint inside the call a step is stepping over. it is a stop the
    // client asked for, and reporting it as the step landing would be a
    // breakpoint the client never saw fire
    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(7, fixture.path(), inside)])
        .expect("the breakpoint request was answered");

    debuggee
        .the_step(StepKind::Over)
        .expect("the thread was stepped");
    let stop = match debuggee.wait(unlogged).expect("the debuggee was waited on") {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected the breakpoint to stop it, got {other:?}"),
    };
    match &stop.reason {
        StopReason::Breakpoint {
            breakpoints, line, ..
        } => {
            assert_eq!(breakpoints, &[7]);
            assert_eq!(*line, inside);
        }
        other => panic!("expected a breakpoint stop, got {other:?}"),
    }

    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    to_exit(&mut debuggee);
}

#[test]
fn a_step_out_of_the_outermost_frame_lets_the_program_finish() {
    let fixture = Fixture::new("program", PROGRAM);
    let before = line_of(PROGRAM, "started = 1");

    let mut debuggee = launch(&fixture);
    held_at(&mut debuggee, &fixture.path(), before);
    // the last statement of the module, reached by a step of its own so that
    // the next one has nowhere left to go
    assert_eq!(function(&mut debuggee), "<module>");
    stepped(&mut debuggee, StepKind::Over);

    // there is no frame above the program's own module that bpd would report —
    // the one above it is the `-c` the interpreter was entered through — so the
    // step has nowhere to land. it is given up, and what the program did is
    // what the client is told
    debuggee
        .the_step(StepKind::Over)
        .expect("the thread was stepped");
    match debuggee.wait(unlogged).expect("the debuggee was waited on") {
        Running::Exited { status, .. } => assert!(status.success()),
        other => panic!("expected the program to finish, got {other:?}"),
    }
    assert!(ran(&fixture, "main_finished"));
}

#[test]
fn stepping_one_thread_does_not_step_or_stop_another() {
    let fixture = Fixture::new("shared", SHARED);
    let first = line_of(SHARED, "a = tag");
    let second = line_of(SHARED, "b = gate(tag)");

    let mut debuggee = launch(&fixture);
    // both threads run this function. the condition is what makes the stop the
    // main thread's, and the spinner goes on calling it throughout
    let resolved = debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), first).when("tag == 'main'"),
        ])
        .expect("the breakpoint request was answered");
    assert!(matches!(resolved[0].binding, Binding::Bound { .. }));

    let stop = match debuggee.run(unlogged).expect("the debuggee was resumed") {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected the main thread to stop, got {other:?}"),
    };
    assert_eq!(stop.thread, ident(&fixture, "main"));
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");

    // the spinner is running the very lines this step is watching. a step that
    // did not belong to a thread would land on whichever thread got there first
    let landed = stepped(&mut debuggee, StepKind::Over);
    assert_eq!(landed.thread, stop.thread, "the step is the main thread's");
    assert_eq!(landing(&landed).2, second);
    assert_eq!(
        value_of(&mut debuggee, "tag"),
        value_of(&mut debuggee, "'main'")
    );
    assert_eq!(
        debuggee.held().len(),
        1,
        "the spinner was never held, and got {:?}",
        debuggee.held()
    );

    // and it really was running the whole time, which is a file it wrote rather
    // than anything the agent said
    tell(&fixture, "go");
    tell(&fixture, "stop");
    expect(&fixture, "spin_done");
    assert!(
        !ran(&fixture, "finished"),
        "the main thread is held one line past its breakpoint"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_step_is_offered_a_line_another_thread_would_have_disabled() {
    let fixture = Fixture::new("shared", SHARED);
    let first = line_of(SHARED, "a = tag");
    let calling = line_of(SHARED, "b = gate(tag)");
    let after = line_of(SHARED, "c = b");

    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), first).when("tag == 'main'"),
        ])
        .expect("the breakpoint request was answered");
    assert!(matches!(resolved[0].binding, Binding::Bound { .. }));
    match debuggee.run(unlogged).expect("the debuggee was resumed") {
        Running::Stopped { .. } => {}
        other => panic!("expected the main thread to stop, got {other:?}"),
    }
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");

    assert_eq!(landing(&stepped(&mut debuggee, StepKind::Over)).2, calling);

    // this step is held open for as long as the test likes: the call it steps
    // over waits for a file. `DISABLE` is process wide, so the other thread
    // running the very same function through the whole of it would tell the
    // interpreter never to offer the line this step is waiting for
    debuggee
        .the_step(StepKind::Over)
        .expect("the thread was stepped");
    expect(&fixture, "gating");
    expect(&fixture, "spun_while_gated");
    tell(&fixture, "go");

    let stop = match debuggee.wait(unlogged).expect("the debuggee was waited on") {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected the step to land, got {other:?}"),
    };
    assert_eq!(landing(&stop).2, after);
    assert_eq!(function(&mut debuggee), "shared");

    tell(&fixture, "stop");
    to_exit(&mut debuggee);
}

#[test]
fn the_interpreter_hands_a_freed_frames_address_to_the_next_one() {
    // why a step holds the frame it is following rather than its address.
    // measured in a **bare** interpreter, because it is a statement about
    // cpython: two coroutines awaited one after another from the same function
    // get the same frame object address, so a step comparing addresses would
    // read the second as the first and land in the wrong instance of it
    let seen = bpd_test::eval(
        interpreter(),
        "import asyncio, json, sys\n\
         seen = []\n\
         async def leaf(tag):\n\
         \x20   seen.append(id(sys._getframe()))\n\
         \x20   await asyncio.sleep(0)\n\
         \x20   return tag\n\
         async def both():\n\
         \x20   a = await leaf('first')\n\
         \x20   b = await leaf('second')\n\
         \x20   return a, b\n\
         asyncio.run(both())\n\
         print(json.dumps(seen[0] == seen[1]))\n",
    );

    let reused: bool = serde_json::from_str(&seen).expect("the snippet prints a json boolean");
    assert!(
        reused,
        "the first coroutine's frame was freed and the second was given its \
         address, which is what a step holding only an address would not see"
    );
}

#[test]
fn a_pause_holds_the_first_thread_that_reaches_a_line() {
    let fixture = Fixture::new("spinning", SPINNING);

    let mut debuggee = launch(&fixture);
    debuggee.resume_all().expect("the entry stop was resumed");
    expect(&fixture, "running");

    // the one request made to a program with nothing held. it says which
    // threads were running python, because a program whose every thread is
    // parked in a C call reaches no line and nothing can hold one
    let running = debuggee.pause().expect("the pause was armed");
    assert_eq!(
        running,
        vec![ident(&fixture, "main")],
        "the only thread of this program was going round a python loop"
    );

    let stop = match debuggee.wait(unlogged).expect("the debuggee was waited on") {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected the pause to hold a thread, got {other:?}"),
    };
    assert_eq!(stop.thread, ident(&fixture, "main"));
    assert!(
        matches!(stop.reason, StopReason::Paused { .. }),
        "expected a paused stop, got {:?}",
        stop.reason
    );

    // where inside the loop it was caught belongs to the operating system —
    // the loop asks the filesystem whether a file exists, and every line of
    // `genericpath` is a line a pause can hold. what is not in doubt is whose
    // thread it is and what it is doing, and the outermost frame says both
    let stack = debuggee.the_stack(None).expect("the stack was answered");
    let outermost = stack.frames.last().expect("a held thread has a stack");
    assert_eq!(Path::new(&outermost.file), fixture.path());
    assert_eq!(outermost.function, "<module>");
    assert!(
        !ran(&fixture, "finished"),
        "the thread is held inside the loop"
    );

    tell(&fixture, "stop");
    to_exit(&mut debuggee);
}
