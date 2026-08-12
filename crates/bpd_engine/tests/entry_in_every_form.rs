//! the entry stop is before the program's first statement, whichever way the
//! interpreter was entered
//!
//! `stop_and_resume.rs` proves it for a script. the other two forms reach the
//! program by different routes — `-m` through `runpy`, which compiles the
//! program itself, and `-c` through source bpd compiles — so the code object
//! the stop waits for is recognised differently in each, and a stop that landed
//! a moment late would look identical from the outside
//!
//! **this is a test binary of its own on purpose.** `-m` resolves a module
//! through the working directory, so the only way to run one from a fixture is
//! to be in its directory — and a test that changes the working directory of
//! its process must not share that process with tests that do not expect it to
//! move. one binary, one test, one directory

use bpd_core::python::Capabilities;
use bpd_core::{Running, StopReason};
use bpd_engine::{Launched, Program};
use bpd_test::debuggee::Fixture;

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// a program whose very first statement is observable from outside the process
fn touches(marker: &std::path::Path) -> String {
    format!(
        "import pathlib\npathlib.Path({:?}).write_text('ran')\nprint('finished')\n",
        marker.display().to_string()
    )
}

#[test]
fn the_program_has_run_nothing_while_it_is_stopped_in_any_form() {
    let markers = tempfile::tempdir().expect("a temporary directory is available");

    // one marker per form, because the first run would leave the next one's
    // assertion already satisfied
    let script_marker = markers.path().join("script");
    let module_marker = markers.path().join("module");
    let command_marker = markers.path().join("command");

    let script = Fixture::new("as_a_script", &touches(&script_marker));
    let module = Fixture::new("as_a_module", &touches(&module_marker));

    // `-m` searches the working directory, so being there is what makes the
    // fixture a module at all. it is restored below rather than left, because a
    // later test in this binary would inherit it
    let was = std::env::current_dir().expect("this process has a working directory");
    std::env::set_current_dir(module.directory()).expect("the fixture directory exists");

    let forms = [
        (
            Program::Script(script.path()),
            script_marker.as_path(),
            "a script",
        ),
        (
            Program::Module(module.module().to_string()),
            module_marker.as_path(),
            "a module",
        ),
        (
            Program::Command(touches(&command_marker)),
            command_marker.as_path(),
            "a command",
        ),
    ];

    for (program, marker, described) in forms {
        let debuggee = match bpd_engine::launch(interpreter(), &program, &[]) {
            Ok(Launched::Stopped(debuggee)) => debuggee,
            Ok(Launched::ExitedBeforeStopping(status)) => {
                panic!("{described} exited with {status} instead of stopping")
            }
            Err(error) => panic!("{described} did not launch: {error}"),
        };

        let held_now = debuggee.held();
        let [held] = held_now.as_slice() else {
            panic!("one thread is held at entry, and got {:?}", debuggee.held())
        };
        assert_eq!(held.reason, StopReason::Entry, "for {described}");
        assert!(
            !marker.exists(),
            "the first statement of {described} had already run when the engine \
             was told it was stopped"
        );

        let mut debuggee = debuggee;
        match debuggee
            .run(&mut bpd_test::reporting::Unreported)
            .expect("the debuggee ran to completion")
        {
            Running::Exited { status, .. } => {
                assert!(status.success(), "{described} exited with {status}");
            }
            other => panic!("nothing was set, and {described} answered with {other:?}"),
        }
        assert!(
            marker.exists(),
            "{described} did not run after being resumed"
        );
    }

    std::env::set_current_dir(was).expect("the directory this test started in is still there");
}
