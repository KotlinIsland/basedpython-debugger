//! `bpd doctor` driven as a real process, against real interpreters
//!
//! the unit tests in `bpd_core` cover the capability model with synthetic
//! reports. this covers the thing a user actually runs: the binary, its exit
//! code, and what it prints

use std::process::Command;

/// the binary this test run built, not whatever `bpd` is on PATH
const BPD: &str = env!("CARGO_BIN_EXE_bpd");

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn doctor(interpreter: &str) -> Run {
    let output = Command::new(BPD)
        .args(["doctor", interpreter])
        .output()
        .expect("the binary was built by the same cargo invocation as this test");

    Run {
        status: output.status,
        stdout: String::from_utf8(output.stdout).expect("bpd writes utf8"),
        stderr: String::from_utf8(output.stderr).expect("bpd writes utf8"),
    }
}

#[test]
fn the_exit_code_matches_the_verdict_for_every_interpreter_present() {
    for capabilities in bpd_test::discovered().all() {
        let debuggable = capabilities.require_debuggable().is_ok();
        let path = capabilities.executable.display().to_string();
        let run = doctor(&path);

        assert_eq!(
            run.status.success(),
            debuggable,
            "`bpd doctor {path}` exited {} for an interpreter that is {}debuggable\n\
             stdout:\n{}\nstderr:\n{}",
            run.status,
            if debuggable { "" } else { "not " },
            run.stdout,
            run.stderr
        );
    }
}

#[test]
fn a_supported_interpreter_is_reported_as_debuggable() {
    for capabilities in bpd_test::discovered().require() {
        let run = doctor(&capabilities.executable.display().to_string());

        assert!(run.status.success());
        assert!(
            run.stdout.contains("this interpreter can be debugged"),
            "stdout was:\n{}",
            run.stdout
        );
        assert!(
            run.stdout.contains(&capabilities.version.to_string()),
            "the report must name the version it inspected, got:\n{}",
            run.stdout
        );
    }
}

#[test]
fn a_refusal_names_the_interpreter_and_the_reason() {
    let run = doctor("/nonexistent/python");

    assert!(!run.status.success());
    assert!(
        run.stderr.contains("/nonexistent/python"),
        "stderr must name what it could not run, got:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("caused by"),
        "stderr must carry the underlying cause, got:\n{}",
        run.stderr
    );
    assert!(
        run.stdout.is_empty(),
        "nothing was inspected, so nothing should be reported, got:\n{}",
        run.stdout
    );
}

#[test]
fn an_unsupported_interpreter_is_reported_before_it_is_refused() {
    // the report is printed either way — "which of my six interpreters can you
    // drive" is answered by the report, not by the exit code alone
    let unsupported: Vec<_> = bpd_test::discovered()
        .all()
        .iter()
        .filter(|capabilities| capabilities.require_debuggable().is_err())
        .collect();

    for capabilities in unsupported {
        let run = doctor(&capabilities.executable.display().to_string());

        assert!(!run.status.success());
        assert!(
            run.stdout.contains(&capabilities.version.to_string()),
            "the report is printed even when the verdict is no, got:\n{}",
            run.stdout
        );
        assert!(
            run.stderr.starts_with("error: "),
            "the reason goes to stderr, got:\n{}",
            run.stderr
        );
    }
}

#[test]
fn the_binary_reports_its_own_version() {
    let output = Command::new(BPD)
        .arg("--version")
        .output()
        .expect("the binary was built by the same cargo invocation as this test");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("bpd writes utf8");
    assert!(
        stdout.starts_with("bpd "),
        "`--version` should name the binary, got {stdout:?}"
    );
}

/// is this a subcommand the binary actually has
///
/// asked of the binary rather than of a list kept beside it, because a list
/// would have to be maintained to stay true and this cannot drift
fn is_a_subcommand(word: &str) -> bool {
    Command::new(BPD)
        .args([word, "--help"])
        .output()
        .expect("the binary was built by the same cargo invocation as this test")
        .status
        .success()
}

/// every ``bpd <word>`` this text names, in the backticks the house style puts
/// commands in
fn commands_named_in(text: &str) -> Vec<String> {
    text.split('`')
        .filter_map(|span| span.strip_prefix("bpd "))
        .map(|rest| {
            rest.split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .filter(|word| !word.is_empty())
        .collect()
}

#[test]
fn doctor_never_names_a_command_the_binary_does_not_have() {
    // a report that names `bpd attach` tells a reader the command is there and
    // the interpreter is what is missing. when the command does not exist at
    // all, the reader goes and runs it, and what they learn is that the
    // debugger's own report was wrong about the debugger
    for capabilities in bpd_test::discovered().all() {
        let run = doctor(&capabilities.executable.display().to_string());

        for word in commands_named_in(&run.stdout) {
            assert!(
                is_a_subcommand(&word),
                "`bpd doctor` named `bpd {word}`, and this binary has no such \
                 subcommand:\n{}",
                run.stdout
            );
        }
    }
}

#[test]
fn doctor_offers_attaching_only_when_the_binary_can_attach() {
    // the interpreter having PEP 768 and bpd being able to use it are two
    // different facts, and the report is read as the second one. reporting the
    // first in the second's words is the debugger claiming a capability on the
    // strength of somebody else having it
    let can_attach = is_a_subcommand("attach");

    for capabilities in bpd_test::discovered().all() {
        let run = doctor(&capabilities.executable.display().to_string());
        let offered = run
            .stdout
            .contains("attaching to an already running process is available");

        assert!(
            !offered || can_attach,
            "`bpd doctor` says attaching is available and this binary has no \
             `attach` subcommand:\n{}",
            run.stdout
        );
    }
}
