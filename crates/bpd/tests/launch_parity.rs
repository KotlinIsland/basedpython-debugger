//! running under `bpd` is indistinguishable from running without it
//!
//! this is the test the whole launch path exists to pass. every assertion runs
//! the same program twice — once bare, once under `bpd launch` — and compares.
//! nothing here encodes what the answer *should* be, because the answer is
//! whatever cpython does, and the only way to be sure is to ask it both times
//!
//! the comparison is literal: `bpd launch` takes the interpreter's own argument
//! vector, so [`both_ways`] hands **the same arguments** to `python` and to
//! `bpd launch --python python`. a form that only one of them understands would
//! fail here rather than in a hand written second spelling
//!
//! all three forms are covered, because they are not variations of one another.
//! `sys.argv[0]`, `sys.path[0]` and `__main__` differ between them, and a
//! launcher that treats one as a special case of another gets at least one
//! wrong
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

/// the argument vector that runs `fixture` in `form`, after the interpreter
///
/// the same words work for `python` and for `bpd launch`, which is what makes
/// the comparison below a comparison of two runners rather than of two
/// spellings
fn invocation<'a>(fixture: &'a Fixture, form: Form, args: &[&'a str]) -> Vec<String> {
    let mut words = match form {
        Form::Script => vec![fixture.path().display().to_string()],
        Form::Module => vec!["-m".to_string(), fixture.module().to_string()],
        Form::Command => vec!["-c".to_string(), fixture.source().to_string()],
    };
    words.extend(args.iter().map(ToString::to_string));
    words
}

/// run one argument vector bare, and then under `bpd launch`
///
/// `environment` is applied to both, so a variable that changes what the
/// interpreter does — `PYTHONSAFEPATH` is the one that matters — changes it for
/// the thing under test and for the thing it is compared against
fn both_ways(
    working_directory: &Path,
    arguments: &[String],
    environment: &[(&str, &str)],
) -> (Run, Run) {
    let mut bare = Command::new(&interpreter().executable);
    bare.current_dir(working_directory).args(arguments);

    let mut debugged = Command::new(BPD);
    debugged
        .current_dir(working_directory)
        .arg("launch")
        .arg("--python")
        .arg(&interpreter().executable)
        .args(arguments);

    for (name, value) in environment {
        bare.env(name, value);
        debugged.env(name, value);
    }

    (finished(&mut bare), finished(&mut debugged))
}

fn finished(command: &mut Command) -> Run {
    let output = command
        .output()
        .expect("the interpreter runs, and the binary was built by the same cargo invocation");

    Run {
        exit_code: output.status.code(),
        success: output.status.success(),
        stdout: String::from_utf8(output.stdout).expect("the program writes utf8"),
        stderr: String::from_utf8(output.stderr).expect("cpython writes utf8 to stderr"),
    }
}

/// every launch form, so nothing here can cover two of the three by accident
const EVERY_FORM: [Form; 3] = [Form::Script, Form::Module, Form::Command];

/// run a fixture both ways, in one form, from its own directory
fn both(fixture: &Fixture, form: Form, args: &[&str]) -> (Run, Run) {
    both_ways(fixture.directory(), &invocation(fixture, form, args), &[])
}

#[test]
fn the_program_sees_the_same_launch_either_way() {
    for form in EVERY_FORM {
        let fixture = Fixture::launch_probe();
        let (bare, debugged) = both(&fixture, form, &["one", "two"]);

        let bare: Observed = bare.observed();
        let debugged: Observed = debugged.observed();

        // compared as a whole rather than field by field, so a field added to
        // the probe later is covered without anyone remembering to add an
        // assertion
        assert_eq!(
            debugged, bare,
            "the program saw a different launch under bpd than without it, as {form:?}"
        );
    }
}

#[test]
fn the_three_forms_are_still_telling_each_other_apart_under_bpd() {
    // the same guard `launch_forms.rs` keeps over cpython, kept over bpd: if
    // the probe ever stopped observing what separates the forms, every parity
    // assertion here would pass while proving nothing
    let fixture = Fixture::launch_probe();
    let observed: Vec<Observed> = EVERY_FORM
        .iter()
        .map(|form| both(&fixture, *form, &[]).1.observed())
        .collect();

    assert_ne!(observed[0], observed[1]);
    assert_ne!(observed[1], observed[2]);
    assert_ne!(observed[0], observed[2]);
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

    let (bare, debugged) = both_ways(&elsewhere, &invocation(&fixture, Form::Script, &[]), &[]);
    let debugged = debugged.observed();

    assert_eq!(debugged, bare.observed());
    assert_eq!(debugged.path0, fixture.directory().display().to_string());
}

#[test]
fn a_module_takes_the_working_directory_and_not_its_own() {
    // the mirror image of the test above, and the reason `-m` cannot reuse the
    // script form's repair. a module is found *through* the working directory,
    // so the two can only be separated by putting the module in a subdirectory
    // of the place it is run from
    let fixture = Fixture::launch_probe();
    let nested = fixture.beside("nested/probe.py", bpd_test::debuggee::LAUNCH_PROBE);

    let (bare, debugged) = both_ways(
        fixture.directory(),
        &["-m".to_string(), "nested.probe".to_string()],
        &[],
    );
    let debugged = debugged.observed();

    assert_eq!(debugged, bare.observed());
    assert_eq!(debugged.path0, fixture.directory().display().to_string());
    assert_ne!(
        debugged.path0,
        nested
            .parent()
            .expect("the nested module has a directory")
            .display()
            .to_string()
    );
}

#[test]
fn a_commands_path_entry_is_the_working_directory_at_import_time() {
    // `sys.path[0]` under `-c` is the **empty string**, which the import system
    // resolves against the working directory every time it is used. spelling
    // the working directory out instead would look identical until the program
    // calls `os.chdir`, and then it would import a different module — or none
    let fixture = Fixture::new("unused", "print('unused')\n");
    let over_there = fixture.beside("over_there/imported.py", "name = 'over there'\n");
    let directory = over_there
        .parent()
        .expect("the module has a directory")
        .display()
        .to_string();

    let source =
        format!("import os\nos.chdir({directory:?})\nimport imported\nprint(imported.name)\n");
    let (bare, debugged) = both_ways(fixture.directory(), &["-c".to_string(), source], &[]);

    assert_eq!(bare.stdout, "over there\n", "stderr:\n{}", bare.stderr);
    assert_eq!(debugged.stdout, bare.stdout, "stderr:\n{}", debugged.stderr);
    assert_eq!(debugged.exit_code, bare.exit_code);
}

#[test]
fn a_package_run_as_a_module_runs_its_main() {
    // a package is not a module with a dot in it: `-m pkg` runs
    // `pkg/__main__.py`, with `__package__` set to the package and
    // `__spec__.name` to `pkg.__main__`. going through the same resolution
    // cpython does is what gets this right rather than a rule about names
    let fixture = Fixture::new("unused", "print('unused')\n");
    fixture.beside("pkg/__init__.py", "");
    fixture.beside("pkg/__main__.py", bpd_test::debuggee::LAUNCH_PROBE);

    let (bare, debugged) = both_ways(
        fixture.directory(),
        &["-m".to_string(), "pkg".to_string()],
        &[],
    );
    let debugged = debugged.observed();

    assert_eq!(debugged, bare.observed());
    assert_eq!(debugged.spec.as_deref(), Some("pkg.__main__"));
    assert_eq!(debugged.package.as_deref(), Some("pkg"));
}

#[test]
fn a_module_that_is_not_there_is_refused_in_the_interpreters_own_words() {
    let fixture = Fixture::new("present", "print('never runs')\n");
    let (bare, debugged) = both_ways(
        fixture.directory(),
        &["-m".to_string(), "absent_module".to_string()],
        &[],
    );

    assert_eq!(debugged.stderr, bare.stderr);
    assert_eq!(debugged.exit_code, bare.exit_code);
    assert!(
        bare.stderr.contains("No module named absent_module"),
        "stderr was:\n{}",
        bare.stderr
    );
}

#[test]
fn a_module_whose_package_raises_on_import_reads_exactly_as_it_would_have() {
    // the failure happens inside runpy's *resolution*, one frame shallower than
    // a failure in the module itself. a launcher that resolved the module
    // itself before handing it to runpy would report this from the wrong depth,
    // or run the package twice
    let fixture = Fixture::new("unused", "print('unused')\n");
    fixture.beside(
        "broken/__init__.py",
        "raise ValueError('from the package')\n",
    );
    fixture.beside("broken/inner.py", "print('never runs')\n");

    let (bare, debugged) = both_ways(
        fixture.directory(),
        &["-m".to_string(), "broken.inner".to_string()],
        &[],
    );

    assert_eq!(debugged.stderr, bare.stderr);
    assert_eq!(debugged.exit_code, bare.exit_code);
    assert_eq!(debugged.stdout, bare.stdout);
}

#[test]
fn the_exit_code_is_the_programs_own() {
    for form in EVERY_FORM {
        for code in [0, 1, 3, 42] {
            let fixture = Fixture::new("exits", &format!("raise SystemExit({code})\n"));
            let (bare, debugged) = both(&fixture, form, &[]);

            assert_eq!(bare.exit_code, Some(code));
            assert_eq!(
                debugged.exit_code, bare.exit_code,
                "for a program exiting {code} as {form:?}"
            );
        }
    }
}

#[test]
fn an_uncaught_exception_reads_exactly_as_it_would_have() {
    // the traceback must hold the program's frames and none of bpd's. an extra
    // line here is a person reading a stack that did not happen
    //
    // under `-m` it must hold **runpy's** two frames as well, because a bare
    // `-m` traceback holds them: cpython runs a module by calling
    // `runpy._run_module_as_main`, and those frames are the program's
    // surroundings rather than the debugger's
    for form in EVERY_FORM {
        let fixture = Fixture::new(
            "raises",
            "def inner():\n    raise ValueError('the message')\n\n\ndef outer():\n    inner()\n\n\nouter()\n",
        );
        let (bare, debugged) = both(&fixture, form, &[]);

        assert_eq!(debugged.stderr, bare.stderr, "as {form:?}");
        assert_eq!(debugged.exit_code, bare.exit_code, "as {form:?}");
        assert!(
            !debugged.stderr.contains("bpd"),
            "the traceback named bpd as {form:?}:\n{}",
            debugged.stderr
        );
    }
}

#[test]
fn a_syntax_error_reads_exactly_as_it_would_have() {
    for form in EVERY_FORM {
        let fixture = Fixture::new("broken", "def (:\n");
        let (bare, debugged) = both(&fixture, form, &[]);

        assert_eq!(debugged.stderr, bare.stderr, "as {form:?}");
        assert_eq!(debugged.exit_code, bare.exit_code, "as {form:?}");
    }
}

#[test]
fn a_traceback_through_code_the_program_compiled_shows_the_programs_own_source() {
    // `compile` defaults to naming its code `<string>`, and cpython keeps the
    // source of a `-c` command under exactly that name so a traceback can print
    // the line. bpd enters *every* form through a `-c` bootstrap, so without
    // care the interpreter has bpd's bootstrap registered there — and a
    // traceback through the program's own `<string>` code prints
    // `import bpd_agent; bpd_agent.main()` with a caret under it
    //
    // a wrong source line is worse than no source line: it is the debugger
    // telling someone the program says something it does not say
    for form in EVERY_FORM {
        let fixture = Fixture::new(
            "compiles",
            "exec(compile(\"raise ValueError('from the compiled code')\", '<string>', 'exec'))\n",
        );
        let (bare, debugged) = both(&fixture, form, &[]);

        assert_eq!(debugged.stderr, bare.stderr, "as {form:?}");
        assert!(
            !debugged.stderr.contains("bpd_agent"),
            "the traceback showed bpd's own bootstrap as the program's source as {form:?}:\n{}",
            debugged.stderr
        );
    }
}

#[test]
fn a_safe_path_run_gains_no_search_path_of_its_own() {
    // `PYTHONSAFEPATH` (and `-P`) turn the whole `sys.path[0]` prepending off,
    // and then there is no entry of the interpreter's to put back. a launcher
    // that writes one anyway hands the program a module a bare run would never
    // have found, and takes out the stdlib entry that was in slot zero
    for form in EVERY_FORM {
        let fixture = Fixture::launch_probe();
        let (bare, debugged) = both_ways(
            fixture.directory(),
            &invocation(&fixture, form, &[]),
            &[("PYTHONSAFEPATH", "1")],
        );

        // `-m` needs the working directory on the path to find the module at
        // all, so under safe path there is nothing to run and nothing to
        // compare but the refusal
        if form == Form::Module {
            assert_eq!(debugged.stderr, bare.stderr, "as {form:?}");
            assert_eq!(debugged.exit_code, bare.exit_code, "as {form:?}");
            continue;
        }

        let bare = bare.observed();
        let debugged = debugged.observed();

        assert!(bare.safe_path, "the safe path flag never took effect");
        assert_eq!(
            debugged.path0, bare.path0,
            "bpd wrote a `sys.path[0]` a bare run does not have, as {form:?}"
        );
    }
}

#[test]
fn output_arrives_in_the_same_order_on_both_streams() {
    for form in EVERY_FORM {
        let fixture = Fixture::new(
            "interleaved",
            "import sys\n\
             for index in range(200):\n    \
                 sys.stdout.write(f'out {index}\\n')\n    \
                 sys.stderr.write(f'err {index}\\n')\n",
        );
        let (bare, debugged) = both(&fixture, form, &[]);

        assert_eq!(debugged.stdout, bare.stdout, "as {form:?}");
        assert_eq!(debugged.stderr, bare.stderr, "as {form:?}");
    }
}

#[test]
fn a_script_that_cannot_be_opened_is_refused_in_the_interpreters_own_words() {
    // cpython says "No such file or directory" and exits 2. rust's own io error
    // says "entity not found", and an uncaught exception exits 1 — getting
    // either wrong sends someone looking for a different problem
    let fixture = Fixture::new("present", "print('never runs')\n");
    let missing = fixture.directory().join("absent.py");

    let (bare, debugged) = both_ways(fixture.directory(), &[missing.display().to_string()], &[]);

    assert_eq!(debugged.stderr, bare.stderr);
    assert_eq!(debugged.exit_code, bare.exit_code);
    assert_eq!(bare.exit_code, Some(2));
}

#[test]
fn a_program_that_reads_its_own_environment_finds_no_debugger_in_it() {
    // the launch parameters go through the environment, and so does the agent's
    // own directory — `PYTHONPATH` is how the bootstrap can import it at all. a
    // program that could see either is a program that can behave differently
    // under the debugger, which is the thing this whole file exists to prevent
    //
    // the whole environment is compared rather than the names beginning `BPD`,
    // because the one that gave this away was `PYTHONPATH`
    for form in EVERY_FORM {
        let fixture = Fixture::new(
            "environment",
            "import os\nprint('\\n'.join(sorted(f'{name}={value}' for name, value in os.environ.items())))\n",
        );
        let (bare, debugged) = both(&fixture, form, &[]);

        assert_eq!(debugged.stdout, bare.stdout, "as {form:?}");
        assert!(
            !debugged.stdout.contains("BPD"),
            "the program could read a launch parameter as {form:?}"
        );
    }
}

#[test]
fn a_program_that_reads_its_own_import_path_finds_no_debugger_on_it() {
    // the other half of the same fingerprint. the agent is imported by putting
    // its staged directory on `PYTHONPATH`, which puts it on `sys.path` — and
    // under `PYTHONSAFEPATH` it lands in slot zero, ahead of the stdlib. a
    // directory searched before everything else is the debugger deciding what
    // the program imports
    for form in EVERY_FORM {
        let fixture = Fixture::new("import_path", "import sys\nprint('\\n'.join(sys.path))\n");
        let (bare, debugged) = both(&fixture, form, &[]);

        assert_eq!(debugged.stdout, bare.stdout, "as {form:?}");
    }
}

#[test]
fn the_program_is_the_only_main_module() {
    for form in EVERY_FORM {
        let fixture = Fixture::new(
            "modules",
            "import sys\n\
             print(sys.modules['__main__'].__name__)\n\
             print([name for name in sys.modules if 'bpd' in name])\n",
        );
        let (bare, debugged) = both(&fixture, form, &[]);

        // the agent is imported by the bootstrap and stays in `sys.modules`,
        // which is the one fingerprint that cannot be removed — unimporting it
        // would unload the code that is running. it is named here so the
        // difference is a recorded fact rather than a surprise
        assert_eq!(bare.stdout, "__main__\n[]\n", "as {form:?}");
        assert_eq!(debugged.stdout, "__main__\n['bpd_agent']\n", "as {form:?}");
    }
}
