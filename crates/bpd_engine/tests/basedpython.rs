//! `.by` breakpoints, through a map that was checked against the files it maps
//!
//! basedpython transpiles `.by` to `.py` and the interpreter runs the `.py`, so
//! every one of these spawns a real interpreter running real generated python
//! and asks for a breakpoint in a file the interpreter has never heard of
//!
//! ## why the pairs here are written rather than transpiled
//!
//! a source map is a claim about two files and a line table between them, and
//! the digests are what make the claim checkable. nothing about `bpd` reading
//! one depends on the transpiler having produced it — the same file written by
//! hand with true digests **is** a valid map, and the format it has to be in is
//! pinned separately against output captured from `by run` itself, in
//! `bpd_core::source_map`
//!
//! what writing them buys is the line table. the cases that decide whether the
//! source mapping rule holds — a generated line the transpiler invented, a `.by`
//! line nothing was generated for, a `.by` edited since the build — are each one
//! entry in that table, and reaching them through a real `by` would mean finding
//! a basedpython program that happens to produce each one and then depending on
//! it going on producing it. this depends on the *rule* instead
//!
//! it also means these run wherever `cargo test` runs. the `by` binary is a
//! sibling repository rather than a package this suite installs, and a test that
//! is skipped when it is missing is a test that reports success while proving
//! nothing

use std::path::{Path, PathBuf};

use bpd_core::Running;
use bpd_core::source_map::{MAP_FILENAME, Mapping};
use bpd_core::{Binding, Resolved, SourceBreakpoint, StopReason, Unbound};
use bpd_engine::{Debuggee, Launched};

/// the `.by` a person wrote
///
/// what is in it barely matters — the interpreter never reads it — but it is
/// real basedpython, and `line_of` is what names a line of it rather than a
/// number that goes stale
const SOURCE: &str = "\
def add(a: int, b: int) -> int:
    total = a + b
    return total


# a comment, which the transpile does not keep
def main() -> None:
    answer = add(2, 3)
    print(answer)


main()  # entry
";

/// the python `by` would have transpiled [`SOURCE`] to
///
/// the six line prelude is the point of it, and so is the comment that is not
/// here: the offset between the two files is not a constant, and a debugger that
/// assumed one would be reporting the wrong line of the wrong file with total
/// confidence
const GENERATED: &str = "\
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


main()
";

/// which `.by` line each generated line came from, zero-based on both sides
///
/// `None` is prelude. the two `import` lines of [`SOURCE`] have no counterpart
/// here on purpose: the transpiler emits its own imports and drops the source's,
/// which is why a line table is not an offset
fn line_table() -> Vec<Option<u32>> {
    vec![
        // the six line prelude, which no `.by` line is behind
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
        // the `.by`'s comment is line 5 and became nothing, so the next
        // generated line skips it — this is why a line table is not an offset
        Some(6), // def main
        Some(7), // answer = add(2, 3)
        Some(8), // print(answer), which became a write_text
        Some(9),
        Some(10),
        Some(11), // main()
    ]
}

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

/// a basedpython build directory: the `.by`, the python, and the map
struct Build {
    directory: tempfile::TempDir,
    source: PathBuf,
    generated: PathBuf,
    /// where the program writes its answer, so a stop is provable
    marks: PathBuf,
}

impl Build {
    fn new() -> Self {
        Self::with(&line_table())
    }

    fn with(lines: &[Option<u32>]) -> Self {
        Self::pair(SOURCE, GENERATED, lines)
    }

    /// the build whose program raises, for the traceback
    fn raising() -> Self {
        Self::pair(RAISING, RAISING_GENERATED, &raising_table())
    }

    fn pair(by: &str, py: &str, lines: &[Option<u32>]) -> Self {
        let directory = tempfile::tempdir().expect("a temporary directory");
        // canonicalised for the reason every fixture in this suite is: a
        // temporary directory is under `/var` on macos and `/tmp` names it
        // through a symlink, and the map's own paths would then be a third
        // spelling of the same file
        let root = directory
            .path()
            .canonicalize()
            .expect("the directory was just made");
        let source = root.join("demo.by");
        let generated = root.join("demo.py");
        std::fs::write(&source, by).expect("the `.by` is written");
        std::fs::write(&generated, py).expect("the generated python is written");
        let build = Self {
            directory,
            source,
            generated,
            marks: root.join("answer.txt"),
        };
        build.write_map(lines);
        build
    }

    /// write `_by_sourcemap.py` with digests that are true of what is on disk
    fn write_map(&self, lines: &[Option<u32>]) {
        let table: Vec<String> = lines
            .iter()
            .map(|line| line.map_or_else(|| "None".to_owned(), |line| line.to_string()))
            .collect();
        std::fs::write(
            self.root().join(MAP_FILENAME),
            format!(
                "# generated by `by run` — maps transpiled python frames to .by source\n\
                 SOURCEMAP = {{\n    \"{generated}\": (\"{source}\", [{table}]),\n}}\n\n\
                 DIGESTS = {{\n    \"{generated}\": {{\"by\": \"{by}\", \"py\": \"{py}\"}},\n}}\n",
                generated = self.generated.display(),
                source = self.source.display(),
                table = table.join(", "),
                by = digest(&self.source),
                py = digest(&self.generated),
            ),
        )
        .expect("the map is written");
    }

    /// a shim that runs the generated python, the way `by run` does
    ///
    /// `_by_runner.py` upstream, and what it buys here is a stack with frames
    /// **under** the build in it: the shim itself and the import machinery it
    /// goes through. none of that is basedpython and none of it may be dressed
    /// as it
    fn runner(&self) -> PathBuf {
        let path = self.root().join("runner.py");
        std::fs::write(
            &path,
            format!(
                "import runpy\n\
                 runpy.run_path({:?}, run_name=\"__main__\")\n",
                self.generated.display().to_string()
            ),
        )
        .expect("the runner is written");
        path
    }

    fn root(&self) -> PathBuf {
        self.directory
            .path()
            .canonicalize()
            .expect("the directory is there for the life of the build")
    }

    /// what the program wrote, which is empty until it has run past the stop
    fn answer(&self) -> String {
        std::fs::read_to_string(&self.marks).unwrap_or_default()
    }
}

/// the sha-256 of a file, as `_by_sourcemap.py` writes one
fn digest(path: &Path) -> String {
    use sha2::Digest as _;
    use std::fmt::Write as _;

    let bytes = std::fs::read(path).expect("a file this fixture just wrote");
    let mut out = String::from("sha256:");
    for byte in sha2::Sha256::digest(&bytes) {
        write!(out, "{byte:02x}").expect("a `String` grows to fit");
    }
    out
}

/// launch the generated python out of its build directory
fn launch(build: &Build) -> Debuggee {
    launch_program(build, &build.generated)
}

/// launch one file of the build directory, so the map beside it is found
fn launch_program(build: &Build, program: &Path) -> Debuggee {
    let arguments = [build.marks.clone().into_os_string()];
    match bpd_engine::launch(
        bpd_test::agent::matching_interpreter(),
        &bpd_engine::Program::Script(program.to_path_buf()),
        &arguments,
    ) {
        Ok(Launched::Stopped(debuggee)) => debuggee,
        Ok(Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

/// set one breakpoint in the `.by` and say what became of it
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

#[test]
fn a_by_breakpoint_binds_to_the_generated_line_and_says_both_locations() {
    let build = Build::new();
    let mut debuggee = launch(&build);
    let asked = bpd_test::debuggee::line_of(SOURCE, "print(answer)");

    let resolved = set(&mut debuggee, asked, &build.source);

    let Binding::BoundInSource {
        line,
        generated,
        sites,
        ..
    } = &resolved.binding
    else {
        panic!("a `.by` breakpoint binds as a mapped one, and this is {resolved:?}")
    };
    assert_eq!(
        *line, asked,
        "the answer is about the line that was asked for"
    );
    assert_eq!(generated.file, build.generated);
    assert_eq!(
        generated.line,
        bpd_test::debuggee::line_of(GENERATED, "write_text"),
        "the generated line is where the interpreter will really stop"
    );
    assert!(
        !sites.is_empty(),
        "a bound breakpoint has a code object behind it"
    );
}

#[test]
fn the_stop_names_the_by_line_the_breakpoint_was_set_on() {
    let build = Build::new();
    let mut debuggee = launch(&build);
    let asked = bpd_test::debuggee::line_of(SOURCE, "print(answer)");
    let resolved = set(&mut debuggee, asked, &build.source);
    assert!(
        matches!(resolved.binding, Binding::BoundInSource { .. }),
        "{resolved:?}"
    );

    let reason = run_to_stop(&mut debuggee);
    let StopReason::Breakpoint { file, line, .. } = &reason else {
        panic!("the program was supposed to stop on the breakpoint, and it {reason:?}")
    };
    // the whole of the other half. the user asked about a line of `demo.by` and
    // the stop is about that line — the generated python it really ran is one
    // field of the frame away, and both of them are true
    assert_eq!(Path::new(file), build.source);
    assert_eq!(*line, asked);
    assert_eq!(
        build.answer(),
        "",
        "the program is held before the line ran, so it has written nothing"
    );
}

#[test]
fn the_stack_reports_the_by_and_carries_where_the_interpreter_really_is() {
    // the consistency rule, which is the one that matters most. a stop that
    // said `demo.by:11` beside a frame that said `demo.py:14` would be the
    // debugger contradicting itself about one place
    let build = Build::new();
    let mut debuggee = launch(&build);
    let asked = bpd_test::debuggee::line_of(SOURCE, "print(answer)");
    set(&mut debuggee, asked, &build.source);
    let reason = run_to_stop(&mut debuggee);
    let StopReason::Breakpoint { file, line, .. } = &reason else {
        panic!("it was supposed to stop on the breakpoint: {reason:?}")
    };

    let stack = debuggee.the_stack(None).expect("the stack was walked");

    let top = stack.frames.first().expect("a held thread has a stack");
    assert_eq!(&top.file, file, "the stop and the frame are one location");
    assert_eq!(top.line, *line);
    let Some(Mapping::FromSource { generated }) = &top.mapping else {
        panic!("a frame of the build says it is mapped, and this is {top:?}")
    };
    assert_eq!(generated.file, build.generated);
    assert_eq!(
        generated.line,
        bpd_test::debuggee::line_of(GENERATED, "write_text"),
        "the frame carries where the interpreter is, which is what a user who \
         does not believe the debugger needs"
    );

    // and every frame of the build, not only the one that stopped. the module
    // frame under it is `demo.by` too
    for frame in &stack.frames {
        assert_eq!(
            Path::new(&frame.file),
            build.source,
            "every frame of this stack is the build's: {:?}",
            stack.frames
        );
    }
}

#[test]
fn a_generated_line_no_by_line_is_behind_is_reported_as_python_and_says_why() {
    // a prelude line has no `.by` behind it — the map says so itself — and
    // reporting one as a `.by` line would be the debugger writing a line the
    // user never did. so the location stays the generated one, and the frame
    // carries the map's own reason rather than leaving a temporary path in
    // front of a user with nothing to explain it
    let build = Build::new();
    let mut debuggee = launch(&build);
    let prelude = bpd_test::debuggee::line_of(GENERATED, "from pathlib import Path");

    let resolved = set(&mut debuggee, prelude, &build.generated);
    assert!(
        matches!(resolved.binding, Binding::Bound { .. }),
        "a breakpoint in the generated python binds as ordinary python: {resolved:?}"
    );
    let reason = run_to_stop(&mut debuggee);

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
fn frames_below_the_build_are_not_dressed_as_basedpython() {
    // `by run` starts a runner shim and the interpreter's own machinery is
    // under that. none of it is `.by`, the map says nothing about any of it,
    // and a debugger that mapped a frame it had no entry for would be
    // inventing a source file
    let build = Build::new();
    let runner = build.runner();
    let mut debuggee = launch_program(&build, &runner);
    let asked = bpd_test::debuggee::line_of(SOURCE, "print(answer)");

    // the module is not compiled at launch — the runner has not run it yet —
    // so the breakpoint binds when the file is loaded, which is what the run
    // reports
    set(&mut debuggee, asked, &build.source);
    let reason = run_to_stop(&mut debuggee);
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "it was supposed to reach the breakpoint through the runner: {reason:?}"
    );

    let stack = debuggee.the_stack(None).expect("the stack was walked");
    let mapped: Vec<&str> = stack
        .frames
        .iter()
        .filter(|frame| frame.mapping.is_some())
        .map(|frame| frame.file.as_str())
        .collect();
    let untouched: Vec<&str> = stack
        .frames
        .iter()
        .filter(|frame| frame.mapping.is_none())
        .map(|frame| frame.file.as_str())
        .collect();

    assert!(
        !mapped.is_empty() && !untouched.is_empty(),
        "this stack was supposed to have both kinds of frame in it: {:?}",
        stack.frames
    );
    for file in mapped {
        assert_eq!(Path::new(file), build.source, "a mapped frame is the `.by`");
    }
    for file in &untouched {
        assert_ne!(
            Path::new(file),
            build.source,
            "a frame the map says nothing about is reported as itself"
        );
    }
    assert!(
        untouched.iter().any(|file| file.ends_with("runner.py")),
        "the shim that started the build is one of them: {untouched:?}"
    );
}

#[test]
fn a_by_line_the_transpiler_generated_nothing_for_moves_to_the_next_one_it_did() {
    // the blank line between `return total` and `def main`. a breakpoint on one
    // moves forward exactly as it does in ordinary python, and the answer says
    // where it went — read back out of the map rather than assumed
    let build = Build::new();
    let mut debuggee = launch(&build);
    let comment = bpd_test::debuggee::line_of(SOURCE, "# a comment");

    let resolved = set(&mut debuggee, comment, &build.source);

    let Binding::BoundInSource { line, .. } = &resolved.binding else {
        panic!("it was supposed to move forward and bind, and it is {resolved:?}")
    };
    assert_eq!(
        *line,
        bpd_test::debuggee::line_of(SOURCE, "def main"),
        "line {comment} generated nothing, so the breakpoint moved to the next \
         `.by` line that did — and the answer says which one that is"
    );
}

#[test]
fn a_by_line_past_everything_the_transpiler_generated_is_unbound_with_the_reason() {
    let build = Build::new();
    let mut debuggee = launch(&build);
    let past = u32::try_from(SOURCE.lines().count() + 10).expect("a fixture is not that long");

    let resolved = set(&mut debuggee, past, &build.source);

    let reason = refused(&resolved);
    assert!(
        matches!(reason, Unbound::Unmappable { .. }),
        "expected the map to refuse it, got {reason:?}"
    );
    let said = reason.to_string();
    assert!(
        said.contains("demo.by"),
        "the refusal names the file: {said}"
    );
    assert!(
        said.contains("generated nothing"),
        "the refusal says what the map found: {said}"
    );
}

#[test]
fn a_by_line_whose_generated_line_the_transpiler_invented_is_refused_not_attributed() {
    // the whole rule, driven end to end. the table here says the `.by`'s last
    // line became a generated line the transpiler also emitted prelude on — so
    // binding walks forward off the end of what has a source, and the nearest
    // `.by` line is exactly the answer a fallback would give and exactly the
    // answer that would be a lie
    let mut lines = line_table();
    let last = lines.len() - 1;
    lines[last] = None;
    let build = Build::with(&lines);
    let mut debuggee = launch(&build);
    // a blank `.by` line, which maps to a blank generated line. the interpreter
    // cannot stop on one, so binding walks forward to the next line it can —
    // and with the table above that is a line the transpiler invented
    let asked = bpd_test::debuggee::line_of(SOURCE, "print(answer)") + 1;

    let resolved = set(&mut debuggee, asked, &build.source);

    let reason = refused(&resolved);
    assert!(
        matches!(reason, Unbound::Unmappable { .. }),
        "expected a refusal rather than a `.by` line nobody wrote, got {reason:?}"
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
    assert!(reason.to_string().contains("bpd by"), "{reason}");
}

#[test]
fn a_by_edited_since_the_transpile_refuses_the_launch_rather_than_the_line() {
    // the milestone. the map still describes a pair of files, one of them is no
    // longer that file, and every line it would report is wrong with total
    // confidence — so nothing is reported and the program is not debugged at all
    let build = Build::new();
    std::fs::write(
        &build.source,
        format!("# a line the build never saw\n{SOURCE}"),
    )
    .expect("the user edits their `.by` and forgets to transpile");

    // moved rather than cloned: the build directory outlives it either way,
    // and nothing asks about the generated path again
    let program = bpd_engine::Program::Script(build.generated);
    let error = match bpd_engine::launch(bpd_test::agent::matching_interpreter(), &program, &[]) {
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
    // breakpoint in a file the interpreter really has, and it binds the way any
    // python one does — an ordinary `Bound`, not a mapped one
    let build = Build::new();
    let mut debuggee = launch(&build);
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
    // beside a stack that does not would be two answers about one place
    let build = Build::raising();
    let mut debuggee = launch(&build);
    debuggee
        .set_exception_breakpoints(false, true)
        .expect("the exception breakpoints were set");

    let reason = run_to_stop(&mut debuggee);

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
fn the_source_around_a_by_frame_is_the_by_and_is_checked_against_the_map() {
    // showing the generated python beside a `.by` location would be the
    // contradiction this milestone is about. the `.by` is read on the
    // debuggee's own filesystem and checked against the digest the transpiler
    // wrote, which is the only thing that can say it is still that file
    let build = Build::new();
    let mut debuggee = launch(&build);
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
fn a_by_edited_after_the_launch_refuses_the_source_rather_than_showing_it() {
    // `bpd` checked this file at launch and the user is asking about now. an
    // editor that saved the `.by` in between leaves a file whose lines are
    // wrong with total confidence, which is the failure a source map exists to
    // prevent
    let build = Build::new();
    let mut debuggee = launch(&build);
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
fn a_frame_reported_as_by_is_moved_by_naming_a_by_line() {
    // the inbound half. once a frame says `demo.by:11`, the line a client names
    // against it is a line of `demo.by` — a debugger that answered in one
    // file's lines and took orders in another's would be two debuggers
    let build = Build::new();
    let mut debuggee = launch(&build);
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
fn a_by_line_nothing_was_generated_for_refuses_the_move_rather_than_guessing() {
    let build = Build::new();
    let mut debuggee = launch(&build);
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
    let past = u32::try_from(SOURCE.lines().count() + 10).expect("a fixture is not that long");

    let error = debuggee
        .set_next_statement(frame, past)
        .expect_err("nothing was generated for that line of the `.by`");

    let said = format!("{error}");
    assert!(said.contains("demo.by"), "{said}");
    assert!(said.contains("generated nothing"), "{said}");
}

#[test]
fn where_a_thread_is_is_sampled_in_by_terms() {
    // a `Where` has no frame id on it and nowhere to carry the generated
    // location, and it is still the same location a frame of the same code
    // reports. one of them naming the other file would be the contradiction
    let build = Build::new();
    let mut debuggee = launch(&build);
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

/// run to the next stop, and hand back the reason it stopped for
fn run_to_stop(debuggee: &mut Debuggee) -> StopReason {
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop.reason,
        Running::Exited {
            status, rebound, ..
        } => panic!(
            "it exited with {status} instead of stopping. what it said about \
             the breakpoints was {rebound:?}"
        ),
        other => panic!("the program neither stopped nor exited: {other:?}"),
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
