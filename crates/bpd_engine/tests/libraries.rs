//! just-my-code: stopping and stepping only in the program's own code
//!
//! measured in the IDE before this existed: "on raise" stopped six times inside
//! `<frozen importlib._bootstrap>` and `zipimport` before any line of the
//! program had run, and under `by run` forty times before the program's first
//! import was done — every `KeyError` the import system raises at itself and
//! catches. a filter that made that usable had to know which code is the
//! program's, and the interpreter is what says
//!
//! every case is run with the setting on and off, because off is the control:
//! it proves the library raises and the library frames are there to be skipped

use std::ffi::OsString;
use std::path::Path;

use bpd_core::{Binding, Running, SourceBreakpoint, StepKind, StopReason};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// four raises, one of each kind there is
///
/// `os.makedirs` with `exist_ok` raises `FileExistsError` inside `os` and
/// catches it there. `mine` raises in the program. `int` raises in C, which has
/// no frame, so it is raised in the frame that called it. `json.loads` raises
/// inside `json` and the exception arrives in the program
const RAISES: &str = r#"import json
import os
import pathlib

HERE = str(pathlib.Path(__file__).parent)


def mine():
    raise ValueError("the program's own")


def main():
    os.makedirs(HERE, exist_ok=True)
    try:
        mine()
    except ValueError:
        pass
    try:
        int("not a number")  # raised in c
    except ValueError:
        pass
    try:
        json.loads("{")  # raised in json
    except ValueError:
        pass


ready = 1
main()
"#;

/// a library calling back into the program
const CALLBACK: &str = r"import json


class Thing:
    pass


def convert(value):
    converted = {'thing': True}
    return converted


def main():
    text = json.dumps({'a': Thing()}, default=convert)  # into json
    return text


main()
";

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

/// run to a breakpoint on `line` of the fixture and take the breakpoint off
fn held_at(debuggee: &mut Debuggee, fixture: &Fixture, line: u32) {
    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), line)])
        .expect("the breakpoint request was answered");
    assert!(
        matches!(resolved[0].binding, Binding::Bound { .. }),
        "{resolved:?}"
    );
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        other => panic!("expected the breakpoint, got {other:?}"),
    }
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
}

/// every raise the program is stopped for until it ends, as `(kind, file, line)`
fn raises(debuggee: &mut Debuggee) -> Vec<(String, String, u32)> {
    let mut seen = Vec::new();
    loop {
        match debuggee
            .run(&mut bpd_test::reporting::Unreported)
            .expect("the debuggee was resumed")
        {
            Running::Stopped { stop, .. } => {
                let StopReason::Raised { error, file, line } = stop.reason else {
                    panic!("expected only raises, got {:?}", stop.reason)
                };
                seen.push((error.kind, file, line));
            }
            Running::Exited { status, .. } => {
                assert!(status.success(), "the program catches everything it raises");
                return seen;
            }
            other => panic!("expected a raise or the end, got {other:?}"),
        }
    }
}

#[test]
fn with_just_my_code_a_raise_stops_only_where_it_is_in_the_programs_own_code() {
    let fixture = Fixture::new("raises", RAISES);
    let program = fixture.path().display().to_string();

    let mut debuggee = launch(&fixture);
    let set = debuggee
        .just_my_code(true)
        .expect("the setting was answered");
    assert!(set.on, "the agent says what is set");
    assert!(
        !set.libraries.is_empty(),
        "the standard library is always a library's, and the answer names it"
    );
    held_at(&mut debuggee, &fixture, line_of(RAISES, "ready = 1"));
    debuggee
        .set_exception_breakpoints(true, false)
        .expect("the exception breakpoints were set");

    assert_eq!(
        raises(&mut debuggee),
        [
            (
                "ValueError".to_string(),
                program.clone(),
                line_of(RAISES, "raise ValueError")
            ),
            (
                "ValueError".to_string(),
                program.clone(),
                line_of(RAISES, "# raised in c")
            ),
            (
                "json.decoder.JSONDecodeError".to_string(),
                program,
                line_of(RAISES, "# raised in json")
            ),
        ],
        "the `FileExistsError` `os` raises and catches is not a stop, and the one \
         `json` raises into the program is a stop where it arrives"
    );
}

#[test]
fn without_just_my_code_a_raise_stops_wherever_it_is() {
    let fixture = Fixture::new("raises", RAISES);
    let program = fixture.path().display().to_string();

    let mut debuggee = launch(&fixture);
    held_at(&mut debuggee, &fixture, line_of(RAISES, "ready = 1"));
    debuggee
        .set_exception_breakpoints(true, false)
        .expect("the exception breakpoints were set");

    let seen = raises(&mut debuggee);
    assert!(
        seen.iter()
            .any(|(kind, file, _)| kind == "FileExistsError" && *file != program),
        "the control: `os` raises inside itself, and with the setting off that is a \
         stop: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|(kind, file, _)| kind.ends_with("JSONDecodeError") && *file != program),
        "the control: `json` raises inside itself: {seen:?}"
    );
}

#[test]
fn with_just_my_code_a_step_in_goes_through_a_library_to_the_programs_callback() {
    let fixture = Fixture::new("callback", CALLBACK);
    let call = line_of(CALLBACK, "# into json");

    for on in [true, false] {
        let mut debuggee = launch(&fixture);
        debuggee.just_my_code(on).expect("the setting was answered");
        held_at(&mut debuggee, &fixture, call);
        debuggee.the_step(StepKind::In).expect("the step was armed");
        let landed = match debuggee
            .wait(&mut bpd_test::reporting::Unreported)
            .expect("the step was waited for")
        {
            Running::Stopped { stop, .. } => match stop.reason {
                StopReason::Stepped { file, line, .. } => (file, line),
                other => panic!("expected the step to land, got {other:?}"),
            },
            other => panic!("expected the step to land, got {other:?}"),
        };
        if on {
            assert_eq!(
                landed,
                (
                    fixture.path().display().to_string(),
                    line_of(CALLBACK, "converted = {'thing': True}")
                ),
                "through `json` to the first line of the program's callback"
            );
        } else {
            assert!(
                Path::new(&landed.0).ends_with("json/__init__.py"),
                "the control: with it off, a step in lands in `json.dumps`: {landed:?}"
            );
        }
    }
}
