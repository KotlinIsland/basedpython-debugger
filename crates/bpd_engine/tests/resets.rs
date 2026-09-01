//! running a frame again where it stands, against a real interpreter
//!
//! the two shapes this exists for are the ones the rewinding restart cannot
//! reach at all: a call whose argument is another call, and a call whose
//! argument is a property. the rewinding mechanism re-executes the caller's
//! whole line, so both would run a second time — and refusing them is what
//! `Unrestartable::SomethingElseOnTheLine` and `MoreThanOneCall` are
//!
//! nothing here takes bpd's word for it. the program appends to `RAN` and writes
//! it out, and every assertion is on **what the program computed**: `f2` running
//! once is the claim, not bpd saying it did

use std::ffi::OsString;
use std::path::Path;

use bpd_core::python::Capabilities;
use bpd_core::{Again, Binding, FrameId, Restarted, Running, SourceBreakpoint, Stop, StopReason};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// the two argument shapes, and a frame whose locals say whether it started over
///
/// `target` records what it was called with, binds one local, leaves another
/// unbound on every path, and then reports which — so "it ran again" and "it ran
/// again as if freshly called" are different entries rather than one
const PROGRAM: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent
RAN = []


def f2():
    RAN.append("f2")
    return "R2"


class Watched:
    @property
    def attr(self):
        RAN.append("getter")
        return "A"


WATCHED = Watched()


def target(arg):
    RAN.append(("entered", arg))
    seen = "first pass"
    if arg == "never happens":
        cond = 1
    try:
        cond
    except UnboundLocalError:
        RAN.append(("cond unbound", seen))
    else:
        RAN.append(("cond bound", cond))
    marker = 1
    return "done"


class Opened:
    def __enter__(self):
        RAN.append("enter")
        return self

    def __exit__(self, *rest):
        RAN.append("exit")
        return False


def blocked(arg):
    with Opened():
        held = arg
        stopper = 1
        return held


def blocked_call():
    return blocked("B")


def deepest(v):
    RAN.append(("deepest", v))
    bottom = 1
    return "D"


def middle(v):
    RAN.append(("middle", v))
    inner = deepest(v)
    settled = inner
    return "M"


def outer(v):
    RAN.append(("outer", v))
    passed = middle(v)
    after = passed
    return "O"


class Latch:
    def __enter__(self):
        RAN.append("latch enter")
        return self

    def __exit__(self, *rest):
        RAN.append("latch exit")
        return False


def held_middle(v):
    RAN.append(("held middle", v))
    with Latch():
        inner = deepest(v)
        settled = inner
    return "HM"


def held_outer(v):
    RAN.append(("held outer", v))
    got = held_middle(v)
    after = got
    return "HO"


MARK = None


def noisy_middle(v):
    global MARK
    RAN.append(("noisy middle", v))
    MARK = deepest(v)
    settled = MARK
    return "NM"


def noisy_outer(v):
    RAN.append(("noisy outer", v))
    got = noisy_middle(v)
    after = got
    return "NO"


def tail_middle(v):
    deepest(v)  # nothing follows the call, so this frame returns when it does


def tail_outer(v):
    RAN.append(("tail outer", v))
    got = tail_middle(v)
    after = got
    return "TO"


def nested_call():
    return target(f2())


def attribute_call():
    return target(WATCHED.attr)


def main():
    nested_call()
    attribute_call()
    blocked_call()
    outer("N")
    noisy_outer("Q")
    held_outer("H")
    tail_outer("T")
    (HERE / "ran.txt").write_text(repr(RAN))


main()
"#;

#[test]
fn the_inner_call_does_not_run_again() {
    let fixture = Fixture::new("reset_nested", PROGRAM);
    let mut debuggee = launch(&fixture);
    reset_target_called_from(&mut debuggee, &fixture, "    return target(f2())");
    to_exit(&mut debuggee);

    let ran = recorded(&fixture);
    assert_eq!(
        ran.matches("'f2'").count(),
        1,
        "`f2` produced the argument once and the restart must not have re-run \
         it — the whole point of resetting the frame rather than the caller's \
         line: {ran}"
    );
    assert_eq!(
        ran.matches("('entered', 'R2')").count(),
        2,
        "`target` ran twice, both times with the argument the one call to `f2` \
         produced: {ran}"
    );
}

#[test]
fn the_property_does_not_run_again() {
    let fixture = Fixture::new("reset_attribute", PROGRAM);
    let mut debuggee = launch(&fixture);
    reset_target_called_from(&mut debuggee, &fixture, "    return target(WATCHED.attr)");
    to_exit(&mut debuggee);

    let ran = recorded(&fixture);
    assert_eq!(
        ran.matches("'getter'").count(),
        1,
        "the property's getter ran once. re-executing the caller's line would \
         have run it again, which is why that mechanism refuses this shape: {ran}"
    );
    assert_eq!(
        ran.matches("('entered', 'A')").count(),
        2,
        "`target` ran twice with the value the one getter call produced: {ran}"
    );
}

#[test]
fn the_frame_starts_over_with_its_locals_unbound() {
    let fixture = Fixture::new("reset_locals", PROGRAM);
    let mut debuggee = launch(&fixture);
    let reset = reset_target_called_from(&mut debuggee, &fixture, "    return target(f2())");
    assert_eq!(
        reset.emptied,
        vec!["seen".to_string(), "cond".to_string(), "marker".to_string()],
        "every local that is not a parameter is put back to unbound"
    );
    assert_eq!(
        reset.kept,
        vec!["arg".to_string()],
        "the parameter is kept, because the frame is the only place what the \
         call passed still exists"
    );
    to_exit(&mut debuggee);

    let ran = recorded(&fixture);
    // counted against `entered` rather than against a number written here:
    // `target` is called from both shapes and restarted in one of them, so the
    // figure is three, and a test that hard-codes three stops meaning anything
    // if the fixture gains a caller
    assert_eq!(
        ran.matches("cond unbound").count(),
        ran.matches("entered").count(),
        "`cond` was unbound on **every** pass, the restarted one included. a jump \
         alone binds every unbound local to `None`, and a frame reading `None` \
         where a real call raises `UnboundLocalError` is the wrong answer this \
         mechanism exists to avoid: {ran}"
    );
    assert!(
        !ran.contains("cond bound"),
        "`cond` was never bound on either pass: {ran}"
    );
}

#[test]
fn a_frame_inside_a_with_says_its_cleanup_did_not_run() {
    let fixture = Fixture::new("reset_blocked", PROGRAM);
    let mut debuggee = launch(&fixture);
    let inside = line_of(PROGRAM, "        stopper = 1");
    held_at(&mut debuggee, &fixture.path(), inside);
    let frame = top(&mut debuggee);
    let reset = match debuggee
        .restart_frame(frame, Again::InPlace)
        .expect("the restart request was answered")
    {
        Restarted::Reset(reset) => reset,
        other => panic!("expected the frame to be reset in place, got {other:?}"),
    };
    assert!(
        reset.inside_a_block,
        "the frame was inside a `with`, and a reset that did not say so would \
         leave the user to discover an unclosed context manager"
    );
    to_exit(&mut debuggee);

    let ran = recorded(&fixture);
    // **this is the claim, and it is a cost rather than a feature.** the jump
    // pops the stack and closes what it pops, which is not running `__exit__` —
    // so the block is entered twice and left once, and `reset.inside_a_block` is
    // what tells the user before they find the open context manager themselves
    assert_eq!(
        ran.matches("'enter'").count(),
        2,
        "the body re-entered the `with` from the top: {ran}"
    );
    assert_eq!(
        ran.matches("'exit'").count(),
        1,
        "the first pass's `__exit__` never ran, which is what `inside_a_block` \
         reports: {ran}"
    );
}

/// hold the program inside `target`, called from the line named, and reset it
///
/// `target` is called from both shapes, so the breakpoint alone does not say
/// which caller this is about — the program is run on until the frame below is
/// at the line the case names
fn reset_target_called_from(
    debuggee: &mut Debuggee,
    fixture: &Fixture,
    caller_line: &str,
) -> bpd_core::Reset {
    let inside = line_of(PROGRAM, "    marker = 1");
    let call = line_of(PROGRAM, caller_line);
    loop {
        held_at(debuggee, &fixture.path(), inside);
        let stack = debuggee.the_stack(None).expect("the stack was answered");
        assert_eq!(stack.frames[0].name(), "target");
        if stack.frames[1].line == call {
            break;
        }
    }
    let frame = top(debuggee);
    let restarted = debuggee
        .restart_frame(frame, Again::InPlace)
        .expect("the restart request was answered");
    match restarted {
        Restarted::Reset(reset) => reset,
        other => panic!("expected the frame to be reset in place, got {other:?}"),
    }
}

#[test]
fn a_frame_two_deep_is_reset_after_the_frames_above_it_are_forced_out() {
    let fixture = Fixture::new("reset_deep", PROGRAM);
    let mut debuggee = launch(&fixture);
    // held in `deepest`, which is two frames above `outer`
    let inside = line_of(PROGRAM, "    bottom = 1");
    held_at(&mut debuggee, &fixture.path(), inside);
    let stack = debuggee.the_stack(None).expect("the stack was answered");
    assert_eq!(stack.frames[0].name(), "deepest");
    assert_eq!(stack.frames[1].name(), "middle");
    assert_eq!(stack.frames[2].name(), "outer");

    let unwinding = match debuggee
        .restart_frame(stack.frames[2].id, Again::InPlace)
        .expect("the restart request was answered")
    {
        Restarted::Unwinding(unwinding) => unwinding,
        other => panic!("expected an unwind to `outer`, got {other:?}"),
    };
    assert!(
        unwinding.above.iter().all(|frame| !frame.inside_a_block),
        "neither frame above was inside a block in this shape"
    );
    let above: Vec<&str> = unwinding
        .above
        .iter()
        .map(|frame| frame.at.function.as_str())
        .collect();
    assert_eq!(
        above,
        ["deepest", "middle"],
        "the frames above the target, innermost first"
    );

    // the thread was let go, so the stop it was held at is **over** — and the
    // engine has to say so before the reset lands, because that is what a front
    // end reads to decide whether to wait for the program at all. it did not,
    // and a client left believing the old stop was still held never waited and
    // never announced the one below
    assert!(
        debuggee.held().is_empty(),
        "the stop the unwind ended is still reported as held: {:?}",
        debuggee.held()
    );

    // the thread was let go, so the reset arrives as a stop of its own
    let stop = landed(&mut debuggee);
    let reset = match &stop.reason {
        StopReason::FrameReset(reset) => reset,
        other => panic!("expected the reset to land, got {other:?}"),
    };
    assert_eq!(reset.frame.function, "outer");
    assert_eq!(reset.kept, vec!["v".to_string()]);
    to_exit(&mut debuggee);

    let ran = recorded(&fixture);
    // **the whole subtree runs again.** `outer` starts over, so it calls
    // `middle`, which calls `deepest` — the point being that the two frames that
    // were forced out are gone rather than resumed, and the program builds fresh
    // ones on the second pass
    assert_eq!(
        ran.matches("('outer', 'N')").count(),
        2,
        "`outer` ran twice: {ran}"
    );
    assert_eq!(
        ran.matches("('middle', 'N')").count(),
        2,
        "`middle` was forced out and then called again by the restarted \
         `outer`: {ran}"
    );
    assert_eq!(
        ran.matches("('deepest', 'N')").count(),
        2,
        "and so was `deepest`: {ran}"
    );
}

#[test]
fn a_tail_that_would_write_a_global_refuses_the_whole_unwind() {
    let fixture = Fixture::new("reset_noisy", PROGRAM);
    let mut debuggee = launch(&fixture);
    // held in `deepest`, called from `noisy_middle`, whose line stores the
    // result into a **global** once the call comes back
    let inside = line_of(PROGRAM, "    bottom = 1");
    loop {
        held_at(&mut debuggee, &fixture.path(), inside);
        let stack = debuggee.the_stack(None).expect("the stack was answered");
        if stack.frames[1].name() == "noisy_middle" {
            break;
        }
    }
    let stack = debuggee.the_stack(None).expect("the stack was answered");
    assert_eq!(stack.frames[2].name(), "noisy_outer");

    let error = debuggee
        .restart_frame(stack.frames[2].id, Again::InPlace)
        .expect_err("the unwind had to be refused");
    let said = error.to_string();
    // **nothing was forced out.** the refusal is decided off the bytecode of
    // every frame in the chain before the first of them is touched, so a chain
    // that cannot finish never starts
    assert!(
        said.contains("noisy_middle") && said.contains("STORE_GLOBAL"),
        "the refusal names the frame whose line would carry on running and the \
         instruction that would run: {said}"
    );
    to_exit(&mut debuggee);

    let ran = recorded(&fixture);
    assert_eq!(
        ran.matches("'noisy outer'").count(),
        1,
        "the refused unwind ran nothing again: {ran}"
    );
}

#[test]
fn a_frame_whose_call_is_its_last_statement_says_cpython_will_not_move_it() {
    let fixture = Fixture::new("reset_tail", PROGRAM);
    let mut debuggee = launch(&fixture);
    // held in `deepest`, called from `tail_middle`, whose whole body is that
    // call — the shape a step out now lands in, and which a reset still cannot
    // reach
    let inside = line_of(PROGRAM, "    bottom = 1");
    loop {
        held_at(&mut debuggee, &fixture.path(), inside);
        let stack = debuggee.the_stack(None).expect("the stack was answered");
        if stack.frames[1].name() == "tail_middle" {
            break;
        }
    }
    let stack = debuggee.the_stack(None).expect("the stack was answered");
    let call = line_of(PROGRAM, "nothing follows the call");

    let error = debuggee
        .restart_frame(stack.frames[1].id, Again::InPlace)
        .expect_err("the reset had to be refused");
    let said = error.to_string();

    // the refusal names the line **and** the interpreter rule behind it. bpd
    // does reach this frame — the `INSTRUCTION` event a step out lands on gets
    // control here before the tail runs — and it is cpython that will not move
    // a frame from one, so a message about line events alone would send someone
    // to rewrite a call site that was never the problem
    assert!(
        said.contains(&call.to_string()),
        "the refusal names the line the call is on: {said}"
    );
    assert!(
        said.contains("f_lineno"),
        "the refusal names the interpreter rule that forbids the move: {said}"
    );
    to_exit(&mut debuggee);

    let ran = recorded(&fixture);
    assert_eq!(
        ran.matches("'tail outer'").count(),
        1,
        "the refused reset ran nothing again: {ran}"
    );
}

#[test]
fn a_discarded_frame_that_held_a_block_open_says_its_cleanup_did_not_run() {
    let fixture = Fixture::new("reset_held", PROGRAM);
    let mut debuggee = launch(&fixture);
    // held in `deepest`, called from inside a `with` in `held_middle`
    let inside = line_of(PROGRAM, "    bottom = 1");
    loop {
        held_at(&mut debuggee, &fixture.path(), inside);
        let stack = debuggee.the_stack(None).expect("the stack was answered");
        if stack.frames[1].name() == "held_middle" {
            break;
        }
    }
    let stack = debuggee.the_stack(None).expect("the stack was answered");
    assert_eq!(stack.frames[2].name(), "held_outer");

    let unwinding = match debuggee
        .restart_frame(stack.frames[2].id, Again::InPlace)
        .expect("the restart request was answered")
    {
        Restarted::Unwinding(unwinding) => unwinding,
        other => panic!("expected an unwind to `held_outer`, got {other:?}"),
    };
    let held: Vec<(&str, bool)> = unwinding
        .above
        .iter()
        .map(|frame| (frame.at.function.as_str(), frame.inside_a_block))
        .collect();
    // `deepest` is not in a block; `held_middle` is, and it is the one that is
    // never coming back — so its `Opened()` stays open with nothing left that
    // could close it
    assert_eq!(held, [("deepest", false), ("held_middle", true)]);
    assert!(
        unwinding
            .told()
            .iter()
            .any(|said| said.contains("held_middle") && said.contains("cleanup did not run")),
        "the note names the discarded frame whose cleanup was skipped: {:?}",
        unwinding.told()
    );

    let stop = landed(&mut debuggee);
    assert!(matches!(&stop.reason, StopReason::FrameReset(_)));
    to_exit(&mut debuggee);

    let ran = recorded(&fixture);
    // and the program shows it: two `enter` against one `exit`, because the
    // first pass's context manager was never closed
    assert_eq!(
        ran.matches("'latch enter'").count() - ran.matches("'latch exit'").count(),
        1,
        "one context manager was left open by the discarded frame: {ran}"
    );
}

/// wait for the next stop, which is where an unwind put the thread
fn landed(debuggee: &mut Debuggee) -> Stop {
    match debuggee
        .wait(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was waited on")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected the reset to land, got {other:?}"),
    }
}

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
    let breakpoints = vec![SourceBreakpoint::at(1, file, line)];
    let resolved = debuggee
        .set_breakpoints(breakpoints)
        .expect("the breakpoint request was answered");
    match &resolved[0].binding {
        Binding::Bound { line: bound, .. } => assert_eq!(*bound, line),
        other => panic!("the breakpoint did not bind: {other:?}"),
    }
    let stop = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected a breakpoint stop, got {other:?}"),
    };
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    stop
}

/// the frame the held thread is executing
fn top(debuggee: &mut Debuggee) -> FrameId {
    debuggee
        .the_stack(Some(1))
        .expect("the stack was answered")
        .frames[0]
        .id
}

fn to_exit(debuggee: &mut Debuggee) {
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Exited { status, .. } => {
            assert!(status.success(), "the program exited with {status}");
        }
        other => panic!("expected the program to exit, got {other:?}"),
    }
}

fn recorded(fixture: &Fixture) -> String {
    std::fs::read_to_string(
        fixture
            .path()
            .parent()
            .expect("a fixture is in a directory")
            .join("ran.txt"),
    )
    .expect("the program wrote what it ran")
}
