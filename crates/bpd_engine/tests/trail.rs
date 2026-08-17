//! where the program went, over a bounded window
//!
//! the half of "step back" that can be afforded. measured before it was built:
//! storing the location per line is 6× a bare run and fits a fixed ring, where
//! storing the values as well is 30× and unbounded
//!
//! everything here drives a real interpreter, because what is under test is
//! whether the program's real path is what comes back

use bpd_core::{Content, Detail, Evaluated, FrameId, Running, SourceBreakpoint};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

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

/// a program that goes somewhere specific, in a fixed order
///
/// the branch is what makes the trail worth reading: which way it went is not
/// knowable from the source, and it is exactly what a person stepping back wants
const PROGRAM: &str = r"def chosen(n):
    picked = n * 2
    return picked


def skipped(n):
    never = n
    return never


def main():
    if True:
        value = chosen(3)
    else:
        value = skipped(3)
    done = value          # the breakpoint
    return done


main()
";

#[test]
fn a_trail_says_where_the_program_really_went_and_not_where_it_did_not() {
    let fixture = Fixture::new("program", PROGRAM);
    let done = line_of(PROGRAM, "done = value");
    let picked = line_of(PROGRAM, "picked = n * 2");
    let never = line_of(PROGRAM, "never = n");
    let mut debuggee = launch(&fixture);

    let (on, held, dropped) = debuggee.record(true).expect("recording was answered");
    assert!(on, "it was asked to record and said it was not");
    assert_eq!((held, dropped), (0, 0), "a fresh recording holds nothing");

    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), done)])
        .expect("the breakpoint was answered");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        other => panic!("the breakpoint never stopped it: {other:?}"),
    }

    let went = debuggee.trail().expect("the trail was answered");
    let lines: Vec<u32> = went.went.iter().map(|step| step.line).collect();

    // the branch it took, and the one it did not. this is the whole feature:
    // the stack at the stop says nothing about which way it came
    assert!(
        lines.contains(&picked),
        "the program ran `chosen` and the trail does not have its line: {lines:?}"
    );
    assert!(
        !lines.contains(&never),
        "the program never ran `skipped`, and a trail that says it did is a \
         debugger inventing history: {lines:?}"
    );

    // and it is a path rather than a set: `picked` comes before `done`, because
    // that is the order the program did them in
    let first = lines.iter().position(|line| *line == picked);
    let last = lines.iter().position(|line| *line == done);
    assert!(
        first < last,
        "the trail is where the program went, in order, and this is not: {lines:?}"
    );

    // the function is carried too, because a line number alone does not say
    // which code was running it
    assert!(
        went.went.iter().any(|step| step.function == "chosen"),
        "the trail has to name the code, not only the line: {:#?}",
        went.went
    );
}

#[test]
fn nothing_is_recorded_until_it_is_asked_for() {
    // the mode is off by default, and that is the whole reason the rest of bpd
    // is fast. a trail that filled up without being asked would mean every
    // session was paying four times a bare run
    let fixture = Fixture::new("program", PROGRAM);
    let done = line_of(PROGRAM, "done = value");
    let mut debuggee = launch(&fixture);

    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), done)])
        .expect("the breakpoint was answered");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        other => panic!("the breakpoint never stopped it: {other:?}"),
    }

    let went = debuggee.trail().expect("the trail was answered");
    assert!(
        went.went.is_empty(),
        "nothing asked for a recording and one happened anyway: {:#?}",
        went.went
    );
    assert!(!went.recording, "and it says it is not recording");
}

#[test]
fn stopping_a_recording_keeps_what_it_recorded() {
    // stopping is what somebody does in order to read it. a trail thrown away at
    // that moment would make the one thing they were about to do impossible
    let fixture = Fixture::new("program", PROGRAM);
    let done = line_of(PROGRAM, "done = value");
    let mut debuggee = launch(&fixture);

    debuggee.record(true).expect("recording was answered");
    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), done)])
        .expect("the breakpoint was answered");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        other => panic!("the breakpoint never stopped it: {other:?}"),
    }

    let (on, held, _) = debuggee.record(false).expect("recording was stopped");
    assert!(!on, "it was asked to stop and said it had not");
    assert!(held > 0, "it recorded something before being stopped");

    let went = debuggee.trail().expect("the trail was answered");
    assert_eq!(
        went.went.len() as u64,
        held,
        "the trail is still there after stopping, and holds what it said it did"
    );
    assert!(!went.recording, "and it says the recording is over");
}

/// a program that watches the recorder's own grip on a code object
///
/// `sys.getrefcount` is the observation. the trail holds a code object so its
/// address cannot come to name a different one, and the question is whether it
/// lets go when the last step naming it has fallen out of the window — a count
/// taken while it is held, and again after 120,000 steps have rolled through a
/// window of 100,000
///
/// both counts include the program's own reference, so the difference between
/// them is exactly what the recorder was holding
const WATCHES_ITS_OWN_REFCOUNT: &str = r#"import sys


def once():
    marker = 1
    return marker


once()
code = once.__code__
before = sys.getrefcount(code)

for i in range(60000):
    pass

after = sys.getrefcount(code)
answer = f"{before} {after}"
done = answer          # the breakpoint
"#;

#[test]
fn the_window_lets_go_of_a_code_object_no_step_in_it_still_names() {
    // the window bounds the steps, and a code object held for every step that
    // ever entered it would grow without one — in a program that compiles code
    // as it runs, which django's template engine does, without any ceiling at
    // all. it is also bpd keeping alive something the program has finished with
    let fixture = Fixture::new("program", WATCHES_ITS_OWN_REFCOUNT);
    let done = line_of(WATCHES_ITS_OWN_REFCOUNT, "done = answer");
    let mut debuggee = launch(&fixture);

    debuggee.record(true).expect("recording was answered");
    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), done)])
        .expect("the breakpoint was answered");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        other => panic!("the breakpoint never stopped it: {other:?}"),
    }

    let counted = match debuggee
        .evaluate(FrameId { stop: 2, depth: 0 }, "answer", Detail::default())
        .expect("the evaluation was answered")
    {
        Evaluated::Value { value } => match value.content {
            Content::Str { text, .. } => text,
            other => panic!("the program made a string, and this is {other:?}"),
        },
        Evaluated::Raised { error } => panic!("`answer` raised {error:?}"),
    };

    let mut counts = counted.split_whitespace().map(|count| {
        count
            .parse::<i64>()
            .expect("the program formatted two `sys.getrefcount` results")
    });
    let before = counts.next().expect("the first count");
    let after = counts.next().expect("the second count");

    assert!(
        after < before,
        "the recorder still holds a code object no step in the window names: \
         the count was {before} while it was held and {after} after 120,000 \
         steps rolled through a window of 100,000"
    );
}
