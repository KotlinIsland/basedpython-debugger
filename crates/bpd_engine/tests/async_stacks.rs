//! the stack a task was created on, at a stop inside that task
//!
//! `create_task` severs the chain. measured, on 3.13, 3.14 and 3.15: a task that
//! raises has a traceback of **one frame** and the process exits **0**, and at a
//! stop inside the task the frames below it are the event loop's — the same
//! frames that are below the scheduler, so the scheduler itself is nowhere
//!
//! everything here drives a real interpreter, because the question is what
//! cpython and asyncio really do

use bpd_core::python::Capabilities;
use bpd_core::{Running, SourceBreakpoint, Stack};
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

/// a program whose work is scheduled from somewhere the stack does not show
///
/// `schedule` calls `create_task` and returns. by the time `work` runs, the
/// frame that scheduled it has gone, and what is under `work` is the event loop
const PROGRAM: &str = r"import asyncio


async def work():
    here = 1              # the breakpoint
    return here


def schedule():
    return asyncio.create_task(work())


async def main():
    task = schedule()
    await task


asyncio.run(main())
";

fn stack_at_the_stop(debuggee: &mut Debuggee, line: u32, file: &std::path::Path) -> Stack {
    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, file, line)])
        .expect("the breakpoint was answered");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        other => panic!("the breakpoint inside the task never stopped it: {other:?}"),
    }
    debuggee.the_stack(None).expect("the stack was answered")
}

#[test]
fn a_stop_inside_a_task_says_where_the_task_was_created() {
    let fixture = Fixture::new("program", PROGRAM);
    let here = line_of(PROGRAM, "here = 1");
    let creating = line_of(PROGRAM, "return asyncio.create_task(work())");
    let mut debuggee = launch(&fixture);

    let stack = stack_at_the_stop(&mut debuggee, here, &fixture.path());

    // the frames themselves are untouched: the running frame's caller really is
    // the event loop, and saying otherwise would be a call chain that never
    // happened
    assert_eq!(
        stack.frames[0].name(),
        "work",
        "the stop is inside the task: {:#?}",
        stack.frames
    );
    assert!(
        !stack.frames.iter().any(|frame| frame.name() == "schedule"),
        "`schedule` is not on this stack and must not be put there — it \
         scheduled `work` rather than calling it: {:#?}",
        stack.frames
    );

    // and the record says who scheduled it, which is the whole point: without
    // this the stack has no trace of `schedule` at all
    let scheduled = &stack.scheduled_by;
    assert!(
        !scheduled.is_empty(),
        "nothing recorded where the task was made, so the stop is exactly as \
         uninformative as it was before this feature"
    );
    assert_eq!(
        scheduled[0].function, "schedule",
        "the innermost creating frame is the one that called `create_task`: \
         {scheduled:#?}"
    );
    assert_eq!(
        scheduled[0].line, creating,
        "the record is the line the task was made on: {scheduled:#?}"
    );
    assert!(
        scheduled.iter().any(|frame| frame.function == "main"),
        "the record is a stack rather than one frame, so the caller of the \
         scheduler is in it: {scheduled:#?}"
    );

    // it is asyncio's own function, and a record that began there would say the
    // scheduler was asyncio rather than the program
    assert_ne!(
        scheduled[0].function, "create_task",
        "the `create_task` frame is asyncio's own and is not part of the answer"
    );
}

/// the same task, reached by every other route there is
///
/// `ensure_future`, the loop's own method and a task group all make a `Task`
/// without going through `asyncio.create_task`. measured: all four go through
/// `BaseEventLoop.create_task`, which is why that is the one thing watched
const OTHER_ROUTES: &str = r"import asyncio


async def work():
    here = 1
    return here


async def main():
    task = asyncio.ensure_future(work())
    await task


asyncio.run(main())
";

#[test]
fn a_task_made_by_another_route_is_recorded_the_same_way() {
    // one hook covers all of them, because every route reaches the loop's own
    // `create_task`. what this proves is the other half of that: the record
    // starts at the **program's** frame, with asyncio's own frames dropped —
    // `ensure_future` sits between the program and the loop, and a record that
    // began there would say the scheduler was asyncio
    let fixture = Fixture::new("routes", OTHER_ROUTES);
    let here = line_of(OTHER_ROUTES, "here = 1");
    let making = line_of(OTHER_ROUTES, "task = asyncio.ensure_future(work())");
    let mut debuggee = launch(&fixture);

    let stack = stack_at_the_stop(&mut debuggee, here, &fixture.path());
    assert!(stack.in_a_task, "the stop really is inside a task");
    assert_eq!(
        stack
            .scheduled_by
            .first()
            .map(|frame| frame.function.as_str()),
        Some("main"),
        "the innermost frame of the record is the program's own, not \
         `ensure_future`: {:#?}",
        stack.scheduled_by
    );
    assert_eq!(
        stack.scheduled_by[0].line, making,
        "and it is the line the program made it on: {:#?}",
        stack.scheduled_by
    );
    // only the **leading** ones are dropped. what sits below the program's own
    // frame is the event loop that was running it, and that is a true part of
    // the stack the task was made on — dropping it would be bpd truncating a
    // real stack because it found it uninteresting
    assert!(
        !stack.scheduled_by[0].file.contains("/asyncio/"),
        "the record still begins in asyncio: {:#?}",
        stack.scheduled_by
    );
    assert!(
        stack
            .scheduled_by
            .iter()
            .any(|frame| frame.file.contains("/asyncio/")),
        "the event loop really was under `main`, and a record without it would \
         be a stack bpd had edited: {:#?}",
        stack.scheduled_by
    );
}

/// a program with no asyncio in it at all, that says whether one arrived
///
/// the marker is written **after** the stop, so it records what `sys.modules`
/// held once bpd had walked the stack. reading it before would say nothing
/// about what the walk did
const PLAIN: &str = r#"import pathlib
import sys

HERE = pathlib.Path(__file__).parent


def work():
    here = 1
    return here


work()
(HERE / "asyncio").write_text(str("asyncio" in sys.modules))
"#;

#[test]
fn a_stop_that_is_not_in_a_task_carries_no_record_rather_than_an_empty_claim() {
    // the ordinary case, and it has to cost nothing and say nothing. a stack
    // that carried an empty `scheduled_by` as though it meant something would
    // have every non-async program answering a question nobody asked
    let fixture = Fixture::new("plain", PLAIN);
    let here = line_of(PLAIN, "here = 1");
    let mut debuggee = launch(&fixture);

    let stack = stack_at_the_stop(&mut debuggee, here, &fixture.path());
    assert!(
        stack.scheduled_by.is_empty(),
        "a program that never imported asyncio reported a creation stack: {:#?}",
        stack.scheduled_by
    );
    assert!(
        !stack.in_a_task,
        "there is no task here at all, and a stack that claimed one would make \
         the empty record above read as a limit of bpd rather than as nothing \
         to say"
    );

    // and walking that stack did not **import asyncio** into a program that
    // never asked for one. finding the current task means asking asyncio, and
    // asking for a module that is not there imports it — which is bpd adding a
    // module to `sys.modules`, running its body, and changing what the program
    // is. the launch parity rule does not stop at the launch
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Exited { status, .. } => assert!(status.success(), "it exited with {status}"),
        other => panic!("expected the program to finish, got {other:?}"),
    }
    let arrived = std::fs::read_to_string(
        fixture
            .path()
            .parent()
            .expect("a fixture has a directory")
            .join("asyncio"),
    )
    .expect("the program recorded what it saw");
    assert_eq!(
        arrived, "False",
        "bpd imported asyncio into a program that never did, so `sys.modules` \
         is not what the program built"
    );
}
