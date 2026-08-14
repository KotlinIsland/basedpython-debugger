//! the failure report a stepping test prints when it lands in the wrong place
//!
//! `bpd_test::trace` exists so that a wrong landing is diagnosable, and the only
//! way to know it is is to drive a real debuggee and read what it produced. a
//! trace asserted against a hand-built sequence would be a test of the
//! formatting and not of the recording

use bpd_core::{Running, SourceBreakpoint, StepKind, StopReason};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};
use bpd_test::trace::Trace;

/// a program with a call to step over, and a line after it to land on
const PROGRAM: &str = r"def helper(value):
    inside = value * 2
    return inside


def main():
    doubled = helper(4)
    after = doubled + 1
    return after


main()
";

fn launch(fixture: &Fixture) -> Debuggee {
    match bpd_engine::launch(
        bpd_test::agent::matching_interpreter(),
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

#[test]
fn a_trace_records_what_was_asked_and_what_happened_in_the_order_they_did() {
    let fixture = Fixture::new("program", PROGRAM);
    let calling = line_of(PROGRAM, "doubled = helper(4)");
    let next = line_of(PROGRAM, "after = doubled + 1");

    let mut debuggee = launch(&fixture);
    let mut trace = Trace::default();
    assert!(
        trace.is_empty(),
        "a trace that started full would make every sequence below a lie"
    );

    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), calling)])
        .expect("the breakpoint request was answered");
    trace.asked("set a breakpoint");

    match trace.run(&mut debuggee).expect("the debuggee was resumed") {
        Running::Stopped { stop, .. } => assert!(
            matches!(stop.reason, StopReason::Breakpoint { .. }),
            "the program stopped for {:?}",
            stop.reason
        ),
        other => panic!("expected the breakpoint to stop it, got {other:?}"),
    }
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");

    let stop = trace.stepped(&mut debuggee, StepKind::Over);
    assert!(
        matches!(stop.reason, StopReason::Stepped { .. }),
        "the step landed as {:?}",
        stop.reason
    );

    let said = trace.to_string();

    // the ask and the outcome are both there, and the ask is **above** its
    // outcome. that ordering is the whole value: a landing under a `step over`
    // that should have been a `step in` is the bug, read off two adjacent lines
    let asked_step = said
        .find("step over")
        .unwrap_or_else(|| panic!("the step was never recorded:\n{said}"));
    let landed = said
        .rfind("Stepped")
        .unwrap_or_else(|| panic!("the landing was never recorded:\n{said}"));
    assert!(
        asked_step < landed,
        "the landing is recorded above the step that produced it, so a reader \
         cannot pair them:\n{said}"
    );

    // and the breakpoint that started it is above both, because "what led here"
    // is the part a landing on its own cannot say
    let breakpoint = said
        .find("set a breakpoint")
        .unwrap_or_else(|| panic!("what the test asked for first is missing:\n{said}"));
    assert!(
        breakpoint < asked_step,
        "the sequence is not in the order it happened:\n{said}"
    );

    assert!(
        said.contains(&next.to_string()),
        "the line the step landed on is what a failure is compared against, and \
         it is not in the report:\n{said}"
    );
    assert_eq!(
        trace.len(),
        7,
        "the breakpoint is one, the run is two, and a step is four — the ask, \
         the threads it set stepping, the wait for one to land, and the \
         landing. a step is not one entry, because the thing that diagnoses a \
         wrong landing is which of those four did something unexpected:\n{said}"
    );

    match trace.run(&mut debuggee).expect("the debuggee was resumed") {
        Running::Exited { status, .. } => assert!(status.success(), "it exited with {status}"),
        other => panic!("expected the program to finish, got {other:?}"),
    }
}

#[test]
fn a_trace_nothing_was_recorded_on_says_so_rather_than_printing_nothing() {
    // an empty trace under a failure means the test drove the debuggee some
    // other way. printing it as blank leaves a reader hunting for a bug in the
    // program, which is the one place the bug is not
    let said = Trace::default().to_string();
    assert!(
        said.contains("drove the debuggee directly"),
        "an empty trace has to say why it is empty, and said {said:?}"
    );
}
