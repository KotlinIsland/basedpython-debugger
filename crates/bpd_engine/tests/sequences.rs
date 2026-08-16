//! a breakpoint that does not arm until another one has been hit
//!
//! "stop in the handler, but only after the request that set this flag came
//! through". what makes it worth having over a condition is that a waiting
//! breakpoint is **not watched at all** — its location has no `LINE` events, so
//! it costs nothing until the one before it fires, where a condition costs an
//! expression on every pass
//!
//! everything here drives a real interpreter, because what is under test is
//! whether the interpreter really stops where the sequence says and really does
//! not stop before it

use bpd_core::python::Capabilities;
use bpd_core::{Binding, NoArming, Resolved, Running, SourceBreakpoint, Stop, StopReason, Unbound};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

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

/// a program that runs the same two lines several times, in a fixed order
///
/// `gate` runs before `guarded` on every pass, so a breakpoint on `guarded`
/// that waits for one on `gate` has to miss the first pass and catch a later
/// one — and the marker file is what says which pass the program was on
const PROGRAM: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent


def gate(n):
    opened = n            # the breakpoint that arms the other one
    return opened


def guarded(n):
    reached = n           # the one that waits
    (HERE / "reached").write_text(str(n))
    return reached


for i in range(4):
    if i >= 2:
        gate(i)
    guarded(i)
"#;

fn resolution(resolved: &[Resolved], id: u32) -> &Resolved {
    resolved
        .iter()
        .find(|one| one.id == id)
        .unwrap_or_else(|| panic!("breakpoint {id} was not resolved: {resolved:#?}"))
}

fn stopped_at(debuggee: &mut Debuggee) -> Stop {
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected a stop, got {other:?}"),
    }
}

#[test]
fn a_breakpoint_that_waits_for_another_is_bound_and_says_it_is_not_armed_yet() {
    let fixture = Fixture::new("program", PROGRAM);
    let opened = line_of(PROGRAM, "opened = n");
    let reached = line_of(PROGRAM, "reached = n");
    let mut debuggee = launch(&fixture);

    let resolved = debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), opened),
            SourceBreakpoint::at(2, fixture.path(), reached).after(1),
        ])
        .expect("the breakpoints were answered");

    // it **bound**. that is the whole reason `waiting_for` is beside the
    // binding rather than a kind of unbound: the interpreter has somewhere to
    // stop, it is simply not watching it yet, and reporting it as unbound would
    // send a user looking for a line that is perfectly good
    let waiting = resolution(&resolved, 2);
    assert!(
        matches!(waiting.binding, Binding::Bound { .. }),
        "a waiting breakpoint still binds: {waiting:#?}"
    );
    assert_eq!(
        waiting.waiting_for,
        Some(1),
        "the client has to be told which breakpoint it is waiting for, or a \
         line that never fires looks broken: {waiting:#?}"
    );

    // and the one nothing waits for says so by carrying nothing
    assert_eq!(resolution(&resolved, 1).waiting_for, None);

    let first = stopped_at(&mut debuggee);
    match &first.reason {
        StopReason::Breakpoint { line, .. } => assert_eq!(
            *line, opened,
            "the first stop has to be the gate. a stop on the guarded line \
             would mean the waiting breakpoint was armed from the start"
        ),
        other => panic!("expected a breakpoint stop, got {other:?}"),
    }

    // the program ran the guarded line twice before ever reaching the gate, and
    // neither pass stopped. this is the assertion the whole feature is for
    assert_eq!(
        std::fs::read_to_string(
            fixture
                .path()
                .parent()
                .expect("a fixture has a directory")
                .join("reached")
        )
        .expect("the guarded line ran before the gate did"),
        "1",
        "the guarded line should have run for i=0 and i=1 without stopping"
    );

    // now the gate has been hit, so the next pass over the guarded line stops
    let second = stopped_at(&mut debuggee);
    match &second.reason {
        StopReason::Breakpoint { line, .. } => assert_eq!(
            *line, reached,
            "the gate was hit, so the breakpoint waiting on it is armed now"
        ),
        other => panic!("expected a breakpoint stop, got {other:?}"),
    }
}

#[test]
fn a_logpoint_arms_what_waits_for_it_even_though_it_never_stops() {
    // the case that forced arming to happen inside the event callback. a
    // logpoint does not stop, so if the arming waited for the program to be
    // held next, it would wait for a stop that only the *armed* breakpoint
    // could produce — and nothing would ever arm anything
    let fixture = Fixture::new("program", PROGRAM);
    let opened = line_of(PROGRAM, "opened = n");
    let reached = line_of(PROGRAM, "reached = n");
    let mut debuggee = launch(&fixture);

    let mut gate = SourceBreakpoint::at(1, fixture.path(), opened);
    gate.log = Some("the gate opened for {n}".to_string());
    let resolved = debuggee
        .set_breakpoints(vec![
            gate,
            SourceBreakpoint::at(2, fixture.path(), reached).after(1),
        ])
        .expect("the breakpoints were answered");
    assert_eq!(resolution(&resolved, 2).waiting_for, Some(1));

    // the only stop the program can produce is the guarded line, and it can
    // only produce it if the logpoint armed it on the way past
    let mut logs = bpd_test::reporting::Logs::default();
    let stop = match debuggee.run(&mut logs).expect("the debuggee was resumed") {
        Running::Stopped { stop, .. } => stop,
        other => panic!("the logpoint never armed the breakpoint waiting on it: {other:?}"),
    };
    match &stop.reason {
        StopReason::Breakpoint { line, .. } => assert_eq!(*line, reached),
        other => panic!("expected the guarded line, got {other:?}"),
    }

    // and it really did log rather than stop: a gate that stopped would have
    // produced this stop for the wrong reason entirely
    assert!(
        !logs.records.is_empty(),
        "the gate is a logpoint and wrote nothing"
    );
    assert!(
        logs.records
            .iter()
            .any(|record| record.message.contains("the gate opened for 2")),
        "the log records were {:?}",
        logs.records
    );
}

#[test]
fn a_breakpoint_waiting_for_one_that_is_not_in_the_set_is_refused_rather_than_left_pending() {
    // the same rule a condition that does not compile follows. a breakpoint
    // that can never arm can never fire, and reporting it bound would be the
    // debugger saying it is set when nothing will ever watch it
    let fixture = Fixture::new("program", PROGRAM);
    let reached = line_of(PROGRAM, "reached = n");
    let mut debuggee = launch(&fixture);

    let resolved = debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(2, fixture.path(), reached).after(99),
        ])
        .expect("the breakpoints were answered");

    match &resolution(&resolved, 2).binding {
        Binding::Unbound {
            reason: Unbound::NeverArms { after, why },
        } => {
            assert_eq!(*after, 99);
            assert_eq!(*why, NoArming::NoSuchBreakpoint);
        }
        other => panic!("a breakpoint that can never arm was not refused: {other:#?}"),
    }
}

#[test]
fn a_cycle_refuses_every_breakpoint_in_it_rather_than_the_one_that_closed_it() {
    // every link of a cycle is waiting for one behind it, so none of them is
    // ever armed. refusing only the breakpoint that closed the loop would leave
    // the others reported as merely waiting — which is true of each one alone
    // and false of the set
    let fixture = Fixture::new("program", PROGRAM);
    let opened = line_of(PROGRAM, "opened = n");
    let reached = line_of(PROGRAM, "reached = n");
    let mut debuggee = launch(&fixture);

    let resolved = debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), opened).after(2),
            SourceBreakpoint::at(2, fixture.path(), reached).after(1),
        ])
        .expect("the breakpoints were answered");

    for id in [1, 2] {
        match &resolution(&resolved, id).binding {
            Binding::Unbound {
                reason: Unbound::NeverArms { why, .. },
            } => assert!(
                matches!(why, NoArming::Cycle { .. }),
                "breakpoint {id} is in a cycle and was refused for {why:?}"
            ),
            other => panic!("breakpoint {id} in a cycle was not refused: {other:#?}"),
        }
    }
}

#[test]
fn a_breakpoint_that_names_itself_is_refused_by_name() {
    let fixture = Fixture::new("program", PROGRAM);
    let opened = line_of(PROGRAM, "opened = n");
    let mut debuggee = launch(&fixture);

    let resolved = debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), opened).after(1),
        ])
        .expect("the breakpoints were answered");

    match &resolution(&resolved, 1).binding {
        Binding::Unbound {
            reason: Unbound::NeverArms { why, .. },
        } => assert_eq!(*why, NoArming::Itself),
        other => panic!("a breakpoint waiting for itself was not refused: {other:#?}"),
    }
}
