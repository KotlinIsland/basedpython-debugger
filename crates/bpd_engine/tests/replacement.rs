//! replacing a running process's code, against a real interpreter
//!
//! the claim this file is written around is the one nothing else can check: a
//! replacement is a change to a **live process**, and whether it worked is
//! whether the program's own later output changed. so the fixture writes what
//! every route into the edited module returns *after* the replacement, and the
//! assertions read that file rather than the debugger's report of what it did
//!
//! the routes are chosen to be the ones a namespace walk would miss — a closure
//! handed out before the edit, a function object a decorator kept while the
//! module name holds a wrapper, and a method reached through an instance that
//! already existed

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use bpd_core::python::Capabilities;
use bpd_core::{
    Binding, Divergence, LiveFrame, Replaced, Replacement, Replacements, Running, SourceBreakpoint,
    Stop, Unreplaceable,
};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// the module that gets edited
///
/// every shape a replacement has to reach at once: a plain function, a closure
/// a factory handed out, a function a decorator captured and replaced in the
/// namespace, and a method of a class whose instances exist. the module body
/// **calls** things — `keep` runs at import — which is exactly the import side
/// effect that must not be repeated
const VICTIM: &str = r#"KEPT = []


def plain(value):
    return ("before", value)


def make_adder(base):
    def add(extra):
        return ("before", base + extra)

    return add


def keep(function):
    KEPT.append(function)

    def wrapper():
        return "wrapping " + function()

    return wrapper


@keep
def registered():
    return "before"


def spread(one, /, two, *rest, three, **extra):
    return ("before", one)


class Widget:
    def describe(self):
        return "before"
"#;

/// the same module with every **body** changed and nothing else
///
/// the leading blank lines move every code object in the file down, which is
/// what makes the line numbers a stack reports wrong until the replacement
const EDITED: &str = r#"# an edit above everything, so every line number in this file moves
# and a stack taken before the replacement reports the old ones

KEPT = []


def plain(value):
    return ("after", value * 10)


def make_adder(base):
    def add(extra):
        return ("after", base + extra + 1000)

    return add


def keep(function):
    KEPT.append(function)

    def wrapper():
        return "wrapping " + function()

    return wrapper


@keep
def registered():
    return "after"


def spread(one, /, two, *rest, three, **extra):
    return ("after", one)


class Widget:
    def describe(self):
        return "after"
"#;

/// the program under test
///
/// everything it reports is read out of the **live objects** it made before the
/// edit: two closures from one factory, the function a decorator captured, the
/// name the decorator replaced, and an instance built by the old class body
const PROGRAM: &str = r#"import pathlib

import victim

HERE = pathlib.Path(__file__).parent
first = victim.make_adder(10)
second = victim.make_adder(100)
widget = victim.Widget()


def report():
    (HERE / "ran.txt").write_text(
        repr(
            [
                victim.plain(1),
                first(1),
                second(1),
                victim.KEPT[0](),
                victim.registered(),
                widget.describe(),
                victim.plain.__code__.co_firstlineno,
            ]
        )
    )


def wait_here():
    ready = 1
    return ready


wait_here()
report()
"#;

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// a fixture whose `victim.py` holds `source`
fn laid_out(source: &str) -> (Fixture, PathBuf) {
    let fixture = Fixture::new("replacing", PROGRAM);
    let victim = fixture.sibling("victim", source);
    (fixture, victim)
}

fn launch(fixture: &Fixture) -> Debuggee {
    match bpd_engine::launch(
        interpreter(),
        &bpd_engine::Program::Script(fixture.path()),
        &[] as &[OsString],
    ) {
        Ok(Launched::Stopped(debuggee)) => debuggee,
        Ok(Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

/// stop the program on `line` of `file`, and take the breakpoint back off again
fn held_at(debuggee: &mut Debuggee, file: &Path, line: u32) -> Stop {
    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, file, line)])
        .expect("the breakpoint request was answered");
    match &resolved[0].binding {
        Binding::Bound { line: bound, .. } => assert_eq!(
            *bound, line,
            "the fixture line has to be executable, or the test is about a \
             different line than it says"
        ),
        other => panic!("the breakpoint did not bind: {other:?}"),
    }

    let stop = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected a breakpoint stop, got {other:?}"),
    };
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    stop
}

/// stop the program on a line of a module it has **not imported yet**
///
/// the binding is not asserted when the breakpoint is set, because at the entry
/// stop the interpreter has compiled nothing from that file — it is reported
/// unbound and binds when the import happens, which is the behaviour
/// `crates/bpd_engine/tests/breakpoints.rs` is about. what is asserted is where
/// the stop landed
fn stopped_inside(debuggee: &mut Debuggee, file: &Path, line: u32) -> Stop {
    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, file, line)])
        .expect("the breakpoint request was answered");

    let stop = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected a breakpoint stop, got {other:?}"),
    };
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    stop
}

/// hold the program at the line that runs before the module is used again
fn held_before_the_report(debuggee: &mut Debuggee, fixture: &Fixture) -> Stop {
    held_at(debuggee, &fixture.path(), line_of(PROGRAM, "    ready = 1"))
}

/// resume everything and require that the program finishes successfully
fn to_exit(debuggee: &mut Debuggee) {
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Exited { status, .. } => {
            assert!(status.success(), "the program exited with {status}");
        }
        other => panic!("expected the program to finish, got {other:?}"),
    }
}

/// what the program itself recorded, after it ran on
///
/// read off the program rather than off the debugger. what a replacement claims
/// and what the process really runs afterwards are two different statements, and
/// only one of them is evidence
fn recorded(fixture: &Fixture) -> String {
    let path = fixture.directory().join("ran.txt");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("the program never wrote {}: {error}", path.display()))
}

/// the one file a replacement was about
///
/// every test here replaces a single file, which now comes back as a list of one.
/// asserting the length rather than taking the first is the point: a request for
/// one file that answered about two would be a different bug than any of these
/// tests are looking for, and would otherwise pass silently
fn only(replaced: &Replacements) -> &Replaced {
    assert_eq!(
        replaced.files.len(),
        1,
        "one file was replaced and the answer was about {}",
        replaced.files.len()
    );
    &replaced.files[0]
}

/// what a replacement changed, or the refusals that say it changed nothing
///
/// `rebound` comes off the whole answer rather than off the file: binding walks
/// down from each file's root code object, so a replacement resolves the build's
/// breakpoints together rather than one file's
fn applied(replaced: &Replacements) -> (&Vec<bpd_core::Rebound>, &Vec<bpd_core::Resolved>) {
    match &only(replaced).outcome {
        Replacement::Applied { changed, .. } => (changed, &replaced.rebound),
        Replacement::Refused { because } => {
            let said: Vec<String> = because.iter().map(ToString::to_string).collect();
            panic!("the replacement was refused: {said:#?}")
        }
    }
}

/// the reasons a replacement was refused, or a panic saying it was not
fn refused(replaced: &Replacements) -> &Vec<Unreplaceable> {
    match &only(replaced).outcome {
        Replacement::Refused { because } => because,
        Replacement::Applied { changed, .. } => {
            panic!("the replacement was applied, changing {changed:#?}")
        }
    }
}

#[test]
fn every_live_object_of_an_edited_module_runs_the_new_code_and_the_top_level_is_not_re_run() {
    let (fixture, victim) = laid_out(VICTIM);
    let mut debuggee = launch(&fixture);
    held_before_the_report(&mut debuggee, &fixture);

    std::fs::write(&victim, EDITED).expect("the fixture directory is writable");
    let replaced = debuggee
        .replace_code(&victim)
        .expect("the replacement was answered");
    let (changed, _) = applied(&replaced);

    // the closure the factory handed out twice is one code object held by two
    // function objects, and both of them were rebound. a walk of the module
    // namespace would have found neither
    let adder = changed
        .iter()
        .find(|one| one.function.ends_with("make_adder.<locals>.add"))
        .unwrap_or_else(|| panic!("the nested closure was not replaced: {changed:#?}"));
    assert_eq!(
        adder.objects, 2,
        "two calls to the factory handed out two function objects over one code \
         object, and {adder:?} says otherwise"
    );

    // and the file really did move down, which is the thing a debugger normally
    // reports wrongly forever
    let plain = changed
        .iter()
        .find(|one| one.function == "plain")
        .unwrap_or_else(|| panic!("`plain` was not replaced: {changed:#?}"));
    assert_eq!(
        (plain.was_at, plain.now_at),
        (
            line_of(VICTIM, "def plain(value):"),
            line_of(EDITED, "def plain(value):")
        ),
        "the report has to say where the code is now, not only that it changed"
    );

    to_exit(&mut debuggee);

    // the program's own answer, through five routes into the module and one
    // line number read off a code object
    let after = recorded(&fixture);
    assert_eq!(
        after,
        format!(
            "[('after', 10), ('after', 1011), ('after', 1101), 'after', \
             'wrapping after', 'after', {}]",
            line_of(EDITED, "def plain(value):")
        ),
        "every live object of the module has to run the code that is on disk"
    );

    // `KEPT` still holds one entry. re-running the top level would have appended
    // a second, which is the difference between a replacement and running the
    // program again
    assert!(
        !after.contains("'wrapping wrapping"),
        "the top level was re-run: the decorator wrapped the function twice"
    );
}

#[test]
fn a_file_that_is_already_what_the_process_runs_replaces_nothing_and_says_so() {
    let (fixture, victim) = laid_out(VICTIM);
    let mut debuggee = launch(&fixture);
    held_before_the_report(&mut debuggee, &fixture);

    // nothing was edited. "nothing needed replacing" and "nothing could be
    // replaced" are different facts, and a client told the second would go
    // looking for what it had done wrong
    let replaced = debuggee
        .replace_code(&victim)
        .expect("the replacement was answered");
    let (changed, _) = applied(&replaced);
    assert!(
        changed.is_empty(),
        "the file on disk is what the process is running, and it replaced \
         {changed:#?}"
    );

    let Replacement::Applied { unchanged, .. } = &only(&replaced).outcome else {
        unreachable!("`applied` already required it")
    };
    assert!(
        unchanged.iter().any(|name| name == "plain")
            && unchanged.iter().any(|name| name == "Widget.describe"),
        "everything the file holds is unchanged, and it listed {unchanged:?}"
    );

    to_exit(&mut debuggee);
    assert!(
        recorded(&fixture).contains("('before', 1)"),
        "nothing was replaced, so the program has to behave as it did"
    );
}

#[test]
fn a_changed_signature_is_refused_by_name_and_nothing_at_all_is_applied() {
    let edited = EDITED
        .replace(
            "def plain(value):\n    return (\"after\", value * 10)",
            "def plain(value, scale):\n    return (\"after\", value * scale)",
        )
        .replace(
            "def spread(one, /, two, *rest, three, **extra):",
            "def spread(one, /, two, *, three, **extra):",
        );
    let (fixture, victim) = laid_out(VICTIM);
    let mut debuggee = launch(&fixture);
    held_before_the_report(&mut debuggee, &fixture);

    std::fs::write(&victim, &edited).expect("the fixture directory is writable");
    let replaced = debuggee
        .replace_code(&victim)
        .expect("the replacement was answered");

    let because = refused(&replaced);
    let named = because
        .iter()
        .find(|reason| matches!(reason, Unreplaceable::SignatureChanged { .. }))
        .unwrap_or_else(|| panic!("no reason named the signature: {because:#?}"));
    let Unreplaceable::SignatureChanged { function, was, now } = named else {
        unreachable!("the match above required it")
    };
    assert_eq!(function, "plain");
    assert_eq!((was.as_str(), now.as_str()), ("(value)", "(value, scale)"));

    // and the whole parameter grammar, written as a person wrote it. the
    // compiler lays `*rest` out *after* the keyword-only parameters in
    // `co_varnames`, so a refusal that copied the slots straight out would show
    // a signature nobody typed
    let spread = because
        .iter()
        .find_map(|reason| match reason {
            Unreplaceable::SignatureChanged { function, was, now } if function == "spread" => {
                Some((was.clone(), now.clone()))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("the second changed signature was not named: {because:#?}"));
    assert_eq!(
        spread,
        (
            "(one, /, two, *rest, three, **extra)".to_string(),
            // the bare `*` that makes the rest keyword-only, which is what the
            // edit wrote in place of `*rest`
            "(one, /, two, *, three, **extra)".to_string()
        )
    );

    // the whole of the point: everything *else* in that file was replaceable,
    // and none of it was replaced
    to_exit(&mut debuggee);
    assert!(
        recorded(&fixture).contains("('before', 1)"),
        "a refusal has to leave the process exactly as it was"
    );
}

#[test]
fn a_changed_module_body_is_refused_as_something_only_re_running_it_could_apply() {
    // the roadmap's "a module with import side effects", as the rule it is an
    // instance of: this module body *calls* its own decorator, and applying a
    // change to it would mean running that call a second time
    let edited = format!("{EDITED}\n\ndef added_later():\n    return 1\n");
    let (fixture, victim) = laid_out(VICTIM);
    let mut debuggee = launch(&fixture);
    held_before_the_report(&mut debuggee, &fixture);

    std::fs::write(&victim, &edited).expect("the fixture directory is writable");
    let replaced = debuggee
        .replace_code(&victim)
        .expect("the replacement was answered");

    let because = refused(&replaced);
    let named = because
        .iter()
        .find(|reason| matches!(reason, Unreplaceable::TopLevelChanged { .. }))
        .unwrap_or_else(|| panic!("no reason named the module body: {because:#?}"));
    let Unreplaceable::TopLevelChanged { differences, .. } = named else {
        unreachable!("the match above required it")
    };
    assert!(
        differences.iter().any(|difference| matches!(
            difference,
            Divergence::Defines { added, .. } if added.iter().any(|name| name == "added_later")
        )),
        "the refusal has to name what the body now defines, and said {differences:#?}"
    );
    assert!(
        named
            .to_string()
            .contains("running the program a second time"),
        "the refusal has to say why re-running the top level is not a reload"
    );

    to_exit(&mut debuggee);
    assert!(recorded(&fixture).contains("('before', 1)"));
}

#[test]
fn a_changed_class_body_is_refused_and_a_changed_method_body_is_not() {
    let edited = EDITED.replace("class Widget:\n", "class Widget:\n    LABEL = \"new\"\n\n");
    let (fixture, victim) = laid_out(VICTIM);
    let mut debuggee = launch(&fixture);
    held_before_the_report(&mut debuggee, &fixture);

    std::fs::write(&victim, &edited).expect("the fixture directory is writable");
    let replaced = debuggee
        .replace_code(&victim)
        .expect("the replacement was answered");

    let because = refused(&replaced);
    let named = because
        .iter()
        .find(|reason| matches!(reason, Unreplaceable::ClassLayoutChanged { .. }))
        .unwrap_or_else(|| panic!("no reason named the class: {because:#?}"));
    let Unreplaceable::ClassLayoutChanged { class, .. } = named else {
        unreachable!("the match above required it")
    };
    assert_eq!(class, "Widget");

    to_exit(&mut debuggee);
    assert!(
        recorded(&fixture).contains("'before'"),
        "a refusal has to leave the process exactly as it was"
    );

    // and the sibling claim, which is what makes the refusal about the *layout*
    // rather than about classes: `EDITED` changes `Widget.describe`'s body and
    // nothing else, and the first test in this file applies it to an instance
    // that already existed
}

/// what an applied replacement says about frames still on the old code
fn still_running(replaced: &Replacements) -> &Vec<bpd_core::StillRunning> {
    match &only(replaced).outcome {
        Replacement::Applied { still_running, .. } => still_running,
        Replacement::Refused { because } => {
            let said: Vec<String> = because.iter().map(ToString::to_string).collect();
            panic!("the replacement was refused: {said:#?}")
        }
    }
}

#[test]
fn a_replacement_asked_for_under_a_live_frame_is_applied_and_names_every_frame_it_left_behind() {
    // the trade this exists for, taken deliberately. the ordinary answer here is
    // the refusal the test below asserts — the same program, the same edit, the
    // same live frame — and the only difference is that the caller asked
    //
    // what makes it honest is that the cost is *returned* rather than assumed
    // away: the frame that goes on running the old body is named, so a caller
    // that wanted the replacement gets it and a caller that reads the answer
    // knows the process is on two versions of one function until it returns
    let (fixture, victim) = laid_out(VICTIM);
    let mut debuggee = launch(&fixture);

    let stop = stopped_inside(
        &mut debuggee,
        &victim,
        line_of(VICTIM, r#"    return ("before", value)"#),
    );

    std::fs::write(&victim, EDITED).expect("the fixture directory is writable");
    let replaced = debuggee
        .replace_code_even_under_a_live_frame(&victim)
        .expect("the replacement was answered");

    let left_behind = still_running(&replaced);
    let named = left_behind
        .iter()
        .find(|running| matches!(running.frame, LiveFrame::Thread { .. }))
        .unwrap_or_else(|| panic!("nothing named the running frame: {left_behind:#?}"));
    assert_eq!(named.function, "plain");
    let LiveFrame::Thread { thread, held, .. } = named.frame else {
        unreachable!("the match above required it")
    };
    assert_eq!(thread, stop.thread, "it named another thread");
    assert_eq!(
        held,
        Some(stop.stop),
        "the frame is one bpd is holding, and saying so is what separates it \
         from a thread that is running and was sampled"
    );

    // the sentence has to say what was traded, not merely that a frame exists.
    // a caller who asked for this and read `applied` with a bare list of frames
    // would have been told a fact and not its consequence
    let said = named.to_string();
    assert!(
        said.contains("two versions of one function"),
        "the report has to name what it cost, and said {said}"
    );

    // and the replacement really happened: the same assertion the ordinary
    // applied case makes, because "applied" that changed nothing would pass
    // every check above
    let (changed, _) = applied(&replaced);
    assert!(
        changed.iter().any(|one| one.function == "plain"),
        "nothing was actually replaced: {changed:#?}"
    );

    // the live frame runs the body it started with, to completion. that is
    // cpython's behaviour rather than bpd's choice, and it is the half of the
    // trade a caller cannot see from the report alone
    to_exit(&mut debuggee);
    let ran = recorded(&fixture);
    assert!(
        ran.contains("'before'"),
        "the frame that was already running finished on the old code, which is \
         what `still_running` was warning about. it recorded {ran}"
    );
}

#[test]
fn a_frame_running_the_code_refuses_the_replacement_and_names_where_it_is() {
    let (fixture, victim) = laid_out(VICTIM);
    let mut debuggee = launch(&fixture);

    // stopped *inside* the module being replaced, which is the case people most
    // want this in and the one it refuses. cpython would accept the assignment
    // — the frame keeps its own reference to its code object and would run the
    // old body to completion — and that is precisely what is being refused
    let stop = stopped_inside(
        &mut debuggee,
        &victim,
        line_of(VICTIM, r#"    return ("before", value)"#),
    );

    std::fs::write(&victim, EDITED).expect("the fixture directory is writable");
    let replaced = debuggee
        .replace_code(&victim)
        .expect("the replacement was answered");

    let because = refused(&replaced);
    let named = because
        .iter()
        .find(|reason| {
            matches!(
                reason,
                Unreplaceable::Running {
                    frame: LiveFrame::Thread { .. },
                    ..
                }
            )
        })
        .unwrap_or_else(|| panic!("no reason named a running frame: {because:#?}"));
    let Unreplaceable::Running {
        function,
        frame: LiveFrame::Thread { thread, held, .. },
    } = named
    else {
        unreachable!("the match above required it")
    };
    assert_eq!(function, "plain");
    assert_eq!(*thread, stop.thread, "the refusal named another thread");
    assert_eq!(
        *held,
        Some(stop.stop),
        "the frame is one bpd is holding, and saying so is what separates it \
         from a thread that is running and was sampled"
    );

    // the reason must never be given as crash prevention. measured on 3.13,
    // 3.14, 3.15 and 3.14t, cpython accepts this assignment and nothing aborts
    let said = named.to_string();
    assert!(
        said.contains("would be accepted by cpython") && said.contains("two versions"),
        "the refusal has to give the honest reason, and said {said}"
    );

    to_exit(&mut debuggee);
    assert!(recorded(&fixture).contains("('before', 1)"));
}

#[test]
fn a_suspended_generator_is_a_frame_that_will_run_the_code_and_refuses_it_too() {
    const WITH_GENERATOR: &str = r#"def counting():
    step = 1
    yield ("before", step)
    yield ("before", step + 1)
"#;
    const EDITED_GENERATOR: &str = r#"def counting():
    step = 1
    yield ("after", step)
    yield ("after", step + 1)
"#;
    const DRIVER: &str = r#"import pathlib

import held

HERE = pathlib.Path(__file__).parent
running = held.counting()
first = next(running)


def wait_here():
    ready = 1
    return ready


wait_here()
(HERE / "ran.txt").write_text(repr([first, next(running)]))
"#;

    let fixture = Fixture::new("driving", DRIVER);
    let generator = fixture.sibling("held", WITH_GENERATOR);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(DRIVER, "    ready = 1"),
    );

    std::fs::write(&generator, EDITED_GENERATOR).expect("the fixture directory is writable");
    let replaced = debuggee
        .replace_code(&generator)
        .expect("the replacement was answered");

    // it is on no thread's stack. only the generator object reaches it, and it
    // will run the old code the moment anything sends into it
    let because = refused(&replaced);
    let named = because
        .iter()
        .find(|reason| {
            matches!(
                reason,
                Unreplaceable::Running {
                    frame: LiveFrame::Suspended { .. },
                    ..
                }
            )
        })
        .unwrap_or_else(|| panic!("no reason named the suspended generator: {because:#?}"));
    let Unreplaceable::Running {
        frame: LiveFrame::Suspended { started, .. },
        ..
    } = named
    else {
        unreachable!("the match above required it")
    };
    assert!(*started, "the driver advanced it once before stopping");

    to_exit(&mut debuggee);
    assert!(
        recorded(&fixture).contains("('before', 2)"),
        "a refusal has to leave the process exactly as it was"
    );
}

#[test]
fn an_edit_that_does_not_compile_changes_nothing_and_hands_back_the_compilers_words() {
    let (fixture, victim) = laid_out(VICTIM);
    let mut debuggee = launch(&fixture);
    held_before_the_report(&mut debuggee, &fixture);

    std::fs::write(&victim, "def plain(value)\n    return 1\n")
        .expect("the fixture directory is writable");
    let replaced = debuggee
        .replace_code(&victim)
        .expect("the replacement was answered");

    let because = refused(&replaced);
    let named = because
        .iter()
        .find(|reason| matches!(reason, Unreplaceable::DoesNotCompile { .. }))
        .unwrap_or_else(|| panic!("no reason named the compiler: {because:#?}"));
    let Unreplaceable::DoesNotCompile { error, .. } = named else {
        unreachable!("the match above required it")
    };
    assert_eq!(
        error.kind, "SyntaxError",
        "the compiler's own error is what comes back, not a sentence of bpd's"
    );

    // compiling runs none of the program, which is why a broken edit is safe to
    // try. the program has to go on being what it was
    to_exit(&mut debuggee);
    assert!(recorded(&fixture).contains("('before', 1)"));
}

#[test]
fn a_file_the_interpreter_has_never_compiled_is_refused_rather_than_replaced() {
    let (fixture, _) = laid_out(VICTIM);
    let never = fixture.sibling("never_imported", "def unused():\n    return 1\n");
    let mut debuggee = launch(&fixture);
    held_before_the_report(&mut debuggee, &fixture);

    let replaced = debuggee
        .replace_code(&never)
        .expect("the replacement was answered");
    let because = refused(&replaced);
    assert!(
        because
            .iter()
            .any(|reason| matches!(reason, Unreplaceable::NotLoaded { .. })),
        "a module that was never imported has no live function objects, and it \
         said {because:#?}"
    );

    // and a path that is not a file at all is a different refusal, because it
    // is a different thing to fix
    let missing = fixture.directory().join("no_such_file.py");
    let replaced = debuggee
        .replace_code(&missing)
        .expect("the replacement was answered");
    assert!(
        refused(&replaced)
            .iter()
            .any(|reason| matches!(reason, Unreplaceable::NotAFile { .. })),
        "a path with nothing behind it is not a module that was not imported"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_class_body_carries_its_own_source_line_and_it_is_the_only_thing_masked() {
    // the cpython behaviour that forces the one exception in the comparison.
    // **since 3.13 every class body stores `__firstlineno__`**, so a class that
    // merely moved down the file has a genuinely different body — measured on
    // 3.13, 3.14, 3.15 and 3.14t. it is a line number, in the same category as
    // the line table, and left in the comparison it would refuse every edit
    // above a class as a changed class layout
    //
    // this is the test that fails if cpython changes: the first half moves the
    // class and requires the replacement to go through, the second half changes
    // what the class body really does and requires it to be refused
    let moved = EDITED.replace("        return \"after\"\n", "        return \"before\"\n");
    let (fixture, victim) = laid_out(VICTIM);
    let mut debuggee = launch(&fixture);
    held_before_the_report(&mut debuggee, &fixture);

    std::fs::write(&victim, &moved).expect("the fixture directory is writable");
    let replaced = debuggee
        .replace_code(&victim)
        .expect("the replacement was answered");
    let (changed, _) = applied(&replaced);
    assert!(
        changed.iter().any(|one| one.function == "Widget"),
        "the class body moved, so its code object is one that changed: {changed:#?}"
    );

    // and the same file with one thing the class body really does changed. if
    // the mask ever grew wide enough to swallow this, a replacement would leave
    // a class whose dictionary is a version behind and say it had applied
    let relabelled = moved.replace("class Widget:\n", "class Widget:\n    LABEL = \"new\"\n\n");
    std::fs::write(&victim, &relabelled).expect("the fixture directory is writable");
    let replaced = debuggee
        .replace_code(&victim)
        .expect("the replacement was answered");
    assert!(
        refused(&replaced)
            .iter()
            .any(|reason| matches!(reason, Unreplaceable::ClassLayoutChanged { .. })),
        "masking the class's own source line must not mask what the body does"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_breakpoint_in_the_replaced_file_is_bound_again_and_fires_where_the_code_is_now() {
    let (fixture, victim) = laid_out(VICTIM);
    let mut debuggee = launch(&fixture);
    held_before_the_report(&mut debuggee, &fixture);

    // armed on the old code object, at the old line. after the replacement that
    // code object is not what any thread will reach, and a breakpoint left
    // pointing at it is one the client can see is set and that never fires
    let was_at = line_of(VICTIM, r#"    return ("before", value)"#);
    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, &victim, was_at)])
        .expect("the breakpoint request was answered");
    assert!(matches!(resolved[0].binding, Binding::Bound { .. }));

    std::fs::write(&victim, EDITED).expect("the fixture directory is writable");
    let replaced = debuggee
        .replace_code(&victim)
        .expect("the replacement was answered");
    let (_, rebound) = applied(&replaced);

    // a breakpoint is a **line of a file**, and the file moved: line 5 was that
    // `return` and is now a blank line above the `def`. so it resolves forward
    // to the `def`, exactly as a breakpoint on a non-executable line always
    // does — and the point is that the client is *told*. left alone it would
    // have stayed armed on a code object no thread will ever reach again, which
    // is a breakpoint that can be seen to be set and never fires
    let moved = rebound
        .iter()
        .find(|resolution| resolution.id == 1)
        .unwrap_or_else(|| panic!("the breakpoint's binding was not reported again: {rebound:#?}"));
    match &moved.binding {
        Binding::Bound { line, .. } => assert_eq!(
            *line,
            line_of(EDITED, "def plain(value):"),
            "line {was_at} of the file on disk is no longer that statement, and \
             the answer has to say where the request lands now"
        ),
        other => panic!("the breakpoint came unbound: {other:?}"),
    }

    // and a breakpoint set on where that statement really is now fires in the
    // code that really is running
    let now_at = line_of(EDITED, r#"    return ("after", value * 10)"#);
    let stop = stopped_inside(&mut debuggee, &victim, now_at);
    assert_eq!(
        stop.reason,
        bpd_core::StopReason::Breakpoint {
            breakpoints: vec![1],
            file: victim.to_string_lossy().into_owned(),
            line: now_at,
        },
        "the replaced code is what the program reached"
    );

    to_exit(&mut debuggee);
    assert!(recorded(&fixture).contains("('after', 10)"));
}
