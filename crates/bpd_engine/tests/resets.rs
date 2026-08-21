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
use bpd_core::{Again, Binding, FrameId, Restarted, Running, SourceBreakpoint, Stop};
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


def nested_call():
    return target(f2())


def attribute_call():
    return target(WATCHED.attr)


def main():
    nested_call()
    attribute_call()
    blocked_call()
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
