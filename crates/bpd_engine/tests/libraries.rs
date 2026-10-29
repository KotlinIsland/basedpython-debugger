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

/// raises of four classes, one of them a library's
const CLASSES: &str = r#"import json


def main():
    try:
        {}["missing"]  # a KeyError
    except KeyError:
        pass
    try:
        next(iter([]))  # a StopIteration
    except StopIteration:
        pass
    try:
        raise ValueError("mine")  # a ValueError
    except ValueError:
        pass
    try:
        json.loads("{")  # into json
    except ValueError:
        pass


ready = 1
main()
"#;

fn rule(name: &str, raised: bool, libraries: bool) -> bpd_core::ExceptionClass {
    bpd_core::ExceptionClass {
        name: name.to_string(),
        raised,
        uncaught: false,
        libraries,
    }
}

#[test]
fn a_rule_for_a_class_decides_for_it_and_its_subclasses_in_place_of_the_filters() {
    let fixture = Fixture::new("classes", CLASSES);
    let program = fixture.path().display().to_string();

    // the filter is on for everything, and the rules carve two classes out of
    // it: `LookupError` decides for the `KeyError`, and `StopIteration` is left
    // out of the filter
    let mut debuggee = launch(&fixture);
    held_at(&mut debuggee, &fixture, line_of(CLASSES, "ready = 1"));
    let set = debuggee
        .set_exception_rules(
            true,
            false,
            vec![
                rule("LookupError", true, true),
                rule("StopIteration", false, true),
            ],
        )
        .expect("the exception breakpoints were set");
    assert_eq!(
        set.classes.len(),
        2,
        "the agent holds what was set: {set:?}"
    );
    let all = raises(&mut debuggee);
    assert!(
        !all.iter().any(|(kind, ..)| kind == "StopIteration"),
        "no stop for the `StopIteration` the rule left out: {all:?}"
    );
    let seen: Vec<(String, u32)> = all
        .into_iter()
        .filter(|(_, file, _)| *file == program)
        .map(|(kind, _, line)| (kind, line))
        .collect();
    assert_eq!(
        seen,
        [
            ("KeyError".to_string(), line_of(CLASSES, "# a KeyError")),
            ("ValueError".to_string(), line_of(CLASSES, "# a ValueError")),
        ],
        "the `KeyError` by the rule for `LookupError`, the `ValueError` by the \
         filter — and the one `json` raised was a stop inside `json`, where the \
         filter with just-my-code off stops for it"
    );
}

const NAMES: &str = r"import json
import os

looked = []


class Loud(type):
    def __getattribute__(cls, name):
        looked.append(name)
        return super().__getattribute__(name)


class Mine(Exception):
    class Inner(Exception):
        pass


class Watched(Exception, metaclass=Loud):
    class Inner(Exception):
        pass


class Plain:
    pass


Alias = Mine


def function():
    pass


ready = 1
";

#[test]
fn what_a_rules_name_finds_is_read_off_the_program_without_importing_or_running_anything() {
    use bpd_core::ClassFound;

    let fixture = Fixture::new("names", NAMES);
    let mut debuggee = launch(&fixture);
    held_at(&mut debuggee, &fixture, line_of(NAMES, "ready = 1"));

    let names = [
        "LookupError",
        "json.decoder.JSONDecodeError",
        "__main__.Mine.Inner",
        "__main__.Watched.Inner",
        "__main__.Alias",
        "os.error",
        "__main__.function",
        "__main__.Plain",
        "NoSuchError",
        "email.errors.MessageError",
        "__main__..Mine",
    ];
    let set = debuggee
        .set_exception_rules(
            false,
            false,
            names.iter().map(|name| rule(name, true, true)).collect(),
        )
        .expect("the exception breakpoints were set");
    let found: Vec<(&str, ClassFound)> = set
        .classes
        .iter()
        .map(|armed| (armed.rule.name.as_str(), armed.found.clone()))
        .collect();
    let unresolved = |why: &str| ClassFound::Unresolved {
        why: why.to_string(),
    };
    assert_eq!(
        found,
        [
            ("LookupError", ClassFound::Class),
            ("json.decoder.JSONDecodeError", ClassFound::Class),
            ("__main__.Mine.Inner", ClassFound::Class),
            ("__main__.Watched.Inner", ClassFound::Class),
            (
                "__main__.Alias",
                ClassFound::Renamed {
                    named: "__main__.Mine".to_string()
                }
            ),
            (
                "os.error",
                ClassFound::Renamed {
                    named: "OSError".to_string()
                }
            ),
            (
                "__main__.function",
                ClassFound::NotAnException {
                    what: "a `function`, not a class".to_string()
                }
            ),
            (
                "__main__.Plain",
                ClassFound::NotAnException {
                    what: "a class that does not derive from `BaseException`".to_string()
                }
            ),
            ("NoSuchError", unresolved("`builtins` has no `NoSuchError`")),
            (
                "email.errors.MessageError",
                unresolved("`email.errors` is not imported; `email` is not imported")
            ),
            (
                "__main__..Mine",
                unresolved("`__main__..Mine` is not a dotted name, so nothing can be called it")
            ),
        ],
        "every rule is held, and each says what its name is in the program now"
    );

    // looking was all it did: the module it did not find is still not
    // imported, and the metaclass that records every attribute asked of its
    // class was asked nothing
    let frame = debuggee
        .the_stack(Some(1))
        .expect("the stack was answered")
        .frames[0]
        .id;
    for (expression, expected) in [
        ("'email.errors' in __import__('sys').modules", false),
        ("looked == []", true),
    ] {
        match debuggee
            .evaluate(frame, expression, bpd_core::Detail::default())
            .expect("the expression was evaluated")
        {
            bpd_core::Evaluated::Value { value } => assert_eq!(
                value.content,
                bpd_core::Content::Bool { value: expected },
                "`{expression}` after the rules were set"
            ),
            bpd_core::Evaluated::Raised { error } => panic!("`{expression}` raised {error}"),
        }
    }
}

#[test]
fn a_rule_that_ignores_library_files_stops_where_a_libraries_raise_arrives() {
    let fixture = Fixture::new("classes", CLASSES);
    let program = fixture.path().display().to_string();

    for libraries in [false, true] {
        let mut debuggee = launch(&fixture);
        held_at(&mut debuggee, &fixture, line_of(CLASSES, "ready = 1"));
        debuggee
            .set_exception_rules(false, false, vec![rule("ValueError", true, libraries)])
            .expect("the exception breakpoints were set");
        let seen = raises(&mut debuggee);
        let decoded = seen
            .iter()
            .find(|(kind, ..)| kind.ends_with("JSONDecodeError"))
            .unwrap_or_else(|| {
                panic!("the rule for `ValueError` decides for its subclass: {seen:?}")
            });
        if libraries {
            assert_ne!(decoded.1, program, "where `json` raised it: {seen:?}");
        } else {
            assert_eq!(
                (&decoded.1, decoded.2),
                (&program, line_of(CLASSES, "# into json")),
                "where it first arrives in the program's own code"
            );
        }
        assert!(
            !seen.iter().any(|(kind, ..)| kind == "KeyError"),
            "no rule and no filter stops a `KeyError`: {seen:?}"
        );
    }
}

#[test]
fn a_command_programs_own_code_is_the_programs_though_it_is_not_a_file() {
    // `python -c` compiles the program under `<string>`, which is also what
    // `dataclasses` compiles the methods it writes under. the command's code is
    // the program's by identity — bpd compiled it — so its raise stops where it
    // is; a frozen dataclass's `__setattr__` is a library's, and what it raises
    // stops where it arrives in the command
    const COMMAND: &str = "import dataclasses\n\
                           \n\
                           @dataclasses.dataclass(frozen=True)\n\
                           class Pair:\n    a: int\n\
                           \n\
                           def mine():\n    raise ValueError('the command raises')\n\
                           \n\
                           try:\n    Pair(1).a = 2  # frozen\nexcept AttributeError:\n    pass\n\
                           try:\n    mine()\nexcept ValueError:\n    pass\n";
    let assigned = line_of(COMMAND, "# frozen");
    let raised = line_of(COMMAND, "raise ValueError");
    for on in [true, false] {
        let mut debuggee = match bpd_engine::launch(
            bpd_test::agent::matching_interpreter(),
            &bpd_engine::Program::Command(COMMAND.to_string()),
            &[] as &[OsString],
        ) {
            Ok(Launched::Stopped(debuggee)) => debuggee,
            other => panic!("the command did not stop at its entry: {other:?}"),
        };
        debuggee.just_my_code(on).expect("just-my-code was set");
        debuggee
            .set_exception_breakpoints(true, false)
            .expect("the exception breakpoints were set");
        let seen: Vec<(String, u32)> = raises(&mut debuggee)
            .into_iter()
            .filter(|(_, file, _)| file == "<string>")
            .map(|(kind, _, line)| (kind, line))
            .collect();
        if on {
            assert_eq!(
                seen,
                [
                    ("dataclasses.FrozenInstanceError".to_string(), assigned),
                    ("ValueError".to_string(), raised),
                ],
                "the command's own raise, and the library's where it arrives"
            );
        } else {
            assert!(
                seen.contains(&("ValueError".to_string(), raised)),
                "{seen:?}"
            );
            assert!(
                seen.iter()
                    .any(|(kind, line)| kind == "dataclasses.FrozenInstanceError"
                        && *line != assigned),
                "the control: off, the dataclass's own code raises it: {seen:?}"
            );
        }
    }
}
