//! what a stop holds: the stack, the scopes, the values, and writing one back
//!
//! two rules are load bearing here and neither can be checked by reading the
//! answer on its own:
//!
//! - **a written local is a write the program received.** the proof is not that
//!   the debugger reads it back — `f_locals` will read back a name the compiled
//!   code never looks at — it is that the *program's own output* changes
//! - **no frame of bpd's is ever in a stack.** the debugger has exactly one
//!   python frame, the `-c` bootstrap, and it is the parent of the program's
//!   module frame. a walk that does not stop at it reports it
//!
//! the fixtures keep a marker after the breakpoint line, as everywhere else in
//! this suite, so no test takes the agent's word for where the program is

use std::path::Path;

use bpd_core::python::Capabilities;
use bpd_core::{
    Binding, Content, Detail, Evaluated, FrameId, Omitted, Resolved, Running, Scope,
    SourceBreakpoint, StopReason, Value, Variables,
};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// a closure over a variable that a global of the same name also has
///
/// `shared` is a free variable of `inner` and a global, holding different text.
/// a debugger that read `f_locals` and called the result "variables" cannot
/// tell them apart, and neither can one that falls back to the globals
const STATE: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent
RESULT = HERE / "result"
shared = "the global one"


def outer(seed):
    shared = "the captured one"
    tally = {"count": 1, "items": [1, 2, 3]}

    def inner(step):
        total = seed + step
        marker = total
        afterwards = len(shared)
        return total * 2 + afterwards

    return inner(2) + len(tally)


def main():
    value = outer(10)
    RESULT.write_text(str(value))


main()
"#;

/// what `inner` captures, which is what a write to `total` has to change
const CAPTURED: &str = "the captured one";

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

/// require that every breakpoint in a request bound
fn bound(resolved: &[Resolved]) {
    for resolution in resolved {
        match &resolution.binding {
            Binding::Bound { .. } | Binding::BoundInTemplate { .. } => {}
            Binding::Unbound { reason } => {
                panic!("breakpoint {} did not bind: {reason}", resolution.id)
            }
        }
    }
}

/// set one breakpoint and run to it
fn stop_at(debuggee: &mut Debuggee, file: &Path, line: u32) {
    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, file, line)])
        .expect("the breakpoint request was answered");
    bound(&resolved);

    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => match stop.reason {
            StopReason::Breakpoint {
                breakpoints,
                line: at,
                ..
            } => {
                assert_eq!(breakpoints, [1]);
                assert_eq!(at, line);
            }
            other => panic!("expected a breakpoint stop, got {other:?}"),
        },
        Running::Exited { status, .. } => {
            panic!("it exited with {status} instead of stopping")
        }
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    }
}

/// clear the breakpoints and let the program finish
fn to_exit(debuggee: &mut Debuggee) {
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Exited { status, .. } => assert!(status.success(), "it exited with {status}"),
        Running::Stopped { stop, .. } => panic!("nothing is set, and it stopped for {stop:?}"),
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    }
}

/// the frame that stopped
const fn top() -> FrameId {
    FrameId { stop: 2, depth: 0 }
}

/// the text of a string value, or a failure naming what it was instead
fn text_of(value: &Value) -> &str {
    match &value.content {
        Content::Str { text, .. } => text,
        other => panic!("expected a string, got {other:?}"),
    }
}

/// the digits of an integer value, or a failure naming what it was instead
fn digits_of(value: &Value) -> &str {
    match &value.content {
        Content::Int { text, .. } => text,
        other => panic!("expected an integer, got {other:?}"),
    }
}

/// what one name holds, or a failure saying the scope did not have it
fn held<'a>(variables: &'a Variables, name: &str) -> &'a Value {
    variables.get(name).unwrap_or_else(|| {
        panic!(
            "expected `{name}`, and the scope held {:?}",
            variables.names()
        )
    })
}

/// evaluate an expression and require that it produced a value
fn evaluated(debuggee: &mut Debuggee, frame: FrameId, expression: &str) -> Value {
    match debuggee
        .evaluate(frame, expression, Detail::default())
        .expect("the evaluation was answered")
    {
        Evaluated::Value { value } => value,
        Evaluated::Raised { error } => panic!("`{expression}` raised {error}"),
    }
}

#[test]
fn the_stack_at_the_entry_stop_is_the_programs_own_and_holds_nothing_of_bpds() {
    let fixture = Fixture::new("state", STATE);
    let mut debuggee = launch(&fixture);

    let stack = debuggee.the_stack(None).expect("the stack was answered");

    // the interpreter was entered through `python -c "import bpd_agent;
    // bpd_agent.main()"`, so the module frame's `f_back` is that bootstrap.
    // a walk that does not stop at it reports a frame in `<string>` running
    // code the user never wrote
    assert_eq!(
        stack
            .frames
            .iter()
            .map(|frame| (frame.name(), frame.file.as_str()))
            .collect::<Vec<_>>(),
        [("<module>", fixture.path().to_str().expect("a utf8 path"))],
        "the program has run nothing, so its own module frame is the whole stack"
    );
    assert_eq!(stack.depth, 1);
    assert_eq!(stack.frames[0].id, FrameId { stop: 1, depth: 0 });

    to_exit(&mut debuggee);
}

#[test]
fn a_stack_is_the_call_chain_of_the_thread_that_stopped() {
    let fixture = Fixture::new("state", STATE);
    let at_line = line_of(STATE, "marker = total");
    let mut debuggee = launch(&fixture);
    stop_at(&mut debuggee, &fixture.path(), at_line);

    let stack = debuggee.the_stack(None).expect("the stack was answered");
    assert_eq!(
        stack
            .frames
            .iter()
            .map(bpd_core::Frame::name)
            .collect::<Vec<_>>(),
        ["outer.<locals>.inner", "outer", "main", "<module>"]
    );
    assert_eq!(stack.depth, 4);
    assert_eq!(stack.frames[0].line, at_line);
    assert_eq!(
        stack.frames[1].line,
        line_of(STATE, "return inner(2) + len(tally)")
    );
    for (depth, frame) in stack.frames.iter().enumerate() {
        assert_eq!(
            frame.id,
            FrameId {
                stop: 2,
                depth: u32::try_from(depth).expect("four frames"),
            }
        );
    }

    // asking for fewer never hides that there are more
    let cut = debuggee.the_stack(Some(2)).expect("the stack was answered");
    assert_eq!(cut.frames.len(), 2);
    assert_eq!(cut.depth, 4);

    to_exit(&mut debuggee);
}

#[test]
fn the_scopes_of_a_frame_are_not_one_namespace() {
    let fixture = Fixture::new("state", STATE);
    let at_line = line_of(STATE, "marker = total");
    let mut debuggee = launch(&fixture);
    stop_at(&mut debuggee, &fixture.path(), at_line);

    let detail = Detail::default();
    let locals = debuggee
        .variables(top(), Scope::Local, detail)
        .expect("the locals were answered");
    let free = debuggee
        .variables(top(), Scope::Free, detail)
        .expect("the free variables were answered");
    // a namespace begins with `__builtins__`, which no byte budget opens three
    // levels deep. the answer is the whole scope, read shallow, and it says so
    let globals = debuggee
        .variables(top(), Scope::Global, detail)
        .expect("the globals were answered");
    assert!(
        globals
            .omitted
            .iter()
            .any(|omitted| matches!(omitted, Omitted::Shallower { .. })),
        "the globals do not fit the default budget at the default depth, and it \
         said {:?}",
        globals.omitted
    );

    // `shared` is a free variable holding one thing and a global holding
    // another. `f_locals` merges the two scopes and would answer this question
    // with whichever one it happened to find
    assert_eq!(text_of(held(&free, "shared")), CAPTURED);
    assert_eq!(text_of(held(&globals, "shared")), "the global one");
    assert!(
        locals.get("shared").is_none(),
        "`shared` is not a local of `inner`, and the locals held {:?}",
        locals.names()
    );

    assert_eq!(locals.names(), ["step", "total"]);
    assert_eq!(digits_of(held(&locals, "total")), "12");
    // both are locals of the frame and neither has been assigned at this line.
    // absent would be a different statement, and `None` would be a false one
    assert_eq!(locals.unbound, ["marker", "afterwards"]);
    assert!(locals.unreadable.is_empty());

    // the enclosing frame is where the cell lives
    let enclosing = FrameId { stop: 2, depth: 1 };
    let outer_cells = debuggee
        .variables(enclosing, Scope::Cell, detail)
        .expect("the cell variables were answered");
    let outer_locals = debuggee
        .variables(enclosing, Scope::Local, detail)
        .expect("the locals were answered");

    assert_eq!(text_of(held(&outer_cells, "shared")), CAPTURED);
    assert!(
        outer_locals.get("shared").is_none(),
        "`shared` is a cell of `outer` and not one of its fast locals, and its \
         locals held {:?}",
        outer_locals.names()
    );
    // an argument a closure captures is in both scopes, because that is what
    // cpython says it is
    assert!(outer_cells.get("seed").is_some() && outer_locals.get("seed").is_some());

    to_exit(&mut debuggee);
}

#[test]
fn a_local_written_at_a_stop_is_the_value_the_program_goes_on_to_use() {
    let fixture = Fixture::new("state", STATE);
    let at_line = line_of(STATE, "marker = total");
    let result = fixture.directory().join("result");
    let mut debuggee = launch(&fixture);
    stop_at(&mut debuggee, &fixture.path(), at_line);

    let written = match debuggee
        .set_variable(top(), Scope::Local, "total", "999", Detail::default())
        .expect("the write was answered")
    {
        Evaluated::Value { value } => value,
        Evaluated::Raised { error } => panic!("the write raised {error}"),
    };
    // read back out of the frame, not the value that was handed over
    assert_eq!(digits_of(&written), "999");

    to_exit(&mut debuggee);

    // the program's own arithmetic. reading the value back proves only that the
    // proxy remembers it — this proves the compiled code loaded it
    let expected = 999 * 2 + CAPTURED.len() + 2;
    assert_eq!(
        std::fs::read_to_string(&result).expect("the program ran to the end"),
        expected.to_string(),
        "the program computed its result from the local bpd wrote"
    );
}

#[test]
fn a_global_written_at_a_stop_is_the_value_the_program_goes_on_to_use() {
    let fixture = Fixture::new("state", STATE);
    let at_line = line_of(STATE, "marker = total");
    let result = fixture.directory().join("result");
    let mut debuggee = launch(&fixture);
    stop_at(&mut debuggee, &fixture.path(), at_line);

    match debuggee
        .set_variable(
            top(),
            Scope::Global,
            "shared",
            "'a much longer global than before'",
            Detail::default(),
        )
        .expect("the write was answered")
    {
        Evaluated::Value { value } => {
            assert_eq!(text_of(&value), "a much longer global than before");
        }
        Evaluated::Raised { error } => panic!("the write raised {error}"),
    }

    to_exit(&mut debuggee);

    // the free variable is the one `inner` reads, and it was not touched, so
    // the program's answer is the one it always had
    let expected = 12 * 2 + CAPTURED.len() + 2;
    assert_eq!(
        std::fs::read_to_string(&result).expect("the program ran to the end"),
        expected.to_string(),
        "writing the global must not have written the closure of the same name"
    );
}

#[test]
fn a_name_the_frame_does_not_have_is_refused_rather_than_written_nowhere() {
    let fixture = Fixture::new("state", STATE);
    let at_line = line_of(STATE, "marker = total");
    let mut debuggee = launch(&fixture);
    stop_at(&mut debuggee, &fixture.path(), at_line);

    let refusal = debuggee
        .set_variable(top(), Scope::Local, "invented", "1", Detail::default())
        .expect_err("`invented` is not a local of that frame");
    let said = refusal.to_string();
    assert!(
        said.contains("`invented`") && said.contains("the program would never see it"),
        "the refusal has to say why accepting it would be a lie, and said {said}"
    );

    // and asking for the wrong scope of a name that does exist says where it is
    let refusal = debuggee
        .set_variable(top(), Scope::Local, "shared", "'x'", Detail::default())
        .expect_err("`shared` is not a local of `inner`");
    let said = refusal.to_string();
    assert!(
        said.contains("free and global"),
        "the refusal has to name the scopes that do hold it, and said {said}"
    );

    to_exit(&mut debuggee);
}

#[test]
fn the_interpreter_accepts_a_write_of_a_name_the_program_can_never_read() {
    // the reason the refusal above exists. `f_locals` is PEP 667's write-through
    // proxy and it keeps a name the code object does not have — it reads back,
    // and the compiled function goes on reading the fast locals the compiler
    // gave it. measured in a bare interpreter, with no agent near it, because
    // it is the behaviour the refusal is protecting against
    let seen = bpd_test::eval(
        interpreter(),
        "import json, sys\n\
         def probe():\n\
         \x20   frame = sys._getframe()\n\
         \x20   frame.f_locals['invented'] = 5\n\
         \x20   read_back = frame.f_locals['invented']\n\
         \x20   try:\n\
         \x20       compiled = invented\n\
         \x20   except NameError:\n\
         \x20       compiled = None\n\
         \x20   return [read_back, compiled]\n\
         print(json.dumps(probe()))\n",
    );
    let seen: Vec<Option<i64>> =
        serde_json::from_str(&seen).expect("the probe prints a json list of what it saw");

    assert_eq!(
        seen,
        [Some(5), None],
        "the proxy accepted the write and read it back, and the compiled code \
         never saw it. a debugger that reported that write as performed would \
         be reporting a change the program did not receive"
    );
}

#[test]
fn an_expression_is_evaluated_in_the_frame_it_names() {
    let fixture = Fixture::new("state", STATE);
    let at_line = line_of(STATE, "marker = total");
    let mut debuggee = launch(&fixture);
    stop_at(&mut debuggee, &fixture.path(), at_line);

    assert_eq!(digits_of(&evaluated(&mut debuggee, top(), "total")), "12");
    // `shared` in `inner` is the closure, and in `outer` it is the local — the
    // same name, two frames, two answers
    assert_eq!(
        text_of(&evaluated(&mut debuggee, top(), "shared")),
        CAPTURED
    );
    let enclosing = FrameId { stop: 2, depth: 1 };
    assert_eq!(
        text_of(&evaluated(&mut debuggee, enclosing, "shared")),
        CAPTURED
    );
    assert_eq!(
        digits_of(&evaluated(&mut debuggee, enclosing, "tally['count']")),
        "1"
    );
    // a name that is not a local of the frame is the interpreter's to resolve
    assert_eq!(
        digits_of(&evaluated(&mut debuggee, top(), "len(shared)")),
        "16"
    );

    to_exit(&mut debuggee);
}

#[test]
fn an_expression_that_raises_is_answered_with_the_exception() {
    let fixture = Fixture::new("state", STATE);
    let at_line = line_of(STATE, "marker = total");
    let mut debuggee = launch(&fixture);
    stop_at(&mut debuggee, &fixture.path(), at_line);

    let raised = |debuggee: &mut Debuggee, expression: &str| match debuggee
        .evaluate(top(), expression, Detail::default())
        .expect("the evaluation was answered")
    {
        Evaluated::Raised { error } => error,
        Evaluated::Value { value } => {
            panic!("`{expression}` was supposed to raise, and produced {value:?}")
        }
    };

    let error = raised(&mut debuggee, "1 / 0");
    assert_eq!(error.kind, "ZeroDivisionError");
    assert_eq!(
        error
            .traceback
            .iter()
            .map(|frame| frame.file.as_str())
            .collect::<Vec<_>>(),
        ["<bpd evaluation>"],
        "the traceback has to say the expression is the debugger's"
    );

    // one that does not compile is the same kind of answer, not a refusal: the
    // interpreter is the authority on what an expression is
    assert_eq!(raised(&mut debuggee, "total ==").kind, "SyntaxError");
    assert_eq!(raised(&mut debuggee, "missing").kind, "NameError");

    to_exit(&mut debuggee);
}

#[test]
fn a_frame_id_from_a_stop_that_has_ended_is_refused() {
    let fixture = Fixture::new("state", STATE);
    let at_line = line_of(STATE, "marker = total");
    let mut debuggee = launch(&fixture);

    let entry = debuggee
        .the_stack(None)
        .expect("the stack was answered")
        .frames[0]
        .id;
    stop_at(&mut debuggee, &fixture.path(), at_line);

    let refusal = debuggee
        .evaluate(entry, "1", Detail::default())
        .expect_err("that frame belonged to the entry stop");
    let said = refusal.to_string();
    assert!(
        said.contains("frame 0 of stop 1") && said.contains("[2]"),
        "the refusal has to name the stale frame and what is held now, and said \
         {said}"
    );

    // and a depth that does not exist is refused rather than answered about
    // whatever is nearest
    let refusal = debuggee
        .variables(
            FrameId { stop: 2, depth: 9 },
            Scope::Local,
            Detail::default(),
        )
        .expect_err("the stack is four frames deep");
    assert!(
        refusal.to_string().contains("4 frames deep"),
        "the refusal has to say how deep it really is, and said {refusal}"
    );

    to_exit(&mut debuggee);
}

/// a condition that raises inside a call, so the stop is held with the
/// expression's own frames already unwound
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


def caller():
    return visit(1)


caller()
"#;

#[test]
fn a_stack_holds_no_frame_of_bpds_even_where_an_expression_of_bpds_just_ran() {
    // a breakpoint reached while a condition is evaluating does not fire at all
    // — cpython refuses to re-enter a tool's callback — so the way to inspect a
    // stop that a debugger expression is responsible for is the one it leaves
    // behind: the program is held inside a `LINE` callback whose condition has
    // raised and unwound. the stack is the program's, and the frames the
    // expression ran in are a separate thing the stop carries
    let fixture = Fixture::new("raises", RAISES);
    let marks = fixture.directory().join("marks");
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
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    };
    let StopReason::EvaluationFailed { error, .. } = &reason else {
        panic!("expected the failure to be reported, got {reason:?}")
    };
    assert_eq!(
        error
            .traceback
            .iter()
            .map(|frame| frame.function.as_str())
            .collect::<Vec<_>>(),
        ["<module>", "boom"],
        "the exception carries where the debugger's expression got to"
    );

    let stack = debuggee.the_stack(None).expect("the stack was answered");
    assert_eq!(
        stack
            .frames
            .iter()
            .map(bpd_core::Frame::name)
            .collect::<Vec<_>>(),
        ["visit", "caller", "<module>"],
        "the stack is where the program is, and holds nothing the debugger ran"
    );
    for frame in &stack.frames {
        assert_eq!(
            Path::new(&frame.file),
            fixture.path(),
            "every frame in a reported stack comes from the program"
        );
    }
    // and it is held at the line, not past it
    assert_eq!(
        std::fs::read_to_string(&marks).expect("the line above has run"),
        "before"
    );

    to_exit(&mut debuggee);
}

/// every shape a value can be, and several that lie about themselves
const VALUES: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent


class Node:
    def __init__(self, name):
        self.name = name
        self.next = None


class Slotted:
    __slots__ = ("only",)

    def __init__(self):
        self.only = 1


class Talkative:
    def __repr__(self):
        (HERE / "repr_ran").write_text("yes")
        return "<talkative>"


class Disguised(list):
    def __getitem__(self, index):
        raise AssertionError("bpd read an item through __getitem__")

    def __iter__(self):
        raise AssertionError("bpd read a list through __iter__")

    def __len__(self):
        raise AssertionError("bpd read a length through __len__")


def inspect_me():
    first = Node("first")
    first.next = Node("second")
    first.next.next = first
    slotted = Slotted()
    talkative = Talkative()
    disguised = Disguised([1, 2, 3])
    text = "x" * 5000
    wide = "\u65e5" * 100
    undecodable = "name\udcff"
    huge = 10**400
    numbers = list(range(200))
    mapping = {"a": 1, (1, 2): "a tuple key"}
    raw = b"\x00\x01\xff"
    edges = [None, True, 1.5, float("inf"), float("nan"), -0.0]
    marker = 1
    return marker


inspect_me()
(HERE / "done").write_text("done")
"#;

/// everything the locals of `inspect_me` hold, under one request
fn values_at(debuggee: &mut Debuggee, detail: Detail) -> Variables {
    debuggee
        .variables(top(), Scope::Local, detail)
        .expect("the locals were answered")
}

/// launch the value fixture and stop where everything is in scope
fn stopped_over_values(fixture: &Fixture) -> Debuggee {
    let mut debuggee = launch(fixture);
    stop_at(
        &mut debuggee,
        &fixture.path(),
        line_of(VALUES, "marker = 1"),
    );
    debuggee
}

#[test]
fn a_value_is_read_from_its_storage_and_not_from_the_types_own_code() {
    let fixture = Fixture::new("values", VALUES);
    let mut debuggee = stopped_over_values(&fixture);
    let locals = values_at(&mut debuggee, Detail::default());

    // `Disguised` raises from `__getitem__`, `__iter__` and `__len__`. a
    // debugger that walked the abstract protocol would be running that code —
    // this reads the list's own storage, so the type cannot change what is
    // reported about what it holds
    let disguised = held(&locals, "disguised");
    // `__main__` is left off a fully qualified name, the same way `builtins`
    // is, so a fixture's own class is named as the program names it
    assert_eq!(disguised.kind, "Disguised");
    let Content::Sequence { items, length, .. } = &disguised.content else {
        panic!("expected a sequence, got {:?}", disguised.content)
    };
    assert_eq!(*length, 3);
    assert_eq!(
        items.iter().map(digits_of).collect::<Vec<_>>(),
        ["1", "2", "3"]
    );

    // the scalars, exactly as python writes them: json has no `inf` and no
    // `nan`, and a float that arrived as `null` would be a different value
    let Content::Sequence { items, .. } = &held(&locals, "edges").content else {
        panic!("expected a sequence")
    };
    let described: Vec<&Content> = items.iter().map(|item| &item.content).collect();
    assert!(matches!(described[0], Content::None));
    assert!(matches!(described[1], Content::Bool { value: true }));
    for (index, expected) in [(2, "1.5"), (3, "inf"), (4, "nan"), (5, "-0.0")] {
        match described[index] {
            Content::Float { text } => assert_eq!(text, expected),
            other => panic!("expected a float, got {other:?}"),
        }
    }

    // a mapping is pairs, because a key is an object and not a name
    let Content::Mapping {
        entries, length, ..
    } = &held(&locals, "mapping").content
    else {
        panic!("expected a mapping")
    };
    assert_eq!(*length, 2);
    assert_eq!(text_of(&entries[0].key), "a");
    assert_eq!(entries[1].key.kind, "tuple");
    assert_eq!(text_of(&entries[1].value), "a tuple key");

    match &held(&locals, "raw").content {
        Content::Bytes { hex, length, .. } => {
            assert_eq!((hex.as_str(), *length), ("0001ff", 3));
        }
        other => panic!("expected bytes, got {other:?}"),
    }

    to_exit(&mut debuggee);
}

#[test]
fn an_object_is_read_from_its_instance_dictionary_and_repr_is_never_run_unasked() {
    let fixture = Fixture::new("values", VALUES);
    let ran = fixture.directory().join("repr_ran");
    let mut debuggee = stopped_over_values(&fixture);

    let locals = values_at(&mut debuggee, Detail::default());
    let Content::Object {
        attributes,
        omitted,
    } = &held(&locals, "first").content
    else {
        panic!("expected an object")
    };
    assert_eq!(*omitted, None);
    assert_eq!(text_of(&attributes[0].value), "first");
    assert_eq!(attributes[0].name, "name");

    // a `__slots__` type has no instance dictionary, and saying nothing about
    // it would leave an object that looks empty
    let Content::Object {
        attributes,
        omitted,
    } = &held(&locals, "slotted").content
    else {
        panic!("expected an object")
    };
    assert!(attributes.is_empty());
    assert_eq!(*omitted, Some(Omitted::NoAttributes));

    assert!(
        !ran.exists(),
        "`__repr__` is user code and nothing asked for it, and it ran anyway"
    );

    // asked for, it runs — and the answer says it is what `__repr__` said
    // rather than what the value is
    let asked = Detail {
        repr: true,
        ..Detail::default()
    };
    let locals = values_at(&mut debuggee, asked);
    match &held(&locals, "talkative").content {
        Content::Repr { text, .. } => assert_eq!(text, "<talkative>"),
        other => panic!("expected a repr, got {other:?}"),
    }
    assert!(
        ran.exists(),
        "the request asked for `__repr__` and it did not run"
    );

    // and it can be turned off entirely, for a program of proxies where even
    // reading `__dict__` is the program's own code
    let closed = Detail {
        attributes: false,
        ..Detail::default()
    };
    let locals = values_at(&mut debuggee, closed);
    assert_eq!(
        held(&locals, "first").content,
        Content::Object {
            attributes: Vec::new(),
            omitted: Some(Omitted::AttributesNotRequested),
        }
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_structure_that_points_back_at_itself_terminates_and_says_where() {
    let fixture = Fixture::new("values", VALUES);
    let mut debuggee = stopped_over_values(&fixture);
    let locals = values_at(&mut debuggee, Detail::default());

    // first.next.next is first. a walk that stopped without saying so would
    // look exactly like a structure that ended
    let Content::Object { attributes, .. } = &held(&locals, "first").content else {
        panic!("expected an object")
    };
    let next = &attributes[1].value;
    assert_eq!(attributes[1].name, "next");
    let Content::Object { attributes, .. } = &next.content else {
        panic!("expected an object")
    };
    assert_eq!(
        attributes[1].value.content,
        Content::Unread {
            omitted: Omitted::Cycle {
                path: "first".to_string()
            }
        },
        "the cycle has to name where it came round to"
    );

    to_exit(&mut debuggee);
}

#[test]
fn everything_a_limit_left_out_is_named_with_what_to_ask_for_instead() {
    let fixture = Fixture::new("values", VALUES);
    let mut debuggee = stopped_over_values(&fixture);

    let detail = Detail {
        text: 64,
        children: 10,
        depth: 1,
        ..Detail::default()
    };
    let locals = values_at(&mut debuggee, detail);

    match &held(&locals, "text").content {
        Content::Str {
            text,
            characters,
            omitted,
        } => {
            assert_eq!((text.len(), *characters), (64, 5000));
            assert_eq!(
                *omitted,
                Some(Omitted::Text {
                    characters: 5000,
                    limit: 64
                })
            );
        }
        other => panic!("expected a string, got {other:?}"),
    }

    // half of a number is a different number, so an integer is whole or absent
    match &held(&locals, "huge").content {
        Content::Int { text, omitted } => {
            assert!(text.is_empty(), "a cut integer would be a wrong integer");
            assert_eq!(
                *omitted,
                Some(Omitted::Text {
                    characters: 401,
                    limit: 64
                })
            );
        }
        other => panic!("expected an integer, got {other:?}"),
    }

    match &held(&locals, "numbers").content {
        Content::Sequence {
            items,
            length,
            omitted,
        } => {
            assert_eq!((items.len(), *length), (10, 200));
            assert_eq!(
                *omitted,
                Some(Omitted::Children {
                    length: 200,
                    limit: 10
                })
            );
        }
        other => panic!("expected a sequence, got {other:?}"),
    }

    // the text limit is characters, and the byte budget is bytes. cutting a
    // string to the budget in characters would spend three times it on this one
    match &held(&locals, "wide").content {
        Content::Str {
            text,
            characters,
            omitted,
        } => {
            assert_eq!((text.chars().count(), *characters), (64, 100));
            assert_eq!(
                *omitted,
                Some(Omitted::Text {
                    characters: 100,
                    limit: 64
                })
            );
        }
        other => panic!("expected a string, got {other:?}"),
    }

    // a lone surrogate is what `surrogateescape` puts in a filename it could not
    // decode. neither utf-8 nor json can carry one, so it is replaced — and the
    // answer says a replacement happened rather than presenting the result as
    // the string the program holds
    match &held(&locals, "undecodable").content {
        Content::Str { omitted, .. } => assert_eq!(*omitted, Some(Omitted::Unencodable)),
        other => panic!("expected a string, got {other:?}"),
    }

    // depth 1 opens the locals themselves and nothing inside them
    let Content::Object {
        attributes,
        omitted,
    } = &held(&locals, "first").content
    else {
        panic!("expected an object")
    };
    let Content::Object { omitted: inner, .. } = &attributes[1].value.content else {
        panic!("expected an object")
    };
    assert_eq!(*omitted, None);
    assert_eq!(*inner, Some(Omitted::Depth { limit: 1 }));

    to_exit(&mut debuggee);
}

#[test]
fn a_budget_that_runs_out_says_so_rather_than_answering_shorter() {
    let fixture = Fixture::new("values", VALUES);
    let mut debuggee = stopped_over_values(&fixture);

    let whole = values_at(&mut debuggee, Detail::default());
    let cramped = values_at(
        &mut debuggee,
        Detail {
            budget: 400,
            ..Detail::default()
        },
    );

    assert!(
        cramped.omitted.contains(&Omitted::Budget { limit: 400 }),
        "the budget was too small for all of it, and it said {:?}",
        cramped.omitted
    );
    // it was read at the shallowest level rather than spending the whole answer
    // on whichever variable came first
    assert!(
        cramped.omitted.contains(&Omitted::Shallower {
            asked: Detail::default().depth,
            used: 0
        }),
        "a reduced depth has to be reported, and it said {:?}",
        cramped.omitted
    );
    // every name is still there. a budget that dropped half the scope would be
    // an answer that reads as a complete list of what the frame holds
    assert_eq!(cramped.names(), whole.names());
    assert!(
        cramped.entries.iter().any(|entry| entry.value.content
            == Content::Unread {
                omitted: Omitted::Budget { limit: 400 }
            }),
        "the value the budget ran out on has to say so where it is, and the \
         scope held {:?}",
        cramped.entries
    );

    // the same frame, with a budget that fits it, is read to the depth asked for
    assert!(
        whole.omitted.is_empty() && !whole.entries.is_empty(),
        "the default budget covers this frame, and it reported {:?}",
        whole.omitted
    );

    to_exit(&mut debuggee);
}

/// a class body, whose free variables live where no frame can reach them
const CLASS_BODY: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent


def make():
    captured = 5

    class Body:
        marker = 1
        doubled = captured * 2

    return Body


make()
(HERE / "done").write_text("done")
"#;

#[test]
fn a_free_variable_a_frame_cannot_reach_is_named_rather_than_called_unbound() {
    let fixture = Fixture::new("class_body", CLASS_BODY);
    let at_line = line_of(CLASS_BODY, "marker = 1");
    let mut debuggee = launch(&fixture);
    stop_at(&mut debuggee, &fixture.path(), at_line);

    let detail = Detail::default();
    let free = debuggee
        .variables(top(), Scope::Free, detail)
        .expect("the free variables were answered");

    // a class body reads `captured` with `LOAD_DEREF` from a cell only the
    // function object holds. it is not in the body's namespace and it is not
    // unbound — it holds 5, and the frame does not expose it
    assert_eq!(free.unreadable, ["captured"]);
    assert!(free.entries.is_empty() && free.unbound.is_empty());

    // and an expression cannot reach it either: `eval` resolves a name through
    // the frame's namespaces, and the cell is in neither of them. the answer is
    // the interpreter's own `NameError` rather than a value bpd invented
    match debuggee
        .evaluate(top(), "captured", detail)
        .expect("the evaluation was answered")
    {
        Evaluated::Raised { error } => assert_eq!(error.kind, "NameError"),
        Evaluated::Value { value } => {
            panic!("a class body cannot reach that cell, and it produced {value:?}")
        }
    }

    // and writing it is refused, because the write would land in the class
    // namespace and the compiled code would never look there
    let refusal = debuggee
        .set_variable(top(), Scope::Free, "captured", "9", detail)
        .expect_err("the frame does not expose that cell");
    assert!(
        refusal.to_string().contains("class body"),
        "the refusal has to say what makes it unreachable, and said {refusal}"
    );

    // a class body's locals are its namespace mapping rather than slots, and
    // they read like one
    let locals = debuggee
        .variables(top(), Scope::Local, detail)
        .expect("the locals were answered");
    assert!(
        locals.get("__qualname__").is_some(),
        "a class body namespace holds what the interpreter put in it, and held \
         {:?}",
        locals.names()
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_module_frames_locals_are_its_globals_because_that_is_what_they_are() {
    let fixture = Fixture::new("class_body", CLASS_BODY);
    let mut debuggee = launch(&fixture);

    let entry = FrameId { stop: 1, depth: 0 };
    let detail = Detail::default();
    let locals = debuggee
        .variables(entry, Scope::Local, detail)
        .expect("the locals were answered");
    let globals = debuggee
        .variables(entry, Scope::Global, detail)
        .expect("the globals were answered");

    assert_eq!(locals.names(), globals.names());
    // ground truth from the interpreter rather than from bpd's own reading
    assert_eq!(
        bpd_test::eval(
            interpreter(),
            "import sys; print(sys._getframe().f_locals is sys._getframe().f_globals)",
        ),
        "True"
    );

    to_exit(&mut debuggee);
}
