//! smart step into: the calls a line makes, and a step into the one chosen
//!
//! measured in the IDE before this existed: `PyCharm`'s smart step into offered
//! nothing for a `.by` line, because bpd answered no `stepInTargets`. what is
//! proved here is that the calls are the line's own, named as the source names
//! them, and that a step into one goes through the others — the program's
//! own marker files say which calls really ran

use std::ffi::OsString;

use bpd_core::{Binding, FrameId, Running, SourceBreakpoint, StopReason};
use bpd_engine::{Debuggee, Launched};
use bpd_test::basedpython::{Build, SOURCE};
use bpd_test::debuggee::{Fixture, line_of};

/// three calls on one line, one of them a builtin
const THREE: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent


def double(x):
    (HERE / "double_ran").write_text("x")
    return x * 2


def triple(x):
    tripled = x * 3
    return tripled


def main():
    total = sum([double(1), triple(2)])  # three calls
    return total


main()
"#;

fn launch(program: &std::path::Path, args: &[OsString]) -> Debuggee {
    match bpd_engine::launch(
        bpd_test::agent::matching_interpreter(),
        &bpd_engine::Program::Script(program.to_path_buf()),
        args,
    ) {
        Ok(Launched::Stopped(debuggee)) => debuggee,
        Ok(Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

/// run to a breakpoint, take it off, and say which stop holds the thread
fn held_at(debuggee: &mut Debuggee, file: &std::path::Path, line: u32) -> u64 {
    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, file.to_path_buf(), line)])
        .expect("the breakpoint request was answered");
    assert!(
        matches!(
            resolved[0].binding,
            Binding::Bound { .. } | Binding::Unbound { .. }
        ),
        "{resolved:?}"
    );
    let stop = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop.stop,
        other => panic!("expected the breakpoint, got {other:?}"),
    };
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    stop
}

/// where a step landed, as the line and the function
fn landed(debuggee: &mut Debuggee) -> (u32, String) {
    match debuggee
        .wait(&mut bpd_test::reporting::Unreported)
        .expect("the step was waited for")
    {
        Running::Stopped { stop, .. } => {
            let StopReason::Stepped { line, .. } = stop.reason else {
                panic!("expected the step to land, got {:?}", stop.reason)
            };
            let stack = debuggee.the_stack(Some(1)).expect("the stack was answered");
            (line, stack.frames[0].name().to_string())
        }
        other => panic!("expected the step to land, got {other:?}"),
    }
}

#[test]
fn the_calls_of_a_line_are_its_own_and_a_step_into_one_goes_through_the_others() {
    let fixture = Fixture::new("three", THREE);
    let line = line_of(THREE, "# three calls");

    let mut debuggee = launch(&fixture.path(), &[]);
    let stop = held_at(&mut debuggee, &fixture.path(), line);
    let targets = debuggee
        .step_in_targets(FrameId { stop, depth: 0 })
        .expect("the targets were answered");
    assert_eq!(
        targets
            .iter()
            .map(|target| (target.label.as_str(), target.line))
            .collect::<Vec<_>>(),
        [("double", line), ("triple", line), ("sum", line)],
        "every call of the line, in the order it makes them: {targets:?}"
    );
    let columns = targets[0].columns.expect("a python file has columns");
    let text = THREE
        .lines()
        .nth(usize::try_from(line - 1).expect("a line fits"))
        .expect("the line");
    assert_eq!(
        &text[usize::try_from(columns.0 - 1).expect("fits")
            ..usize::try_from(columns.1 - 1).expect("fits")],
        "double(1)",
        "the columns are the call's own"
    );

    // into `triple`: `double` runs on the way without being stopped in
    let triple = targets[1].offset;
    debuggee
        .step_into(stop, triple)
        .expect("the step was armed");
    assert_eq!(
        landed(&mut debuggee),
        (line_of(THREE, "tripled = x * 3"), "triple".to_string())
    );
    assert!(
        fixture.directory().join("double_ran").exists(),
        "the call before it ran, and was gone through"
    );
}

#[test]
fn a_step_into_a_builtin_lands_where_a_step_over_would() {
    let fixture = Fixture::new("three", THREE);
    let line = line_of(THREE, "# three calls");

    let mut debuggee = launch(&fixture.path(), &[]);
    let stop = held_at(&mut debuggee, &fixture.path(), line);
    let targets = debuggee
        .step_in_targets(FrameId { stop, depth: 0 })
        .expect("the targets were answered");
    let sum = targets
        .iter()
        .find(|target| target.label == "sum")
        .expect("`sum` is a call of the line");
    debuggee
        .step_into(stop, sum.offset)
        .expect("the step was armed");
    assert_eq!(
        landed(&mut debuggee),
        (line_of(THREE, "return total"), "main".to_string()),
        "`sum` enters no python frame, and neither of the calls before it is a landing"
    );
}

#[test]
fn a_by_lines_calls_are_named_by_the_generated_python_and_carry_no_columns() {
    // `by run` gives every location of a `.by` module the whole of its line, so
    // the frame has no columns to read a call from. the bytecode is the
    // generated python's, and that is where the label comes from
    let build = Build::demo();
    let line = line_of(SOURCE, "answer = add(2, 3)");

    let mut debuggee = launch(&build.runner(), &build.arguments());
    let stop = held_at(&mut debuggee, &build.source, line);
    let targets = debuggee
        .step_in_targets(FrameId { stop, depth: 0 })
        .expect("the targets were answered");
    assert_eq!(targets.len(), 1, "{targets:?}");
    assert_eq!(targets[0].label, "add");
    assert_eq!(targets[0].line, line);
    assert_eq!(targets[0].columns, None);

    debuggee
        .step_into(stop, targets[0].offset)
        .expect("the step was armed");
    assert_eq!(
        landed(&mut debuggee),
        (line_of(SOURCE, "total = a + b"), "add".to_string())
    );
}
