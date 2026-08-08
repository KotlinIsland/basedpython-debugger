//! running under `bpd` is indistinguishable from running without it
//!
//! this is the test the whole launch path exists to pass. every assertion runs
//! the same program twice — once bare, once under `bpd launch` — and compares.
//! nothing here encodes what the answer *should* be, because the answer is
//! whatever cpython does, and the only way to be sure is to ask it both times
//!
//! "it only fails under the debugger" is the bug class this prevents

use std::path::Path;
use std::process::Command;

use bpd_core::python::Capabilities;
use bpd_test::debuggee::{Fixture, Form, Observed, Run};

/// the binary this test run built
const BPD: &str = env!("CARGO_BIN_EXE_bpd");

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// run a fixture under `bpd launch`, from the fixture's own directory
fn under_bpd(fixture: &Fixture, args: &[&str]) -> Run {
    launch_from(fixture.directory(), fixture, args)
}

fn launch_from(working_directory: &Path, fixture: &Fixture, args: &[&str]) -> Run {
    let output = Command::new(BPD)
        .current_dir(working_directory)
        .arg("launch")
        .arg("--python")
        .arg(&interpreter().executable)
        .arg(fixture.path())
        .args(args)
        .output()
        .expect("the binary was built by the same cargo invocation as this test");

    Run {
        exit_code: output.status.code(),
        success: output.status.success(),
        stdout: String::from_utf8(output.stdout).expect("the program writes utf8"),
        stderr: String::from_utf8(output.stderr).expect("cpython writes utf8 to stderr"),
    }
}

/// the same fixture, run both ways
fn both(fixture: &Fixture, args: &[&str]) -> (Run, Run) {
    let bare = fixture.run(interpreter(), Form::Script, args);
    let debugged = under_bpd(fixture, args);
    (bare, debugged)
}

#[test]
fn the_program_sees_the_same_launch_either_way() {
    let fixture = Fixture::launch_probe();
    let (bare, debugged) = both(&fixture, &["one", "two"]);

    let bare: Observed = bare.observed();
    let debugged: Observed = debugged.observed();

    // compared as a whole rather than field by field, so a field added to the
    // probe later is covered without anyone remembering to add an assertion
    assert_eq!(
        debugged, bare,
        "the program saw a different launch under bpd than without it"
    );
}

#[test]
fn a_program_started_from_elsewhere_still_finds_its_own_directory() {
    // `sys.path[0]` is the script's directory, not the working directory. the
    // two coincide often enough that only separating them proves which one the
    // launcher used
    let fixture = Fixture::launch_probe();
    let elsewhere = std::env::temp_dir()
        .canonicalize()
        .expect("the system temporary directory exists");

    let bare = fixture
        .run_in(&elsewhere, interpreter(), Form::Script, &[])
        .observed();
    let debugged = launch_from(&elsewhere, &fixture, &[]).observed();

    assert_eq!(debugged, bare);
    assert_eq!(debugged.path0, fixture.directory().display().to_string());
}

#[test]
fn the_exit_code_is_the_programs_own() {
    for code in [0, 1, 3, 42] {
        let fixture = Fixture::new("exits", &format!("raise SystemExit({code})\n"));
        let (bare, debugged) = both(&fixture, &[]);

        assert_eq!(bare.exit_code, Some(code));
        assert_eq!(
            debugged.exit_code, bare.exit_code,
            "for a program exiting {code}"
        );
    }
}

#[test]
fn an_uncaught_exception_reads_exactly_as_it_would_have() {
    // the traceback must hold the program's frames and none of bpd's. an extra
    // line here is a person reading a stack that did not happen
    let fixture = Fixture::new(
        "raises",
        "def inner():\n    raise ValueError('the message')\n\n\ndef outer():\n    inner()\n\n\nouter()\n",
    );
    let (bare, debugged) = both(&fixture, &[]);

    assert_eq!(debugged.stderr, bare.stderr);
    assert_eq!(debugged.exit_code, bare.exit_code);
    assert!(
        !debugged.stderr.contains("bpd"),
        "the traceback named bpd:\n{}",
        debugged.stderr
    );
}

#[test]
fn a_syntax_error_reads_exactly_as_it_would_have() {
    let fixture = Fixture::new("broken", "def (:\n");
    let (bare, debugged) = both(&fixture, &[]);

    assert_eq!(debugged.stderr, bare.stderr);
    assert_eq!(debugged.exit_code, bare.exit_code);
}

#[test]
fn a_script_that_cannot_be_opened_is_refused_in_the_interpreters_own_words() {
    // cpython says "No such file or directory" and exits 2. rust's own io error
    // says "entity not found", and an uncaught exception exits 1 — getting
    // either wrong sends someone looking for a different problem
    let fixture = Fixture::new("present", "print('never runs')\n");
    let missing = fixture.directory().join("absent.py");

    let bare = Command::new(&interpreter().executable)
        .arg(&missing)
        .output()
        .expect("the interpreter runs");
    let debugged = Command::new(BPD)
        .arg("launch")
        .arg("--python")
        .arg(&interpreter().executable)
        .arg(&missing)
        .output()
        .expect("the binary was built by the same cargo invocation as this test");

    assert_eq!(
        String::from_utf8_lossy(&debugged.stderr),
        String::from_utf8_lossy(&bare.stderr)
    );
    assert_eq!(debugged.status.code(), bare.status.code());
    assert_eq!(bare.status.code(), Some(2));
}

#[test]
fn output_arrives_in_the_same_order_on_both_streams() {
    let fixture = Fixture::new(
        "interleaved",
        "import sys\n\
         for index in range(200):\n    \
             sys.stdout.write(f'out {index}\\n')\n    \
             sys.stderr.write(f'err {index}\\n')\n",
    );
    let (bare, debugged) = both(&fixture, &[]);

    assert_eq!(debugged.stdout, bare.stdout);
    assert_eq!(debugged.stderr, bare.stderr);
}

#[test]
fn a_program_that_reads_its_own_environment_finds_no_debugger_in_it() {
    // the launch parameters go through the environment, and a program that
    // could see them is a program that can behave differently under the
    // debugger — which is the thing this whole file exists to prevent
    let fixture = Fixture::new(
        "environment",
        "import os\nprint(sorted(name for name in os.environ if name.startswith('BPD')))\n",
    );
    let (bare, debugged) = both(&fixture, &[]);

    assert_eq!(debugged.stdout.trim(), "[]");
    assert_eq!(debugged.stdout, bare.stdout);
}

#[test]
fn the_program_is_the_only_main_module() {
    let fixture = Fixture::new(
        "modules",
        "import sys\n\
         print(sys.modules['__main__'].__name__)\n\
         print([name for name in sys.modules if 'bpd' in name])\n",
    );
    let (bare, debugged) = both(&fixture, &[]);

    // the agent is imported by the bootstrap and stays in `sys.modules`, which
    // is the one fingerprint that cannot be removed — unimporting it would
    // unload the code that is running. it is named here so the difference is a
    // recorded fact rather than a surprise
    assert_eq!(bare.stdout, "__main__\n[]\n");
    assert_eq!(debugged.stdout, "__main__\n['bpd_agent']\n");
}
