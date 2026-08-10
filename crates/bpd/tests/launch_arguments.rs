//! `bpd launch` accepts exactly what the interpreter accepts, and refuses the
//! rest while parsing
//!
//! a launch that names no program is a parse error rather than something
//! discovered once a process has been started: a launcher that spawned an
//! interpreter and then reported that the arguments made no sense would have
//! already changed the world it was asked about
//!
//! the three forms cannot be combined, and it is worth pinning **why**. it is
//! not a conflict rule — `-m` and `-c` each take the whole of the rest of the
//! line, which is what the interpreter's own option parsing does, so there is
//! no arrangement of arguments in which two of them are given. a rule saying so
//! could never fire, and the tests below are what hold the shape up instead

use std::process::{Command, Output};

use bpd_core::python::Capabilities;
use bpd_test::debuggee::Fixture;

/// the binary this test run built
const BPD: &str = env!("CARGO_BIN_EXE_bpd");

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

fn launch(arguments: &[&str]) -> Output {
    Command::new(BPD)
        .arg("launch")
        .args(arguments)
        .output()
        .expect("the binary was built by the same cargo invocation as this test")
}

#[test]
fn a_launch_with_no_program_at_all_is_refused() {
    let refused = launch(&[]);
    let said = String::from_utf8_lossy(&refused.stderr);

    assert!(!refused.status.success());
    // clap's own message, which names every way a program can be given rather
    // than just the one that happens to be first
    assert!(said.contains("MODULE"), "stderr was:\n{said}");
    assert!(said.contains("SOURCE"), "stderr was:\n{said}");
    assert!(said.contains("SCRIPT"), "stderr was:\n{said}");
}

#[test]
fn everything_after_a_module_belongs_to_the_program() {
    // including something that looks like another launch form. `python -m pkg
    // -c x` runs `pkg` with `-c x` as its arguments rather than refusing, and
    // this is where that rule is written down: it is what makes the three forms
    // exclusive without a conflict rule, because no two of them can be given
    let fixture = Fixture::new("echo", "import sys\nprint(sys.argv[1:])\n");

    let ran = Command::new(BPD)
        .current_dir(fixture.directory())
        .arg("launch")
        .arg("--python")
        .arg(&interpreter().executable)
        .args(["-m", "echo", "-c", "print()", "--python", "nothing"])
        .output()
        .expect("the binary was built by the same cargo invocation as this test");

    assert!(
        ran.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&ran.stdout),
        "['-c', 'print()', '--python', 'nothing']\n"
    );
}

#[test]
fn everything_after_a_command_belongs_to_the_program() {
    // `python -c src --python x` passes `--python x` to the program, and so
    // does this. the interpreter is chosen by what comes **before** the form,
    // which is the same rule python's own option parsing has
    let ran = Command::new(BPD)
        .arg("launch")
        .arg("--python")
        .arg(&interpreter().executable)
        .args(["-c", "import sys; print(sys.argv)", "--python", "-m", "-c"])
        .output()
        .expect("the binary was built by the same cargo invocation as this test");

    assert!(
        ran.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&ran.stdout),
        "['-c', '--python', '-m', '-c']\n"
    );
}
