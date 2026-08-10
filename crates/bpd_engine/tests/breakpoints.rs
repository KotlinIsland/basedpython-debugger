//! a breakpoint binds to a real code object, and the program really stops there
//!
//! "the breakpoint is set" is the easiest thing in a debugger to claim and get
//! wrong, so nothing here takes the agent's word for it. every stop is proved
//! the way the entry stop is: by a side effect the program performs on a
//! **later** line, which has not happened while the engine is told it is
//! stopped
//!
//! the line tables are not written down either. they come from `co_lines()` in
//! a separate interpreter process, so the expected answer is whatever cpython
//! says on the machine the test is running on rather than what was true on the
//! machine the test was written on

use std::path::Path;

use bpd_core::python::Capabilities;
use bpd_core::{Binding, Resolved, Running, Site, SourceBreakpoint, Stop, StopReason, Unbound};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// a class, a method, an inlined comprehension and a generator expression
///
/// the nesting is the point: `Widget.render` is two levels down the module's
/// `co_consts` tree and the generator expression is three, so a binder that
/// does not recurse binds none of them
const WIDGET: &str = r#"import pathlib

MARKS = pathlib.Path(__file__).with_name("marks")


class Widget:
    def render(self, items):
        # a comment, which is not executable
        return [
            item * 2
            for item in items
        ]

    def lazily(self, items):
        return (item + 1 for item in items)


def main():
    widget = Widget()
    MARKS.write_text("before")
    rendered = widget.render([1, 2, 3])
    lazy = list(widget.lazily([1, 2, 3]))
    MARKS.write_text("after")
    return rendered, lazy


main()

# a trailing comment with nothing executable after it
"#;

/// the interpreter the built agent matches, or a failure saying how to get one
fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// launch a fixture and require that it stopped before running anything
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

/// no breakpoint in this file is a logpoint, so a log record would mean the
/// agent invented one
#[expect(
    clippy::needless_pass_by_value,
    reason = "it stands in for a `FnMut(LogRecord)` sink, which is handed the \
              record to own"
)]
fn unlogged(record: bpd_core::LogRecord) {
    panic!("no logpoint was set, and the agent sent {record:?}")
}

/// ask for one breakpoint per `(id, line)` on the same file
fn at(file: &Path, lines: &[(u32, u32)]) -> Vec<SourceBreakpoint> {
    lines
        .iter()
        .map(|&(id, line)| SourceBreakpoint::at(id, file, line))
        .collect()
}

/// the line and the code objects a breakpoint bound to, or the reason it did not
fn bound(resolved: &Resolved) -> (u32, &[Site]) {
    match &resolved.binding {
        Binding::Bound { line, sites, .. } => (*line, sites),
        Binding::BoundInTemplate { line, nodes, .. } => panic!(
            "breakpoint {} bound to line {line} of a django template, in \
             {nodes:?}, and this asked about a python binding",
            resolved.id
        ),
        Binding::Unbound { reason } => {
            panic!("breakpoint {} did not bind: {reason}", resolved.id)
        }
    }
}

/// why a breakpoint did not bind, or where it did
fn unbound(resolved: &Resolved) -> &Unbound {
    match &resolved.binding {
        Binding::Unbound { reason } => reason,
        Binding::Bound { line, sites, .. } => panic!(
            "breakpoint {} bound to line {line} in {sites:?}, and was not supposed to",
            resolved.id
        ),
        Binding::BoundInTemplate { line, nodes, .. } => panic!(
            "breakpoint {} bound to line {line} of a django template, in \
             {nodes:?}, and was not supposed to bind at all",
            resolved.id
        ),
    }
}

/// resume every held thread and require that the debuggee stops again
fn to_stop(debuggee: &mut Debuggee) -> (Stop, Vec<Resolved>) {
    match debuggee.run(unlogged).expect("the debuggee was resumed") {
        Running::Stopped { stop, rebound } => (stop, rebound),
        Running::Exited { status, .. } => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        Running::Finishing { threads, .. } => {
            panic!("the debuggee ended holding {threads:?} instead of stopping")
        }
    }
}

/// take every breakpoint back off and let the program finish
fn finish(mut debuggee: Debuggee) {
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");

    match debuggee.run(unlogged).expect("the debuggee was resumed") {
        Running::Exited { status, rebound } => {
            assert!(status.success(), "the program exited with {status}");
            assert!(rebound.is_empty(), "nothing is set, and got {rebound:?}");
        }
        Running::Stopped { stop, .. } => {
            panic!("every breakpoint was cleared, and it still stopped for {stop:?}")
        }
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    }
}

/// what the file's code objects say their executable lines are
///
/// ground truth from `co_lines()`, walked in python in a separate process, so
/// the expected answer never comes from the rust that is under test
fn executable_lines(path: &Path) -> Vec<u32> {
    let listing = bpd_test::eval(
        interpreter(),
        &format!(
            "import json, types\n\
             path = {path:?}\n\
             with open(path, 'rb') as handle:\n\
             \x20   code = compile(handle.read(), path, 'exec')\n\
             def walk(unit):\n\
             \x20   yield unit\n\
             \x20   for constant in unit.co_consts:\n\
             \x20       if isinstance(constant, types.CodeType):\n\
             \x20           yield from walk(constant)\n\
             lines = set()\n\
             for unit in walk(code):\n\
             \x20   lines.update(line for _, _, line in unit.co_lines() if line)\n\
             print(json.dumps(sorted(lines)))\n",
            path = path.display().to_string()
        ),
    );

    serde_json::from_str(&listing).expect("the ground truth snippet prints a json list")
}

/// the first instrumentable offset for `line`, per `co_lines()`
fn first_offset(path: &Path, qualname: &str, line: u32) -> u32 {
    let offset = bpd_test::eval(
        interpreter(),
        &format!(
            "import types\n\
             path = {path:?}\n\
             with open(path, 'rb') as handle:\n\
             \x20   code = compile(handle.read(), path, 'exec')\n\
             def walk(unit):\n\
             \x20   yield unit\n\
             \x20   for constant in unit.co_consts:\n\
             \x20       if isinstance(constant, types.CodeType):\n\
             \x20           yield from walk(constant)\n\
             starts = [\n\
             \x20   start\n\
             \x20   for unit in walk(code)\n\
             \x20   if unit.co_qualname == {qualname:?}\n\
             \x20   for start, _, line in unit.co_lines()\n\
             \x20   if line == {line}\n\
             ]\n\
             print(min(starts))\n",
            path = path.display().to_string()
        ),
    );

    offset
        .parse()
        .expect("the ground truth snippet prints one integer")
}

#[test]
fn a_breakpoint_inside_a_comprehension_inside_a_method_binds_and_stops() {
    let fixture = Fixture::new("widget", WIDGET);
    let marks = fixture.directory().join("marks");
    // the comprehension is inlined into its method on 3.12 and later, so the
    // code object that holds this line is the method's — which is still two
    // levels down the module's `co_consts`, and unreachable without recursing
    let inside = line_of(WIDGET, "item * 2");

    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(at(&fixture.path(), &[(1, inside)]))
        .expect("the breakpoint request was answered");

    let (line, sites) = bound(&resolved[0]);
    assert_eq!(
        line, inside,
        "the line is executable and must not have moved"
    );
    assert_eq!(
        sites
            .iter()
            .map(|site| site.qualname.as_str())
            .collect::<Vec<_>>(),
        ["Widget.render"]
    );

    let (stop, _) = to_stop(&mut debuggee);
    let StopReason::Breakpoint {
        breakpoints,
        file,
        line,
        ..
    } = &stop.reason
    else {
        panic!("expected a breakpoint stop, got {stop:?}")
    };
    assert_eq!(breakpoints, &[1]);
    assert_eq!(*line, inside);
    assert_eq!(Path::new(file), fixture.path());

    // the proof that the stop is real: the program writes "before" on a line
    // above this one and "after" on a line below it
    assert_eq!(
        std::fs::read_to_string(&marks).expect("the program reached the line above"),
        "before",
        "the program had already run past the breakpoint when the engine was \
         told it was stopped"
    );

    finish(debuggee);
    assert_eq!(
        std::fs::read_to_string(&marks).expect("the program ran to the end"),
        "after"
    );
}

#[test]
fn a_breakpoint_fires_on_every_pass_over_the_line() {
    let fixture = Fixture::new("widget", WIDGET);
    let inside = line_of(WIDGET, "item * 2");

    let mut debuggee = launch(&fixture);
    bound(
        &debuggee
            .set_breakpoints(at(&fixture.path(), &[(1, inside)]))
            .expect("the breakpoint request was answered")[0],
    );

    // the comprehension runs over three items, so a breakpoint on its body is
    // reached three times. one stop would mean the first pass disabled the line
    let mut stops = 0;
    loop {
        match debuggee.run(unlogged).expect("the debuggee was resumed") {
            Running::Stopped { .. } => stops += 1,
            Running::Exited { status, .. } => {
                assert!(status.success(), "the program exited with {status}");
                break;
            }
            Running::StillRunning { waited, .. } => unreachable!(
                "this wait carries no deadline and was answered after {waited:?} \
                 with the program still running"
            ),
            Running::Finishing { threads, .. } => {
                panic!("nothing was held, and the debuggee ended holding {threads:?}")
            }
        }
    }
    assert_eq!(stops, 3);
}

#[test]
fn a_generator_expression_is_its_own_code_object_and_binds_there() {
    let fixture = Fixture::new("widget", WIDGET);
    let genexp = line_of(WIDGET, "item + 1 for item in items");

    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(at(&fixture.path(), &[(1, genexp)]))
        .expect("the breakpoint request was answered");

    // that line belongs to two code objects at once: the method builds the
    // generator, and the generator's own body runs it. both are armed, because
    // binding to only the one that is easy to find is how a breakpoint in a
    // comprehension silently never fires
    let (line, sites) = bound(&resolved[0]);
    assert_eq!(line, genexp);
    assert_eq!(
        sites
            .iter()
            .map(|site| site.qualname.as_str())
            .collect::<Vec<_>>(),
        ["Widget.lazily", "Widget.lazily.<locals>.<genexpr>"]
    );

    for site in sites {
        assert_eq!(
            site.offset,
            first_offset(&fixture.path(), &site.qualname, genexp),
            "the offset reported for {} must be the first one the line covers",
            site.qualname
        );
    }

    finish(debuggee);
}

#[test]
fn a_breakpoint_on_a_comment_says_which_line_it_moved_to() {
    let fixture = Fixture::new("widget", WIDGET);
    let comment = line_of(WIDGET, "# a comment, which is not executable");
    let statement = line_of(WIDGET, "return [");

    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(at(&fixture.path(), &[(1, comment)]))
        .expect("the breakpoint request was answered");

    let (line, sites) = bound(&resolved[0]);
    assert_eq!(
        line, statement,
        "a breakpoint on a comment moves to the next executable line, and the \
         answer has to say so"
    );
    assert_eq!(
        sites
            .iter()
            .map(|site| site.qualname.as_str())
            .collect::<Vec<_>>(),
        ["Widget.render"]
    );

    let (stop, _) = to_stop(&mut debuggee);
    let StopReason::Breakpoint { line, .. } = &stop.reason else {
        panic!("expected a breakpoint stop, got {stop:?}")
    };
    assert_eq!(*line, statement, "it stopped where it said it would");

    finish(debuggee);
}

#[test]
fn two_breakpoints_that_land_on_one_line_are_both_reported() {
    let fixture = Fixture::new("widget", WIDGET);
    let comment = line_of(WIDGET, "# a comment, which is not executable");
    let statement = line_of(WIDGET, "return [");

    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(at(&fixture.path(), &[(1, comment), (2, statement)]))
        .expect("the breakpoint request was answered");
    assert_eq!(bound(&resolved[0]).0, statement);
    assert_eq!(bound(&resolved[1]).0, statement);

    let (stop, _) = to_stop(&mut debuggee);
    let StopReason::Breakpoint { breakpoints, .. } = &stop.reason else {
        panic!("expected a breakpoint stop, got {stop:?}")
    };
    assert_eq!(
        breakpoints,
        &[1, 2],
        "one line held both breakpoints, and reporting one of them would leave \
         the client believing the other never fired"
    );

    finish(debuggee);
}

#[test]
fn the_lines_that_can_hold_a_breakpoint_are_exactly_the_ones_co_lines_yields() {
    let fixture = Fixture::new("widget", WIDGET);
    let executable = executable_lines(&fixture.path());
    let total = u32::try_from(WIDGET.lines().count()).expect("the fixture is short");

    // from zero, because cpython attributes a module's leading `RESUME` to line
    // 0 and that line never fires. binding to it would be a breakpoint reported
    // as set that could not possibly be reached
    let requested: Vec<(u32, u32)> = (0..=total + 2).map(|line| (line, line)).collect();
    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(at(&fixture.path(), &requested))
        .expect("the breakpoint request was answered");

    let last = *executable.last().expect("the fixture has executable lines");
    for resolution in &resolved {
        let asked = resolution.id;
        let expected = executable.iter().copied().find(|line| *line >= asked);

        match expected {
            // every line at or below the last executable one binds, and binds
            // to the first executable line at or after it — that single rule is
            // both the line table and the rule for moving off a blank line
            Some(expected) => assert_eq!(
                bound(resolution).0,
                expected,
                "a breakpoint asked for on line {asked}"
            ),
            None => {
                let reason = unbound(resolution);
                assert!(
                    matches!(
                        reason,
                        Unbound::NoExecutableLine {
                            requested,
                            last_executable: Some(reported),
                            ..
                        } if *requested == asked && *reported == last
                    ),
                    "line {asked} is past the end of the code and got {reason}"
                );
            }
        }
    }

    finish(debuggee);
}

#[test]
fn a_breakpoint_in_a_module_that_is_not_imported_yet_is_unbound_and_then_binds() {
    const LATE: &str = "def helped(value):\n    return value + 1\n";
    const IMPORTS_LATE: &str = r#"import pathlib

MARKS = pathlib.Path(__file__).with_name("marks")
MARKS.write_text("before")
import late
MARKS.write_text("imported")
late.helped(1)
MARKS.write_text("after")
"#;

    let fixture = Fixture::new("imports_late", IMPORTS_LATE);
    let late = fixture.sibling("late", LATE);
    let marks = fixture.directory().join("marks");
    let inside = line_of(LATE, "return value + 1");

    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(at(&late, &[(1, inside)]))
        .expect("the breakpoint request was answered");

    // it is a real file that will really be imported, and it is still reported
    // unbound, because nothing has compiled it yet
    let reason = unbound(&resolved[0]);
    assert!(
        matches!(reason, Unbound::NotLoaded { file, .. } if file == &late),
        "expected the module to be reported as not loaded, got {reason}"
    );

    let (stop, rebound) = to_stop(&mut debuggee);
    assert_eq!(
        rebound.len(),
        1,
        "the client has to be told, unprompted, that importing the module bound \
         it — got {rebound:?}"
    );
    assert_eq!(rebound[0].id, 1);
    assert_eq!(bound(&rebound[0]).0, inside);

    let StopReason::Breakpoint { file, line, .. } = &stop.reason else {
        panic!("expected a breakpoint stop, got {stop:?}")
    };
    assert_eq!(Path::new(file), late);
    assert_eq!(*line, inside);
    assert_eq!(
        std::fs::read_to_string(&marks).expect("the program reached the import"),
        "imported"
    );

    finish(debuggee);
}

#[test]
fn a_breakpoint_binds_in_a_function_exec_built_after_the_program_started() {
    const GENERATED: &str = "def produced(value):\n    return value * 3\n";
    const EXECS: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent
MARKS = HERE / "marks"
MARKS.write_text("before")
SOURCE = HERE / "generated.py"
namespace = {}
exec(compile(SOURCE.read_text(), str(SOURCE), "exec"), namespace)
MARKS.write_text("compiled")
namespace["produced"](2)
MARKS.write_text("after")
"#;

    let fixture = Fixture::new("execs", EXECS);
    let generated = fixture.sibling("generated", GENERATED);
    let marks = fixture.directory().join("marks");
    let inside = line_of(GENERATED, "return value * 3");

    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(at(&generated, &[(1, inside)]))
        .expect("the breakpoint request was answered");
    assert!(matches!(unbound(&resolved[0]), Unbound::NotLoaded { .. }));

    // that file is never imported. the only way its code object is ever seen is
    // the global `PY_START` that registers every code object on its first call,
    // which is the only notification PEP 669 offers for code that did not exist
    // when the program started
    let (stop, rebound) = to_stop(&mut debuggee);
    assert_eq!(rebound.len(), 1, "got {rebound:?}");
    assert_eq!(bound(&rebound[0]).0, inside);

    let StopReason::Breakpoint { file, line, .. } = &stop.reason else {
        panic!("expected a breakpoint stop, got {stop:?}")
    };
    assert_eq!(Path::new(file), generated);
    assert_eq!(*line, inside);
    assert_eq!(
        std::fs::read_to_string(&marks).expect("the program reached the exec"),
        "compiled"
    );

    finish(debuggee);
}

#[test]
fn a_file_the_interpreter_never_loads_binds_nothing_and_says_which() {
    let fixture = Fixture::new("widget", WIDGET);
    let never = fixture.sibling("never_imported", "value = 1\n");
    let missing = fixture.directory().join("no_such_file.py");

    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, never.clone(), 1),
            SourceBreakpoint::at(2, missing.clone(), 1),
            SourceBreakpoint::at(3, fixture.directory(), 1),
        ])
        .expect("the breakpoint request was answered");

    let reason = unbound(&resolved[0]);
    assert!(
        matches!(reason, Unbound::NotLoaded { file, .. } if file == &never),
        "a real file nothing imports is not loaded, and got {reason}"
    );

    let reason = unbound(&resolved[1]);
    assert!(
        matches!(
            reason,
            Unbound::Unresolvable { file, loaded_under_that_name: false, .. } if file == &missing
        ),
        "a path that is not there at all is unresolvable, and got {reason}"
    );

    let reason = unbound(&resolved[2]);
    assert!(
        matches!(reason, Unbound::Unresolvable { reason, .. } if reason.contains("regular file")),
        "a directory is not somewhere a breakpoint can live, and got {reason}"
    );

    finish(debuggee);
}

#[test]
fn a_breakpoint_in_a_zipimported_module_is_refused_with_the_reason() {
    const PACKS: &str = r#"import pathlib, sys, zipfile

HERE = pathlib.Path(__file__).parent
ARCHIVE = HERE / "packed.zip"
with zipfile.ZipFile(ARCHIVE, "w") as archive:
    archive.writestr("packed_mod.py", "def f():\n    return 1\n")
sys.path.insert(0, str(ARCHIVE))
import packed_mod
(HERE / "zipname").write_text(packed_mod.f.__code__.co_filename)
packed_mod.f()
(HERE / "marks").write_text("after")
"#;

    let fixture = Fixture::new("packs", PACKS);
    let called = line_of(PACKS, "packed_mod.f()");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(at(&fixture.path(), &[(1, called)]))
        .expect("the breakpoint request was answered");
    to_stop(&mut debuggee);

    // the program reports the spelling the interpreter gave the module, so the
    // test asks about exactly the location that exists rather than one it made
    // up. it is not a file on disk, and no amount of resemblance makes it one
    let inside_the_zip = std::fs::read_to_string(fixture.directory().join("zipname"))
        .expect("the program imported the zipped module");

    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(2, inside_the_zip, 2)])
        .expect("the breakpoint request was answered");

    let reason = unbound(&resolved[0]);
    assert!(
        matches!(
            reason,
            Unbound::Unresolvable {
                loaded_under_that_name: true,
                ..
            }
        ),
        "the refusal has to say that the code is real and the file is not, and \
         it said {reason}"
    );
    assert!(
        reason.to_string().contains("zip archive"),
        "the reason has to name what to do about it, and it said {reason}"
    );

    finish(debuggee);
}

#[test]
fn a_file_with_no_executable_lines_holds_no_breakpoint() {
    const IMPORTS_BLANK: &str = r#"import pathlib
import blank

(pathlib.Path(__file__).parent / "marks").write_text("after")
"#;
    const BLANK: &str = "# nothing here\n# and nothing here either\n";

    let fixture = Fixture::new("imports_blank", IMPORTS_BLANK);
    let blank = fixture.sibling("blank", BLANK);
    let after_the_import = line_of(IMPORTS_BLANK, "write_text");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(at(&fixture.path(), &[(1, after_the_import)]))
        .expect("the breakpoint request was answered");
    to_stop(&mut debuggee);

    // the module is loaded by now, and it still cannot hold a breakpoint. that
    // is a different answer from "not loaded", and saying so is the difference
    // between a user waiting for a stop and a user editing the right file
    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(2, blank.clone(), 1)])
        .expect("the breakpoint request was answered");

    let reason = unbound(&resolved[0]);
    assert!(
        matches!(
            reason,
            Unbound::NoExecutableLine {
                file,
                last_executable: None,
                ..
            } if file == &blank
        ),
        "a file of nothing but comments has no executable line at all, and got {reason}"
    );

    finish(debuggee);
}

#[test]
fn a_path_that_differs_only_in_case_is_never_bound_to_a_different_file() {
    let fixture = Fixture::new("widget", WIDGET);
    let inside = line_of(WIDGET, "item * 2");
    let shouted = fixture.directory().join("WIDGET.py");

    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), inside),
            SourceBreakpoint::at(2, shouted, inside),
        ])
        .expect("the breakpoint request was answered");

    // whether those two spellings are one file is the filesystem's decision, not
    // the debugger's, and it differs between a mac and a linux box. both answers
    // are asserted, because the wrong one — binding to some *other* file, or
    // binding a path that cannot be opened — is neither
    match &resolved[1].binding {
        Binding::Bound { .. } => assert_eq!(
            bound(&resolved[1]),
            bound(&resolved[0]),
            "the filesystem says those are one file, so they are one breakpoint"
        ),
        Binding::Unbound { reason } => assert!(
            matches!(reason, Unbound::Unresolvable { .. }),
            "the filesystem says that path is not there, and got {reason}"
        ),
        Binding::BoundInTemplate { nodes, .. } => {
            panic!("a python file bound as a django template, in {nodes:?}")
        }
    }

    finish(debuggee);
}

#[test]
fn discovery_is_turned_off_while_nothing_is_set() {
    const OBSERVES: &str = r#"import pathlib, sys

HERE = pathlib.Path(__file__).parent


def observe():
    (HERE / "events").write_text(
        "%d" % sys.monitoring.get_events(sys.monitoring.DEBUGGER_ID)
    )


(HERE / "marks").write_text("before")
observe()
"#;

    let events = |fixture: &Fixture| {
        std::fs::read_to_string(fixture.directory().join("events"))
            .expect("the program reported what it was instrumented with")
    };
    let py_start = bpd_test::eval(
        interpreter(),
        "import sys; print(sys.monitoring.events.PY_START)",
    );

    // with nothing set there is nothing that could ever stop, so the discovery
    // callback is taken back off and the program runs uninstrumented
    let fixture = Fixture::new("observes", OBSERVES);
    let debuggee = launch(&fixture);
    finish(debuggee);
    assert_eq!(events(&fixture), "0");

    // with something set it stays on, because that is the only way a module
    // imported later can ever bind
    let fixture = Fixture::new("observes", OBSERVES);
    let before = line_of(OBSERVES, r#"(HERE / "marks").write_text("before")"#);
    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(at(&fixture.path(), &[(1, before)]))
        .expect("the breakpoint request was answered");
    to_stop(&mut debuggee);

    match debuggee.run(unlogged).expect("the debuggee was resumed") {
        Running::Exited { status, .. } => assert!(status.success()),
        Running::Stopped { stop, .. } => panic!("it stopped again for {stop:?}"),
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    }
    assert_eq!(events(&fixture), py_start);
}

#[cfg(unix)]
#[test]
fn a_breakpoint_set_through_a_symlink_binds_the_same_code_object() {
    // creating a symlink on windows needs an elevated process or developer
    // mode, so this is a unix test rather than a test that quietly does nothing
    let fixture = Fixture::new("widget", WIDGET);
    let elsewhere = tempfile::tempdir().expect("a temporary directory is available");
    let link = elsewhere.path().join("Linked.py");
    std::os::unix::fs::symlink(fixture.path(), &link).expect("the symlink was created");

    let inside = line_of(WIDGET, "item * 2");
    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), inside),
            SourceBreakpoint::at(2, link, inside),
        ])
        .expect("the breakpoint request was answered");

    assert_eq!(
        bound(&resolved[0]),
        bound(&resolved[1]),
        "a path through a symlink and the real path are one file, and a \
         breakpoint set through either is one breakpoint"
    );

    let (stop, _) = to_stop(&mut debuggee);
    let StopReason::Breakpoint { breakpoints, .. } = &stop.reason else {
        panic!("expected a breakpoint stop, got {stop:?}")
    };
    assert_eq!(breakpoints, &[1, 2]);

    finish(debuggee);
}

#[test]
fn a_stop_names_the_thread_that_hit_the_breakpoint() {
    const THREADED: &str = r#"import pathlib, threading

HERE = pathlib.Path(__file__).parent


def work():
    (HERE / "ident").write_text(str(threading.get_ident()))
    (HERE / "marks").write_text("in the thread")


worker = threading.Thread(target=work)
worker.start()
worker.join()
"#;

    let fixture = Fixture::new("threaded", THREADED);
    let inside = line_of(THREADED, r#"(HERE / "marks").write_text("in the thread")"#);

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(at(&fixture.path(), &[(1, inside)]))
        .expect("the breakpoint request was answered");

    let (stop, _) = to_stop(&mut debuggee);
    assert!(
        matches!(stop.reason, StopReason::Breakpoint { .. }),
        "expected a breakpoint stop, got {stop:?}"
    );

    // the program said which thread it was, on the line above the breakpoint.
    // the stop names the one thread it holds, and every other thread in the
    // process is still running — which is the model, not a shortfall of it
    let reported = std::fs::read_to_string(fixture.directory().join("ident"))
        .expect("the worker thread reached the line above");
    assert_eq!(reported, stop.thread.to_string());

    finish(debuggee);
}

#[test]
fn a_breakpoint_moved_onto_a_line_that_already_ran_still_fires() {
    const TWICE: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent


def twice(value):
    first = value + 1
    second = first + 1
    return second


twice(1)
(HERE / "between").write_text("between")
twice(2)
(HERE / "marks").write_text("after")
"#;

    let fixture = Fixture::new("twice", TWICE);
    let first = line_of(TWICE, "first = value + 1");
    let second = line_of(TWICE, "second = first + 1");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(at(&fixture.path(), &[(1, second)]))
        .expect("the breakpoint request was answered");

    let (stop, _) = to_stop(&mut debuggee);
    let StopReason::Breakpoint { line, .. } = &stop.reason else {
        panic!("expected a breakpoint stop, got {stop:?}")
    };
    assert_eq!(*line, second);

    // the line above has already run once and, not being a breakpoint, told the
    // interpreter never to report it again. moving the breakpoint onto it has to
    // undo that — PEP 669 has no per-location undo, so the whole process has its
    // disabled locations restarted
    debuggee
        .set_breakpoints(at(&fixture.path(), &[(1, first)]))
        .expect("the breakpoint request was answered");

    let (stop, _) = to_stop(&mut debuggee);
    let StopReason::Breakpoint { line, .. } = &stop.reason else {
        panic!("expected a breakpoint stop, got {stop:?}")
    };
    assert_eq!(
        *line, first,
        "a line that had already reported once was never offered again"
    );
    assert!(
        fixture.directory().join("between").exists(),
        "the stop must be the second call, not a leftover from the first"
    );

    finish(debuggee);
}

#[test]
fn clearing_a_breakpoint_stops_it_firing() {
    let fixture = Fixture::new("widget", WIDGET);
    let marks = fixture.directory().join("marks");
    let inside = line_of(WIDGET, "item * 2");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(at(&fixture.path(), &[(1, inside)]))
        .expect("the breakpoint request was answered");

    // `finish` clears the set before resuming. the comprehension would
    // otherwise stop three times, so reaching the end at all is the assertion
    finish(debuggee);
    assert_eq!(
        std::fs::read_to_string(&marks).expect("the program ran to the end"),
        "after"
    );
}

#[test]
fn two_breakpoints_cannot_share_one_id() {
    let fixture = Fixture::new("widget", WIDGET);
    let inside = line_of(WIDGET, "item * 2");

    let mut debuggee = launch(&fixture);
    let error = debuggee
        .set_breakpoints(at(&fixture.path(), &[(7, inside), (7, inside + 1)]))
        .expect_err("an id names one breakpoint");
    assert!(
        error.to_string().contains("id 7"),
        "the refusal must name the id, and it said {error}"
    );

    finish(debuggee);
}

#[test]
fn a_debuggee_that_has_exited_says_so_rather_than_that_nothing_is_held() {
    // "nothing is held" invites holding something, and there is nothing left to
    // hold. a client told the weaker of the two would go on pausing a process
    // that is not there
    let fixture = Fixture::new("widget", WIDGET);
    let mut debuggee = launch(&fixture);

    match debuggee.run(unlogged).expect("the debuggee was resumed") {
        Running::Exited { .. } => {}
        Running::Stopped { stop, .. } => panic!("nothing was set, got {stop:?}"),
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    }

    let error = debuggee
        .set_breakpoints(at(&fixture.path(), &[(1, 1)]))
        .expect_err("a finished debuggee cannot bind anything");
    let said = error.to_string();
    assert!(
        said.contains("the program has exited with 0"),
        "the refusal must name what became of the program, and it said {said}"
    );
    assert!(
        said.contains("the breakpoints to resolve"),
        "the refusal must name what was asked for, and it said {said}"
    );
}

#[test]
fn a_running_debuggee_refuses_a_request_rather_than_leaving_it_unanswered() {
    // still alive, and still unaskable: the agent answers on a thread it is
    // holding, so a request made here would be answered whenever the program
    // next happened to stop — which is indistinguishable from a hang
    let fixture = Fixture::new("sleeper", SLEEPER);
    let mut debuggee = launch(&fixture);

    let running = debuggee
        .dispatch(
            bpd_core::Request::Run {
                deadline: Some(std::time::Duration::from_millis(100)),
            },
            &mut Nothing,
        )
        .expect("the debuggee was resumed");
    match running {
        bpd_core::Response::Ran(Running::StillRunning { .. }) => {}
        other => panic!("this program sleeps for a second, and the run answered {other:?}"),
    }

    let error = debuggee
        .set_breakpoints(at(&fixture.path(), &[(1, 1)]))
        .expect_err("a running debuggee has no thread to bind on");
    let said = error.to_string();
    assert!(
        said.contains("no thread of the debuggee is held"),
        "the refusal must say why, and it said {said}"
    );
    assert!(
        said.contains("pausing it"),
        "the refusal must say what to do about it, and it said {said}"
    );

    debuggee
        .interrupt()
        .terminate()
        .expect("the sleeping program was ended");
}

/// a program that is still running a moment after it is let go
const SLEEPER: &str = "import time\n\ntime.sleep(1)\n";

/// a `Reporting` that is asked for nothing, since nothing here logs or pauses
struct Nothing;

impl bpd_core::Reporting for Nothing {
    fn logged(&mut self, record: bpd_core::LogRecord) {
        panic!("nothing here sets a logpoint, and one recorded {record:?}")
    }

    fn pausing(&mut self, running: Vec<u64>) {
        panic!("nothing here arms a pause, and one acknowledged {running:?}")
    }
}
