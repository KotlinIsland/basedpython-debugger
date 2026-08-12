//! a condition decides a stop, and it decides it the way python would
//!
//! the load bearing test here is the differential one. a fast path that
//! disagrees with the interpreter is not a trade-off, it is a debugger
//! reporting a value the program does not have — so every condition in the
//! corpus is asked twice, once in a shape the native comparison can read and
//! once wrapped in parentheses, which changes nothing about what the expression
//! means and everything about whether the shape can be read. the two answers
//! have to be the same on every pass over the line
//!
//! nothing here takes the agent's word for a stop either: the fixtures write
//! which pass they are on to a file *above* the breakpoint line and their
//! result *below* it

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::path::Path;

use bpd_core::python::Capabilities;
use bpd_core::{
    Binding, Evaluation, HitCondition, Part, Resolved, Running, SourceBreakpoint, StopReason,
    Unbound,
};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// how many requests the engine has sent the only session's agent
fn requests_sent(debuggee: &Debuggee) -> u64 {
    debuggee
        .requests_sent()
        .expect("this debuggee holds the one session the test launched")
}

/// a loop whose body has every kind of local a condition might read
///
/// `VISITED` is written on the line above the breakpoint, so a stop can say
/// which pass it is, and `DONE` on the last line of the program, so a stop can
/// prove the program has not run past it
const CORPUS_PROGRAM: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent
VISITED = HERE / "visited"
GLOBAL = 2


def visit(value, text, flag):
    VISITED.write_text(str(value))
    marker = value * 10
    return marker


for item in range(1, 6):
    visit(item, "abc" if item % 2 else "xyz", item == 3)
(HERE / "done").write_text("done")
"#;

/// every condition asked of the corpus, and how it has to be answered
///
/// the second column is the assertion that the native path is really being
/// taken where it is claimed and really declining where it must. a fast path
/// nobody can see is a fast path nobody can check
const CORPUS: &[(&str, Evaluation)] = &[
    ("value == 3", Evaluation::Comparison),
    ("value != 3", Evaluation::Comparison),
    ("value < 3", Evaluation::Comparison),
    ("value <= 3", Evaluation::Comparison),
    ("value > 3", Evaluation::Comparison),
    ("value >= 3", Evaluation::Comparison),
    ("value==3", Evaluation::Comparison),
    ("text == 'abc'", Evaluation::Comparison),
    ("text != \"abc\"", Evaluation::Comparison),
    ("flag is True", Evaluation::Comparison),
    ("flag is not False", Evaluation::Comparison),
    ("marker is None", Evaluation::Comparison),
    // the shape reads natively and the name is not a local of that frame, so
    // the interpreter resolves it — the same answer by a different route
    ("GLOBAL == 2", Evaluation::Comparison),
    ("GLOBAL > 5", Evaluation::Comparison),
    // every one of these is a shape the native path declines
    ("(value == 3)", Evaluation::Expression),
    ("value == 3.5", Evaluation::Expression),
    ("value > 1.5", Evaluation::Expression),
    ("value == 1_0", Evaluation::Expression),
    ("value == 100000000000000000000000", Evaluation::Expression),
    ("value.bit_length() == 2", Evaluation::Expression),
    ("value % 2 == 0", Evaluation::Expression),
    ("text.startswith('a')", Evaluation::Expression),
    ("value in (2, 4)", Evaluation::Expression),
    ("1 < value < 4", Evaluation::Expression),
    ("not flag", Evaluation::Expression),
    ("3 == value", Evaluation::Expression),
    // identity is only knowable for the three singletons. against anything
    // else the interpreter compares with whatever object it put in
    // `co_consts`, which is not the one the native path would build, so the two
    // could answer differently — and a fast path that can disagree is a bug
    ("value is 3", Evaluation::Expression),
];

/// the interpreter the built agent matches, or a failure saying how to get one
fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// launch a fixture and require that it stopped before running anything
fn launch(fixture: &Fixture) -> Debuggee {
    match bpd_engine::launch(
        interpreter(),
        &bpd_engine::Program::Script(fixture.path()),
        &[],
    ) {
        Ok(Launched::Stopped(debuggee)) => debuggee,
        Ok(Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

/// how a breakpoint's condition will be answered, or the reason it did not bind
fn evaluation(resolved: &Resolved) -> Evaluation {
    match &resolved.binding {
        Binding::Bound { evaluation, .. } | Binding::BoundInTemplate { evaluation, .. } => {
            *evaluation
        }
        Binding::Unbound { reason } => {
            panic!("breakpoint {} did not bind: {reason}", resolved.id)
        }
    }
}

/// why a breakpoint did not bind, or where it did
fn unbound(resolved: &Resolved) -> &Unbound {
    match &resolved.binding {
        Binding::Unbound { reason } => reason,
        Binding::Bound { line, .. } | Binding::BoundInTemplate { line, .. } => panic!(
            "breakpoint {} bound to line {line}, and was not supposed to",
            resolved.id
        ),
    }
}

/// the breakpoints that stopped, or a failure naming what happened instead
fn stopped_for(reason: &StopReason) -> &[u32] {
    match reason {
        StopReason::Breakpoint { breakpoints, .. } => breakpoints,
        other => panic!("expected a breakpoint stop, got {other:?}"),
    }
}

/// run to the end, handing every stop to `at_stop`
///
/// returns nothing about the stops on purpose: a test that wants to know
/// something about one says so in the closure, at the moment the program is
/// held, which is the only moment its side effects can be checked
fn run_to_exit(debuggee: &mut Debuggee, mut at_stop: impl FnMut(&StopReason)) {
    loop {
        match debuggee
            .run(&mut bpd_test::reporting::Unreported)
            .expect("the debuggee was resumed")
        {
            Running::Stopped { stop, .. } => at_stop(&stop.reason),
            Running::Exited { status, .. } => {
                assert!(status.success(), "the program exited with {status}");
                return;
            }
            Running::StillRunning { waited, .. } => unreachable!(
                "this wait carries no deadline and was answered after {waited:?} \
                 with the program still running"
            ),
            // bpd launched this program and holds its child, so it is bpd that
            // reads the exit
            Running::Ended { .. } => unreachable!(
                "the program bpd launched ended without an exit status, and bpd \
                 holds its child"
            ),
            Running::Finishing { threads, .. } => {
                panic!("nothing was held, and the debuggee ended holding {threads:?}")
            }
        }
    }
}

/// ask for every corpus condition twice, bare and parenthesised
fn corpus_breakpoints(file: &Path, line: u32) -> Vec<SourceBreakpoint> {
    let mut requests = Vec::with_capacity(CORPUS.len() * 2);
    for (index, (condition, _)) in CORPUS.iter().enumerate() {
        let native = native_id(index);
        requests.push(SourceBreakpoint::at(native, file, line).when(*condition));
        requests.push(SourceBreakpoint::at(native + 1, file, line).when(format!("({condition})")));
    }
    requests
}

/// the id of the bare form of the `index`th corpus condition
fn native_id(index: usize) -> u32 {
    u32::try_from(index * 2 + 1).expect("the corpus is not two billion conditions")
}

#[test]
fn the_native_comparison_and_the_interpreter_agree_on_every_condition_in_the_corpus() {
    let fixture = Fixture::new("corpus", CORPUS_PROGRAM);
    let visited = fixture.directory().join("visited");
    let done = fixture.directory().join("done");
    let at_line = line_of(CORPUS_PROGRAM, "return marker");

    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(corpus_breakpoints(&fixture.path(), at_line))
        .expect("the breakpoint request was answered");

    for (index, (condition, expected)) in CORPUS.iter().enumerate() {
        assert_eq!(
            evaluation(&resolved[index * 2]),
            *expected,
            "for the condition `{condition}`"
        );
        assert_eq!(
            evaluation(&resolved[index * 2 + 1]),
            Evaluation::Expression,
            "parentheses are a shape the native path cannot read, so `({condition})` \
             has to be the interpreter's"
        );
    }

    let mut passes: Vec<(String, BTreeSet<u32>)> = Vec::new();
    run_to_exit(&mut debuggee, |reason| {
        let fired: BTreeSet<u32> = stopped_for(reason).iter().copied().collect();
        let pass = std::fs::read_to_string(&visited).expect("the line above the stop has run");
        assert!(
            !done.exists(),
            "the program had already finished when the engine was told it was stopped"
        );
        passes.push((pass, fired));
    });

    assert_eq!(
        passes.len(),
        5,
        "`value != 3` is true on four passes and `value == 3` on the fifth, so \
         every pass over the line stops — got {passes:?}"
    );

    for (pass, fired) in &passes {
        for (index, (condition, _)) in CORPUS.iter().enumerate() {
            let native = native_id(index);
            assert_eq!(
                fired.contains(&native),
                fired.contains(&(native + 1)),
                "`{condition}` and `({condition})` are the same expression, and on \
                 the pass where value was {pass} they did not agree"
            );
        }
    }

    // an anchor, so a run in which nothing at all fired could not pass the
    // agreement check by agreeing about nothing
    let third = passes
        .iter()
        .find(|(pass, _)| pass == "3")
        .expect("the loop reaches 3");
    assert!(third.1.contains(&native_id(0)), "`value == 3` on pass 3");
    let first = passes
        .iter()
        .find(|(pass, _)| pass == "1")
        .expect("the loop starts at 1");
    assert!(!first.1.contains(&native_id(0)), "`value == 3` on pass 1");
}

/// a loop with an observable marker above the breakpoint and below it
const COUNTED: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent
VISITED = HERE / "visited"


def visit(value):
    VISITED.write_text(str(value))
    marker = value
    return marker


for item in range(1, 11):
    visit(item)
(HERE / "done").write_text("done")
"#;

/// every pass the program made that the engine was told about
fn passes_that_stopped(debuggee: &mut Debuggee, visited: &Path) -> Vec<(String, Vec<u32>)> {
    let mut passes = Vec::new();
    run_to_exit(debuggee, |reason| {
        let pass = std::fs::read_to_string(visited).expect("the line above the stop has run");
        passes.push((pass, stopped_for(reason).to_vec()));
    });
    passes
}

#[test]
fn a_condition_that_is_false_does_not_stop_and_a_true_one_does() {
    let fixture = Fixture::new("counted", COUNTED);
    let visited = fixture.directory().join("visited");
    let at_line = line_of(COUNTED, "return marker");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), at_line).when("value == 4"),
        ])
        .expect("the breakpoint request was answered");

    let passes = passes_that_stopped(&mut debuggee, &visited);
    assert_eq!(
        passes,
        [("4".to_string(), vec![1])],
        "the line runs ten times and the condition is true once"
    );
}

#[test]
fn a_hit_count_stops_on_the_right_pass_and_not_before() {
    let fixture = Fixture::new("counted", COUNTED);
    let visited = fixture.directory().join("visited");
    let at_line = line_of(COUNTED, "return marker");
    let count = |value: u32| NonZeroU32::new(value).expect("the counts here are not zero");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), at_line)
                .counting(HitCondition::Exactly { count: count(3) }),
            SourceBreakpoint::at(2, fixture.path(), at_line)
                .counting(HitCondition::AtLeast { count: count(8) }),
            SourceBreakpoint::at(3, fixture.path(), at_line)
                .counting(HitCondition::Every { count: count(4) }),
        ])
        .expect("the breakpoint request was answered");

    assert_eq!(
        passes_that_stopped(&mut debuggee, &visited),
        [
            ("3".to_string(), vec![1]),
            ("4".to_string(), vec![3]),
            ("8".to_string(), vec![2, 3]),
            ("9".to_string(), vec![2]),
            ("10".to_string(), vec![2]),
        ]
    );
}

#[test]
fn a_hit_count_counts_only_the_hits_the_condition_let_through() {
    let fixture = Fixture::new("counted", COUNTED);
    let visited = fixture.directory().join("visited");
    let at_line = line_of(COUNTED, "return marker");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), at_line)
                .when("value % 2 == 0")
                .counting(HitCondition::Exactly {
                    count: NonZeroU32::new(2).expect("2 is not zero"),
                }),
        ])
        .expect("the breakpoint request was answered");

    // the qualifying hits are 2, 4, 6, 8 and 10. counting every pass instead
    // would make the second hit the one where value is 2
    assert_eq!(
        passes_that_stopped(&mut debuggee, &visited),
        [("4".to_string(), vec![1])]
    );
}

#[test]
fn a_breakpoint_asked_for_again_unchanged_keeps_its_hit_count() {
    let fixture = Fixture::new("counted", COUNTED);
    let visited = fixture.directory().join("visited");
    let at_line = line_of(COUNTED, "return marker");
    let third = || {
        SourceBreakpoint::at(1, fixture.path(), at_line).counting(HitCondition::Exactly {
            count: NonZeroU32::new(3).expect("3 is not zero"),
        })
    };

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(vec![third()])
        .expect("the breakpoint request was answered");
    let (reason, _) = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, rebound } => (stop.reason, rebound),
        Running::Exited { status, .. } => panic!("it exited with {status} instead of stopping"),
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        // bpd launched this program and holds its child, so it is bpd that
        // reads the exit
        Running::Ended { .. } => unreachable!(
            "the program bpd launched ended without an exit status, and bpd \
             holds its child"
        ),
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    };
    assert_eq!(stopped_for(&reason), [1]);
    assert_eq!(
        std::fs::read_to_string(&visited).expect("the line above the stop has run"),
        "3"
    );

    // the same request again. the count is three, and "the third hit" has
    // already happened — a counter that started over would stop again on six
    debuggee
        .set_breakpoints(vec![third()])
        .expect("the breakpoint request was answered");
    assert_eq!(passes_that_stopped(&mut debuggee, &visited), []);
}

#[test]
fn a_breakpoint_that_changed_starts_counting_again() {
    let fixture = Fixture::new("counted", COUNTED);
    let visited = fixture.directory().join("visited");
    let at_line = line_of(COUNTED, "return marker");
    let third = SourceBreakpoint::at(1, fixture.path(), at_line).counting(HitCondition::Exactly {
        count: NonZeroU32::new(3).expect("3 is not zero"),
    });

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(vec![third.clone()])
        .expect("the breakpoint request was answered");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        Running::Exited { status, .. } => panic!("it exited with {status} instead of stopping"),
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        // bpd launched this program and holds its child, so it is bpd that
        // reads the exit
        Running::Ended { .. } => unreachable!(
            "the program bpd launched ended without an exit status, and bpd \
             holds its child"
        ),
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    }

    // a different breakpoint, even though it carries the same id: it counts
    // something else now, so counting it from where the old one had got to
    // would answer a question nobody asked
    debuggee
        .set_breakpoints(vec![third.when("value > 0")])
        .expect("the breakpoint request was answered");
    assert_eq!(
        passes_that_stopped(&mut debuggee, &visited),
        [("6".to_string(), vec![1])],
        "the count restarted at the fourth pass, so its third hit is the sixth"
    );
}

/// a condition that reaches a breakpoint of its own, two ways
///
/// `helper` holds a breakpoint and is called by the condition on the line above
/// it, and separately by the program. `double` holds a breakpoint and its own
/// condition calls it
const REENTRANT: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent


def helper(value):
    doubled = value * 2
    return doubled


def double(value):
    twice = value + value
    return twice


def main():
    for item in [1, 2, 3]:
        marker = item
    for item in [1, 2]:
        double(item)
    return helper(9) + marker


(HERE / "result").write_text(str(main()))
"#;

#[test]
fn a_condition_that_calls_a_function_with_a_breakpoint_in_it_does_not_stop_inside_itself() {
    let fixture = Fixture::new("reentrant", REENTRANT);
    let outer = line_of(REENTRANT, "marker = item");
    let inner = line_of(REENTRANT, "doubled = value * 2");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), outer).when("helper(item) > 4"),
            SourceBreakpoint::at(2, fixture.path(), inner),
        ])
        .expect("the breakpoint request was answered");

    let mut lines = Vec::new();
    run_to_exit(&mut debuggee, |reason| {
        let StopReason::Breakpoint {
            breakpoints, line, ..
        } = reason
        else {
            panic!("expected a breakpoint stop, got {reason:?}")
        };
        lines.push((*line, breakpoints.clone()));
    });

    // the condition calls `helper` three times and the program calls it once.
    // a breakpoint that fired during the evaluation would put three more stops
    // in here, each of them inside a frame the debugger created
    assert_eq!(
        lines,
        [(outer, vec![1]), (inner, vec![2])],
        "the condition is true on the third pass, and `helper` only stops when \
         the program is the one calling it"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.directory().join("result"))
            .expect("the program ran to the end"),
        "21"
    );
}

#[test]
fn the_interpreter_does_not_report_an_event_raised_from_inside_a_callback() {
    // the two tests around this one assert the *rule* — a condition does not
    // stop inside itself — and they would pass with the agent's own suppression
    // deleted, because cpython refuses to re-enter a tool's callback on a thread
    // that is already inside one. that is `tstate->tracing` in
    // `instrumentation.c` and it is not in PEP 669, so it is measured here
    // rather than assumed, in a bare interpreter with no agent anywhere near it
    //
    // it also says something the discovery design has to live with: `PY_START`
    // is suppressed the same way, so a code object first run by a condition is
    // not registered by it
    let seen = bpd_test::eval(
        interpreter(),
        "import json, sys\n\
         TOOL = 2\n\
         sys.monitoring.use_tool_id(TOOL, 'probe')\n\
         seen = []\n\
         def inner():\n\
         \x20   return 1\n\
         def outer():\n\
         \x20   return 2\n\
         def on_line(code, line):\n\
         \x20   seen.append(code.co_qualname)\n\
         \x20   if code.co_qualname == 'outer':\n\
         \x20       inner()\n\
         \x20   return None\n\
         def on_start(code, offset):\n\
         \x20   seen.append('start ' + code.co_qualname)\n\
         \x20   return None\n\
         events = sys.monitoring.events\n\
         sys.monitoring.register_callback(TOOL, events.LINE, on_line)\n\
         sys.monitoring.register_callback(TOOL, events.PY_START, on_start)\n\
         sys.monitoring.set_local_events(TOOL, outer.__code__, events.LINE)\n\
         sys.monitoring.set_local_events(TOOL, inner.__code__, events.LINE)\n\
         sys.monitoring.set_events(TOOL, events.PY_START)\n\
         outer()\n\
         sys.monitoring.set_events(TOOL, 0)\n\
         print(json.dumps(seen))\n",
    );
    let seen: Vec<String> =
        serde_json::from_str(&seen).expect("the probe prints a json list of what it saw");

    assert_eq!(
        seen,
        ["start outer", "outer"],
        "the callback called `inner`, and the interpreter reported nothing about \
         it — neither its `PY_START` nor the line it ran. if this ever changes, \
         the agent's own suppression becomes the thing that stops a condition \
         recursing, and the tests around it start being able to fail"
    );
}

#[test]
fn a_condition_that_reaches_the_line_it_is_attached_to_terminates() {
    let fixture = Fixture::new("reentrant", REENTRANT);
    let recursive = line_of(REENTRANT, "twice = value + value");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), recursive).when("double(1) == 2"),
        ])
        .expect("the breakpoint request was answered");

    // evaluating the condition runs the very line the breakpoint is on. without
    // the suppression this recurses until the interpreter gives up, so the
    // assertion is as much that this test finishes as that it counts two stops
    let mut stops = 0;
    run_to_exit(&mut debuggee, |reason| {
        assert_eq!(stopped_for(reason), [1]);
        stops += 1;
    });
    assert_eq!(stops, 2, "the program calls `double` twice");
}

/// a condition that raises, and one that raises inside a call
const RAISES: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent
MARKS = HERE / "marks"


def boom():
    return 1 / 0


def visit(value):
    MARKS.write_text("before")
    marker = value
    MARKS.write_text("after")
    return marker


visit(1)
"#;

#[test]
fn a_condition_that_raises_stops_and_reports_the_exception() {
    let fixture = Fixture::new("raises", RAISES);
    let marks = fixture.directory().join("marks");
    let at_line = line_of(RAISES, "marker = value");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), at_line).when("value.missing"),
        ])
        .expect("the breakpoint request was answered");

    let reason = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop.reason,
        Running::Exited { status, .. } => {
            panic!("a condition that raised was treated as false, and the program ran to {status}")
        }
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        // bpd launched this program and holds its child, so it is bpd that
        // reads the exit
        Running::Ended { .. } => unreachable!(
            "the program bpd launched ended without an exit status, and bpd \
             holds its child"
        ),
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    };

    let StopReason::EvaluationFailed {
        breakpoint,
        part,
        expression,
        line,
        error,
        ..
    } = &reason
    else {
        panic!("expected the failure to be reported, got {reason:?}")
    };
    assert_eq!(*breakpoint, 1);
    assert_eq!(*part, Part::Condition);
    assert_eq!(expression, "value.missing");
    assert_eq!(*line, at_line);
    assert_eq!(error.kind, "AttributeError");
    assert!(
        error.message.contains("missing"),
        "the message has to name what went wrong, and it said {}",
        error.message
    );
    assert_eq!(
        error
            .traceback
            .iter()
            .map(|frame| frame.file.as_str())
            .collect::<Vec<_>>(),
        ["<bpd condition of breakpoint 1>"],
        "the traceback has to say the expression is the debugger's, not the \
         program's"
    );

    // and it is held *at* the line, not past it
    assert_eq!(
        std::fs::read_to_string(&marks).expect("the line above has run"),
        "before"
    );

    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    run_to_exit(&mut debuggee, |reason| {
        panic!("nothing is set, and it stopped for {reason:?}")
    });
}

#[test]
fn a_condition_that_raises_inside_a_call_carries_the_frames_it_raised_in() {
    let fixture = Fixture::new("raises", RAISES);
    let at_line = line_of(RAISES, "marker = value");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), at_line).when("boom()"),
        ])
        .expect("the breakpoint request was answered");

    let reason = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop.reason,
        Running::Exited { status, .. } => panic!("it ran to {status} instead of stopping"),
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        // bpd launched this program and holds its child, so it is bpd that
        // reads the exit
        Running::Ended { .. } => unreachable!(
            "the program bpd launched ended without an exit status, and bpd \
             holds its child"
        ),
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    };
    let StopReason::EvaluationFailed { error, .. } = &reason else {
        panic!("expected the failure to be reported, got {reason:?}")
    };

    assert_eq!(error.kind, "ZeroDivisionError");
    assert_eq!(
        error
            .traceback
            .iter()
            .map(|frame| frame.function.as_str())
            .collect::<Vec<_>>(),
        ["<module>", "boom"],
        "the expression called into the program, and the frames say where it got \
         to — got {:?}",
        error.traceback
    );
    assert_eq!(
        error.traceback[1].line,
        line_of(RAISES, "return 1 / 0"),
        "the frame in the program names the line that raised"
    );

    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    run_to_exit(&mut debuggee, |reason| {
        panic!("nothing is set, and it stopped for {reason:?}")
    });
}

#[test]
fn an_expression_that_does_not_compile_is_refused_when_it_is_set() {
    let fixture = Fixture::new("counted", COUNTED);
    let at_line = line_of(COUNTED, "return marker");

    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), at_line).when("value =="),
            SourceBreakpoint::at(2, fixture.path(), at_line).logging("count is {"),
            SourceBreakpoint::at(3, fixture.path(), at_line).logging("count is }"),
            SourceBreakpoint::at(4, fixture.path(), at_line).logging("count is {}"),
            SourceBreakpoint::at(5, fixture.path(), at_line).logging("count is {1 +}"),
            // the file is fine and the line is fine, and it still cannot be set
            SourceBreakpoint::at(6, fixture.path(), at_line).when("value == 1"),
        ])
        .expect("the breakpoint request was answered");

    let reason = unbound(&resolved[0]);
    assert!(
        matches!(
            reason,
            Unbound::ConditionInvalid { condition, error }
                if condition == "value ==" && error.kind == "SyntaxError"
        ),
        "a condition that does not compile is refused with the interpreter's own \
         words, and got {reason}"
    );

    for (index, expected) in [(1, "never closed"), (2, "no `{`"), (3, "empty")] {
        let reason = unbound(&resolved[index]);
        assert!(
            matches!(reason, Unbound::LogMessageInvalid { reason, .. } if reason.contains(expected)),
            "expected {expected:?}, and got {reason}"
        );
    }

    let reason = unbound(&resolved[4]);
    assert!(
        matches!(
            reason,
            Unbound::LogMessageInvalid { expression: Some(expression), .. } if expression == "1 +"
        ),
        "the refusal has to name which piece of the message is wrong, and got {reason}"
    );

    assert_eq!(
        evaluation(&resolved[5]),
        Evaluation::Comparison,
        "a bad breakpoint in the set must not take the good ones with it"
    );

    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    run_to_exit(&mut debuggee, |reason| {
        panic!("nothing is set, and it stopped for {reason:?}")
    });
}

#[test]
fn a_logpoint_reports_what_the_frame_held_and_does_not_stop() {
    let fixture = Fixture::new("counted", COUNTED);
    let at_line = line_of(COUNTED, "return marker");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(7, fixture.path(), at_line)
                .when("value <= 3")
                .logging("value={value} doubled={value * 2} literal={{brace}}"),
        ])
        .expect("the breakpoint request was answered");

    let mut logs = bpd_test::reporting::Logs::default();
    match debuggee.run(&mut logs).expect("the debuggee was resumed") {
        Running::Exited { status, .. } => assert!(status.success(), "it exited with {status}"),
        Running::Stopped { stop, .. } => {
            panic!("a logpoint logs instead of stopping, and it stopped for {stop:?}")
        }
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        // bpd launched this program and holds its child, so it is bpd that
        // reads the exit
        Running::Ended { .. } => unreachable!(
            "the program bpd launched ended without an exit status, and bpd \
             holds its child"
        ),
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    }

    assert_eq!(
        logs.records
            .iter()
            .map(|record| (record.hit, record.message.as_str(), record.line))
            .collect::<Vec<_>>(),
        [
            (1, "value=1 doubled=2 literal={brace}", at_line),
            (2, "value=2 doubled=4 literal={brace}", at_line),
            (3, "value=3 doubled=6 literal={brace}", at_line),
        ]
    );
    for record in &logs.records {
        assert_eq!(record.breakpoint, 7);
        assert_eq!(Path::new(&record.file), fixture.path());
    }
}

#[test]
fn a_log_message_that_raises_stops_and_reports_it() {
    let fixture = Fixture::new("counted", COUNTED);
    let at_line = line_of(COUNTED, "return marker");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), at_line).logging("value is {value.missing}"),
        ])
        .expect("the breakpoint request was answered");

    let reason = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop.reason,
        Running::Exited { status, .. } => {
            panic!("a log message that raised was skipped, and it ran to {status}")
        }
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        // bpd launched this program and holds its child, so it is bpd that
        // reads the exit
        Running::Ended { .. } => unreachable!(
            "the program bpd launched ended without an exit status, and bpd \
             holds its child"
        ),
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    };
    let StopReason::EvaluationFailed {
        part, expression, ..
    } = &reason
    else {
        panic!("expected the failure to be reported, got {reason:?}")
    };
    assert_eq!(*part, Part::LogMessage);
    assert_eq!(expression, "value.missing");

    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    run_to_exit(&mut debuggee, |reason| {
        panic!("nothing is set, and it stopped for {reason:?}")
    });
}

/// a line executed a million times, with the count observable from outside
const HOT: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent
TOTAL = 1000000


def spin():
    total = 0
    for index in range(TOTAL):
        total += index
    return total


(HERE / "sum").write_text(str(spin()))
"#;

#[test]
fn a_logpoint_on_a_hot_line_costs_no_round_trips() {
    const TOTAL: u64 = 1_000_000;

    let fixture = Fixture::new("hot", HOT);
    let at_line = line_of(HOT, "total += index");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), at_line).logging("{index}"),
        ])
        .expect("the breakpoint request was answered");

    let before = requests_sent(&debuggee);
    let mut counted = Counted::default();
    match debuggee
        .run(&mut counted)
        .expect("the debuggee was resumed")
    {
        Running::Exited { status, .. } => assert!(status.success(), "it exited with {status}"),
        Running::Stopped { stop, .. } => panic!("a logpoint does not stop, and got {stop:?}"),
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        // bpd launched this program and holds its child, so it is bpd that
        // reads the exit
        Running::Ended { .. } => unreachable!(
            "the program bpd launched ended without an exit status, and bpd \
             holds its child"
        ),
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    }

    assert_eq!(counted.records, TOTAL, "one record per pass over the line");

    // the number that makes this a claim rather than a hope. the agent reads
    // the control connection only inside a stop, so a request per hit would
    // show up here as a million of them — and the debuggee would have waited
    // for every one
    assert_eq!(
        requests_sent(&debuggee),
        before + 1,
        "resuming is one request, and a million log records must not add any"
    );

    let first = counted.first.expect("a million records is more than none");
    assert_eq!((first.hit, first.message.as_str()), (1, "0"));
    let last = counted.last.expect("a million records is more than none");
    assert_eq!((last.hit, last.message.as_str()), (TOTAL, "999999"));

    // the program's own arithmetic, so the loop really ran a million times
    // rather than the agent having counted something else
    assert_eq!(
        std::fs::read_to_string(fixture.directory().join("sum"))
            .expect("the program ran to the end"),
        (TOTAL * (TOTAL - 1) / 2).to_string()
    );
}

/// a sink for a logpoint that fires a million times
///
/// `bpd_test::reporting::Logs` keeps every record, which is the right thing for
/// a countable logpoint and the wrong thing for this one: a million records
/// held in the test's own heap would be measuring the test rather than the
/// agent. what this keeps is the count and the two ends, which is what the
/// assertions are about
#[derive(Debug, Default)]
struct Counted {
    records: u64,
    first: Option<bpd_core::LogRecord>,
    last: Option<bpd_core::LogRecord>,
}

impl bpd_core::Reporting for Counted {
    fn logged(&mut self, record: bpd_core::LogRecord) {
        self.records += 1;
        if self.records == 1 {
            self.first = Some(record.clone());
        }
        self.last = Some(record);
    }

    fn pausing(&mut self, running: Vec<u64>) {
        panic!("no pause was armed, and the agent acknowledged one naming {running:?}")
    }

    fn spawned(&mut self, child: bpd_core::Spawn) {
        panic!("this program starts no child, and it started {child}")
    }

    fn blind_to(&mut self, blindspot: bpd_core::Blindspot) {
        panic!("this interpreter announced a blind spot nothing here is about: {blindspot}")
    }

    fn attached(&mut self, session: bpd_core::SessionId) {
        panic!("this program does not fork, and {session} joined this debuggee")
    }
}

/// a program whose condition is the first thing to run another module
const IMPORTS_IN_A_CONDITION: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent


def visit(value):
    marker = value
    return marker


visit(1)
import late
late.helped(2)
(HERE / "done").write_text("done")
"#;

/// two functions, so a partial view of the file is visibly partial
const LATE: &str = r"def helped(value):
    doubled = value + 1
    return doubled


def untouched(value):
    tripled = value * 3
    return tripled
";

#[test]
fn a_file_only_half_seen_binds_nothing_and_says_which_half_is_missing() {
    let fixture = Fixture::new("imports_in_a_condition", IMPORTS_IN_A_CONDITION);
    let late = fixture.sibling("late", LATE);
    let outer = line_of(IMPORTS_IN_A_CONDITION, "marker = value");
    let helped = line_of(LATE, "doubled = value + 1");
    let untouched = line_of(LATE, "tripled = value * 3");

    let mut debuggee = launch(&fixture);
    let resolved = debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), outer)
                .when("__import__('late').helped(value) == 2"),
            SourceBreakpoint::at(2, late.clone(), helped),
            SourceBreakpoint::at(3, late, untouched),
        ])
        .expect("the breakpoint request was answered");
    for resolution in &resolved[1..] {
        assert!(
            matches!(unbound(resolution), Unbound::NotLoaded { .. }),
            "nothing has run `late.py` yet, and got {:?}",
            resolution.binding
        );
    }

    // the condition imports `late` and calls `helped`. the import happens
    // inside a monitoring callback, so the interpreter reports no `PY_START`
    // for the module — and then the program calls `helped` itself, which *is*
    // reported. that leaves bpd holding one function out of a file of two
    let mut rebindings = Vec::new();
    loop {
        match debuggee
            .run(&mut bpd_test::reporting::Unreported)
            .expect("the debuggee was resumed")
        {
            Running::Stopped { stop, rebound } => {
                assert_eq!(stopped_for(&stop.reason), [1]);
                rebindings.extend(rebound);
            }
            Running::Exited { status, rebound } => {
                assert!(status.success(), "the program exited with {status}");
                rebindings.extend(rebound);
                break;
            }
            Running::StillRunning { waited, .. } => unreachable!(
                "this wait carries no deadline and was answered after {waited:?} \
                 with the program still running"
            ),
            // bpd launched this program and holds its child, so it is bpd that
            // reads the exit
            Running::Ended { .. } => unreachable!(
                "the program bpd launched ended without an exit status, and bpd \
                 holds its child"
            ),
            Running::Finishing { threads, .. } => {
                panic!("nothing was held, and the debuggee ended holding {threads:?}")
            }
        }
    }

    // the line `helped` really does hold could be bound soundly. the one in
    // `untouched` could not, and taking the union of what is visible would have
    // said `late.py` has no executable line after line 3 — which is a false
    // statement about the file. a half seen file answers neither question
    assert_eq!(
        rebindings
            .iter()
            .map(|resolution| (resolution.id, &resolution.binding))
            .filter(|(_, binding)| matches!(
                binding,
                Binding::Unbound {
                    reason: Unbound::PartiallyLoaded { .. }
                }
            ))
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        [2, 3],
        "both breakpoints in the half seen file have to say so, and the \
         rebindings were {rebindings:?}"
    );
    assert!(
        rebindings
            .iter()
            .all(|resolution| matches!(resolution.binding, Binding::Unbound { .. })),
        "nothing in a half seen file may be reported as set, and got {rebindings:?}"
    );
}
