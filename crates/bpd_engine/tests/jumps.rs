//! moving a frame, against a real interpreter
//!
//! the trap this file is written around is the one that makes the feature
//! subtly wrong rather than obviously broken: **no `LINE` event is delivered for
//! the line a jump moves to**. everything about where the program is afterwards
//! is derived from the jump itself, and a debugger that waited to be told would
//! report the line after the one it moved to — or wait forever
//!
//! so nothing here takes the interpreter's word for where the program got to
//! either. the fixtures write a marker file from the lines a jump is supposed to
//! run again and from the ones it is supposed to skip, and the program's own
//! return value is checked against what re-running really does

use std::ffi::OsString;
use std::path::Path;

use bpd_core::python::Capabilities;
use bpd_core::{
    Binding, Detail, Evaluated, FrameId, Jump, Jumped, Refusal, Running, SourceBreakpoint,
    StepKind, Stop, StopReason,
};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// three statements in a row, a loop, and a call — the shapes a jump is made in
///
/// every line that matters appends to `RAN`, which the program prints at the
/// end: what really executed is read off the program rather than off the
/// debugger
const PROGRAM: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent
RAN = []


def note(name):
    (HERE / name).write_text("x")


def straight():
    first = 1
    RAN.append("second")
    third = 3
    return first + third


def looping():
    total = 0
    for item in (1, 2):
        total += item
    return total


def deeper():
    inner = 1
    note("deeper_ran")
    return inner


def calling():
    got = deeper()
    after = got + 1
    return after


def twice(n):
    entered = n
    middle = entered + 1
    tail = middle + 1
    return tail


def growing(value):
    RAN.append(("growing", value))
    value = value + 100
    total = value + 1
    return total


def counter(tag):
    step_one = tag
    RAN.append("yielded")
    yield step_one


def unbound_after():
    early = 1
    later = 2
    last = 3
    return (early, later, last)


straight()
looping()
calling()
twice(1)
twice(2)
grown = growing(1)
list(counter("t"))
unbound_after()
(HERE / "ran.txt").write_text(repr(RAN) + " grown " + repr(grown))
note("finished")
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

/// stop the program on `line`, and take the breakpoint back off again
fn held_at(debuggee: &mut Debuggee, file: &Path, line: u32) -> Stop {
    let stop = stopped_by(debuggee, &[SourceBreakpoint::at(1, file, line)]);
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    stop
}

/// arm a whole breakpoint set and run to the first of them that fires
fn stopped_by(debuggee: &mut Debuggee, breakpoints: &[SourceBreakpoint]) -> Stop {
    let resolved = debuggee
        .set_breakpoints(breakpoints.to_vec())
        .expect("the breakpoint request was answered");
    for (requested, resolved) in breakpoints.iter().zip(&resolved) {
        match &resolved.binding {
            Binding::Bound { line, .. }
            | Binding::BoundInTemplate { line, .. }
            | Binding::BoundInSource { line, .. } => assert_eq!(
                *line, requested.line,
                "the fixture line has to be executable, or the test is about a \
                 different line than it says"
            ),
            Binding::Unbound { reason } => panic!("the breakpoint did not bind: {reason}"),
        }
    }

    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected a breakpoint stop, got {other:?}"),
    }
}

/// the frame the held thread is executing
fn top(debuggee: &mut Debuggee) -> FrameId {
    debuggee
        .the_stack(Some(1))
        .expect("the stack was answered")
        .frames[0]
        .id
}

/// what a jump did, or the refusal that says it did not happen
fn moved(jumped: &Jumped) -> (u32, &Vec<String>, &Vec<u32>) {
    match &jumped.outcome {
        Jump::Moved {
            from,
            bound_to_none,
            unannounced,
        } => (*from, bound_to_none, unannounced),
        Jump::Refused { wanted, error } => {
            panic!("the move to line {wanted} was refused: {error}")
        }
    }
}

/// what an expression is worth in the frame that is held
fn value_of(debuggee: &mut Debuggee, expression: &str) -> String {
    let frame = top(debuggee);
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
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Exited { status, .. } => {
            assert!(status.success(), "the program exited with {status}");
        }
        other => panic!("expected the program to finish, got {other:?}"),
    }
}

fn ran(fixture: &Fixture, name: &str) -> bool {
    fixture.directory().join(name).exists()
}

/// what the program itself recorded about the lines it ran
///
/// read off the program rather than off the debugger, which is the whole point
/// of it: what a jump claims and what really executed are two different
/// statements, and only one of them is evidence
fn recorded(fixture: &Fixture) -> String {
    let path = fixture.directory().join("ran.txt");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("the program never wrote {}: {error}", path.display()))
}

#[test]
fn a_backward_jump_re_executes_the_lines_between_and_says_where_the_frame_is() {
    let fixture = Fixture::new("jumping", PROGRAM);
    let mut debuggee = launch(&fixture);
    let first = line_of(PROGRAM, "first = 1");
    let third = line_of(PROGRAM, "third = 3");

    held_at(&mut debuggee, &fixture.path(), third);
    let frame = top(&mut debuggee);
    let jumped = debuggee
        .set_next_statement(frame, first)
        .expect("the frame was asked to move");

    // where it is now is the frame's own answer, taken after the move. no line
    // event is delivered for the destination, so a debugger that waited to be
    // told would either report `third` or never answer at all
    assert_eq!(jumped.at.line, first, "the answer said {}", jumped.at);
    assert_eq!(jumped.at.function, "straight");
    let (from, _, _) = moved(&jumped);
    assert_eq!(from, third);

    // and the stack agrees, which is the other place a client reads it from
    let stack = debuggee.the_stack(Some(1)).expect("the stack was answered");
    assert_eq!(stack.frames[0].line, first);

    to_exit(&mut debuggee);
    // `second` twice: the block really re-executed rather than the frame merely
    // reporting a different number
    let said = recorded(&fixture);
    assert!(
        said.contains("'second', 'second'"),
        "the lines between the destination and where it was did not run again: \
         {said}"
    );
}

#[test]
fn a_breakpoint_on_the_line_a_jump_moves_to_is_named_as_one_that_will_not_fire() {
    // the whole of finding 4, as a test: the destination executes and no line
    // event is delivered for it, so a breakpoint bound there is passed over
    // exactly once — and a client that was not told would watch its own
    // breakpoint be ignored
    let fixture = Fixture::new("unannounced", PROGRAM);
    let mut debuggee = launch(&fixture);
    let entered = line_of(PROGRAM, "entered = n");
    let middle = line_of(PROGRAM, "middle = entered + 1");

    let file = fixture.path();
    let on_entered = SourceBreakpoint::at(1, &file, entered);
    let on_middle = SourceBreakpoint::at(2, &file, middle);

    // the first call reaches `entered` first, which is breakpoint 1
    let stop = stopped_by(&mut debuggee, &[on_entered.clone(), on_middle]);
    assert!(
        matches!(&stop.reason, StopReason::Breakpoint { breakpoints, .. } if breakpoints == &[1]),
        "expected breakpoint 1 to fire first, got {:?}",
        stop.reason
    );

    // let it reach `middle`, and move back onto the line breakpoint 1 is on
    debuggee.resume_all().expect("the thread was resumed");
    let stop = match debuggee
        .wait(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was waited on")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected breakpoint 2, got {other:?}"),
    };
    assert!(
        matches!(&stop.reason, StopReason::Breakpoint { breakpoints, .. } if breakpoints == &[2]),
        "got {:?}",
        stop.reason
    );

    let frame = top(&mut debuggee);
    let jumped = debuggee
        .set_next_statement(frame, entered)
        .expect("the frame was asked to move");
    let (_, _, unannounced) = moved(&jumped);
    assert_eq!(
        unannounced,
        &vec![1],
        "breakpoint 1 is on the line the frame moved to and will not fire for \
         this pass, and the answer said {jumped:?}"
    );

    // only breakpoint 1 from here, so what stops next says whether the
    // destination was announced
    debuggee
        .set_breakpoints(vec![on_entered])
        .expect("the breakpoint set was replaced");
    debuggee.resume_all().expect("the thread was resumed");
    match debuggee
        .wait(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was waited on")
    {
        Running::Stopped { .. } => {}
        other => panic!("expected the second call to stop, got {other:?}"),
    }

    // the pass the jump landed in ran `entered` without being offered it. the
    // breakpoint is still set, and this is `twice(2)` reaching it
    let value = value_of(&mut debuggee, "n");
    assert!(
        value.contains('2'),
        "the breakpoint fired for the pass the jump moved into rather than for \
         the next call, and `n` was {value}"
    );

    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    to_exit(&mut debuggee);
}

#[test]
fn a_line_cpython_will_not_move_to_is_refused_in_cpythons_own_words() {
    let fixture = Fixture::new("refused", PROGRAM);
    let mut debuggee = launch(&fixture);
    let total = line_of(PROGRAM, "total = 0");
    let inside = line_of(PROGRAM, "total += item");

    held_at(&mut debuggee, &fixture.path(), total);
    let frame = top(&mut debuggee);
    let jumped = debuggee
        .set_next_statement(frame, inside)
        .expect("the request was answered");

    let Jump::Refused { wanted, error } = &jumped.outcome else {
        panic!("jumping into the body of a for loop is not something cpython takes: {jumped:?}")
    };
    assert_eq!(*wanted, inside);
    assert_eq!(error.kind, "ValueError");
    // cpython's reason, intact. rewriting it into something of bpd's would lose
    // the one part a caller can act on
    assert!(
        error
            .message
            .contains("can't jump into the body of a for loop"),
        "the refusal said {error}"
    );
    // and the frame did not move, read back the same way a move is
    assert_eq!(jumped.at.line, total, "the frame moved anyway: {jumped:?}");

    to_exit(&mut debuggee);
}

#[test]
fn a_frame_that_is_not_the_one_the_thread_is_executing_is_refused_with_the_reason() {
    // cpython does **not** refuse this, which is why bpd does. it accepts a
    // move in a frame suspended in a call and the frame then returns something
    // it never computed
    let fixture = Fixture::new("deeper", PROGRAM);
    let mut debuggee = launch(&fixture);
    let inner = line_of(PROGRAM, "note(\"deeper_ran\")");
    let after = line_of(PROGRAM, "after = got + 1");

    held_at(&mut debuggee, &fixture.path(), inner);
    let stack = debuggee.the_stack(None).expect("the stack was answered");
    assert_eq!(stack.frames[0].name(), "deeper");
    assert_eq!(stack.frames[1].name(), "calling");
    let caller = stack.frames[1].id;

    for refused in [
        debuggee
            .set_next_statement(caller, after)
            .expect_err("a frame suspended in a call cannot be moved"),
        debuggee
            .restart_frame(caller)
            .expect_err("a frame suspended in a call cannot be restarted"),
    ] {
        let said = refused.to_string();
        assert!(
            said.contains("frame 1 of stop") && said.contains("frame 0 of stop"),
            "the refusal has to name what was asked about and what can move, \
             and said {said}"
        );
        assert!(
            said.contains("never computed"),
            "the refusal has to say why bpd refuses what cpython accepts, and \
             said {said}"
        );
        assert!(
            said.contains("cannot clear an executing frame"),
            "the refusal has to say why the frames above cannot be discarded \
             instead, and said {said}"
        );
        assert!(matches!(
            refused,
            bpd_engine::Error::Session(bpd_core::Error::Refused {
                reason: Refusal::NotTheExecutingFrame { .. }
            })
        ));
    }

    // and the program is untouched by having been asked
    to_exit(&mut debuggee);
    assert!(ran(&fixture, "deeper_ran"), "the program did not run on");
}

#[test]
fn restarting_a_frame_runs_it_again_with_what_its_parameters_hold_now() {
    // the milestone's hard question, answered by measurement rather than by
    // wording: nothing captured what the call was made with, so a parameter the
    // frame has already assigned to is what the second run sees
    let fixture = Fixture::new("restarting", PROGRAM);
    let mut debuggee = launch(&fixture);
    let total = line_of(PROGRAM, "total = value + 1");

    held_at(&mut debuggee, &fixture.path(), total);
    assert!(
        value_of(&mut debuggee, "value").contains("101"),
        "the fixture reassigns its parameter before the breakpoint, and it had \
         not"
    );

    let frame = top(&mut debuggee);
    let jumped = debuggee
        .restart_frame(frame)
        .expect("the frame was asked to restart");
    let (from, _, _) = moved(&jumped);
    assert_eq!(from, total);
    assert_eq!(
        jumped.at.line,
        line_of(PROGRAM, "def growing(value):"),
        "a restart moves to the first line of the code object, and the answer \
         said {jumped:?}"
    );

    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    // 1 and then 101: the second run is the frame re-entered with what the
    // parameter holds now, and `bpd` says exactly that rather than claiming the
    // arguments were restored
    assert!(
        said.contains("('growing', 1), ('growing', 101)"),
        "the frame did not run again with the parameter it now holds: {said}"
    );
    assert!(
        said.contains("grown 202"),
        "the re-entered frame's own return value has to be the one the program \
         got: {said}"
    );
}

#[test]
fn a_restart_lands_before_the_first_statement_and_a_step_runs_it() {
    // a restart moves to the line of the code object's first instruction, which
    // is the `def`. no line event is delivered for it either, so what proves
    // the frame is really there is a step: it lands on the first statement of
    // the body, which has not run yet
    let fixture = Fixture::new("stepping_after", PROGRAM);
    let mut debuggee = launch(&fixture);
    let third = line_of(PROGRAM, "third = 3");

    held_at(&mut debuggee, &fixture.path(), third);
    let frame = top(&mut debuggee);
    let jumped = debuggee
        .restart_frame(frame)
        .expect("the frame was asked to restart");
    assert_eq!(jumped.at.line, line_of(PROGRAM, "def straight():"));

    debuggee
        .the_step(StepKind::Over)
        .expect("the thread stepped");
    let stop = match debuggee
        .wait(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was waited on")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected the step to land, got {other:?}"),
    };
    let StopReason::Stepped { line, .. } = &stop.reason else {
        panic!("expected a step to have landed, got {:?}", stop.reason)
    };
    assert_eq!(
        *line,
        line_of(PROGRAM, "first = 1"),
        "a step out of a restarted frame lands on the first statement of the \
         body"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_step_after_a_jump_runs_the_destination_and_lands_on_the_line_after_it() {
    let fixture = Fixture::new("stepping_over", PROGRAM);
    let mut debuggee = launch(&fixture);
    let first = line_of(PROGRAM, "first = 1");
    let second = line_of(PROGRAM, "RAN.append(\"second\")");
    let third = line_of(PROGRAM, "third = 3");

    held_at(&mut debuggee, &fixture.path(), third);
    let frame = top(&mut debuggee);
    debuggee
        .set_next_statement(frame, first)
        .expect("the frame was asked to move");

    // a step is armed on the resume, which is after the move — so it steps from
    // where the frame is now. the destination is what it executes
    debuggee
        .the_step(StepKind::Over)
        .expect("the thread stepped");
    let stop = match debuggee
        .wait(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was waited on")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected the step to land, got {other:?}"),
    };
    let StopReason::Stepped { line, .. } = &stop.reason else {
        panic!("expected a step to have landed, got {:?}", stop.reason)
    };
    assert_eq!(
        *line, second,
        "a step after a jump executes the destination and lands on the line \
         after it"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_jump_binds_the_frames_unbound_locals_to_none_and_says_which() {
    // cpython's doing rather than bpd's, and a change to the program that
    // nothing else in the session would ever mention
    let fixture = Fixture::new("unbinding", PROGRAM);
    let mut debuggee = launch(&fixture);
    let early = line_of(PROGRAM, "early = 1");
    let last = line_of(PROGRAM, "last = 3");

    held_at(&mut debuggee, &fixture.path(), last);
    let frame = top(&mut debuggee);
    let jumped = debuggee
        .set_next_statement(frame, early)
        .expect("the frame was asked to move");

    let (_, bound_to_none, _) = moved(&jumped);
    assert_eq!(
        bound_to_none,
        &vec!["last".to_string()],
        "`last` held nothing when the frame moved, and the answer said \
         {jumped:?}"
    );
    // and the claim is checked against the frame rather than against the
    // warning cpython raised
    assert_eq!(value_of(&mut debuggee, "last"), "None");

    to_exit(&mut debuggee);
}

#[test]
fn a_generator_frame_is_refused_a_restart_and_takes_a_jump_to_a_body_line() {
    // the first instruction of a generator's code object is the `RESUME` its
    // driver sends into rather than the top of the body: moving there ends the
    // generator instead of running it again. the refusal names the operation
    // that does work, and this checks that one really does
    let fixture = Fixture::new("generating", PROGRAM);
    let mut debuggee = launch(&fixture);
    let yielded = line_of(PROGRAM, "RAN.append(\"yielded\")");
    let yielding = line_of(PROGRAM, "yield step_one");

    // held on the `yield` itself, so the line the jump goes back to is one that
    // has already run — otherwise re-running it and never having run it look
    // exactly the same in what the program recorded
    held_at(&mut debuggee, &fixture.path(), yielding);
    let frame = top(&mut debuggee);

    let refused = debuggee
        .restart_frame(frame)
        .expect_err("a generator frame cannot be re-entered from the top");
    let said = refused.to_string();
    assert!(said.contains("`counter`"), "said {said}");
    assert!(said.contains("a generator"), "said {said}");
    assert!(
        said.contains("StopIteration"),
        "the refusal has to say what moving there really does, and said {said}"
    );
    assert!(
        said.contains("set the next statement"),
        "the refusal has to name the operation that works here, and said {said}"
    );

    // and it does work: a body line is an ordinary destination in a generator
    let jumped = debuggee
        .set_next_statement(frame, yielded)
        .expect("a body line is a place a generator frame can be moved to");
    assert_eq!(jumped.at.line, yielded, "{jumped:?}");

    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    assert!(
        said.contains("'yielded', 'yielded'"),
        "the generator did not run the line again: {said}"
    );
}

#[test]
fn the_entry_stop_cannot_be_moved_and_cpython_says_why() {
    // bpd reports the entry stop from `PY_START`, where cpython refuses a move
    // outright — a frame that has not begun has no line to move from. the
    // refusal is cpython's and it is passed through rather than pre-emptied
    let fixture = Fixture::new("at_entry", PROGRAM);
    let mut debuggee = launch(&fixture);
    let stop = debuggee
        .held()
        .first()
        .expect("the entry stop is held")
        .stop;
    assert!(matches!(stop, 1));

    let frame = top(&mut debuggee);
    let jumped = debuggee
        .restart_frame(frame)
        .expect("the request was answered");
    let Jump::Refused { error, .. } = &jumped.outcome else {
        panic!("a frame that has not begun has no line to move from: {jumped:?}")
    };
    assert_eq!(error.kind, "ValueError");
    assert!(
        error.message.contains("new frame") || error.message.contains("'line' trace event"),
        "the refusal said {error}"
    );

    to_exit(&mut debuggee);
    assert!(ran(&fixture, "finished"), "the program did not finish");
}
