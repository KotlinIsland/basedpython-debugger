//! `.by` breakpoints, in a build `by run` runs, with a map checked against the
//! files it maps
//!
//! `by run` transpiles `.by` to `.py`, and its runner compiles each staged module
//! **as its `.by`**: named after the `.by` path, every line the `.by` line it came
//! from. so most of these spawn a real interpreter running `by run`'s own runner
//! over real generated python, and ask for a breakpoint in the `.by` it runs as.
//! the rest are about a module running as the generated python itself — what a
//! loader of the program's own stands in front of the runner for — and launch
//! the generated python directly, because that is what such a module is
//!
//! the pairs are written rather than transpiled and the runner is captured, and
//! [`bpd_test::basedpython`] is where the build they are written into lives —
//! along with the account of why. what is here is the pairs themselves, because
//! each one exists for the line table it carries

use std::path::{Path, PathBuf};

use bpd_core::Running;
use bpd_core::source_map::Mapping;
use bpd_core::{Binding, Resolved, SourceBreakpoint, StopReason, Unbound};
use bpd_engine::{Debuggee, Launched};
use bpd_test::basedpython::{Build, GENERATED, SOURCE, line_table};

/// a `.by` that raises, so a traceback has more than one frame of the build
///
/// a second pair rather than a flag on the first: what is under test is a
/// traceback, and a traceback needs a call in it
const RAISING: &str = "\
def boom() -> int:
    return 1 // 0


def main() -> None:
    boom()


main()  # the outermost frame the exception leaves
";

/// the python `by` would have transpiled [`RAISING`] to
const RAISING_GENERATED: &str = "\
import sys


def boom() -> int:
    return 1 // 0


def main() -> None:
    boom()


main()
";

/// which `.by` line each line of [`RAISING_GENERATED`] came from
fn raising_table() -> Vec<Option<u32>> {
    vec![
        // the three line prelude, which no `.by` line is behind
        None,
        None,
        None,
        Some(0), // def boom
        Some(1), // return 1 // 0
        Some(2),
        Some(3),
        Some(4), // def main
        Some(5), // boom()
        Some(6),
        Some(7),
        Some(8), // main()
    ]
}

/// the build this file's pairs make
///
/// [`Build::pair`] and the map it writes are in `bpd_test` because the DAP
/// acceptance asks the same questions from the other end of the adapter; what is
/// local here is which pair, which is what each case is about
fn build() -> Build {
    Build::demo()
}

/// the same pair, mapped by a line table this case wrote
fn build_mapped(lines: &[Option<u32>]) -> Build {
    Build::pair(SOURCE, GENERATED, lines)
}

/// the build whose program raises, for the traceback
fn raising_build() -> Build {
    Build::pair(RAISING, RAISING_GENERATED, &raising_table())
}

/// run the build the way `by run` does: its runner, running the build's module
fn by_run(build: &Build) -> Debuggee {
    launch_program(&build.runner(), &build.arguments())
}

/// launch the generated python itself, which is a module running as generated
/// python rather than as its `.by`
fn launch_generated(build: &Build) -> Debuggee {
    launch_program(&build.generated, &[build.marks.clone().into_os_string()])
}

/// launch one file of the build directory, so the map beside it is found
fn launch_program(program: &Path, arguments: &[std::ffi::OsString]) -> Debuggee {
    match bpd_engine::launch(
        bpd_test::agent::matching_interpreter(),
        &bpd_engine::Program::Script(program.to_path_buf()),
        arguments,
    ) {
        Ok(Launched::Stopped(debuggee)) => debuggee,
        Ok(Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

/// set one breakpoint and say what became of it
fn set(debuggee: &mut Debuggee, line: u32, file: &Path) -> Resolved {
    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, file, line)])
        .expect("the breakpoint request was answered");
    let [only] = <[Resolved; 1]>::try_from(resolved).expect("one breakpoint was asked about");
    only
}

/// why a breakpoint did not bind, or where it did
fn refused(resolved: &Resolved) -> &Unbound {
    match &resolved.binding {
        Binding::Unbound { reason } => reason,
        bound => panic!("the breakpoint was supposed to be refused, and it is {bound:?}"),
    }
}

/// the last thing a run said about breakpoint `id`, among what it rebound
fn last_about(id: u32, rebound: &[Resolved]) -> &Resolved {
    rebound
        .iter()
        .rev()
        .find(|one| one.id == id)
        .unwrap_or_else(|| panic!("nothing was said about breakpoint {id}: {rebound:?}"))
}

#[test]
fn a_by_breakpoint_is_pending_until_by_run_compiles_the_by_and_then_binds_on_its_line() {
    // the regression this file is pinned against. `by run` compiles the module
    // as the `.by` it came from, so a `.by` breakpoint is a breakpoint on a file
    // the interpreter **does** compile — and a debugger that sent it on as a line
    // of the generated python would be waiting for code nothing ever names
    let build = build();
    let mut debuggee = by_run(&build);
    let asked = bpd_test::debuggee::line_of(SOURCE, "print(answer)");

    let resolved = set(&mut debuggee, asked, &build.source);
    // held at entry, the runner has not run the module yet
    let reason = refused(&resolved);
    assert!(
        matches!(reason, Unbound::NotLoaded { file, .. } if *file == build.source),
        "before the runner compiles it there is no code to bind to: {reason:?}"
    );
    assert!(reason.will_bind_later(), "and it will bind when there is");

    let (reason, rebound) = run_to_stop(&mut debuggee);
    let Binding::Bound { line, sites, .. } = &last_about(1, &rebound).binding else {
        panic!("compiling the module bound it: {rebound:?}")
    };
    assert_eq!(*line, asked, "on the `.by` line that was asked for");
    assert!(
        !sites.is_empty(),
        "a bound breakpoint has a code object behind it"
    );
    let StopReason::Breakpoint { file, line, .. } = &reason else {
        panic!("the program was supposed to stop on the breakpoint, and it {reason:?}")
    };
    assert_eq!(Path::new(file), build.source);
    assert_eq!(*line, asked);
    assert_eq!(
        build.answer(),
        "",
        "the program is held before the line ran, so it has written nothing"
    );
}

#[test]
fn the_stack_is_the_by_the_interpreter_runs_and_the_runner_under_it_is_itself() {
    // the consistency rule, which is the one that matters most: the stop and the
    // frame are one location, every frame of the build is the `.by`, and nothing
    // under the build — the runner `by run` starts, the import machinery — is
    // dressed as basedpython
    let build = build();
    let mut debuggee = by_run(&build);
    let asked = bpd_test::debuggee::line_of(SOURCE, "print(answer)");
    set(&mut debuggee, asked, &build.source);
    let (reason, _) = run_to_stop(&mut debuggee);
    let StopReason::Breakpoint { file, line, .. } = &reason else {
        panic!("it was supposed to stop on the breakpoint: {reason:?}")
    };

    let stack = debuggee.the_stack(None).expect("the stack was walked");

    let top = stack.frames.first().expect("a held thread has a stack");
    assert_eq!(&top.file, file, "the stop and the frame are one location");
    assert_eq!(top.line, *line);
    assert_eq!(
        top.mapping, None,
        "the interpreter is running the `.by` itself, so nothing was mapped: {top:?}"
    );

    let build_frames = stack
        .frames
        .iter()
        .filter(|frame| Path::new(&frame.file) == build.source)
        .count();
    assert!(
        build_frames >= 2,
        "`main` and the module body under it are both the `.by`: {:?}",
        stack.frames
    );
    assert!(
        stack
            .frames
            .iter()
            .all(|frame| Path::new(&frame.file) != build.generated),
        "no frame names the generated python: {:?}",
        stack.frames
    );
    assert!(
        stack
            .frames
            .iter()
            .any(|frame| frame.file.ends_with("_by_runner.py") && frame.mapping.is_none()),
        "the runner that started the build is on the stack as itself: {:?}",
        stack.frames
    );
}

#[test]
fn a_generated_line_no_by_line_is_behind_is_reported_as_python_and_says_why() {
    // a module running as the generated python — a loader of the program's own
    // compiled it — is reported through the map. a prelude line has no `.by`
    // behind it, the map says so itself, and reporting one as a `.by` line would
    // be the debugger writing a line the user never did. so the location stays
    // the generated one, and the frame carries the map's own reason rather than
    // leaving a temporary path in front of a user with nothing to explain it
    let build = build();
    let mut debuggee = launch_generated(&build);
    let prelude = bpd_test::debuggee::line_of(GENERATED, "from pathlib import Path");

    let resolved = set(&mut debuggee, prelude, &build.generated);
    assert!(
        matches!(resolved.binding, Binding::Bound { .. }),
        "a breakpoint in the generated python binds as ordinary python: {resolved:?}"
    );
    let (reason, _) = run_to_stop(&mut debuggee);

    let StopReason::Breakpoint { file, line, .. } = &reason else {
        panic!("it was supposed to stop in the prelude: {reason:?}")
    };
    assert_eq!(Path::new(file), build.generated);
    assert_eq!(*line, prelude);

    let stack = debuggee.the_stack(None).expect("the stack was walked");
    let top = stack.frames.first().expect("a held thread has a stack");
    let Some(Mapping::InGeneratedPython { reason }) = &top.mapping else {
        panic!("a prelude frame says what the map said about it, and this is {top:?}")
    };
    assert!(
        matches!(reason, bpd_core::Unmapped::NoSourceLine { .. }),
        "{reason:?}"
    );
    let said = reason.to_string();
    assert!(said.contains("demo.py"), "{said}");
    assert!(said.contains("demo.by"), "{said}");
}

#[test]
fn a_by_line_the_transpiler_generated_nothing_for_moves_to_the_next_one_it_did() {
    // the comment above `def main`, which the transpile does not keep. `by run`
    // compiled no code onto it, so a breakpoint on it moves forward exactly as it
    // does in ordinary python, and the answer says where it went
    let build = build();
    let mut debuggee = by_run(&build);
    let comment = bpd_test::debuggee::line_of(SOURCE, "# a comment");
    set(&mut debuggee, comment, &build.source);

    let (reason, rebound) = run_to_stop(&mut debuggee);

    let def_main = bpd_test::debuggee::line_of(SOURCE, "def main");
    let Binding::Bound { line, .. } = &last_about(1, &rebound).binding else {
        panic!("it was supposed to move forward and bind: {rebound:?}")
    };
    assert_eq!(
        *line, def_main,
        "line {comment} has no code, so the breakpoint moved to the next `.by` line \
         that does — and the answer says which one that is"
    );
    assert!(
        matches!(&reason, StopReason::Breakpoint { line, .. } if *line == def_main),
        "{reason:?}"
    );
}

#[test]
fn a_by_line_past_everything_the_transpiler_generated_is_unbound_with_the_reason() {
    let build = build();
    let mut debuggee = by_run(&build);
    let past = u32::try_from(SOURCE.lines().count() + 10).expect("a fixture is not that long");
    set(&mut debuggee, past, &build.source);

    let rebound = run_to_exit(&mut debuggee);

    let reason = refused(last_about(1, &rebound));
    assert!(
        matches!(reason, Unbound::NoExecutableLine { requested, .. } if *requested == past),
        "expected the file's own line table to refuse it, got {reason:?}"
    );
    assert!(
        reason.to_string().contains("demo.by"),
        "the refusal names the file: {reason}"
    );
}

#[test]
fn code_the_transpiler_wrote_on_its_own_is_never_a_by_line_a_breakpoint_lands_on() {
    // the whole rule, driven end to end. the table here says the `.by`'s last
    // line became a generated line the transpiler emitted on its own — so `by
    // run` compiles that code onto line 0, the line no `.by` has. a breakpoint on
    // the blank line before it walks forward off the end of what has a source,
    // and binding to the call anyway is exactly the answer a fallback would give
    // and exactly the answer that would be a lie
    let mut lines = line_table();
    let last = lines.len() - 1;
    lines[last] = None;
    let build = build_mapped(&lines);
    let mut debuggee = by_run(&build);
    let asked = bpd_test::debuggee::line_of(SOURCE, "print(answer)") + 1;
    set(&mut debuggee, asked, &build.source);

    let rebound = run_to_exit(&mut debuggee);

    let reason = refused(last_about(1, &rebound));
    assert!(
        matches!(reason, Unbound::NoExecutableLine { .. }),
        "expected a refusal rather than a `.by` line nobody wrote, got {reason:?}"
    );
    assert_eq!(
        build.answer().trim(),
        "5",
        "the call ran, as line 0, and nothing stopped on it"
    );
}

#[test]
fn a_by_breakpoint_in_a_program_with_no_map_is_refused_naming_what_makes_one() {
    // no map at all, which is what a `.by` breakpoint in an ordinary python
    // session is. the alternative would be binding to a `.py` of the same name
    // and hoping the lines line up
    let fixture = bpd_test::debuggee::Fixture::new("plain", "x = 1\nprint(x)\n");
    let mut debuggee = match bpd_engine::launch(
        bpd_test::agent::matching_interpreter(),
        &bpd_engine::Program::Script(fixture.directory().join("plain.py")),
        &[],
    ) {
        Ok(Launched::Stopped(debuggee)) => debuggee,
        other => panic!("the debuggee did not stop: {other:?}"),
    };
    let by = fixture.directory().join("plain.by");
    std::fs::write(&by, SOURCE).expect("a `.by` on disk that nothing transpiled");

    let resolved = set(&mut debuggee, 1, &by);

    let reason = refused(&resolved);
    assert!(
        matches!(reason, Unbound::NoSourceMap { .. }),
        "expected a refusal naming the missing map, got {reason:?}"
    );
    assert!(reason.to_string().contains("by run --launcher"), "{reason}");
}

#[test]
fn a_by_the_build_does_not_hold_is_refused_rather_than_left_waiting() {
    // a map, and a `.by` it says nothing about. the interpreter could compile a
    // file of that name one day, but not in this program: `by run` staged this
    // build, and a `.by` outside it is never run as one. "not loaded yet" would
    // be a promise nothing keeps
    let build = build();
    let mut debuggee = by_run(&build);
    let stranger = build.root().join("stranger.by");
    std::fs::write(&stranger, SOURCE).expect("a `.by` the build never staged");

    let resolved = set(&mut debuggee, 1, &stranger);

    let reason = refused(&resolved);
    assert!(
        matches!(
            reason,
            Unbound::Unmappable {
                reason: bpd_core::Unmapped::NotInTheMap { .. }
            }
        ),
        "{reason:?}"
    );
    assert!(!reason.will_bind_later(), "{reason}");
}

#[test]
fn a_by_edited_since_the_transpile_refuses_the_launch_rather_than_the_line() {
    // the milestone. the map still describes a pair of files, one of them is no
    // longer that file, and every line it would report is wrong with total
    // confidence — so nothing is reported and the program is not debugged at all
    let build = build();
    std::fs::write(
        &build.source,
        format!("# a line the build never saw\n{SOURCE}"),
    )
    .expect("the user edits their `.by` and forgets to transpile");

    let program = bpd_engine::Program::Script(build.runner());
    let error = match bpd_engine::launch(
        bpd_test::agent::matching_interpreter(),
        &program,
        &build.arguments(),
    ) {
        Err(error) => error,
        Ok(other) => panic!("a stale build was launched anyway: {other:?}"),
    };

    let said = format!("{error}: {}", source_chain(&error));
    assert!(said.contains("stale"), "{said}");
    assert!(
        said.contains("demo.by"),
        "the refusal names the file: {said}"
    );
    assert!(said.contains("transpile again"), "{said}");
}

#[test]
fn a_python_breakpoint_in_a_mapped_build_is_untouched_by_the_map() {
    // the map is about `.by` files. a breakpoint in the generated python is a
    // breakpoint in a file the interpreter can have, and it binds the way any
    // python one does
    let build = build();
    let mut debuggee = launch_generated(&build);
    let line = bpd_test::debuggee::line_of(GENERATED, "write_text");

    let resolved = set(&mut debuggee, line, &build.generated);

    assert!(
        matches!(resolved.binding, Binding::Bound { .. }),
        "a python breakpoint stays a python one: {resolved:?}"
    );
}

#[test]
fn an_exception_of_the_build_is_reported_in_by_lines_all_the_way_down() {
    // a traceback is a location too, and one entry naming the generated python
    // beside a stack that does not would be two answers about one place. a
    // module running as generated python is the one whose traceback has to be
    // mapped, so that is what runs here
    let build = raising_build();
    let mut debuggee = launch_generated(&build);
    debuggee
        .set_exception_breakpoints(false, true)
        .expect("the exception breakpoints were set");

    let (reason, _) = run_to_stop(&mut debuggee);

    let StopReason::Uncaught { error, file, line } = &reason else {
        panic!("it was supposed to stop where the exception leaves: {reason:?}")
    };
    assert_eq!(Path::new(file), build.source);
    assert_eq!(
        *line,
        bpd_test::debuggee::line_of(RAISING, "the outermost frame")
    );
    let named: Vec<&str> = error
        .traceback
        .iter()
        .map(|frame| frame.file.as_str())
        .collect();
    assert!(
        named.len() >= 2,
        "the exception came through the frames it was raised in: {named:?}"
    );
    for file in &named {
        assert_eq!(
            Path::new(file),
            build.source,
            "every frame of this traceback is the build's: {named:?}"
        );
    }
    assert!(
        error
            .traceback
            .iter()
            .any(|frame| frame.line == bpd_test::debuggee::line_of(RAISING, "1 // 0")),
        "and the line it was raised on is the `.by`'s: {:?}",
        error.traceback
    );
}

#[test]
fn the_source_around_a_by_frame_is_the_by_and_is_proved_the_way_by_run_compiled_it() {
    // showing the generated python beside a `.by` location would be the
    // contradiction this milestone is about. the frame's code object is proved
    // by compiling the generated python the way `by run` did — line table
    // included, so the `.by` lines it carries are the runner's own — and the
    // `.by` is read on the debuggee's filesystem and checked against the digest
    // the transpiler wrote
    let build = build();
    let mut debuggee = by_run(&build);
    let asked = bpd_test::debuggee::line_of(SOURCE, "print(answer)");
    set(&mut debuggee, asked, &build.source);
    run_to_stop(&mut debuggee);

    let source = the_source(&mut debuggee, 2);

    let bpd_core::Source::Lines {
        at, lines, total, ..
    } = &source
    else {
        panic!("the `.by` is on disk and is the file the map describes: {source:?}")
    };
    assert_eq!(*at, asked, "the window is around the `.by` line");
    assert_eq!(
        *total,
        u32::try_from(SOURCE.lines().count()).expect("a fixture is not that long"),
        "the file is the `.by`, so its length is the `.by`'s"
    );
    assert!(
        lines.iter().any(|line| line.contains("print(answer)")),
        "these are the lines of the `.by` the user wrote: {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("write_text")),
        "and not the generated python's: {lines:?}"
    );
}

#[test]
fn a_frame_in_code_the_transpiler_wrote_is_line_zero_and_has_no_source_to_show() {
    // the table here says the line that raises is one the transpiler wrote on
    // its own, so `by run` compiles it onto line 0 of the `.by`. a stop there is
    // a stop in the `.by` — the code is named after it — on a line no `.by` has,
    // and the source beside it is proved and then refused by name rather than
    // shown as whichever `.by` line was nearest
    let mut lines = raising_table();
    let raises = usize::try_from(bpd_test::debuggee::line_of(RAISING_GENERATED, "1 // 0") - 1)
        .expect("a fixture line fits");
    lines[raises] = None;
    let build = Build::pair(RAISING, RAISING_GENERATED, &lines);
    let mut debuggee = by_run(&build);
    debuggee
        .set_exception_breakpoints(true, false)
        .expect("the exception breakpoints were set");

    // the import machinery raises and catches on its own on the way to the
    // build, and every raise is a stop. the one this is about is the build's
    let reason = loop {
        let (reason, _) = run_to_stop(&mut debuggee);
        if matches!(&reason, StopReason::Raised { file, .. } if Path::new(file) == build.source) {
            break reason;
        }
    };

    let StopReason::Raised { file, line, .. } = &reason else {
        panic!("it was supposed to stop where the exception was raised: {reason:?}")
    };
    assert_eq!(Path::new(file), build.source);
    assert_eq!(
        *line, 0,
        "the line the runner gives code with no `.by` line"
    );

    let source = the_source(&mut debuggee, 2);
    let bpd_core::Source::Unverified { why } = &source else {
        panic!("there is no `.by` line to show, and lines were shown: {source:?}")
    };
    assert!(
        matches!(why, bpd_core::Unverified::TranspilerWritten { function, .. } if function == "boom"),
        "{why:?}"
    );
}

#[test]
fn a_by_edited_after_the_launch_refuses_the_source_rather_than_showing_it() {
    // `bpd` checked this file at launch and the user is asking about now. an
    // editor that saved the `.by` in between leaves a file whose lines are
    // wrong with total confidence, which is the failure a source map exists to
    // prevent
    let build = build();
    let mut debuggee = by_run(&build);
    let asked = bpd_test::debuggee::line_of(SOURCE, "print(answer)");
    set(&mut debuggee, asked, &build.source);
    run_to_stop(&mut debuggee);
    std::fs::write(&build.source, format!("# an edit\n{SOURCE}")).expect("the user saves");

    let source = the_source(&mut debuggee, 2);

    let bpd_core::Source::Unverified { why } = &source else {
        panic!("the `.by` moved and its lines were shown anyway: {source:?}")
    };
    assert!(
        matches!(why, bpd_core::Unverified::NotTheSameSource { .. }),
        "{why:?}"
    );
    let said = why.to_string();
    assert!(said.contains("demo.by"), "{said}");
    assert!(said.contains("transpile again"), "{said}");
}

#[test]
fn a_by_frame_is_moved_by_naming_a_by_line() {
    // the inbound half. a frame says `demo.by:11`, and the line a client names
    // against it is a line of `demo.by` — which, in a module `by run` compiled,
    // is also the line the interpreter's own table holds
    let build = build();
    let mut debuggee = by_run(&build);
    let asked = bpd_test::debuggee::line_of(SOURCE, "print(answer)");
    set(&mut debuggee, asked, &build.source);
    run_to_stop(&mut debuggee);
    let frame = debuggee
        .the_stack(None)
        .expect("the stack was walked")
        .frames
        .first()
        .expect("a held thread has a stack")
        .id;

    // back to the line above, which is `answer = add(2, 3)` in the `.by`
    let back = bpd_test::debuggee::line_of(SOURCE, "answer = add(2, 3)");
    let jumped = debuggee
        .set_next_statement(frame, back)
        .expect("the frame was moved");

    assert!(
        matches!(jumped.outcome, bpd_core::Jump::Moved { .. }),
        "{jumped:?}"
    );
    assert_eq!(
        Path::new(&jumped.at.file),
        build.source,
        "where the frame is now is said the way a client was told the rest"
    );
    assert_eq!(
        jumped.at.line, back,
        "and it is the `.by` line that was named"
    );
}

#[test]
fn where_a_thread_is_is_sampled_in_by_terms() {
    // a `Where` has no frame id on it, and it is still the same location a frame
    // of the same code reports. one of them naming the other file would be the
    // contradiction
    let build = build();
    let mut debuggee = by_run(&build);
    let asked = bpd_test::debuggee::line_of(SOURCE, "print(answer)");
    set(&mut debuggee, asked, &build.source);
    run_to_stop(&mut debuggee);

    let census = debuggee
        .threads(std::time::Duration::from_millis(0))
        .expect("the threads were sampled");

    let places: Vec<&bpd_core::Where> = census
        .threads
        .iter()
        .filter_map(|thread| thread.at.as_ref())
        .collect();
    assert!(
        !places.is_empty(),
        "the held thread is somewhere: {census:?}"
    );
    assert!(
        places
            .iter()
            .any(|at| Path::new(&at.file) == build.source && at.line == asked),
        "the held thread is at the `.by` line the stack reports: {places:?}"
    );
}

/// the source around the frame that stopped, as a state query reads it
fn the_source(debuggee: &mut Debuggee, around: u32) -> bpd_core::Source {
    let snapshot = debuggee
        .the_query(bpd_core::StateQuery {
            frames: 1,
            source: Some(around),
            ..bpd_core::StateQuery::default()
        })
        .expect("the state was read");
    snapshot
        .state
        .frames
        .into_iter()
        .next()
        .expect("a held thread has a frame")
        .source
        .expect("the query asked for source")
}

/// run to the next stop, and hand back why it stopped and what it rebound
fn run_to_stop(debuggee: &mut Debuggee) -> (StopReason, Vec<Resolved>) {
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, rebound } => (stop.reason, rebound),
        Running::Exited {
            status, rebound, ..
        } => panic!(
            "it exited with {status} instead of stopping. what it said about \
             the breakpoints was {rebound:?}"
        ),
        other => panic!("the program neither stopped nor exited: {other:?}"),
    }
}

/// run to the end, and hand back what the run rebound on the way
fn run_to_exit(debuggee: &mut Debuggee) -> Vec<Resolved> {
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Exited { rebound, .. } => rebound,
        other => panic!("the program was supposed to run to the end: {other:?}"),
    }
}

/// every cause behind an error, joined, so an assertion can look through it
fn source_chain(error: &dyn std::error::Error) -> String {
    let mut said = String::new();
    let mut source = error.source();
    while let Some(cause) = source {
        said.push_str(&cause.to_string());
        said.push_str(" | ");
        source = cause.source();
    }
    said
}

// ── staging the build again, which is what a hot reload is ──────────────────

/// a `.by` whose module body only **defines**, so a program can be stopped while
/// none of it is running
///
/// [`SOURCE`] calls `main()` at the end, which is a fine program and the wrong
/// shape for this: a module that calls into itself has its own module frame live
/// for as long as the call lasts, and a replacement is refused while any frame of
/// the file is running. that refusal is bpd's own rule and is tested where it
/// belongs; what is being tested here is what happens when it does *not* apply
const IMPORTED: &str = "\
def add(a: int, b: int) -> int:
    total = a + b
    return total


# a comment, which the transpile does not keep
def main() -> None:
    answer = add(2, 3)
    print(answer)
";

/// the python [`IMPORTED`] transpiles to
const IMPORTED_GENERATED: &str = "\
from __future__ import annotations
import os
from pathlib import Path
import sys


def add(a: int, b: int) -> int:
    total = a + b
    return total


def main() -> None:
    answer = add(2, 3)
    Path(sys.argv[1]).write_text(str(answer) + os.linesep)
";

/// [`IMPORTED`] after the user changed what `main` adds, on the same line
const IMPORTED_EDITED: &str = "\
def add(a: int, b: int) -> int:
    total = a + b
    return total


# a comment, which the transpile does not keep
def main() -> None:
    answer = add(2, 4)
    print(answer)
";

/// what `by` staged [`IMPORTED_EDITED`] to, with one more line of prelude
///
/// the body of `main` is different code, and every generated line below the
/// prelude moved — which is what makes the table matter: the new code is
/// compiled onto the `.by` through the **new** table, and the old one would put
/// every line of it one line off
const IMPORTED_EDITED_GENERATED: &str = "\
from __future__ import annotations
import os
from pathlib import Path
import sys



def add(a: int, b: int) -> int:
    total = a + b
    return total


def main() -> None:
    answer = add(2, 4)
    Path(sys.argv[1]).write_text(str(answer) + os.linesep)
";

/// which `.by` line each generated line of [`IMPORTED_GENERATED`] came from
fn imported_table() -> Vec<Option<u32>> {
    vec![
        None,
        None,
        None,
        None,
        None,
        None,
        Some(0), // def add
        Some(1), // total = a + b
        Some(2), // return total
        Some(3),
        Some(4),
        Some(6), // def main, the `.by`'s comment on line 5 generating nothing
        Some(7), // answer = add(2, 3)
        Some(8), // print(answer), which became a write_text
    ]
}

/// [`imported_table`] with the prelude one line longer, to match
/// [`IMPORTED_EDITED_GENERATED`]
fn imported_edited_table() -> Vec<Option<u32>> {
    let mut lines = vec![None];
    lines.extend(imported_table());
    lines
}

/// an entry module of the build that **imports** the `.by` and stops outside it
///
/// what `by run` really runs is a module that imports others, and the runner
/// compiles each one it imports as its `.by`. this one is plain python — a
/// hand-written `.py` carried into the build — so it is compiled as itself, and
/// at its breakpoint `demo` has been imported and its module body has returned:
/// no frame of the file being replaced is on the stack
fn entry(build: &Build) -> PathBuf {
    let path = build.root().join("entry.py");
    std::fs::write(
        &path,
        "import demo\n\
         held = True  # the line this stops on\n\
         demo.main()\n",
    )
    .expect("the entry module is written");
    path
}

/// **the whole of a hot reload from bpd's side, in one request.**
///
/// `by` has staged one file of the build again after an edit: the generated
/// python is new, and `_by_sourcemap.py` beside it was rewritten — so the tables
/// this session holds describe the tree it used to be.
///
/// What is asserted is that the code that replaces `main` is the code `by run`
/// would have compiled from the new tree: named after the `.by`, on the `.by`'s
/// lines through the new table — so the breakpoint the user set on a `.by` line
/// before the edit binds in it and stops the program there, and the program runs
/// the edit.
#[test]
fn a_by_breakpoint_binds_and_hits_in_the_code_a_restage_replaced() {
    let build = Build::pair(IMPORTED, IMPORTED_GENERATED, &imported_table());
    let entry = entry(&build);
    let mut debuggee = launch_program(&build.runner(), &build.arguments_running("entry"));

    let asked = bpd_test::debuggee::line_of(IMPORTED, "print(answer)");
    let held = bpd_test::debuggee::line_of(
        &std::fs::read_to_string(&entry).expect("the entry module was just written"),
        "held = True",
    );
    let resolved = debuggee
        .set_breakpoints(vec![
            // the one that stops the program, in the entry module — which is not
            // part of the map and is bound as the python it is
            SourceBreakpoint::at(1, &entry, held),
            // and the one this is about, on a line of the `.by` about to change
            SourceBreakpoint::at(2, &build.source, asked),
        ])
        .expect("the breakpoints were answered");
    let pending = resolved
        .iter()
        .find(|one| one.id == 2)
        .expect("the `.by` breakpoint was answered");
    assert!(
        matches!(
            &pending.binding,
            Binding::Unbound {
                reason: Unbound::NotLoaded { .. }
            }
        ),
        "before the import there is no code to bind to: {pending:?}"
    );

    let (reason, rebound) = run_to_stop(&mut debuggee);
    assert!(
        matches!(&reason, StopReason::Breakpoint { file, .. } if Path::new(file) == entry),
        "the program stops in the entry module, with the build imported and none of it running: {reason:?}"
    );
    assert!(
        matches!(&last_about(2, &rebound).binding, Binding::Bound { line, .. } if *line == asked),
        "importing the `.by` bound its breakpoint on the line asked for: {rebound:?}"
    );

    // what `by` does to the tree, done here directly: this suite builds its own
    // builds rather than shelling out to `by`, for the reason every fixture in it
    // does — what is under test is bpd's half, and a test that needed the
    // transpiler would be testing two things and reporting one
    std::fs::write(&build.source, IMPORTED_EDITED).expect("the user edits the `.by`");
    std::fs::write(&build.generated, IMPORTED_EDITED_GENERATED)
        .expect("the re-staged python is written");
    build.write_map(&imported_edited_table());

    // named as the `.by`, and replaced from the generated python behind it
    let replaced = debuggee
        .restage_and_replace([build.source.clone()])
        .expect("the replacement was answered");

    let [only] =
        <[bpd_core::Replaced; 1]>::try_from(replaced.files.clone()).expect("one file was replaced");
    let bpd_core::Replacement::Applied {
        changed, unchanged, ..
    } = &only.outcome
    else {
        panic!("a body that changed is applicable: {:#?}", only.outcome)
    };
    assert_eq!(
        changed
            .iter()
            .map(|rebound| rebound.function.as_str())
            .collect::<Vec<_>>(),
        ["main"],
        "the edit is to `main`, and only `main` is different code — `add` compiled \
         through the new table onto the same `.by` lines is the code it was: \
         {changed:?} {unchanged:?}"
    );
    let remapped = replaced
        .remapped
        .as_ref()
        .unwrap_or_else(|| panic!("a remap was asked for and the tables moved: {replaced:#?}"));
    assert_eq!(remapped.directory, build.root());
    assert_eq!(remapped.files, 1, "the build has one mapped file");

    let (reason, _) = run_to_stop(&mut debuggee);
    let StopReason::Breakpoint { file, line, .. } = &reason else {
        panic!("the `.by` breakpoint was supposed to stop the new code: {reason:?}")
    };
    assert_eq!(Path::new(file), build.source);
    assert_eq!(*line, asked);
    assert_eq!(build.answer(), "", "held before the line ran");

    run_to_exit(&mut debuggee);
    assert_eq!(
        build.answer().trim(),
        "6",
        "the program ran the edit, not the code it was launched with"
    );
}
