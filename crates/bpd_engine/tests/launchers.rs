//! a program started through a launcher, whose frames are not the program's
//!
//! `by run` starts `<python> _by_runner.py <module>`, and the runner makes the
//! module `__main__` and runs it. to a debugger that runs the runner as a
//! script, the runner **is** the program: its three frames were the outermost
//! of every stack, a step over the program's last line went on through them,
//! and an exception the program did not catch was reported from the runner's
//! `<module>` — measured against a real build before this existed
//!
//! the launcher here is that shape and nothing more — a hook that imports one
//! module itself, and a function that makes the program `__main__` and runs it —
//! so what is tested is the rule rather than one runner. every assertion is made
//! twice: once with the script named as a launcher, and once as an ordinary
//! script, which is the control that proves the launcher's frames are there to
//! be left out

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use bpd_core::python::Capabilities;
use bpd_core::{Binding, Running, SourceBreakpoint, StepKind, StopReason};
use bpd_engine::{Debuggee, Launched, Program};
use bpd_test::debuggee::{Fixture, line_of};

/// a launcher in the shape of `by run`'s runner
///
/// `helper` is imported through a loader of the launcher's own, which puts a
/// launcher frame in the **middle** of the program's stack: the program's
/// `import`, the import system, the launcher's `exec_module`, and the helper's
/// module. that frame is reported where it is, and a step never lands in it
const LAUNCHER: &str = r#"import importlib.machinery
import importlib.util
import os
import sys
import types

HERE = os.path.dirname(os.path.abspath(__file__))


class Loader:
    def create_module(self, spec):
        return None

    def exec_module(self, module):
        with open(module.__spec__.origin) as handle:
            code = compile(handle.read(), module.__spec__.origin, "exec")
        exec(code, module.__dict__)


class Hook:
    def find_spec(self, name, path=None, target=None):
        if name != "helper":
            return None
        return importlib.util.spec_from_file_location(
            name, os.path.join(HERE, "helper.py"), loader=Loader()
        )


def run_as_main(name):
    spec = importlib.util.find_spec(name)
    code = spec.loader.get_code(name)
    main = types.ModuleType("__main__")
    main.__file__ = spec.origin
    sys.modules["__main__"] = main
    exec(code, main.__dict__)


def main():
    sys.meta_path.insert(0, Hook())
    name = sys.argv[1]
    sys.argv = sys.argv[1:]
    run_as_main(name)


main()
"#;

/// the program the launcher runs
const PROGRAM: &str = r"import helper


def work(value):
    doubled = value * 2
    return doubled


answer = work(helper.BASE)
last = answer + 1
";

/// a module the launcher's own loader imports
const HELPER: &str = r"BASE = 20
ready = BASE + 1
";

/// a program that raises and catches nothing
const RAISING: &str = r#"def fail():
    raise ValueError("nothing catches this")


fail()  # leaves the program
"#;

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// the launcher, the program and its helper, written side by side
fn written() -> (Fixture, PathBuf, PathBuf) {
    let fixture = Fixture::new("launcher", LAUNCHER);
    let program = fixture.sibling("program", PROGRAM);
    let helper = fixture.sibling("helper", HELPER);
    fixture.sibling("raising", RAISING);
    (fixture, program, helper)
}

/// start the launcher on `module`, as a launcher or as an ordinary script
fn launch(fixture: &Fixture, module: &str, as_launcher: bool) -> Debuggee {
    let program = if as_launcher {
        Program::Launcher(fixture.path())
    } else {
        Program::Script(fixture.path())
    };
    match bpd_engine::launch(interpreter(), &program, &[OsString::from(module)]) {
        Ok(Launched::Stopped(debuggee)) => debuggee,
        Ok(Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

/// run on from the entry stop to a breakpoint at `line` of `file`
fn held_at(debuggee: &mut Debuggee, file: &Path, line: u32) {
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
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => assert!(
            matches!(stop.reason, StopReason::Breakpoint { .. }),
            "expected the breakpoint, got {:?}",
            stop.reason
        ),
        other => panic!("expected the breakpoint, got {other:?}"),
    }
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
}

/// every frame of the held stack, outermost last, as `(function, file)`
fn stack_of(debuggee: &mut Debuggee) -> Vec<(String, PathBuf)> {
    debuggee
        .the_stack(None)
        .expect("the stack was answered")
        .frames
        .iter()
        .map(|frame| (frame.name().to_string(), PathBuf::from(&frame.file)))
        .collect()
}

#[test]
fn no_stack_reports_the_frames_of_the_launcher_the_program_was_started_through() {
    let (fixture, program, _) = written();
    let line = line_of(PROGRAM, "doubled = value * 2");

    let mut debuggee = launch(&fixture, "program", true);
    held_at(&mut debuggee, &program, line);
    assert_eq!(
        stack_of(&mut debuggee),
        [
            ("work".to_string(), program.clone()),
            ("<module>".to_string(), program.clone()),
        ],
        "the program's two frames and nothing of the launcher's"
    );

    // the control: run as an ordinary script, the same launcher is three
    // frames under the program, so the assertion above is not about a launcher
    // that happened to leave none
    let mut debuggee = launch(&fixture, "program", false);
    held_at(&mut debuggee, &program, line);
    let launchers: Vec<String> = stack_of(&mut debuggee)
        .into_iter()
        .filter(|(_, file)| file == &fixture.path())
        .map(|(function, _)| function)
        .collect();
    assert_eq!(launchers, ["run_as_main", "main", "<module>"]);
}

#[test]
fn a_step_over_the_programs_last_line_ends_the_program_rather_than_landing_in_the_launcher() {
    let (fixture, program, _) = written();
    let line = line_of(PROGRAM, "last = answer + 1");

    let mut debuggee = launch(&fixture, "program", true);
    held_at(&mut debuggee, &program, line);
    debuggee
        .the_step(StepKind::Over)
        .expect("the step was armed");
    match debuggee
        .wait(&mut bpd_test::reporting::Unreported)
        .expect("the step was waited for")
    {
        Running::Exited { status, .. } => assert!(status.success()),
        other => panic!(
            "the program's module had nowhere left to return to but the launcher, and \
             the step stopped instead of letting the program end: {other:?}"
        ),
    }

    let mut debuggee = launch(&fixture, "program", false);
    held_at(&mut debuggee, &program, line);
    debuggee
        .the_step(StepKind::Over)
        .expect("the step was armed");
    match debuggee
        .wait(&mut bpd_test::reporting::Unreported)
        .expect("the step was waited for")
    {
        Running::Stopped { stop, .. } => assert!(
            matches!(&stop.reason, StopReason::Stepped { file, .. } if Path::new(file) == fixture.path()),
            "as a script, the launcher is the program and the step lands in it: {:?}",
            stop.reason
        ),
        other => panic!("as a script the step lands in the launcher, and got {other:?}"),
    }
}

#[test]
fn a_step_never_lands_in_the_launchers_code_in_the_middle_of_the_program() {
    // the helper's module returns into the launcher's `exec_module`. a step
    // over its last line follows that frame out without stopping in it
    let (fixture, _, helper) = written();
    let line = line_of(HELPER, "ready = BASE + 1");

    let mut debuggee = launch(&fixture, "program", true);
    held_at(&mut debuggee, &helper, line);
    let stack = stack_of(&mut debuggee);
    assert!(
        stack
            .iter()
            .any(|(function, file)| function == "Loader.exec_module" && file == &fixture.path()),
        "the launcher's loader is in the middle of the program's stack, and is reported \
         there: {stack:?}"
    );

    debuggee
        .the_step(StepKind::Over)
        .expect("the step was armed");
    match debuggee
        .wait(&mut bpd_test::reporting::Unreported)
        .expect("the step was waited for")
    {
        Running::Stopped { stop, .. } => {
            let StopReason::Stepped { file, .. } = &stop.reason else {
                panic!("expected the step to land, got {:?}", stop.reason)
            };
            assert_ne!(
                Path::new(file),
                fixture.path(),
                "the step landed in the launcher's own code"
            );
        }
        other => panic!("expected the step to land, got {other:?}"),
    }
}

#[test]
fn an_exception_the_program_does_not_catch_stops_in_its_outermost_frame() {
    let (fixture, _, _) = written();
    let raising = fixture.directory().join("raising.py");

    for as_launcher in [true, false] {
        let mut debuggee = launch(&fixture, "raising", as_launcher);
        debuggee
            .set_exception_breakpoints(false, true)
            .expect("the exception breakpoints were set");
        let reason = match debuggee
            .run(&mut bpd_test::reporting::Unreported)
            .expect("the debuggee was resumed")
        {
            Running::Stopped { stop, .. } => stop.reason,
            other => panic!("expected the exception to stop the program, got {other:?}"),
        };
        let StopReason::Uncaught { error, file, line } = &reason else {
            panic!("expected an uncaught exception, got {reason:?}")
        };
        assert_eq!(error.kind, "ValueError");

        if as_launcher {
            assert_eq!(
                (Path::new(file), *line),
                (raising.as_path(), line_of(RAISING, "# leaves the program")),
                "reported where it leaves the program, which is the program's module"
            );
            assert_eq!(
                stack_of(&mut debuggee),
                [("<module>".to_string(), raising.clone())]
            );
            // once, and not again in each of the launcher's frames it leaves
            match debuggee
                .run(&mut bpd_test::reporting::Unreported)
                .expect("the debuggee was resumed")
            {
                Running::Exited { status, .. } => assert_eq!(status.code(), Some(1)),
                other => panic!("expected the program to end, got {other:?}"),
            }
        } else {
            assert_eq!(
                Path::new(file),
                fixture.path(),
                "as a script, the launcher's module is the outermost frame"
            );
        }
    }
}
