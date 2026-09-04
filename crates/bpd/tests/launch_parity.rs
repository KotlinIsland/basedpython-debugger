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
//! **how** the two are run is under test as well as what they are handed. most
//! of this file compares two piped runs, which is what `Command::output` gives
//! — and that leaves anything differing only between a terminal and a pipe
//! invisible. `a_program_on_a_terminal_is_still_on_one_under_bpd` is the
//! comparison that needs a real one, and it opens a pseudo-terminal to make it
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

/// the two commands that run one argument vector — bare, and under `bpd launch`
///
/// built here rather than at each call site so that both halves of a comparison
/// are constructed by the same code. what runs them is the caller's, because
/// **how** they are run is itself under test: through pipes for most of this
/// file, and through a real terminal for the one comparison pipes cannot make
///
/// `environment` is applied to both, so a variable that changes what the
/// interpreter does — `PYTHONSAFEPATH` is the one that matters — changes it for
/// the thing under test and for the thing it is compared against
fn each_way(
    working_directory: &Path,
    arguments: &[String],
    environment: &[(&str, &str)],
) -> (Command, Command) {
    let python = &interpreter().executable;
    let mut bare = Command::new(python);
    bare.current_dir(working_directory).args(arguments);

    let mut debugged = Command::new(BPD);
    debugged
        .current_dir(working_directory)
        .arg("launch")
        .arg("--python")
        .arg(python)
        .args(arguments);

    for (name, value) in environment {
        bare.env(name, value);
        debugged.env(name, value);
    }

    (bare, debugged)
}

/// run one argument vector bare, and then under `bpd launch`, through pipes
fn both_ways(
    working_directory: &Path,
    arguments: &[String],
    environment: &[(&str, &str)],
) -> (Run, Run) {
    let (mut bare, mut debugged) = each_way(working_directory, arguments, environment);
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

/// what a program can see about the streams it was given, and what its own
/// buffering does with them
///
/// the last line is a **measurement** rather than a claim. `os.write` goes
/// straight to the file descriptor, past python's buffer, so where it lands
/// among the lines above it *is* the buffering: on a line-buffered stream every
/// print is already out and it arrives last, and on a block-buffered one it
/// overtakes all of them and the prints arrive together at exit
///
/// `isatty` is the half that matters more. it is what `rich`, `click`, `pytest`
/// and `colorama` check to decide colour, progress bars and formatting, so a
/// program that is told the wrong answer *renders* differently — the buffering
/// only changes when its output arrives
///
/// it writes to **both** streams as well, which is a comparison only a terminal
/// makes possible. a terminal has one stream, so the two arrive on it in the
/// order the program wrote them and the order is recoverable afterwards.
/// through pipes it is not — `output_arrives_in_the_same_order_on_both_streams`
/// above compares each stream against its own counterpart and cannot see
/// across, because two pipes carry no record of how the two were interleaved
const STREAM_PROBE: &str = "import os\n\
     import sys\n\
     print('stdout.isatty', sys.stdout.isatty())\n\
     print('stderr.isatty', sys.stderr.isatty(), file=sys.stderr)\n\
     print('stdin.isatty', sys.stdin.isatty())\n\
     print('stdout.line_buffering', sys.stdout.line_buffering)\n\
     print('stderr.line_buffering', sys.stderr.line_buffering, file=sys.stderr)\n\
     os.write(sys.stdout.fileno(), b'past the buffer\\n')\n";

/// a terminal's own newline translation, undone, so terminal text can be
/// compared with piped text
///
/// only ever applied to one side of a comparison **against a pipe**. the two
/// terminal runs are compared with it left in, because both went through the
/// same line discipline and removing it there would be the test editing what it
/// is comparing
#[cfg(unix)]
fn without_carriage_returns(written: &str) -> String {
    written.replace("\r\n", "\n")
}

#[cfg(unix)]
#[test]
fn a_program_on_a_terminal_is_still_on_one_under_bpd() {
    // the comparison the rest of this file structurally cannot make. everything
    // else here runs both halves through `Command::output`, which is two pipes,
    // so anything that differs only between a terminal and a pipe is invisible
    // to it — and `isatty()` is exactly that
    //
    // what it pins is the rule: `bpd` gives the debuggee **what a bare run
    // would have given it**. not always a terminal and not always a pipe. the
    // way `bpd launch` achieves that is by inheriting its own standard streams,
    // so a change that put a pipe in front of the debuggee would pass every
    // other test in this file and fail here
    for form in EVERY_FORM {
        let fixture = Fixture::new("streams", STREAM_PROBE);
        let arguments = invocation(&fixture, form, &[]);
        let (bare, debugged) = each_way(fixture.directory(), &arguments, &[]);

        let bare = bpd_test::terminal::through_a_terminal(bare);
        let debugged = bpd_test::terminal::through_a_terminal(debugged);

        assert!(
            bare.success && debugged.success,
            "the probe has to run to the end both ways, as {form:?}. bare exited \
             {:?} having written:\n{}\ndebugged exited {:?} having written:\n{}",
            bare.exit_code,
            bare.written,
            debugged.exit_code,
            debugged.written
        );
        assert_eq!(debugged.exit_code, bare.exit_code, "as {form:?}");
        assert_eq!(
            debugged.written, bare.written,
            "on a terminal the program saw different streams under bpd than \
             without it, as {form:?}. `isatty()` decides colour, progress bars \
             and formatting in most CLI libraries, and the buffering that \
             follows it decides when a `print` arrives at all"
        );

        // and the guard that keeps this from being the piped comparison a
        // second time. if a terminal and a pipe ever stopped differing here,
        // every assertion above would pass while proving nothing — which is the
        // state this test exists to get the file out of
        let (piped_bare, piped_debugged) = both(&fixture, form, &[]);
        assert_eq!(
            piped_debugged.stdout, piped_bare.stdout,
            "through pipes the program saw different streams under bpd than \
             without it, as {form:?}"
        );
        assert_eq!(piped_debugged.stderr, piped_bare.stderr, "as {form:?}");
        assert!(
            bare.written.contains("stdout.isatty True")
                && piped_bare.stdout.contains("stdout.isatty False"),
            "the two runs have to be of different shapes, or this measured a \
             pipe twice, as {form:?}. on a terminal the probe wrote:\n{}\nand \
             through a pipe:\n{}",
            bare.written,
            piped_bare.stdout
        );
        assert_ne!(
            without_carriage_returns(&bare.written),
            piped_bare.stdout,
            "as {form:?} a bare run wrote the same thing on a terminal as into \
             a pipe, so there is no difference left for bpd to get wrong and \
             the comparison above is vacuous"
        );
    }
}

#[test]
fn the_program_is_run_by_the_interpreter_bpd_was_given() {
    // bpd probes the interpreter it is handed and records two things: the path
    // it was named by, and what that interpreter says `sys.executable` is. it
    // used to **start** the second one, and on a macos framework build those
    // differ — naming `…/Versions/3.13/bin/python3.13` probes as
    // `…/Resources/Python.app/Contents/MacOS/Python`
    //
    // the debuggee then reports an executable nobody mentioned, and cpython
    // prints that name in front of its own errors, so
    // `a_script_that_cannot_be_opened_is_refused_in_the_interpreters_own_words`
    // failed on every macos runner with two spellings of the same interpreter.
    // this is the same claim without the error message in the way
    let fixture = Fixture::new("unused", "print('never runs')\n");
    let (bare, debugged) = both_ways(
        fixture.directory(),
        &[
            "-c".to_string(),
            "import sys; print(sys.executable)".to_string(),
        ],
        &[],
    );

    assert_eq!(
        debugged.stdout, bare.stdout,
        "the program's own `sys.executable` is not the one it would have had. \
         a debuggee that names a different interpreter than the bare run is one \
         whose children, whose error messages and whose `sys.executable` all \
         say bpd was there"
    );
}

#[test]
fn a_script_that_cannot_be_opened_is_refused_in_the_interpreters_own_words() {
    // cpython says "No such file or directory" and exits 2. rust's own io error
    // says "entity not found", and an uncaught exception exits 1 — getting
    // either wrong sends someone looking for a different problem
    //
    // the **prefix** is the third of those and the one this caught: under bpd
    // the program is reached through the `-c` bootstrap, so the agent writes
    // this message rather than cpython, and it wrote `sys.executable` where
    // cpython writes the name it was invoked by. on a macos framework build
    // those are two different files — the runners report the invocation as
    // `…/Resources/Python.app/Contents/MacOS/Python` and `sys.executable` as
    // `…/Versions/3.13/bin/python3.13` — and every macos job failed here
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
    //
    // **this is the off case, and it is the default.** child debugging is the
    // one thing that changes it, and what it changes is enumerated in
    // `a_program_whose_children_are_debugged_can_tell_and_only_that_much`
    // below. not one byte of this assertion moved when that arrived
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
    //
    // the off case, like the one above, and the same note applies: child
    // debugging appends **one** entry and nothing else touches this
    for form in EVERY_FORM {
        let fixture = Fixture::new("import_path", "import sys\nprint('\\n'.join(sys.path))\n");
        let (bare, debugged) = both(&fixture, form, &[]);

        assert_eq!(debugged.stdout, bare.stdout, "as {form:?}");
    }
}

#[test]
fn a_program_in_a_basedpython_build_cannot_tell_the_map_reached_the_debuggee() {
    // the map used never to leave `bpd`, so nothing about it could be visible.
    // it does now — the agent reports every location of the build as the `.by`
    // line behind it, and it has to be given the tables to do that — so the
    // claim needs proving rather than assuming
    //
    // what makes it invisible is what it is: bytes over the control connection
    // into the agent's own memory, exactly as the breakpoint table is. nothing
    // is imported, nothing is written to `sys.path` or to the environment, and
    // `_by_sourcemap.py` is parsed by `bpd` out of process
    //
    // the program here **is** the generated python of a build: a map beside it
    // names it, with digests that are true of the two files on disk, so `bpd`
    // finds one and sends it. bare, the interpreter runs the same file out of
    // the same directory and the map is a file nothing reads
    let fixture = Fixture::launch_probe();
    let source = fixture.directory().join("launch_probe.by");
    std::fs::write(&source, "# the `.by` a person wrote\n").expect("the `.by` is written");
    write_source_map(&fixture, &source);

    let (bare, debugged) = both(&fixture, Form::Script, &[]);
    let (bare, debugged): (Observed, Observed) = (bare.observed(), debugged.observed());
    assert_eq!(debugged, bare, "the program saw a different launch");

    // and the two fingerprints that a map could plausibly show up in. a reader
    // that imported `_by_sourcemap.py` would be one module and one path entry
    // away from being visible in both
    for source in [
        "import sys\nprint('\\n'.join(sorted(sys.modules)))\n",
        "import sys\nprint('\\n'.join(sys.path))\n",
    ] {
        let fixture = Fixture::new("launch_probe", source);
        let by = fixture.directory().join("launch_probe.by");
        std::fs::write(&by, "# the `.by` a person wrote\n").expect("the `.by` is written");
        write_source_map(&fixture, &by);

        let (bare, debugged) = both(&fixture, Form::Script, &[]);
        let gained: Vec<&str> = debugged
            .stdout
            .lines()
            .filter(|line| !bare.stdout.lines().any(|had| had == *line))
            .collect();
        assert!(
            gained
                .iter()
                .all(|name| ALLOWED.iter().any(|(allowed, _)| allowed == name)),
            "a debuggee of a mapped build gained {gained:?}, which a bare run of \
             the same program in the same build directory does not have"
        );
    }
}

/// write a `_by_sourcemap.py` beside a fixture that maps it to `source`
///
/// the digests are true of what is on disk, which is what makes it a map `bpd`
/// will load rather than refuse. the table is deliberately trivial: what is
/// under test here is whether **any** of it reaches the program, not what it
/// says
fn write_source_map(fixture: &Fixture, source: &Path) {
    use std::fmt::Write as _;

    use sha2::Digest as _;

    let digest = |path: &Path| {
        let bytes = std::fs::read(path).expect("a file this test just wrote");
        let mut hex = String::from("sha256:");
        for byte in sha2::Sha256::digest(&bytes) {
            write!(hex, "{byte:02x}").expect("a `String` grows to fit");
        }
        hex
    };
    let generated = fixture.path();
    let lines: Vec<&str> = std::fs::read_to_string(&generated)
        .expect("the fixture is on disk")
        .lines()
        .map(|_| "0")
        .collect();

    std::fs::write(
        fixture.directory().join(bpd_core::source_map::MAP_FILENAME),
        format!(
            "# generated by `by run`\n\
             SOURCEMAP = {{\n    \"{generated}\": (\"{source}\", [{table}]),\n}}\n\n\
             DIGESTS = {{\n    \"{generated}\": {{\"by\": \"{by}\", \"py\": \"{py}\"}},\n}}\n",
            generated = generated.display(),
            source = source.display(),
            table = lines.join(", "),
            by = digest(source),
            py = digest(&generated),
        ),
    )
    .expect("the map is written");
}

// ---- the on case ---------------------------------------------------------
//
// everything above this line is the guarantee that a program cannot tell it is
// being debugged, and nothing below it weakens one of those assertions. what
// follows is the **other** rule, which is the one child debugging made
// necessary:
//
// > a program run under `bpd` cannot tell it is being debugged. a program run
// > under `bpd` **with child debugging asked for** can — it has `PYTHONPATH`
// > ending in a directory holding a `sitecustomize`, three `BPD_CHILD_*` names,
// > and exactly one extra `sys.path` entry, which is the last one. it can see
// > nothing else, and off is the default
//
// it has to be tested in both directions, and it is: the off case above is
// untouched, and the enumerated list below fails on a fourth name

/// a program that writes down its whole environment and its whole import path
///
/// one program rather than two, because the two halves of this fingerprint have
/// to agree: the directory `PYTHONPATH` gains is the directory `sys.path` gains,
/// and a test that read them in separate runs could not say so
const WHAT_THE_PROGRAM_CAN_SEE: &str = "import os\n\
     import sys\n\
     print('ENVIRONMENT')\n\
     for name, value in sorted(os.environ.items()):\n    \
         print(f'{name}={value}')\n\
     print('PATH')\n\
     for entry in sys.path:\n    \
         print(entry)\n";

/// every name a debuggee's environment gains when child debugging is asked for,
/// and why each one has to be there
///
/// the reasons are the point, exactly as they are in [`ALLOWED`]. a bare list of
/// names is something people add to when it fails; a reason is something they
/// have to disagree with — and this is the one list in the project that
/// enumerates a way for a program to notice the debugger
const ALLOWED_WITH_CHILD_DEBUGGING: &[(&str, &str)] = &[
    (
        "PYTHONPATH",
        "the channel itself, and there is no second candidate. an interpreter \
         that has not started yet reads nothing bpd could write but this and \
         the files it opens at startup, and a child that was `exec`'d is a \
         fresh interpreter with none of this process's memory in it. the \
         directory is **appended**, where it cannot shadow a module of the \
         program's own — the agent's own staged directory is prepended, and \
         that is what the off case above exists to catch",
    ),
    (
        "BPD_CHILD_ENDPOINT",
        "where a child connects, which is this debuggee's own listener. it is \
         in the environment rather than in memory because an `exec` inherits no \
         memory — that is the whole difference between this and a debugged fork, \
         which needs no variable at all",
    ),
    (
        "BPD_CHILD_TOKEN",
        "what a child presents, and **not** the session token. this one is \
         readable by every descendant and by anything that can read this \
         process's environment, so its whole power is to open a session of its \
         own — a session token here would let any of them write into the \
         session bpd is already answering",
    ),
    (
        "BPD_CHILD_AGENT",
        "where the agent is staged. the `sitecustomize` that enters a child is \
         alone in its directory, so the agent's directory has to be named \
         somewhere — the child puts it on `sys.path` for one import and the \
         agent takes it off again before the child is held",
    ),
];

/// what the probe wrote down, split back into its two halves
struct Seen {
    environment: std::collections::BTreeMap<String, String>,
    path: Vec<String>,
}

fn parsed(run: &Run) -> Seen {
    assert!(
        run.success,
        "the probe exited with {:?}\nstderr:\n{}",
        run.exit_code, run.stderr
    );

    let mut environment = std::collections::BTreeMap::new();
    let mut path = Vec::new();
    let mut in_path = false;
    for line in run.stdout.lines() {
        match line {
            "ENVIRONMENT" => {}
            "PATH" => in_path = true,
            entry if in_path => path.push(entry.to_string()),
            variable => {
                let (name, value) = variable
                    .split_once('=')
                    .unwrap_or_else(|| panic!("`{variable}` is not `name=value`"));
                environment.insert(name.to_string(), value.to_string());
            }
        }
    }
    assert!(
        in_path,
        "the probe never reached its import path:\n{}",
        run.stdout
    );
    Seen { environment, path }
}

/// the two forms this can be driven in
///
/// **not** `-m`, and only for the run that goes through the engine:
/// `bpd_engine::launch` takes no working directory, which is the one thing `-m`
/// is resolved through. the same guarantee driven through `bpd launch
/// --debug-children` covers all three, because a command line has a directory
///
/// both are driven rather than one, because they are two ways in and the
/// promise is about the debuggee either way. what this adds to a launch is
/// written at a **stop**, after the form has already decided everything it
/// decides
const WITHOUT_A_WORKING_DIRECTORY: [Form; 2] = [Form::Script, Form::Command];

/// run a fixture under the engine, with child debugging on, and collect what it
/// printed
fn with_children_debugged(fixture: &Fixture, form: Form) -> Run {
    use std::io::Read as _;
    use std::sync::{Arc, Mutex};

    let program = match form {
        Form::Script => bpd_engine::Program::Script(fixture.path()),
        Form::Command => bpd_engine::Program::Command(fixture.source().to_string()),
        Form::Module => unreachable!("`-m` is resolved through a working directory"),
    };

    let collected: Arc<Mutex<(String, String)>> =
        Arc::new(Mutex::new((String::new(), String::new())));
    let writing = Arc::clone(&collected);
    // the reading threads are handed to the engine rather than detached, which
    // is what makes the comparison below sound: the engine waits for them before
    // it reports the program exited, so everything the program wrote is in
    // `collected` by the time there is an exit status to compare
    let launched = bpd_engine::launch_piped(interpreter(), &program, &[], move |stdout, stderr| {
        bpd_engine::Forwarders::on(
            [
                (Box::new(stdout) as Box<dyn std::io::Read + Send>, 0_usize),
                (Box::new(stderr) as Box<dyn std::io::Read + Send>, 1),
            ]
            .into_iter()
            .map(|(mut stream, which)| {
                let into = Arc::clone(&writing);
                std::thread::spawn(move || {
                    let mut read = String::new();
                    // a pipe nobody reads fills up and stops the process, so
                    // both are drained for as long as the program has them open
                    let _finished = stream.read_to_string(&mut read);
                    let mut held = into.lock().expect("nothing panics holding the output");
                    if which == 0 {
                        held.0.push_str(&read);
                    } else {
                        held.1.push_str(&read);
                    }
                })
            })
            .collect(),
        )
    })
    .expect("the debuggee launched");

    let mut debuggee = match launched {
        bpd_engine::Launched::Stopped(debuggee) => debuggee,
        bpd_engine::Launched::ExitedBeforeStopping(status) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
    };
    assert!(
        debuggee
            .debug_children(true)
            .expect("the debuggee took the setting"),
        "the agent has to say the setting took, or this measures the off case"
    );

    let mut seen = bpd_test::reporting::Children::default();
    let status = match debuggee.run(&mut seen).expect("the program was resumed") {
        bpd_core::Running::Exited { status, .. } => status,
        other => panic!("the probe prints and ends: {other:?}"),
    };
    drop(debuggee);

    // every writer is gone once the process has exited and the debuggee is
    // dropped, so the reading threads have finished or are about to
    for _ in 0..200 {
        if Arc::strong_count(&collected) == 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let held = collected.lock().expect("nothing panics holding the output");
    Run {
        exit_code: status.code(),
        success: status.success(),
        stdout: held.0.clone(),
        stderr: held.1.clone(),
    }
}

#[test]
fn a_program_whose_children_are_debugged_can_tell_and_only_that_much() {
    for form in WITHOUT_A_WORKING_DIRECTORY {
        let fixture = Fixture::new("what_it_sees", WHAT_THE_PROGRAM_CAN_SEE);
        let bare = fixture.run(interpreter(), form, &[]);
        let bare = parsed(&bare);
        let on = parsed(&with_children_debugged(&fixture, form));

        only_the_enumerated_channel(form, &bare, &on);
    }
}

/// the same guarantee, for the front end a person drives
///
/// `bpd launch --debug-children` reaches the same setting through the same
/// session, and a promise about what a debuggee can see is a promise about the
/// debuggee rather than about who asked. it is the same list, checked by the
/// same function, so a fifth name fails in both places or in neither
///
/// all three forms here, because a command line has a working directory —
/// [`WITHOUT_A_WORKING_DIRECTORY`] says why the engine-driven one has two
#[test]
fn a_program_launched_with_debug_children_can_tell_exactly_as_much_and_no_more() {
    for form in EVERY_FORM {
        let fixture = Fixture::new("what_it_sees", WHAT_THE_PROGRAM_CAN_SEE);
        let bare = parsed(&fixture.run(interpreter(), form, &[]));

        // the flag goes **before** the program, exactly as `--python` does:
        // everything from the first positional on is the program's own
        let mut debugged = Command::new(BPD);
        debugged
            .current_dir(fixture.directory())
            .arg("launch")
            .arg("--python")
            .arg(&interpreter().executable)
            .arg("--debug-children")
            .args(invocation(&fixture, form, &[]));
        let on = parsed(&finished(&mut debugged));

        only_the_enumerated_channel(form, &bare, &on);
    }
}

/// what a debuggee with child debugging on may differ by, and nothing else
///
/// one function for both drivers. the list is the point — see
/// [`ALLOWED_WITH_CHILD_DEBUGGING`] — and two copies of the comparison would be
/// two places for a name to be quietly allowed
fn only_the_enumerated_channel(form: Form, bare: &Seen, on: &Seen) {
    for (name, value) in &on.environment {
        let same = bare.environment.get(name) == Some(value);
        assert!(
            same || ALLOWED_WITH_CHILD_DEBUGGING
                .iter()
                .any(|(allowed, _)| allowed == name),
            "as {form:?} a debuggee with child debugging on has `{name}` in \
                 its environment, which a bare run of the same program does not \
                 have — or has differently — and which nothing in this list \
                 accounts for:\n{}\n\
                 this is the **one** list in bpd that enumerates a way for a \
                 program to notice the debugger. a name added here is a name \
                 every program run this way carries, so it needs a reason \
                 somebody can disagree with, not an entry",
            child_reasons()
        );
    }

    let lost: Vec<&String> = bare
        .environment
        .keys()
        .filter(|name| !on.environment.contains_key(*name))
        .collect();
    assert!(
        lost.is_empty(),
        "as {form:?} the debuggee is missing {lost:?}, which a bare run has. \
             child debugging adds a channel and takes nothing away"
    );

    for (name, _) in ALLOWED_WITH_CHILD_DEBUGGING {
        assert!(
            on.environment.contains_key(*name),
            "as {form:?} the list claims `{name}`, and the program could not \
                 read it. a reason nobody needs is a reason nobody reads — and a \
                 channel that is not there is a child that will not attach"
        );
    }

    // the two halves have to agree: what `PYTHONPATH` gained is what
    // `sys.path` gained, and it is the **last** entry of both
    let gained: Vec<&String> = on
        .path
        .iter()
        .filter(|entry| !bare.path.contains(entry))
        .collect();
    assert_eq!(
        gained.len(),
        1,
        "as {form:?} child debugging put {gained:?} on the import path. it \
             is one directory, holding one file"
    );
    let added = gained[0];
    assert_eq!(
        on.path.last(),
        Some(added),
        "as {form:?} the entry is **appended**. anywhere else and it is a \
             directory searched before something of the program's own, which is \
             the debugger deciding what the program imports"
    );
    let separator = if cfg!(windows) { ';' } else { ':' };
    assert_eq!(
        on.environment["PYTHONPATH"]
            .rsplit(separator)
            .next()
            .expect("a split has a last part"),
        added.as_str(),
        "as {form:?} `PYTHONPATH` and `sys.path` name different directories. \
             a variable saying this interpreter imports from somewhere it does \
             not is a lie about this process, and programs read it back"
    );
    assert_ne!(
        on.environment["BPD_CHILD_AGENT"], *added,
        "as {form:?} the agent's directory and the hook's are the same one. \
             the hook is appended to every descendant's path, so anything beside \
             it there is a module bpd added to programs it is not debugging"
    );
}

/// the child-debugging list, spelled out for a failure to print
fn child_reasons() -> String {
    ALLOWED_WITH_CHILD_DEBUGGING
        .iter()
        .map(|(name, reason)| format!("  - `{name}`: {reason}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// the audit events `bpd` watches a child by, on this interpreter
///
/// it is a **list**, and it is the agent's own — taken from
/// [`bpd_core::spawn::making_a_process`] rather than restated, because a
/// restatement of it is what this guard used to be. it named
/// `_posixsubprocess.fork_exec` on 3.14 and later, and which event an ordinary
/// `subprocess.run` raises is the interpreter's choice on the day: cpython
/// reaches for `posix_spawn` where it can, so on a machine that took that path
/// the guard failed a run in which nothing at all was wrong
///
/// what the guard is for is making sure the fixture reached bpd's hook. any
/// event on the list is bpd's hook — see
/// [child processes](../../../docs/development/subprocesses.md)
fn watched_events() -> Vec<&'static str> {
    let version = interpreter().version;
    bpd_core::spawn::making_a_process(version.major, version.minor)
        .iter()
        .map(|event| {
            event
                .to_str()
                .expect("every audit event bpd watches is written in ascii")
        })
        .collect()
}

#[test]
fn a_program_that_watches_its_own_audit_events_sees_exactly_the_ones_it_would_have() {
    // the third fingerprint, and the one the child-process report could have
    // left. `bpd` watches for a child through `PySys_AddAuditHook`, which is a
    // hook in the program's own interpreter — so the question is whether the
    // program can tell it is there
    //
    // it cannot be *listed*: cpython exposes `sys.addaudithook` and `sys.audit`
    // and nothing that enumerates what is installed. what is left is whether
    // the events a program's own hook receives change, and that is what this
    // compares — for a spawn, which is the one thing bpd's hook acts on
    for form in EVERY_FORM {
        let fixture = Fixture::new(
            "audited",
            "import subprocess\n\
             import sys\n\n\
             seen = []\n\
             sys.addaudithook(lambda event, arguments: seen.append(event))\n\
             subprocess.run([sys.executable, '-c', 'pass'], check=True)\n\
             subprocess.run(['/bin/echo'], check=True, capture_output=True)\n\
             print('\\n'.join(seen))\n\
             print(sorted(name for name in dir(sys) if 'audit' in name.lower()))\n",
        );
        let (bare, debugged) = both(&fixture, form, &[]);

        assert_eq!(
            debugged.stdout, bare.stdout,
            "the program's own audit hook saw a different set of events under \
             bpd than without it, as {form:?}. a hook the program can detect is \
             a program that can behave differently under the debugger"
        );
        let watched = watched_events();
        assert!(
            watched.iter().any(|event| bare.stdout.contains(event)),
            "the fixture has to reach an event bpd watches on *this* \
             interpreter, or this compared two runs of a program that proves \
             nothing. it was looking for any of {watched:?} and the program \
             saw:\n{}",
            bare.stdout
        );
        assert!(
            bare.stdout.contains("['addaudithook', 'audit']"),
            "if cpython grows a way to list the installed hooks, this guarantee \
             has to be re-established rather than assumed. `sys` now has:\n{}",
            bare.stdout
        );
    }
}

/// a program that forks twice: once as it was launched, once with a thread of
/// its own running
///
/// the warning cpython raises on `os.fork()` in a multi-threaded process is
/// **recorded** rather than printed, which is the point: a program can put it
/// in its own data with `warnings.catch_warnings(record=True)`, so this is not
/// a question about stderr. the agent reads the control connection on a thread
/// of its own, and a thread the program did not start is exactly what this
/// would report
///
/// the second fork is the control. it has to warn — on both runs — or the first
/// one is being compared against a cpython that stopped counting, and the whole
/// assertion would pass while proving nothing
#[cfg(unix)]
const FORK_PROBE: &str = r"import os
import threading
import warnings


def os_threads():
    # what linux counts, read the way cpython reads it. it is `None` where there
    # is no `/proc`, which is the same answer on both runs and so says nothing
    # either way — and on linux it is the difference itself, in the output being
    # compared, rather than something to go and find out afterwards
    try:
        with open('/proc/self/status') as status:
            for line in status:
                if line.startswith('Threads:'):
                    return int(line.split()[1])
    except OSError:
        pass
    return None


def forked():
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter('always')
        pid = os.fork()
        if pid == 0:
            os._exit(0)
        os.waitpid(pid, 0)
    return [warning.category.__name__ for warning in caught]


print('os threads:', os_threads())
print('as launched:', forked())

running = threading.Event()
finish = threading.Event()
thread = threading.Thread(target=lambda: (running.set(), finish.wait()))
thread.start()
running.wait()
print('with a thread of its own:', forked())
print('threads:', threading.active_count(), sorted(t.name for t in threading.enumerate()))
finish.set()
thread.join()
";

/// the thread count the fixture reported, and everything it said after it
///
/// `None` where there is no `/proc` to read one from, which is every platform
/// but linux. the count is not a comparison there, and the rest of the output
/// is the same question either way
#[cfg(unix)]
fn threads_and_rest(said: &str) -> (Option<usize>, String) {
    let (first, rest) = said
        .split_once('\n')
        .unwrap_or_else(|| panic!("the fixture prints its thread count first, and said:\n{said}"));
    let counted = first
        .strip_prefix("os threads: ")
        .unwrap_or_else(|| panic!("the fixture prints its thread count first, and said:\n{said}"));
    (counted.parse().ok(), rest.to_string())
}

#[cfg(unix)]
#[test]
fn a_program_that_forks_records_exactly_the_warnings_it_would_have() {
    for form in EVERY_FORM {
        let fixture = Fixture::new("forker", FORK_PROBE);
        let (bare, debugged) = both(&fixture, form, &[]);

        assert!(
            bare.success && debugged.success,
            "the fixture has to run to the end both ways, as {form:?}. bare \
             exited {:?}:\n{}\ndebugged exited {:?}:\n{}",
            bare.exit_code,
            bare.stderr,
            debugged.exit_code,
            debugged.stderr
        );
        assert!(
            bare.stdout.contains("as launched: []")
                && bare
                    .stdout
                    .contains("with a thread of its own: ['DeprecationWarning']"),
            "this interpreter no longer tells a single-threaded fork from a \
             multi-threaded one, so the comparison below is vacuous. it \
             printed:\n{}",
            bare.stdout
        );

        // the debugger's reader thread really is on the process, and that is
        // what makes the rest of this a result rather than a coincidence: a
        // debuggee that happened to be single-threaded would record the same
        // warnings for a reason that has nothing to do with bpd
        //
        // it is asserted rather than compared, because it is a difference bpd
        // is **entitled** to. the thread is joined for the fork itself and put
        // back after, and what the program can see is the whole question
        let (threads_bare, rest_bare) = threads_and_rest(&bare.stdout);
        let (threads_debugged, rest_debugged) = threads_and_rest(&debugged.stdout);
        if let (Some(without), Some(with)) = (threads_bare, threads_debugged) {
            assert_eq!(
                (without, with),
                (1, 2),
                "the bare run has the program's thread and the debugged one has \
                 that and the agent's reader. as {form:?} they were {without} \
                 and {with}, and a debuggee with no debugger thread on it \
                 proves nothing about forking beside one"
            );
        }

        // and the warnings themselves, which have to be the same set. the agent
        // stands its reader thread down in an `os.register_at_fork(before=…)`
        // handler precisely so that the fork happens on a single-threaded
        // process
        //
        // cpython counted the process's threads **before** running those
        // handlers until [python/cpython#137109], which is fixed in main and
        // backported to 3.13 and 3.14. an interpreter without that fix counts a
        // thread that is gone by the time `fork` is called, and this is where
        // that shows up: `as launched: ['DeprecationWarning']` under bpd
        // against `as launched: []` without it. the thread counts above say
        // whether the reader really was there, which is the first thing to read
        // when this fails
        //
        // [python/cpython#137109]: https://github.com/python/cpython/issues/137109
        assert_eq!(
            rest_debugged, rest_bare,
            "the program recorded a different set of warnings for its own fork \
             under bpd than without it, as {form:?}. a debugger whose reader \
             thread is on the process while it forks changes what the program \
             can see about itself"
        );
    }
}

/// every module a debuggee has that a bare run of the same program does not,
/// and why each one is still there
///
/// this is the whole of the fingerprint `bpd` leaves in `sys.modules`, and it is
/// a short list on purpose. a program that lists its own modules can tell it is
/// being debugged from any entry here, and — worse — a plugin scanner, a lazy
/// importer or a test that asserts on an import side effect behaves *differently*
/// because of one
///
/// the reasons are the point. a bare list of names is something people update
/// when it fails; a reason is something they have to disagree with.
/// [launching](../../../docs/development/launching.md) has the measurement this
/// was cut down from — thirty-two names on 3.14, of which twenty-nine were one
/// call to `sysconfig.get_config_var`
const ALLOWED: &[(&str, &str)] = &[
    (
        "bpd_agent",
        "the agent itself. it cannot go — unimporting it would unload the code \
         that is running — and importing it costs exactly this one name",
    ),
    (
        "linecache",
        "cpython's, not the agent's. every `-c` run imports it to keep the \
         command's source for a traceback, and bpd enters all three forms \
         through a `-c` bootstrap — so it is already there before the agent \
         exists, and a bare `-c` run has it too",
    ),
];

/// every module a program could see in its own `sys.modules`
fn modules(run: &Run) -> std::collections::BTreeSet<String> {
    assert!(
        run.success,
        "the fixture exited with {:?}\nstderr:\n{}",
        run.exit_code, run.stderr
    );
    run.stdout.lines().map(ToString::to_string).collect()
}

#[test]
fn the_only_modules_a_debuggee_gains_are_the_ones_written_down() {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for form in EVERY_FORM {
        let fixture = Fixture::new(
            "modules",
            "import sys\nprint('\\n'.join(sorted(sys.modules)))\n",
        );
        let (bare, debugged) = both(&fixture, form, &[]);
        let (bare, debugged) = (modules(&bare), modules(&debugged));

        let gained: Vec<&String> = debugged.difference(&bare).collect();
        for name in &gained {
            assert!(
                ALLOWED.iter().any(|(allowed, _)| *allowed == name.as_str()),
                "as {form:?} the debuggee imported `{name}`, which a bare run of \
                 the same program does not have and which nothing in this list \
                 accounts for:\n{}\n\
                 should it have? a module in the debuggee that is not in a bare \
                 run is a program that can tell it is being debugged, and one \
                 that behaves differently when it is. if it genuinely has to be \
                 there, add it above with the reason — and if it does not, the \
                 import that pulled it in is the thing to move",
                reasons()
            );
        }
        seen.extend(gained.into_iter().cloned());

        let lost: Vec<&String> = bare.difference(&debugged).collect();
        assert!(
            lost.is_empty(),
            "as {form:?} the debuggee is missing {lost:?}, which a bare run has. \
             taking a module back out of `sys.modules` is not a way to hide it: \
             the next import of it runs its top level a second time"
        );

        assert!(
            debugged.contains("bpd_agent"),
            "as {form:?} the debuggee had no agent in it, so this compared two \
             bare runs and proved nothing"
        );
    }

    let written_down: std::collections::BTreeSet<String> = ALLOWED
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect();
    assert_eq!(
        seen, written_down,
        "the list above claims a module the debuggee no longer gains in any \
         form. a reason nobody needs is a reason nobody reads — take it out"
    );
}

/// the allowed list, spelled out for a failure to print
fn reasons() -> String {
    ALLOWED
        .iter()
        .map(|(name, reason)| format!("  - `{name}`: {reason}"))
        .collect::<Vec<_>>()
        .join("\n")
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
