//! the debuggee is genuinely held before it has run anything
//!
//! "stopped at entry" is easy to claim and easy to get wrong — a debugger that
//! reported the stop a moment too late would look identical from the outside.
//! so the proof here is not the message: it is that a side effect on the
//! program's **first** line has not happened yet while the engine is holding it,
//! and has happened once it lets go

use std::ffi::OsString;

use bpd_core::python::Capabilities;
use bpd_protocol::message::StopReason;
use bpd_test::debuggee::Fixture;

/// the interpreter the built agent matches, or a failure saying how to get one
fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// launch a fixture and require that it actually stopped
fn stopped(fixture: &Fixture, args: &[OsString]) -> bpd_engine::Debuggee {
    match bpd_engine::launch(interpreter(), &fixture.path(), args) {
        Ok(bpd_engine::Launched::Stopped(debuggee)) => debuggee,
        Ok(bpd_engine::Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

/// a program whose very first statement is observable from outside the process
fn touches(marker: &std::path::Path) -> String {
    format!(
        "import pathlib\npathlib.Path({:?}).write_text('ran')\nprint('finished')\n",
        marker.display().to_string()
    )
}

#[test]
fn the_program_has_run_nothing_while_it_is_stopped() {
    let directory = tempfile::tempdir().expect("a temporary directory is available");
    let marker = directory.path().join("ran");
    let fixture = Fixture::new("first_line", &touches(&marker));

    let debuggee = stopped(&fixture, &[]);

    assert_eq!(debuggee.stopped(), StopReason::Entry);
    assert!(
        !marker.exists(),
        "the program's first statement had already run when the engine was told \
         it was stopped"
    );

    let status = debuggee
        .resume_to_exit()
        .expect("the debuggee ran to completion");
    assert!(status.success());
    assert!(
        marker.exists(),
        "the program did not run after being resumed"
    );
}

#[test]
fn the_exit_code_is_the_programs_own() {
    for code in [0, 3, 42] {
        let fixture = Fixture::new("exits", &format!("raise SystemExit({code})\n"));
        let debuggee = stopped(&fixture, &[]);

        let status = debuggee.resume_to_exit().expect("the debuggee finished");
        assert_eq!(status.code(), Some(code), "for a program exiting {code}");
    }
}

#[test]
fn the_programs_arguments_reach_it_untouched() {
    let fixture = Fixture::new("echo", "import sys; print(' '.join(sys.argv[1:]))\n");
    let arguments: Vec<OsString> = ["--flag", "-x", "a b", "--"]
        .iter()
        .map(Into::into)
        .collect();

    let debuggee = stopped(&fixture, &arguments);
    let status = debuggee.resume_to_exit().expect("the debuggee finished");

    // the arguments went to the debuggee's own stdout, which is inherited, so
    // the assertion here is that it ran at all — `bpd`'s own launch parity test
    // is what compares the text
    assert!(status.success());
}
