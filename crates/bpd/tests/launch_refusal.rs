//! an interpreter `bpd` cannot debug is refused before the program starts
//!
//! this is the half of the support policy a user actually meets. `bpd doctor`
//! answers "can you drive this interpreter" when it is asked; `launch` has to
//! answer it when it was **not** asked, and it has to answer before it has
//! changed anything
//!
//! "before" is the whole claim, and it is why these run a program that would
//! announce itself. a launcher that started an interpreter, ran a line and then
//! reported the interpreter unsupported has already done the thing it was
//! refusing to do — and on a program with a side effect in it, exiting non-zero
//! afterwards does not take that back
//!
//! the check lives in `bpd_engine::launch::start`, which every front end goes
//! through, so this is also where the parity rule is held for the refusal: the
//! DAP adapter answers a `launch` request with the same sentence, and
//! `a_client_is_refused_the_same_interpreter_the_command_line_is` is what says
//! so

use std::process::{Command, Output};

/// the binary this test run built
const BPD: &str = env!("CARGO_BIN_EXE_bpd");

/// what the program prints if it ever gets to run
///
/// nothing else in the output can produce it, so its absence is evidence rather
/// than an assumption about what an empty stdout means
const ANNOUNCED: &str = "the-program-ran";

fn launch(interpreter: &str, program: &[&str]) -> Output {
    Command::new(BPD)
        .arg("launch")
        .arg("--python")
        .arg(interpreter)
        .args(program)
        .output()
        .expect("the binary was built by the same cargo invocation as this test")
}

/// the three ways a program can be named, each one announcing itself
///
/// all three are here because the refusal has to come first in every form. the
/// script and the module are deliberately ones that do not exist: if the
/// interpreter were checked *after* the program was resolved, these would fail
/// with the wrong complaint, and the assertion on the message would catch it
fn every_form() -> Vec<Vec<String>> {
    vec![
        vec!["-c".to_string(), format!("print('{ANNOUNCED}')")],
        vec!["-m".to_string(), "this_module_is_never_reached".to_string()],
        vec!["definitely_not_a_script.py".to_string()],
    ]
}

#[test]
fn an_interpreter_too_old_to_debug_is_refused_before_the_program_runs() {
    for capabilities in bpd_test::discovered().unsupported() {
        let named = capabilities.executable.display().to_string();

        for form in every_form() {
            let arguments: Vec<&str> = form.iter().map(String::as_str).collect();
            let refused = launch(&named, &arguments);
            let said = String::from_utf8_lossy(&refused.stderr);
            let printed = String::from_utf8_lossy(&refused.stdout);

            assert!(
                !refused.status.success(),
                "launching {arguments:?} on python {} succeeded, and it cannot \
                 be debugged\nstderr:\n{said}",
                capabilities.version
            );
            assert!(
                !printed.contains(ANNOUNCED),
                "the program ran before the interpreter was refused, which is \
                 the one thing a refusal must not allow\nstdout:\n{printed}"
            );
            assert!(
                said.contains(&named),
                "the refusal must name the interpreter it refused, got:\n{said}"
            );
            assert!(
                said.contains(&capabilities.version.to_string()),
                "the refusal must name the version it found, got:\n{said}"
            );
            assert!(
                said.contains(&bpd_core::python::MINIMUM_SUPPORTED.to_string()),
                "the refusal must name the minimum, so the reader knows what \
                 would work instead, got:\n{said}"
            );
        }
    }
}

#[test]
fn an_interpreter_that_cannot_be_run_at_all_is_refused_with_the_cause() {
    // a missing interpreter and an old one are different failures a user acts
    // on differently — one is a path to fix, the other is a version. the
    // refusal carries the os error rather than flattening both into
    // "unsupported"
    //
    // this one needs nothing installed to be meaningful, which is why it is
    // separate from the loop above
    let refused = launch("/nonexistent/python", &["-c", "print('unreachable')"]);
    let said = String::from_utf8_lossy(&refused.stderr);

    assert!(!refused.status.success());
    assert!(
        said.contains("/nonexistent/python"),
        "the refusal must name what it could not run, got:\n{said}"
    );
    assert!(
        said.contains("caused by"),
        "the refusal must carry the underlying cause, got:\n{said}"
    );
}
