//! why the program's ui recomposed, against a real interpreter
//!
//! the claim under test is that the agent reads the trace ring of
//! basedpython-ui **exactly** — every slot of every record by the layout the
//! runtime documents, refusing by name where a slot is not what the layout
//! says — through the runtime's own storage and without running a line of the
//! program. none of that can be checked against a mocked object: what is
//! under test is `PyObject_GenericGetDict` finding what a real instance holds,
//! an exact-type check telling a `bool` from an `int`, and an audit hook seeing
//! what `sys.audit` really hands it
//!
//! the ring is written by a **stand-in** runtime the fixture ships beside
//! itself — `bpd_test::basedpython_ui::STAND_IN_RUNTIME` — which keeps the
//! documented layouts and nothing else, so each test writes the records it
//! means to read back. the one test over the real framework, named by
//! `BPD_TEST_BASEDPYTHON_UI`, is the one that proves the stand-in imitates the
//! layouts the framework really writes; it is `#[ignore]`d so a checkout with
//! no framework beside it reports it as ignored rather than failing, and it is
//! run with `--ignored` and the variable set

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use bpd_core::source_map::MAP_FILENAME;
use bpd_core::{
    Addressed, Blindspot, Cause, Content, Detail, Disposed, Evaluated, FrameId, Kept, Key,
    Location, LogRecord, Origin, Recomposed, Refusal, Reporting, Request, Response, Running,
    SessionId, SourceBreakpoint, Spawn, Stop, StopReason, TraceRecord,
};
use bpd_engine::{Debuggee, Launched};
use bpd_test::basedpython_ui::{RECOMPOSING, stand_in};
use bpd_test::debuggee::{Fixture, line_of};
use bpd_test::reporting::{Recompositions, Unreported};

fn launch(fixture: &Fixture, args: &[OsString]) -> Debuggee {
    match bpd_engine::launch(
        bpd_test::agent::matching_interpreter(),
        &bpd_engine::Program::Script(fixture.path()),
        args,
    ) {
        Ok(Launched::Stopped(debuggee)) => debuggee,
        Ok(Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

/// arm one breakpoint on the line holding `needle` and run to it
///
/// with a sink that panics on any report: what runs to the line announces
/// records, and nothing is watching
fn run_to(debuggee: &mut Debuggee, fixture: &Fixture, needle: &str) -> Stop {
    run_to_watched(debuggee, fixture, needle, &mut Unreported)
}

/// the same, with the records the run forwards going to `watched`
fn run_to_watched(
    debuggee: &mut Debuggee,
    fixture: &Fixture,
    needle: &str,
    watched: &mut dyn Reporting,
) -> Stop {
    let line = line_of(fixture.source(), needle);
    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), line)])
        .expect("the breakpoint was answered");
    match debuggee.run(watched).expect("the debuggee was resumed") {
        Running::Stopped { stop, .. } => stop,
        other => panic!("the breakpoint on {needle:?} never stopped it: {other:?}"),
    }
}

/// a location nothing mapped, as the agent reports one
fn at(fixture: &Fixture, line: u32) -> Location {
    Location {
        file: fixture.path().display().to_string(),
        line,
        generated: None,
        reason: None,
    }
}

/// what the program wrote beside itself
fn marks(fixture: &Fixture, name: &str) -> String {
    std::fs::read_to_string(fixture.directory().join(name))
        .unwrap_or_else(|error| panic!("the program wrote {name}: {error}"))
        .trim()
        .to_string()
}

/// one of every record kind and every cause kind, with the value slots holding
/// every shape a reader has to render
///
/// `Todo` is the trap, the way `Loud` is in the facts test: every one of its
/// methods appends to `TOUCHED`, so a reader that rendered a value by calling
/// anything of the object's would say so in the program's own state
const EVERY_KIND: &str = r#"import pathlib
import threading
from basedpython_ui import runtime as rt

HERE = pathlib.Path(__file__).resolve()
TOUCHED = []


class Todo:
    def __repr__(self):
        TOUCHED.append("repr")
        return "Todo()"

    def __str__(self):
        TOUCHED.append("str")
        return "todo"

    def __len__(self):
        TOUCHED.append("len")
        return 3

    def __bool__(self):
        TOUCHED.append("bool")
        return True


def Counter(step=1):
    return step


def Row():
    return None


thread = threading.get_ident()
runtime = rt.Runtime()
runtime.frames = 3
file = str(HERE)
counter = Counter.__code__
row = Row.__code__

written = ("state", 4401, "state", "set", None, 0, 2, file, 30, "count", file, 40, thread, False, 1)
appended = ("state", 4402, "list", "append", 2, None, Todo(), None, None, None, file, 41, thread, True, 0)
cleared = ("state", 4403, "dict", "clear", "k", {"a": 1, "b": 2}, 0, file, 31, "table", file, 42, thread, False, 2)
derived = ("derived", 77, file, 32, "total", 1.5, "x" * 100, True, written)
unchanged = ("derived", 78, None, None, None, (), True, False, cleared)

runtime.record((2, 3, written))
runtime.record((2, 3, appended))
runtime.record((2, 3, derived))
runtime.record((1, 3, 0, None, "root", counter.co_filename, counter.co_firstlineno, None, None, None, "first", (("created",),), (), (), 100))
runtime.record((1, 3, 5, 0, "Counter", counter.co_filename, counter.co_firstlineno, file, 60, 2, "self", (written, derived, ("args", "step", 1, 2, True), ("args", "config", Todo(), [1, 2, 3], False)), (7, 8), ((9, "Row", 2), (10, "Row", None)), 12345))
runtime.record((1, 3, 6, 5, "Row", row.co_filename, row.co_firstlineno, file, 61, "second", "parent", (("inline",), ("uncommitted",), ("invalidated",), ("recovery", "boom"), ("dirty", (("created",), unchanged))), (), (), 200))
runtime.record((3, 3, 2, 4, 15000, 300000))
runtime.record((4, 4, 6, "Row", "boom", True))
runtime.record((5, 4, 5, "Counter", "state set"))
(HERE.parent / "thread.txt").write_text(f"{thread}\n")
done = 1  # the breakpoint
"#;

/// the written cause of [`EVERY_KIND`], as the agent reports it
fn written(fixture: &Fixture, thread: u64) -> Cause {
    Cause::State {
        cell: 4401,
        kind: "state".to_string(),
        op: "set".to_string(),
        at: None,
        old: "0".to_string(),
        new: "2".to_string(),
        declared: Some(at(fixture, 30)),
        declared_name: Some("count".to_string()),
        written: at(fixture, 40),
        thread,
        posted: false,
        readers: 1,
    }
}

/// the derived cause of [`EVERY_KIND`], whose new value is cut at 64
fn derived(fixture: &Fixture, thread: u64) -> Cause {
    let mut cut = "'".to_string();
    cut.extend(std::iter::repeat_n('x', 63));
    cut.push('…');
    Cause::Derived {
        derived: 77,
        declared: Some(at(fixture, 32)),
        declared_name: Some("total".to_string()),
        old: "1.5".to_string(),
        new: cut,
        changed: true,
        because: Box::new(written(fixture, thread)),
    }
}

/// the run of `Counter` in [`EVERY_KIND`], with a cause of every value shape
fn counter_ran(fixture: &Fixture, thread: u64) -> TraceRecord {
    TraceRecord::Run {
        runtime: 0,
        frame: 3,
        scope: 5,
        parent: Some(0),
        name: "Counter".to_string(),
        defined: at(fixture, line_of(EVERY_KIND, "def Counter")),
        called: Some(at(fixture, 60)),
        key: Some(Key::Int(2)),
        origin: Origin::Itself,
        causes: vec![
            written(fixture, thread),
            derived(fixture, thread),
            Cause::Args {
                parameter: "step".to_string(),
                old: "1".to_string(),
                new: "2".to_string(),
                compared: true,
            },
            Cause::Args {
                parameter: "config".to_string(),
                old: "a Todo".to_string(),
                new: "list[3]".to_string(),
                compared: false,
            },
        ],
        skipped: vec![7, 8],
        disposed: vec![
            Disposed {
                scope: 9,
                name: "Row".to_string(),
                key: Some(Key::Int(2)),
            },
            Disposed {
                scope: 10,
                name: "Row".to_string(),
                key: None,
            },
        ],
        elapsed_ns: 12_345,
    }
}

/// the run of `Row` in [`EVERY_KIND`], with every structural cause
fn row_ran(fixture: &Fixture, thread: u64) -> TraceRecord {
    let cleared = Cause::State {
        cell: 4403,
        kind: "dict".to_string(),
        op: "clear".to_string(),
        at: Some(Key::Text("k".to_string())),
        old: "dict{2}".to_string(),
        new: "0".to_string(),
        declared: Some(at(fixture, 31)),
        declared_name: Some("table".to_string()),
        written: at(fixture, 42),
        thread,
        posted: false,
        readers: 2,
    };
    TraceRecord::Run {
        runtime: 0,
        frame: 3,
        scope: 6,
        parent: Some(5),
        name: "Row".to_string(),
        defined: at(fixture, line_of(EVERY_KIND, "def Row")),
        called: Some(at(fixture, 61)),
        key: Some(Key::Text("second".to_string())),
        origin: Origin::Parent,
        causes: vec![
            Cause::Inline,
            Cause::Uncommitted,
            Cause::Invalidated,
            Cause::Recovery {
                error: "boom".to_string(),
            },
            Cause::Dirty {
                causes: vec![
                    Cause::Created,
                    Cause::Derived {
                        derived: 78,
                        declared: None,
                        declared_name: None,
                        old: "tuple[0]".to_string(),
                        new: "True".to_string(),
                        changed: false,
                        because: Box::new(cleared),
                    },
                ],
            },
        ],
        skipped: Vec::new(),
        disposed: Vec::new(),
        elapsed_ns: 200,
    }
}

/// every record [`EVERY_KIND`] writes, as the agent has to report them
fn every_record(fixture: &Fixture, thread: u64) -> Vec<TraceRecord> {
    vec![
        TraceRecord::Write {
            runtime: 0,
            frame: 3,
            cause: written(fixture, thread),
        },
        TraceRecord::Write {
            runtime: 0,
            frame: 3,
            cause: Cause::State {
                cell: 4402,
                kind: "list".to_string(),
                op: "append".to_string(),
                at: Some(Key::Int(2)),
                old: "None".to_string(),
                // rendered by type and never by `repr`, which is the program's
                new: "a Todo".to_string(),
                declared: None,
                declared_name: None,
                written: at(fixture, 41),
                thread,
                posted: true,
                readers: 0,
            },
        },
        TraceRecord::Write {
            runtime: 0,
            frame: 3,
            cause: derived(fixture, thread),
        },
        TraceRecord::Run {
            runtime: 0,
            frame: 3,
            scope: 0,
            parent: None,
            name: "root".to_string(),
            defined: at(fixture, line_of(EVERY_KIND, "def Counter")),
            called: None,
            key: None,
            origin: Origin::First,
            causes: vec![Cause::Created],
            skipped: Vec::new(),
            disposed: Vec::new(),
            elapsed_ns: 100,
        },
        counter_ran(fixture, thread),
        row_ran(fixture, thread),
        TraceRecord::Frame {
            runtime: 0,
            frame: 3,
            runs: 2,
            skips: 4,
            compose_ns: 15_000,
            commit_ns: 300_000,
        },
        TraceRecord::Error {
            runtime: 0,
            frame: 4,
            scope: 6,
            name: "Row".to_string(),
            error: "boom".to_string(),
            kept_previous: true,
        },
        TraceRecord::Refused {
            runtime: 0,
            frame: 4,
            scope: 5,
            name: "Counter".to_string(),
            what: "state set".to_string(),
        },
    ]
}

#[test]
fn the_ring_is_read_slot_by_slot_with_every_kind_of_record_and_cause() {
    let fixture = Fixture::new("app", EVERY_KIND);
    stand_in(&fixture);
    let mut debuggee = launch(&fixture, &[]);
    // the program announces every record it appends and nothing is watching,
    // so `Unreported` — which panics on any report — is what proves the stream
    // stays off until it is asked for
    let stop = run_to(&mut debuggee, &fixture, "done = 1");

    let thread: u64 = marks(&fixture, "thread.txt")
        .parse()
        .expect("the program wrote its thread ident");
    let ring = debuggee.recompositions().expect("the ring was answered");

    assert_eq!(ring.format, 1);
    assert_eq!(ring.runtimes, 1);
    assert!(ring.tracing);
    assert_eq!(
        ring.records.dropped, 0,
        "nine records fit a ring of four thousand: {:#?}",
        ring.records
    );
    assert_eq!(ring.records.kept, every_record(&fixture, thread));

    // and the program's own state says the reader called nothing of `Todo`'s.
    // asserting on the answer alone would prove nothing: a value rendered
    // through `repr` and one rendered by type read differently, but a `__len__`
    // called on the way would not show at all
    let touched = debuggee
        .evaluate(
            FrameId {
                stop: stop.stop,
                depth: 0,
            },
            "TOUCHED",
            Detail::default(),
        )
        .expect("the evaluation was answered");
    match touched {
        Evaluated::Value { value } => match value.content {
            Content::Sequence { length, .. } => assert_eq!(
                length, 0,
                "reading the ring ran the program's own code: {value:?}"
            ),
            other => panic!("`TOUCHED` is a list, and this is {other:?}"),
        },
        Evaluated::Raised { error } => panic!("`TOUCHED` raised {error:?}"),
    }
}

/// a program whose records point into itself, so a map can cover them
///
/// `import threading` is the line the map marks as having no `.by` line behind
/// it, and the write site of the cause is put exactly there: a location the
/// map covers and cannot resolve is the case a reader is most tempted to
/// paper over
const MAPPED: &str = r#"import pathlib
import threading
from basedpython_ui import runtime as rt

HERE = str(pathlib.Path(__file__).resolve())


def Counter():
    return None


runtime = rt.Runtime()
runtime.frames = 1
written = ("state", 1, "state", "set", None, 0, 1, HERE, 8, "count", HERE, 2, threading.get_ident(), False, 1)
runtime.record((1, 1, 5, 0, "Counter", Counter.__code__.co_filename, Counter.__code__.co_firstlineno, HERE, 12, None, "self", (written,), (), (), 7))
done = 1  # the breakpoint
"#;

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

/// a path as a source map carries one
fn in_a_map(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', r"\\")
        .replace('"', "\\\"")
}

/// write a `.by` and a map beside the fixture, every line its own except the
/// one holding `unmapped`, which the map marks as prelude
fn write_map(fixture: &Fixture, unmapped: &str) -> std::path::PathBuf {
    let source = fixture.beside(
        "app.by",
        "# the source the map says the program came from\n",
    );
    let generated = fixture.path();
    let gone = line_of(fixture.source(), unmapped);
    let table: Vec<String> = (0..fixture.source().lines().count())
        .map(|index| {
            let line = u32::try_from(index).expect("a fixture is short") + 1;
            if line == gone {
                "None".to_string()
            } else {
                index.to_string()
            }
        })
        .collect();
    std::fs::write(
        fixture.directory().join(MAP_FILENAME),
        format!(
            "# generated by `by run` — maps transpiled python frames to .by source\n\
             SOURCEMAP = {{\n    \"{generated}\": (\"{source}\", [{table}]),\n}}\n\n\
             DIGESTS = {{\n    \"{generated}\": {{\"by\": \"{by}\", \"py\": \"{py}\"}},\n}}\n",
            generated = in_a_map(&generated),
            source = in_a_map(&source),
            table = table.join(", "),
            by = digest(&source),
            py = digest(&generated),
        ),
    )
    .expect("the map is written");
    source
}

#[test]
fn a_build_with_a_source_map_reports_every_location_of_the_ring_as_by_lines() {
    // the runtime records the generated python it runs, and the agent maps
    // every location through the same table a stack frame goes through. the
    // three shapes a location can take are all in one record: a line the map
    // resolves, a line the map covers and marks as prelude, and — for the
    // stand-in's own file — nothing mapped at all
    let fixture = Fixture::new("app", MAPPED);
    stand_in(&fixture);
    let source = write_map(&fixture, "import threading");
    let mut debuggee = launch(&fixture, &[]);
    run_to(&mut debuggee, &fixture, "done = 1");

    let ring = debuggee.recompositions().expect("the ring was answered");
    let [
        TraceRecord::Run {
            defined,
            called,
            causes,
            ..
        },
    ] = ring.records.kept.as_slice()
    else {
        panic!("the program wrote one run record: {:#?}", ring.records)
    };

    let by = source.display().to_string();
    let py = fixture.path();
    let defined_at = line_of(MAPPED, "def Counter");
    assert_eq!(
        (defined.file.as_str(), defined.line),
        (by.as_str(), defined_at),
        "the composable is defined at a `.by` line: {defined:?}"
    );
    assert_eq!(
        defined
            .generated
            .as_ref()
            .map(|generated| (generated.file.clone(), generated.line)),
        Some((py.clone(), defined_at)),
        "and the generated location is beside it: {defined:?}"
    );
    assert!(defined.reason.is_none(), "{defined:?}");

    let called = called.as_ref().expect("the run was called from somewhere");
    assert_eq!((called.file.as_str(), called.line), (by.as_str(), 12));

    let [
        Cause::State {
            declared, written, ..
        },
    ] = causes.as_slice()
    else {
        panic!("the run has one state cause: {causes:#?}")
    };
    let declared = declared
        .as_ref()
        .expect("the cell was declared in composition");
    assert_eq!((declared.file.as_str(), declared.line), (by.as_str(), 8));

    // the write site is the line the map marks as prelude: the generated
    // location stands, is carried under `generated` as well, and the map's own
    // reason is beside it. a `.by` line here would be one the user never wrote
    assert_eq!(
        (written.file.as_str(), written.line),
        (py.display().to_string().as_str(), 2),
        "{written:?}"
    );
    assert_eq!(
        written
            .generated
            .as_ref()
            .map(|generated| (generated.file.clone(), generated.line)),
        Some((py, 2))
    );
    assert!(
        written
            .reason
            .as_ref()
            .is_some_and(|reason| reason.to_string().contains("app.py")),
        "a line the map covers and cannot resolve says why, naming the file: \
         {written:?}"
    );
}

/// two runtimes, one whose own ring overflowed and one with more records than
/// an answer carries
const OVERFLOWING: &str = r"from basedpython_ui import runtime as rt

first = rt.Runtime(limit=100)
for i in range(150):
    first.record((3, i, 1, 0, 0, 0))
second = rt.Runtime(limit=10000)
for i in range(5000):
    second.record((3, i, 1, 0, 0, 0))
done = 1  # the breakpoint
";

#[test]
fn an_answer_holds_the_newest_records_and_counts_every_one_it_left_out() {
    // the two bounds that bite, both counted into one number: the runtime's
    // ring halved itself, and the answer keeps four thousand and ninety-six
    // of what is left. an answer that reported either count alone would be an
    // answer whose oldest entry reads as where the trace began
    let fixture = Fixture::new("app", OVERFLOWING);
    stand_in(&fixture);
    let mut debuggee = launch(&fixture, &[]);
    run_to(&mut debuggee, &fixture, "done = 1");

    let ring = debuggee.recompositions().expect("the ring was answered");
    assert_eq!(ring.runtimes, 2);
    assert_eq!(ring.records.kept.len(), 4096, "the answer's own bound");
    // the first ring halves when the append that reaches its limit lands, as
    // the real `Trace` does: at the hundredth record it dropped fifty, and at
    // the hundred and fiftieth it dropped fifty more, so it held 50 after
    // dropping 100. the second held 5000; 5050 concatenated, the oldest 954
    // left out of the answer
    assert_eq!(ring.records.dropped, 100 + 954);
    assert!(
        ring.records.kept.iter().all(|record| record.runtime() == 1),
        "the newest records are all the second runtime's"
    );
    assert_eq!(
        ring.records.kept.first(),
        Some(&TraceRecord::Frame {
            runtime: 1,
            frame: 904,
            runs: 1,
            skips: 0,
            compose_ns: 0,
            commit_ns: 0,
        }),
        "the oldest record kept is the one after the 954 that went — the \
         first runtime's 50 and the second's first 904"
    );
    assert!(
        matches!(
            ring.records.kept.last(),
            Some(TraceRecord::Frame { frame: 4999, .. })
        ),
        "the newest record is the last one written"
    );
}

/// the refusal a request was answered with, or a panic naming what came back
fn refused(outcome: bpd_engine::Result<bpd_core::Recompositions>) -> Refusal {
    match outcome {
        Err(bpd_engine::Error::Session(bpd_core::Error::Refused { reason })) => reason,
        Ok(ring) => panic!("the request was answered rather than refused: {ring:#?}"),
        Err(other) => panic!("the request failed for a reason that is bpd's own: {other}"),
    }
}

#[test]
fn a_program_without_the_ui_runtime_is_refused_by_name() {
    let fixture = Fixture::new("plain", "done = 1  # the breakpoint\n");
    let mut debuggee = launch(&fixture, &[]);
    run_to(&mut debuggee, &fixture, "done = 1");

    let reason = refused(debuggee.recompositions());
    assert!(matches!(reason, Refusal::NoUiRuntime), "{reason}");
    assert!(
        reason
            .to_string()
            .contains("has not imported basedpython_ui.runtime"),
        "the refusal names what to do: {reason}"
    );

    // the watch is not refused: watching is an interest in records to come,
    // and a program that never imports the runtime announces none. what is
    // set is what the agent says is set, both ways
    assert!(
        debuggee
            .watch_recompositions(true)
            .expect("the watch was answered"),
        "a watch on a program without the runtime is accepted"
    );
    assert!(
        !debuggee
            .watch_recompositions(false)
            .expect("the watch was answered")
    );
}

/// a program that puts something other than a module under the runtime's name
const IMPOSTOR: &str = r#"import sys
from basedpython_ui import runtime as rt

runtime = rt.Runtime()
sys.modules["basedpython_ui.runtime"] = object()
done = 1  # the breakpoint
"#;

#[test]
fn a_non_module_under_the_runtimes_name_is_refused_naming_what_was_there() {
    // not `no_ui_runtime`: the program imported the runtime, and a refusal
    // telling it to import something it imported would be a sentence that is
    // not true. what stands under the name is named instead
    let fixture = Fixture::new("app", IMPOSTOR);
    stand_in(&fixture);
    let mut debuggee = launch(&fixture, &[]);
    run_to(&mut debuggee, &fixture, "done = 1");

    let reason = refused(debuggee.recompositions());
    let Refusal::UiTraceUnreadable { what } = &reason else {
        panic!("an entry that is not a module has to be refused as unreadable: {reason}")
    };
    assert!(
        what.contains("`sys.modules[\"basedpython_ui.runtime\"]` is a object")
            && what.contains("not a module"),
        "{what}"
    );
}

/// the runtime declaring a format this bpd does not read, and then none at all
const FORMATS: &str = r"from basedpython_ui import runtime as rt

runtime = rt.Runtime()
rt.TRACE_FORMAT = 2
first = 1  # the first breakpoint
del rt.TRACE_FORMAT
second = 2  # the second breakpoint
";

#[test]
fn a_trace_format_this_bpd_does_not_read_is_refused_naming_both_formats() {
    let fixture = Fixture::new("app", FORMATS);
    stand_in(&fixture);
    let mut debuggee = launch(&fixture, &[]);

    run_to(&mut debuggee, &fixture, "first = 1");
    let reason = refused(debuggee.recompositions());
    assert!(
        matches!(
            reason,
            Refusal::UiTraceFormat {
                found: Some(2),
                wanted: 1
            }
        ),
        "{reason}"
    );
    assert!(
        reason.to_string().contains("writes trace format 2")
            && reason.to_string().contains("this bpd reads 1"),
        "{reason}"
    );

    // the module without the name at all is a different thing to act on, and
    // the refusal says which
    run_to(&mut debuggee, &fixture, "second = 2");
    let reason = refused(debuggee.recompositions());
    assert!(
        matches!(
            reason,
            Refusal::UiTraceFormat {
                found: None,
                wanted: 1
            }
        ),
        "{reason}"
    );
    assert!(reason.to_string().contains("no TRACE_FORMAT"), "{reason}");
}

/// one runtime with tracing off, and then a second with it on
const TRACING_OFF: &str = r"from basedpython_ui import runtime as rt

quiet = rt.Runtime(trace=False)
first = 1  # the first breakpoint
loud = rt.Runtime()
loud.record((3, 1, 1, 0, 10, 20))
second = 2  # the second breakpoint
";

#[test]
fn a_runtime_with_tracing_off_is_refused_rather_than_answered_with_an_empty_ring() {
    let fixture = Fixture::new("app", TRACING_OFF);
    stand_in(&fixture);
    let mut debuggee = launch(&fixture, &[]);

    run_to(&mut debuggee, &fixture, "first = 1");
    let reason = refused(debuggee.recompositions());
    assert!(matches!(reason, Refusal::UiTracingOff), "{reason}");
    assert!(reason.to_string().contains("trace=True"), "{reason}");

    // refused only when **every** runtime is off. a second one that traces is
    // answered, with the quiet one counted among the runtimes and nothing of
    // it among the records
    run_to(&mut debuggee, &fixture, "second = 2");
    let ring = debuggee.recompositions().expect("one runtime traces");
    assert_eq!(ring.runtimes, 2);
    assert!(ring.tracing);
    assert_eq!(
        ring.records.kept,
        vec![TraceRecord::Frame {
            runtime: 1,
            frame: 1,
            runs: 1,
            skips: 0,
            compose_ns: 10,
            commit_ns: 20,
        }]
    );
}

#[test]
fn a_program_that_imported_the_runtime_and_made_no_runtime_is_answered_with_nothing_to_trace() {
    // not a refusal: there is no runtime to have tracing off. `tracing` false
    // is what says so, and it is the only case it is false in an answer
    let fixture = Fixture::new(
        "app",
        "from basedpython_ui import runtime as rt\n\ndone = 1  # the breakpoint\n",
    );
    stand_in(&fixture);
    let mut debuggee = launch(&fixture, &[]);
    run_to(&mut debuggee, &fixture, "done = 1");

    let ring = debuggee.recompositions().expect("the ring was answered");
    assert_eq!(ring.runtimes, 0);
    assert!(!ring.tracing);
    assert_eq!(ring.records, Kept::whole(Vec::new()));
}

/// a run record whose `frame` slot holds a str, a cause nobody has heard of,
/// a `bool` in an int slot, and the `key` cause an earlier format had
const WRONG_SLOTS: &str = r#"from basedpython_ui import runtime as rt

runtime = rt.Runtime()
runtime.record((1, "three", 5, 0, "Counter", __file__, 1, None, None, None, "self", (("created",),), (), (), 1))
first = 1  # the first breakpoint
runtime.trace.records.clear()
runtime.record((2, 3, ("weather", "rainy")))
second = 2  # the second breakpoint
runtime.trace.records.clear()
runtime.record((1, True, 5, 0, "Counter", __file__, 1, None, None, None, "self", (("created",),), (), (), 1))
third = 3  # the third breakpoint
runtime.trace.records.clear()
runtime.record((1, 3, 5, 0, "Counter", __file__, 1, None, None, None, "first", (("key", 2),), (), (), 1))
fourth = 4  # the fourth breakpoint
"#;

#[test]
fn a_record_with_a_slot_the_layout_does_not_allow_is_refused_naming_the_slot() {
    let fixture = Fixture::new("app", WRONG_SLOTS);
    stand_in(&fixture);
    let mut debuggee = launch(&fixture, &[]);

    run_to(&mut debuggee, &fixture, "first = 1");
    let reason = refused(debuggee.recompositions());
    let Refusal::UiTraceUnreadable { what } = &reason else {
        panic!("a str in an int slot has to be refused as unreadable: {reason}")
    };
    assert_eq!(
        what,
        "slot 1 (`frame`) of a `run` record is a str and the layout says a \
         non-negative int",
        "the refusal names the record kind, the slot and what was there"
    );

    // a cause kind the layout does not have costs the answer, by name
    run_to(&mut debuggee, &fixture, "second = 2");
    let reason = refused(debuggee.recompositions());
    let Refusal::UiTraceUnreadable { what } = &reason else {
        panic!("{reason}")
    };
    assert!(
        what.contains("\"weather\"") && what.contains("slot 2 (`cause`) of a `write` record"),
        "{what}"
    );

    // and a `bool` in an int slot, which is the case an exact-type check
    // exists for: `True` is an `int` to `isinstance`, and it is not a frame
    run_to(&mut debuggee, &fixture, "third = 3");
    let reason = refused(debuggee.recompositions());
    let Refusal::UiTraceUnreadable { what } = &reason else {
        panic!("{reason}")
    };
    assert!(
        what.starts_with("slot 1 (`frame`) of a `run` record is a bool"),
        "{what}"
    );

    // there is no `key` cause: a key change is `created` on the new scope and
    // the old key under the parent's `disposed`. a runtime writing one is
    // writing a layout this reader does not have, and it is refused as one
    run_to(&mut debuggee, &fixture, "fourth = 4");
    let reason = refused(debuggee.recompositions());
    let Refusal::UiTraceUnreadable { what } = &reason else {
        panic!("{reason}")
    };
    assert!(
        what.contains("\"key\"") && what.contains("not a kind the layout has"),
        "{what}"
    );
}

/// an error record whose text holds a lone surrogate
///
/// `str(OSError)` over a `surrogateescape`d file name is one; the record is
/// the runtime's own layout with text no UTF-8 can spell
const SURROGATE: &str = r#"from basedpython_ui import runtime as rt

runtime = rt.Runtime()
runtime.record((4, 1, 0, "root", "bad \udcff name", True))
done = 1  # the breakpoint
"#;

#[test]
fn a_text_slot_no_utf8_can_spell_is_read_with_the_character_spelled_as_repr_would() {
    // the slot is a str, so a refusal saying it is not one would be false —
    // and the alternative to a false refusal is the answer a value slot's
    // `repr` already gets: the character with no spelling written as its
    // escape, which names it rather than losing it
    let fixture = Fixture::new("app", SURROGATE);
    stand_in(&fixture);
    let mut debuggee = launch(&fixture, &[]);
    run_to(&mut debuggee, &fixture, "done = 1");

    let ring = debuggee.recompositions().expect("the ring was answered");
    assert_eq!(
        ring.records.kept,
        vec![TraceRecord::Error {
            runtime: 0,
            frame: 1,
            scope: 0,
            name: "root".to_string(),
            error: "bad \\udcff name".to_string(),
            kept_previous: true,
        }]
    );
}

/// a runtime that announces a record before the first of three breakpoints,
/// and one between each
const ANNOUNCING: &str = r"from basedpython_ui import runtime as rt

runtime = rt.Runtime()
runtime.record((3, 0, 1, 0, 10, 20))
first = 1  # the first breakpoint
runtime.record((3, 1, 1, 0, 10, 20))
second = 2  # the second breakpoint
runtime.record((3, 2, 1, 0, 10, 20))
third = 3  # the third breakpoint
";

/// a frame record of [`ANNOUNCING`], as the agent forwards it
fn announced(frame: u64) -> TraceRecord {
    TraceRecord::Frame {
        runtime: 0,
        frame,
        runs: 1,
        skips: 0,
        compose_ns: 10,
        commit_ns: 20,
    }
}

#[test]
fn watching_forwards_every_record_the_runtime_announces_and_nothing_once_it_is_off() {
    let fixture = Fixture::new("app", ANNOUNCING);
    stand_in(&fixture);
    let mut debuggee = launch(&fixture, &[]);

    // the watch goes on at the entry stop, before the program has imported
    // the runtime. it is accepted: watching is an interest in records to
    // come, and the record the program announces the moment it has a runtime
    // is the first one forwarded
    assert!(
        debuggee
            .watch_recompositions(true)
            .expect("the watch was answered"),
        "what is set is what the agent says is set"
    );
    let mut watched = Recompositions::default();
    run_to_watched(&mut debuggee, &fixture, "first = 1", &mut watched);
    assert_eq!(watched.records, vec![announced(0)]);
    assert_eq!(watched.dropped, 0, "the stream was whole");

    // the record the program announces between the first and second stops
    // arrives through the sink, read off the audit event on the thread that
    // appended it — and it is the record, slot for slot
    let mut watched = Recompositions::default();
    run_to_watched(&mut debuggee, &fixture, "second = 2", &mut watched);
    assert_eq!(watched.records, vec![announced(1)]);
    assert_eq!(watched.dropped, 0);

    // off again, and the next record reaches nobody: `Unreported` panics on
    // any report, so the run to the third stop is the proof
    assert!(
        !debuggee
            .watch_recompositions(false)
            .expect("the watch was answered")
    );
    run_to(&mut debuggee, &fixture, "third = 3");

    // the ring holds all three, whichever were forwarded
    let ring = debuggee.recompositions().expect("the ring was answered");
    assert_eq!(ring.records.kept.len(), 3);
}

/// a program that raises the trace event by hand: with a record that is in
/// the ring but is not its last entry, and with a tuple that is in no ring
const HAND_RAISED: &str = r#"import sys
from basedpython_ui import runtime as rt

runtime = rt.Runtime()
runtime.record((3, 1, 1, 0, 10, 20))
runtime.record((3, 2, 1, 0, 10, 20))
armed = 1  # the breakpoint
sys.audit("basedpython_ui.trace", runtime.trace.records[0])
sys.audit("basedpython_ui.trace", (3, 99, 1, 0, 1, 1))
runtime.record((3, 3, 1, 0, 10, 20))
done = 2  # the second breakpoint
"#;

#[test]
fn a_record_the_program_announces_by_hand_is_not_forwarded() {
    // the stream carries what the runtime writes, and the runtime announces a
    // record once, right after appending it — so the announced tuple is the
    // last entry of its ring. a stale one re-raised from the front of the
    // ring would show a client the ui going back in time, and a tuple in no
    // ring is not the runtime's at all
    let fixture = Fixture::new("app", HAND_RAISED);
    stand_in(&fixture);
    let mut debuggee = launch(&fixture, &[]);
    run_to(&mut debuggee, &fixture, "armed = 1");
    assert!(
        debuggee
            .watch_recompositions(true)
            .expect("the watch was answered")
    );

    let mut watched = Recompositions::default();
    run_to_watched(&mut debuggee, &fixture, "done = 2", &mut watched);
    assert_eq!(
        watched.records,
        vec![announced(3)],
        "only the record the runtime appended was forwarded"
    );
    assert_eq!(watched.dropped, 0);
}

/// a program that wraps `builtins.__import__` with a spy while it records
///
/// the spy goes on after the breakpoint line and off again before anything
/// else runs, so what it saw is what happened during the five records alone:
/// a stop's own work — the stack, the breakpoint set — imports nothing this
/// test is about, and it is kept out of the window
const SPYING: &str = r#"import builtins
import pathlib
from basedpython_ui import runtime as rt

HERE = pathlib.Path(__file__).resolve().parent
IMPORTED = []
real_import = builtins.__import__


def spy(name, *args, **kwargs):
    IMPORTED.append(name)
    return real_import(name, *args, **kwargs)


runtime = rt.Runtime()
armed = 1  # the breakpoint
builtins.__import__ = spy
for i in range(5):
    runtime.record((3, i, 1, 0, 10, 20))
builtins.__import__ = real_import
(HERE / "imported.txt").write_text(repr(IMPORTED))
done = 2  # the second breakpoint
"#;

#[test]
fn forwarding_a_record_runs_no_import_machinery_in_the_program() {
    // `sys.modules` is read through `PySys_GetObject`, a dictionary lookup on
    // the `sys` module. `python.import("sys")` would be `PyImport_Import`,
    // which calls `builtins.__import__` — a function the program can replace
    // and would then see the debugger calling once per record, on the ui
    // thread, from inside `sys.audit`
    let fixture = Fixture::new("app", SPYING);
    stand_in(&fixture);
    let mut debuggee = launch(&fixture, &[]);
    run_to(&mut debuggee, &fixture, "armed = 1");
    assert!(
        debuggee
            .watch_recompositions(true)
            .expect("the watch was answered")
    );

    let mut watched = Recompositions::default();
    run_to_watched(&mut debuggee, &fixture, "done = 2", &mut watched);
    assert_eq!(watched.records.len(), 5, "{:?}", watched.records);
    assert_eq!(
        marks(&fixture, "imported.txt"),
        "[]",
        "the program's own `__import__` was called while its records were forwarded"
    );
}

/// a program that floods the stream: forty thousand records in one loop,
/// timed by its own clock
///
/// each record carries half a kilobyte of text so the flood is far larger than
/// any socket buffer between the agent and the engine, on any platform this
/// runs on: what does not fit has to be queued, and what the queue cannot
/// hold has to be dropped and counted
const FLOODING: &str = r#"import pathlib
import time
from basedpython_ui import runtime as rt

HERE = pathlib.Path(__file__).resolve().parent
runtime = rt.Runtime()
armed = 1  # the breakpoint
started = time.monotonic()
for i in range(40000):
    runtime.record((4, i, 0, "root", "x" * 500, True))
finished = time.monotonic()
(HERE / "loop.txt").write_text(f"{finished - started}\n")
"#;

/// how many records [`FLOODING`] writes
const FLOOD: u64 = 40_000;

#[test]
fn a_watched_program_nobody_reads_runs_at_its_own_pace_and_the_gap_is_counted() {
    // the shape that froze a ui for eight seconds before the stream had a
    // queue: a client turns the watch on, resumes, and thinks for a while. the
    // engine reads the connection only inside a request, so the socket fills
    // and every write after that waits for the client's next call — on the
    // ui thread, when the hook wrote the socket itself. now the hook hands the
    // record to a queue, and the program's own clock says its loop ran at its
    // own pace while the client was idle
    const IDLE: Duration = Duration::from_secs(3);

    let fixture = Fixture::new("app", FLOODING);
    stand_in(&fixture);
    let mut debuggee = launch(&fixture, &[]);
    run_to(&mut debuggee, &fixture, "armed = 1");
    assert!(
        debuggee
            .watch_recompositions(true)
            .expect("the watch was answered")
    );

    // resumed and then not read: nothing looks at the connection until the
    // wait below
    debuggee.resume_all().expect("the resume was answered");
    std::thread::sleep(IDLE);

    let mut watched = Recompositions::default();
    match debuggee.wait(&mut watched).expect("the wait was answered") {
        Running::Exited { status, .. } => assert!(status.success(), "{status}"),
        other => panic!("the program did not run to its end: {other:?}"),
    }

    let loop_took: f64 = marks(&fixture, "loop.txt")
        .parse()
        .expect("the program wrote how long its loop took");
    assert!(
        loop_took < IDLE.as_secs_f64(),
        "the program's loop took {loop_took} s while the client was idle for \
         {IDLE:?}, so the stream held the ui thread until the client read it"
    );

    // the gap is counted, exactly: every record was either forwarded or
    // counted as dropped, and a stream that lost one without saying so would
    // read as a ui that wrote one fewer
    assert!(
        watched.dropped > 0,
        "{FLOOD} records of half a kilobyte fit nowhere while nothing read \
         them, and the agent says it dropped none"
    );
    let forwarded = u64::try_from(watched.records.len()).expect("a count of records fits");
    assert_eq!(
        forwarded + watched.dropped,
        FLOOD,
        "{forwarded} forwarded and {} counted as dropped do not account for \
         every record the program wrote",
        watched.dropped
    );

    // and what was forwarded is in the order the runtime wrote it, with the
    // oldest dropped ahead of the newest — the newest is the one a client
    // watching the ui is waiting for
    let frames: Vec<u64> = watched
        .records
        .iter()
        .map(|record| match record {
            TraceRecord::Error { frame, .. } => *frame,
            other => panic!("the program wrote error records, and this is {other:?}"),
        })
        .collect();
    assert!(
        frames.windows(2).all(|pair| pair[0] < pair[1]),
        "the stream is out of order, or holds a record twice"
    );
    assert_eq!(
        frames.last().copied(),
        Some(FLOOD - 1),
        "the last record the program wrote is the last one forwarded"
    );
}

/// a sink for a program that forks under `debug_children` while watched
///
/// the children it starts, the sessions that join, and the records that
/// arrive — on whichever session's wait they arrive during — with the count
/// of what the agent dropped ahead of each
#[derive(Debug, Default)]
struct Forking {
    started: Vec<Spawn>,
    unseen: Vec<Blindspot>,
    joined: Vec<SessionId>,
    records: Vec<TraceRecord>,
    dropped: u64,
}

impl Reporting for Forking {
    fn logged(&mut self, record: LogRecord) {
        panic!("no logpoint was set, and the agent sent {record:?}")
    }

    fn pausing(&mut self, running: Vec<u64>) {
        panic!("no pause was armed, and the agent acknowledged one naming {running:?}")
    }

    fn spawned(&mut self, child: Spawn) {
        self.started.push(child);
    }

    fn blind_to(&mut self, blindspot: Blindspot) {
        self.unseen.push(blindspot);
    }

    fn attached(&mut self, session: SessionId) {
        self.joined.push(session);
    }

    fn recomposed(&mut self, recomposed: Recomposed) {
        self.dropped += recomposed.dropped_before;
        self.records.push(recomposed.record);
    }
}

/// a program that forks while watched, whose child records before and after a
/// breakpoint of its own and stops once more before it leaves, and whose
/// parent records after the child is gone
///
/// the child leaves through `os._exit`, which writes nothing out — so the stop
/// after its second record is what carries that record to the engine, the way
/// every stop carries what was written before it. `signal.alarm` is the
/// watchdog every fixture whose child could be left held arms — see
/// `crates/bpd_engine/tests/forks.rs`
#[cfg(unix)]
const FORKING: &str = r"import os
import signal
from basedpython_ui import runtime as rt

runtime = rt.Runtime()
armed = 1  # the breakpoint
pid = os.fork()
if pid == 0:
    signal.alarm(120)
    runtime.record((3, 7, 1, 0, 10, 20))
    held = 2  # the child's breakpoint
    runtime.record((3, 9, 1, 0, 10, 20))
    leaving = 3  # the child's last breakpoint
    os._exit(7)
_, status = os.waitpid(pid, 0)
assert os.waitstatus_to_exitcode(status) == 7, status
runtime.record((3, 8, 1, 0, 10, 20))
";

/// ask one session for something, and require it to be answered
#[cfg(unix)]
fn ask(debuggee: &mut Debuggee, at: SessionId, request: Request, seen: &mut Forking) -> Response {
    match debuggee.dispatch(Addressed::to(at, request), seen) {
        Ok(answer) => answer,
        Err(error) => panic!("{at} was not answered: {error}"),
    }
}

/// resume one session and wait for what its program does next
#[cfg(unix)]
fn run_in(debuggee: &mut Debuggee, at: SessionId, seen: &mut Forking) -> Running {
    match ask(
        debuggee,
        at,
        Request::Run {
            deadline: Some(Duration::from_secs(20)),
        },
        seen,
    ) {
        Response::Ran(ran) => ran,
        other => panic!("a run was answered with {other:?}"),
    }
}

#[test]
#[cfg(unix)]
#[expect(
    clippy::too_many_lines,
    reason = "it is one program's whole life with a watched fork in it — the \
              watch, the fork, the child held and watched on its own, and both \
              processes ending. splitting it would launch the same program \
              several times and assert on a different half of one sequence \
              each time"
)]
fn a_forked_child_starts_with_the_watch_off_and_is_watched_on_its_own() {
    // the watch is a flag in the agent and the queue is memory, and a fork
    // inherits both. a child that kept them would forward its own ui's
    // records onto the session that asked about its parent's — so the child
    // starts with the watch off, and a watch of its own puts a writer of its
    // own on it. the parent's writer is stood down for the fork and put back
    // afterwards, which the parent's last record is the proof of
    let fixture = Fixture::new("forker", FORKING);
    stand_in(&fixture);
    let mut debuggee = launch(&fixture, &[]);
    let parent = match debuggee.sessions().as_slice() {
        [only] => *only,
        open => panic!("this debuggee holds {open:?} and the test launched one program"),
    };
    assert!(
        debuggee
            .debug_children(true)
            .expect("the debuggee took the setting")
    );
    // all three breakpoints at once: the child inherits the table, and
    // `held` and `leaving` are lines only the child runs
    debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), line_of(FORKING, "armed = 1")),
            SourceBreakpoint::at(2, fixture.path(), line_of(FORKING, "held = 2")),
            SourceBreakpoint::at(3, fixture.path(), line_of(FORKING, "leaving = 3")),
        ])
        .expect("the breakpoints were answered");
    let mut seen = Forking::default();
    match run_in(&mut debuggee, parent, &mut seen) {
        Running::Stopped { .. } => {}
        other => panic!("the first breakpoint never stopped it: {other:?}"),
    }
    assert!(
        debuggee
            .watch_recompositions(true)
            .expect("the watch was answered")
    );

    // the parent forks and waits on its child, so it does not end until the
    // child is let go; the child's session arrives while the parent is
    // waited on
    match ask(
        &mut debuggee,
        parent,
        Request::Run {
            deadline: Some(Duration::from_secs(2)),
        },
        &mut seen,
    ) {
        Response::Ran(Running::StillRunning { .. }) => {}
        other => panic!("the parent waits on its child and does not end: {other:?}"),
    }
    for _ in 0..15 {
        if debuggee.sessions().len() >= 2 {
            break;
        }
        match ask(
            &mut debuggee,
            parent,
            Request::Wait {
                deadline: Some(Duration::from_secs(2)),
            },
            &mut seen,
        ) {
            Response::Ran(Running::StillRunning { .. }) => {}
            other => panic!("the parent was supposed to be busy with its child: {other:?}"),
        }
    }
    let child = match debuggee
        .sessions()
        .into_iter()
        .filter(|session| *session != parent)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [only] => *only,
        open => panic!("one session was supposed to have joined, and {open:?} did"),
    };
    assert_eq!(seen.joined, vec![child]);
    assert_eq!(seen.started.len(), 1, "{:?}", seen.started);

    // the child is held where it forked
    match ask(
        &mut debuggee,
        child,
        Request::Wait {
            deadline: Some(Duration::from_secs(20)),
        },
        &mut seen,
    ) {
        Response::Ran(Running::Stopped { stop, .. }) => {
            assert!(
                matches!(stop.reason, StopReason::Forked { .. }),
                "{:?}",
                stop.reason
            );
        }
        other => panic!("the child was supposed to be held at the fork: {other:?}"),
    }

    // let go, it records once and stops at its own breakpoint. nothing
    // arrives: the watch its parent turned on is not its own
    match run_in(&mut debuggee, child, &mut seen) {
        Running::Stopped { stop, .. } => assert_eq!(stop.session, child),
        other => panic!("the child's breakpoint never stopped it: {other:?}"),
    }
    assert!(
        seen.records.is_empty(),
        "the child forwarded records on a watch it inherited: {:?}",
        seen.records
    );

    // watched on its own, it forwards its next record on its own session —
    // ahead of the stop that follows the record, as every stop is preceded by
    // what was written before it
    match ask(
        &mut debuggee,
        child,
        Request::WatchRecompositions { on: true },
        &mut seen,
    ) {
        Response::WatchingRecompositions { on: true } => {}
        other => panic!("the child's watch was answered with {other:?}"),
    }
    match run_in(&mut debuggee, child, &mut seen) {
        Running::Stopped { stop, .. } => assert_eq!(stop.session, child),
        other => panic!("the child's last breakpoint never stopped it: {other:?}"),
    }
    assert_eq!(seen.records, vec![announced(9)]);
    match run_in(&mut debuggee, child, &mut seen) {
        Running::Ended { .. } => {}
        other => panic!("the child did not end: {other:?}"),
    }

    // and the parent, whose writer was stood down for the fork, forwards the
    // record it writes once its child is gone
    match ask(
        &mut debuggee,
        parent,
        Request::Wait {
            deadline: Some(Duration::from_secs(20)),
        },
        &mut seen,
    ) {
        Response::Ran(Running::Exited { status, .. }) => assert!(status.success(), "{status}"),
        other => panic!("the parent did not end: {other:?}"),
    }
    assert_eq!(seen.records, vec![announced(9), announced(8)]);
    assert_eq!(seen.dropped, 0, "both streams were whole");
}

/// a real basedpython-ui program: a counter, a click, a second frame
///
/// the build directory is the program's first argument, so the framework the
/// test named is the one that runs. `Final` is what the framework's own tests
/// annotate a cell with, and the handler takes the argument a button hands one
const COUNTING: &str = r#"import sys

sys.path.insert(0, sys.argv[1])

from typing import Final

from basedpython_ui import Button, Text
from basedpython_ui.runtime import Runtime, composable, state
from tests.test_runtime import FakeCore, click


@composable
def Counter():
    count: Final = state(0)
    Text(f"count = {count.value}")

    def bump(it=None):
        count.value = count.value + 1

    Button("+", on_click=bump)


core = FakeCore()
runtime = Runtime(core, trace=True)
runtime.set_root(Counter)
runtime.frame()
click(runtime, core, "+")
runtime.frame()
done = 1  # the breakpoint
"#;

#[test]
#[ignore = "needs BPD_TEST_BASEDPYTHON_UI naming a by build of basedpython-ui"]
fn the_real_runtime_writes_a_run_whose_cause_is_the_state_write_a_handler_made() {
    // the one test over the framework itself, and the one that proves the
    // stand-in above imitates the layouts the framework really writes. it is
    // ignored rather than run on a plain checkout, and when it is run with
    // `--ignored` and no build is named it fails rather than skips — see
    // `bpd_test::basedpython_ui::build`
    let build = bpd_test::basedpython_ui::build();
    let fixture = Fixture::new("counting", COUNTING);
    let mut debuggee = launch(&fixture, &[build.into_os_string()]);
    run_to(&mut debuggee, &fixture, "done = 1");

    let ring = debuggee.recompositions().expect("the ring was answered");
    assert_eq!(ring.format, 1);
    assert_eq!(ring.runtimes, 1);
    assert!(ring.tracing);

    // the second frame ran `Counter` again because the handler wrote `count`,
    // and the record says so: the cell by the name it was bound to, the write
    // by the line the handler made it on
    let reran = ring
        .records
        .kept
        .iter()
        .find_map(|record| match record {
            TraceRecord::Run {
                name,
                origin: Origin::Itself,
                causes,
                ..
            } if name == "Counter" => Some(causes),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "no run of `Counter` off the dirty heap in the ring: {:#?}",
                ring.records
            )
        });
    let write = reran
        .iter()
        .find_map(|cause| match cause {
            Cause::State {
                declared_name: Some(name),
                op,
                old,
                new,
                written,
                ..
            } if name == "count" => Some((op, old, new, written)),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!("`Counter` ran again for a reason other than `count`: {reran:#?}")
        });
    let (op, old, new, written) = write;
    assert_eq!(op, "set");
    assert_eq!((old.as_str(), new.as_str()), ("0", "1"));
    assert_eq!(
        (written.file.as_str(), written.line),
        (
            fixture.path().display().to_string().as_str(),
            line_of(COUNTING, "count.value = count.value + 1")
        ),
        "the write site is the handler's own line: {written:?}"
    );
}

/// the program every front end's acceptance drives, read here first
#[test]
fn the_acceptance_program_writes_the_ring_the_front_ends_read() {
    // the two front ends' acceptance tests assert on this program's ring
    // through a real `bpd dap` and `bpd mcp`. what they assert is pinned here
    // against the engine, so a change to the program that moved a record
    // fails where the record is rather than behind a protocol
    let fixture = Fixture::new("recomposing", RECOMPOSING);
    stand_in(&fixture);
    let mut debuggee = launch(&fixture, &[]);
    run_to(&mut debuggee, &fixture, "done = 1");

    let ring = debuggee.recompositions().expect("the ring was answered");
    assert_eq!(ring.records.kept.len(), 3);
    assert!(matches!(
        &ring.records.kept[1],
        TraceRecord::Run { name, defined, .. }
            if name == "Counter" && defined.line == line_of(RECOMPOSING, "def Counter")
    ));
}
