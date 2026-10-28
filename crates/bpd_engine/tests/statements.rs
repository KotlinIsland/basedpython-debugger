//! statements in an evaluation: what a console runs in a frame
//!
//! measured in the IDE before this existed: `s.radius = 5.0` in Evaluate was
//! `SyntaxError (<bpd evaluation>)`, where `PyCharm`'s own debugger runs it. the
//! proof that a statement ran **in the frame** is the program's own output
//! afterwards, not the debugger's word for it

use std::ffi::OsString;

use bpd_core::{
    Addressed, Binding, Content, Detail, Evaluated, FrameId, Request, Response, Running,
    SourceBreakpoint,
};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// a local the program prints after the line it is held on
const PRINTS: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent


def work():
    total = 1
    (HERE / "seen").write_text(repr(total))  # held here
    return total


work()
"#;

fn launch(fixture: &Fixture) -> Debuggee {
    match bpd_engine::launch(
        bpd_test::agent::matching_interpreter(),
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

/// evaluate `text` in the held frame, with or without statements
fn evaluated(debuggee: &mut Debuggee, frame: FrameId, text: &str, statements: bool) -> Evaluated {
    match debuggee
        .dispatch(
            Addressed::unnamed(Request::Evaluate {
                frame,
                expression: text.to_string(),
                detail: Detail::default(),
                statements,
            }),
            &mut bpd_test::reporting::Unreported,
        )
        .expect("the evaluation was answered")
    {
        Response::Evaluated(evaluated) => evaluated,
        other => panic!("an evaluation was answered with {other:?}"),
    }
}

#[test]
fn a_statement_runs_in_the_frame_and_the_program_sees_what_it_did() {
    let fixture = Fixture::new("prints", PRINTS);
    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(
            1,
            fixture.path(),
            line_of(PRINTS, "# held here"),
        )])
        .expect("the breakpoint request was answered");
    assert!(matches!(resolved[0].binding, Binding::Bound { .. }));
    let stop = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected the breakpoint, got {other:?}"),
    };
    let frame = FrameId {
        stop: stop.stop,
        depth: 0,
    };

    // without statements, an assignment is the syntax error it is: a hover or
    // a watch must not change the program for having looked
    match evaluated(&mut debuggee, frame, "total = 41", false) {
        Evaluated::Raised { error } => assert_eq!(error.kind, "SyntaxError"),
        other @ Evaluated::Value { .. } => panic!("an assignment is not an expression: {other:?}"),
    }

    // with them, an import and an assignment run in the frame and answer `None`
    for statement in ["import math", "total = math.floor(41.5) + 1"] {
        match evaluated(&mut debuggee, frame, statement, true) {
            Evaluated::Value { value } => assert!(
                matches!(value.content, Content::None),
                "a statement answers `None`: {value:?}"
            ),
            Evaluated::Raised { error } => panic!("`{statement}` raised {error}"),
        }
    }

    // and an expression is still an expression
    match evaluated(&mut debuggee, frame, "total", true) {
        Evaluated::Value { value } => assert!(
            matches!(&value.content, Content::Int { text, .. } if text == "42"),
            "{value:?}"
        ),
        Evaluated::Raised { error } => panic!("`total` raised {error}"),
    }

    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Exited { status, .. } => assert!(status.success()),
        other => panic!("expected the program to end, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(fixture.directory().join("seen")).expect("the program wrote it"),
        "42",
        "the program read the local the statement wrote"
    );
}
