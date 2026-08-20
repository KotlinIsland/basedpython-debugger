//! restarting a frame, against a real interpreter
//!
//! **the refusals are most of this file, and that is the feature.** a restart
//! re-executes a whole line of the caller, so whether it is safe is a question
//! about that line's instructions — and a debugger that got it wrong would not
//! produce an error, it would produce a program that had silently run a property
//! twice, or re-sorted a list, or lost a frame and never made a new one
//!
//! nothing here takes bpd's word for what happened. every fixture appends to
//! `RAN`, which the program writes out at the end, and the assertions are on
//! **what the program computed**. bpd reporting a fresh frame and the frame
//! actually being fresh are different claims, and only the second one matters

use std::ffi::OsString;
use std::path::Path;

use bpd_core::Access;
use bpd_core::python::Capabilities;
use bpd_core::{
    Abandoned, Binding, FrameId, Refusal, Restarted, Restarting, Running, SourceBreakpoint, Stop,
    StopReason, Unrestartable, Whose,
};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// one shape per rule the restart has, and every one of them called for real
///
/// `grows` is the frame that gets restarted almost everywhere: it records what
/// its parameter held on entry, and then changes it. so "the frame ran again
/// with fresh locals" and "the frame ran again with what it had" are different
/// entries in `RAN`, rather than two readings of the same one
const PROGRAM: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent
RAN = []


def note(name):
    (HERE / name).write_text("x")


class Watched:
    @property
    def attr(self):
        RAN.append("property")
        held = 1
        return held


def sink(value):
    RAN.append(("sink", value))
    carried = value
    return carried


class Gated:
    @property
    def open(self):
        RAN.append("gate")
        return True


class Split:
    @property
    def attr(self):
        RAN.append("split_property")
        return 1


class Keyed:
    def __getitem__(self, key):
        RAN.append("getitem")
        return 1


def grows(value):
    RAN.append(("grows", value))
    value = value + 100
    total = value + 1
    return total


def implicit(value):
    RAN.append(("implicit", value))
    value = value + 7


def expressive(value):
    RAN.append(("expressive", value))
    return len(RAN)


def readable(value):
    RAN.append(("readable", value))
    read_on = value + 1
    RAN.append(("readable ran on", read_on))


def guarded(value):
    RAN.append(("guarded", value))
    with Opened():
        value = value + 1
        inner = value
    return inner


class Opened:
    def __enter__(self):
        RAN.append("enter")
        return self

    def __exit__(self, *unused):
        RAN.append("exit")
        return False


def counter(tag):
    step_one = tag
    RAN.append("yielded")
    yield step_one


async def waited(value):
    RAN.append(("waited", value))
    value = value + 1
    settled = value
    return settled


async def streamed(value):
    RAN.append(("streamed", value))
    value = value + 1
    emitted = value
    yield emitted
    return


async def awaiting():
    got = await waited(1)
    RAN.append(("awaiting", got))
    return got


async def streaming():
    out = []
    async for item in streamed(1):
        out.append(item)
    RAN.append(("streaming", out))
    return out


class Ordered(type):
    @classmethod
    def __prepare__(mcls, name, bases, **kw):
        import collections

        return collections.OrderedDict()


def plain():
    got = grows(1)
    RAN.append(("plain", got))
    return got


def with_attribute(obj):
    got = grows(obj.attr)
    RAN.append(("with_attribute", got))
    return got


def with_subscript(obj):
    got = grows(obj[0])
    RAN.append(("with_subscript", got))
    return got


def with_operator(k):
    got = grows(k + 1)
    RAN.append(("with_operator", got))
    return got


def with_unpacking(args):
    got = grows(*args)
    RAN.append(("with_unpacking", got))
    return got


def twice():
    got = [grows(1), grows(2)]
    RAN.append(("twice", got))
    return got


def branched(gate):
    chosen = grows(1) if gate.open else grows(2)
    RAN.append(("branched", chosen))
    return chosen


def through_a_property(obj):
    got = sink(obj.attr)
    RAN.append(("through_a_property", got))
    return got


def into_itself(seed):
    seed = grows(seed)
    RAN.append(("into_itself", seed))
    return seed


def chained():
    first = second = grows(1)
    RAN.append(("chained", first, second))
    return first


def late_global(seed):
    RAN.append(("late_global", seed))
    kept = seed
    if kept < 0:
        return LATE_AND_UNDEFINED
    return kept


def calls_late_global():
    got = late_global(1)
    RAN.append(("calls_late_global", got))
    return got


def caught_late_global():
    try:
        got = late_global(2)
    except BaseException as exc:
        RAN.append(("late caught", type(exc).__name__))
        got = "handled"
    RAN.append(("caught_late_global", got))
    return got


def only_late(seed):
    RAN.append(("only_late", seed))
    stashed = seed
    return LATE_AND_UNDEFINED


def calls_only_late():
    try:
        got = only_late(1)
    except NameError:
        RAN.append("only_late raised")
        got = None
    RAN.append(("calls_only_late", got))


def counting_up():
    try:
        yield 1
        yield 2
    finally:
        RAN.append("generator finally")


def holds_a_generator(seed):
    RAN.append(("holds_a_generator", seed))
    gen = counting_up()
    started = next(gen)
    added = seed + started
    return added


def calls_holds_a_generator():
    got = holds_a_generator(1)
    RAN.append(("calls_holds_a_generator", got))
    return got


def mixed_exits(seed):
    RAN.append(("mixed_exits", seed))
    settled = seed
    if settled < 0:
        return grows(settled)
    return MISSING_ENTIRELY


def calls_mixed_exits():
    try:
        got = mixed_exits(1)
    except NameError:
        RAN.append("mixed_exits raised")
        got = None
    RAN.append(("calls_mixed_exits", got))


class Finalised:
    def __init__(self, tag):
        self.tag = tag

    def __del__(self):
        RAN.append(("finalised", self.tag))


def holding_one(seed):
    RAN.append(("holding_one", seed))
    held = Finalised("held")
    summed = seed + 1
    return summed


def calls_holding_one():
    got = holding_one(1)
    RAN.append(("calls_holding_one", got))
    return got


def tupled(seed):
    packed = (grows(seed), NEVER_BOUND)
    RAN.append(("tupled", packed))
    return packed


def one_liner(seed):
    return seed


def calls_one_liner():
    got = one_liner(1)
    RAN.append(("calls_one_liner", got))
    return got


def side_effect(n):
    RAN.append(("side_effect", n))
    return n


def charged(flag):
    RAN.append(("charged", flag))
    total = flag
    return (side_effect(99)
            if total
            else side_effect(0))


def wrapped(obj):
    fetched = grows(
        obj.attr
    )
    RAN.append(("wrapped", fetched))
    return fetched


def protected(k):
    try:
        head = k
    finally:
        got = grows(2)
    RAN.append(("protected", got, head))
    return got


def protected_raising(k):
    try:
        if k:
            raise ValueError("up")
        head = k
    finally:
        caught_got = grows(2)
        RAN.append(("protected_raising", caught_got))


def comprehended():
    got = [grows(n) for n in (1,)]
    RAN.append(("comprehended", got))
    return got


def last_statement():
    grows(1)


def tail():
    last_statement()
    RAN.append("tail")


def into_attribute(obj):
    obj.slot = grows(1)
    RAN.append(("into_attribute", obj.slot))
    return obj.slot


def implicitly():
    got = implicit(1)
    RAN.append(("implicitly", got))
    return got


def expressively():
    got = expressive(1)
    RAN.append(("expressively", got))
    return got


def reading():
    before = tuple(readable.__code__.co_lines())
    got = readable(1)
    RAN.append(("reading", tuple(readable.__code__.co_lines()) == before))


def guarding():
    got = guarded(1)
    RAN.append(("guarding", got))
    return got


class Slotted:
    pass


import asyncio
import sys

INNER = HERE / "inner.py"


class Missing(dict):
    def __missing__(self, key):
        RAN.append(("missing", key))
        return 7


def run_subclassed():
    if not INNER.exists():
        return
    ns = Missing()
    ns["RAN"] = RAN
    exec(compile(INNER.read_text(), str(INNER), "exec"), ns)
    ns["outer"]()


def closes(seed):
    def _use():
        return cell
    held = seed
    cell = 2
    return cell


def calls_closes():
    try:
        got = closes(1)
    except BaseException as exc:
        RAN.append(("caller caught", type(exc).__name__))
        got = "handled"
    RAN.append(("calls_closes", got))


def fused_callee(value):
    RAN.append(("fused_callee", value))
    lifted = value + 100
    settled = lifted + 1
    return settled


def fused_store(spare):
    holds = fused_callee(1); echo = spare
    RAN.append(("fused_store", holds, echo))


def calls_fused_store():
    fused_store(9)


def mirror_callee(value):
    RAN.append(("mirror_callee", value))
    raised = value + 100
    ended = raised + 1
    return ended


def fused_reads(spare):
    kept = mirror_callee(spare); mirror = spare
    RAN.append(("fused_reads", kept, mirror))


def calls_fused_reads():
    fused_reads(9)


def split_callee(value):
    RAN.append(("split_callee", value))
    grown = value + 100
    summed = grown + 1
    return summed


def split_inner():
    return 1


def boxed_callee(value):
    RAN.append(("boxed_callee", value))
    boxed_step = value + 100
    boxed_sum = boxed_step + 1
    return boxed_sum


def boxes(spare):
    box = [boxed_callee(1), spare]
    RAN.append(("boxes", box))


def calls_boxes():
    boxes(9)


def under_callee(value):
    RAN.append(("under_callee", value))
    under_step = value + 100
    under_sum = under_step + 1
    return under_sum


def under_first(spare):
    first, second = spare, under_callee(1)
    RAN.append(("under_first", first, second))


def calls_under_first():
    under_first(9)


SHARED_SEEN = 1


def reads_shared():
    RAN.append(("reads_shared", SHARED_SEEN))
    shared_step = SHARED_SEEN + 100
    shared_sum = shared_step + 1
    return shared_sum


def writes_shared():
    global SHARED_SEEN
    shared_got, SHARED_SEEN = reads_shared(), 99
    RAN.append(("writes_shared", shared_got, SHARED_SEEN))


def which_callee(value):
    RAN.append(("which_entered", value))
    which_step = value + 100
    which_sum = which_step + 1
    return which_sum


def which_name(wx, wy):
    wx, wy = wx, which_callee(wy)
    RAN.append(("which_name", wx, wy))


def calls_which_name():
    which_name(1, 2)


def cell_outer():
    cell_seen = 1

    def cell_inner():
        RAN.append(("cell_inner", cell_seen))
        cell_step = cell_seen + 100
        cell_sum = cell_step + 1
        return cell_sum

    cell_got, cell_seen = cell_inner(), 77
    RAN.append(("cell_outer", cell_got, cell_seen))


def const_callee(value):
    RAN.append(("const_entered", value))
    const_step = value + 100
    const_sum = const_step + 1
    return const_sum


def const_named():
    ca = const_callee(0); cb = 'ca'
    RAN.append(("const_named", ca, cb))


def swapped_callee(value):
    RAN.append(("swapped_entered", value, sorted(sys._getframe(1).f_locals.items())))
    swapped_step = value + 100
    swapped_sum = swapped_step + 1
    return swapped_sum


def swapped(y, spare):
    sx, y = swapped_callee(y), spare
    RAN.append(("swapped", sx, y))


def calls_swapped():
    swapped(7, 99)


def victim_callee(value):
    RAN.append(("victim_entered", value, sorted(sys._getframe(1).f_locals.items())))
    victim_step = value + 100
    victim_sum = victim_step + 1
    return victim_sum


def victim(spare, other):
    va = spare; vb = other; vgot = victim_callee(1); va = vgot
    RAN.append(("victim", va, vb, vgot))


def calls_victim():
    victim(5, 6)


def alias_callee(value):
    RAN.append(("alias_entered", value, sorted(sys._getframe(1).f_locals.items())))
    alias_step = value + 100
    alias_sum = alias_step + 1
    return alias_sum


def aliases(spare):
    kept3 = alias_callee(1); echo3 = spare; copy3 = kept3
    RAN.append(("aliases", kept3, echo3, copy3))


def calls_aliases():
    aliases(7)


def feed_callee(value):
    RAN.append(("feed_entered", value))
    feed_step = value + 100
    feed_sum = feed_step + 1
    return feed_sum


def feeds_back(seed):
    held3 = feed_callee(seed); seed = held3
    RAN.append(("feeds_back", held3, seed))


def calls_feeds_back():
    feeds_back(7)


def over_callee(value):
    RAN.append(("over_entered", value))
    over_step = value + 100
    over_sum = over_step + 1
    return over_sum


def overwritten(other):
    slot3 = over_callee(1); slot3 = other
    RAN.append(("overwritten", slot3))


def calls_overwritten():
    overwritten(7)


def declared_global_body():
    class Declared(metaclass=Ordered):
        global grows
        declared = grows(1)
        trailing = 2


def starred_callee(value):
    RAN.append(("starred_callee", value))
    hoisted = value + 100
    tallied = hoisted + 1
    return tallied


def starred():
    args = [1]
    got = starred_callee(*args)
    RAN.append(("starred", got))


def split_over_lines():
    got = split_callee(
        split_inner()
    )
    RAN.append(("split_over_lines", got))


def tail_callee(value):
    RAN.append(("tail_callee", value))
    step = value + 100
    tallied = step + 1
    return tallied


def maybe_unbound(flag):
    if flag:
        spare = 9
    packed = (tail_callee(1), spare)
    RAN.append(("maybe_unbound", packed))


def calls_maybe_unbound():
    try:
        maybe_unbound(0)
    except UnboundLocalError:
        RAN.append("unbound local out of the tail")


class Marking:
    def __enter__(self):
        RAN.append("cm_enter")
        return self

    def __exit__(self, kind, value, tb):
        RAN.append(("cm_exit", kind.__name__ if kind else None))
        return False


def stranded(cm, flag):
    try:
        if flag:
            raise ValueError("up")
    finally:
        with cm:
            marker = 1
        return


def calls_stranded():
    cm = Marking()
    stranded(cm, 1)
    RAN.append("after_stranded")


INNER_B = HERE / "inner_builtins.py"


def run_subclassed_builtins():
    if not INNER_B.exists():
        return
    import builtins
    exotic = Missing(vars(builtins))
    ns = {"__builtins__": exotic, "RAN": RAN}
    exec(compile(INNER_B.read_text(), str(INNER_B), "exec"), ns)
    ns["outer"]()


plain()
branched(Gated())
through_a_property(Watched())
into_itself(1)
chained()
try:
    calls_late_global()
except BaseException as exc:
    RAN.append(("late blew up", type(exc).__name__))
caught_late_global()
calls_one_liner()
calls_holding_one()
calls_holds_a_generator()
calls_mixed_exits()
calls_only_late()
try:
    tupled(1)
except NameError:
    RAN.append("tupled raised")
charged(1)
wrapped(Split())
protected(1)
try:
    protected_raising(1)
except ValueError:
    RAN.append("caught")
with_attribute(Watched())
with_subscript(Keyed())
with_operator(1)
with_unpacking(iter((1,)))
twice()
comprehended()
tail()
into_attribute(Slotted())
implicitly()
expressively()
reading()
guarding()
list(counter("t"))
run_subclassed()
calls_stranded()
calls_fused_store()
calls_fused_reads()
split_over_lines()
calls_boxes()
calls_under_first()
cell_outer()
calls_which_name()
writes_shared()
const_named()
calls_swapped()
calls_victim()
calls_aliases()
calls_feeds_back()
calls_overwritten()
declared_global_body()
starred()
calls_maybe_unbound()
try:
    calls_closes()
except BaseException as exc:
    RAN.append(("closes blew up", type(exc).__name__))
run_subclassed_builtins()
asyncio.run(awaiting())
asyncio.run(streaming())


class Built(metaclass=Ordered):
    made = grows(1)
    tail = 1


module_level = grows(1)
RAN.append(("module_level", module_level))
grows(3)


class Aliased:
    aliased_kept = grows(2); aliased_copy = aliased_kept
    aliased_tail = 1


RAN.append(("named", Aliased.aliased_kept, Aliased.aliased_copy))
(HERE / "ran.txt").write_text(repr(RAN))
note("finished")
"#;

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
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

/// stop the program on `line`, and take the breakpoint back off again
fn held_at(debuggee: &mut Debuggee, file: &Path, line: u32) -> Stop {
    let breakpoints = vec![SourceBreakpoint::at(1, file, line)];
    let resolved = debuggee
        .set_breakpoints(breakpoints)
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

/// the frame the held thread is executing
fn top(debuggee: &mut Debuggee) -> FrameId {
    debuggee
        .the_stack(Some(1))
        .expect("the stack was answered")
        .frames[0]
        .id
}

/// hold the program inside `grows`, called from the caller line named
///
/// `grows` is called from every shape in the fixture, so the breakpoint alone
/// does not say which caller this is about. the program is run on until the
/// frame **below** the breakpoint is at the line the case names, which is what
/// makes each case about the line it says it is about
fn held_in_grows_called_from(debuggee: &mut Debuggee, fixture: &Fixture, caller_line: &str) {
    let inside = line_of(PROGRAM, "    total = value + 1");
    let call = line_of(PROGRAM, caller_line);
    loop {
        held_at(debuggee, &fixture.path(), inside);
        let stack = debuggee.the_stack(None).expect("the stack was answered");
        assert_eq!(stack.frames[0].name(), "grows");
        if stack.frames[1].line == call {
            return;
        }
        assert!(
            debuggee.held().len() == 1,
            "the walk past a caller this case is not about left something held"
        );
    }
}

/// hold the program inside `grows` and ask for a restart of that frame
fn restart_grows(debuggee: &mut Debuggee, fixture: &Fixture) -> bpd_engine::Result<Restarted> {
    let inside = line_of(PROGRAM, "    total = value + 1");
    held_at(debuggee, &fixture.path(), inside);
    let frame = top(debuggee);
    debuggee.restart_frame(frame)
}

/// what an arranged restart said, or the outcome that was not one
fn arranged(restarted: &Restarted) -> &Restarting {
    match restarted {
        Restarted::Arranged(restarting) => restarting,
        Restarted::Refused { tried, error } => {
            panic!("cpython refused every exit line of {tried:?}: {error}")
        }
    }
}

/// wait for the next stop, which is where the restart landed
fn landed(debuggee: &mut Debuggee) -> Stop {
    match debuggee
        .wait(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was waited on")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected the restart to land, got {other:?}"),
    }
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
        other => panic!("expected the program to exit, got {other:?}"),
    }
}

fn recorded(fixture: &Fixture) -> String {
    std::fs::read_to_string(
        fixture
            .path()
            .parent()
            .expect("a fixture is in a directory")
            .join("ran.txt"),
    )
    .expect("the program wrote what it ran")
}

/// the refusal a request came back with, and its reason
fn refused(result: bpd_engine::Result<Restarted>) -> (String, Unrestartable) {
    let error = result.expect_err("the restart had to be refused");
    let said = error.to_string();
    match error {
        bpd_engine::Error::Session(bpd_core::Error::Refused {
            reason: Refusal::NotRestartable { reason, .. },
        }) => (said, reason),
        other => panic!("expected a restart refusal, got {other:?}"),
    }
}

#[test]
fn a_restarted_frame_runs_again_with_locals_the_call_bound_and_not_the_ones_it_had() {
    // **the whole point.** the old mechanism moved the frame to its own first
    // line, so it re-entered holding what it had already assigned — `grows` saw
    // its parameter as 101 and returned 202. this one makes the caller call it
    // again, so the frame is one the interpreter built: the parameter is 1
    // because that is what the call passes, and the answer is 102
    //
    // asserted on `RAN`, which the program itself wrote. bpd saying the locals
    // are fresh and the locals being fresh are different claims
    let fixture = Fixture::new("fresh_locals", PROGRAM);
    let mut debuggee = launch(&fixture);

    let restarted = restart_grows(&mut debuggee, &fixture).expect("the restart was arranged");
    let restarting = arranged(&restarted);
    assert_eq!(
        restarting.exit_line,
        line_of(PROGRAM, "    return total"),
        "the frame is forced out through a line that is only loads and a \
         return, and it said {restarting:?}"
    );
    assert_eq!(
        restarting.caller.line,
        line_of(PROGRAM, "    got = grows(1)"),
        "the line that runs again is the caller's call line, and it said \
         {restarting:?}"
    );
    assert_eq!(
        restarting.caller.function, "plain",
        "the caller is what runs again, and it said {restarting:?}"
    );
    // the forced return is a real return with a real value, and the rest of the
    // caller's line runs with it. saying so is the difference between a
    // debugger and a debugger that quietly wrote a value into a live frame
    assert_eq!(restarting.disturbed, vec!["got".to_string()]);

    let stop = landed(&mut debuggee);
    let StopReason::Restarted { function, line, .. } = &stop.reason else {
        panic!("expected the restart to land, got {:?}", stop.reason)
    };
    assert_eq!(function, "grows");
    assert_eq!(
        *line,
        line_of(PROGRAM, "    RAN.append((\"grows\", value))"),
        "the fresh frame is held before its first statement"
    );

    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    assert!(
        said.contains("('grows', 1), ('grows', 1)"),
        "the second run did not get the locals the call bound: {said}"
    );
    assert!(
        !said.contains("('grows', 101)"),
        "the frame was re-entered with what it already held, which is the \
         defect this replaced: {said}"
    );
    assert!(
        said.contains("('plain', 102)"),
        "the caller did not receive the restarted call's own answer: {said}"
    );
}

#[test]
fn a_context_manager_open_across_a_restart_is_not_exited() {
    // **the claim this replaced said the opposite.** forcing the frame out is an
    // `f_lineno` jump, and a jump out of a `with` body does not call `__exit__`
    // — so the frame that is restarted leaves its context manager open and the
    // restarted call opens a second one
    //
    // the fixture is a plain class context manager on purpose. the measurement
    // that produced the false claim used `@contextlib.contextmanager`, whose
    // `finally` runs when the generator is **collected** rather than when the
    // block is left — so it showed two ENTER/EXIT pairs for a reason that has
    // nothing to do with the restart
    let fixture = Fixture::new("cleanup", PROGRAM);
    let mut debuggee = launch(&fixture);
    let inside = line_of(PROGRAM, "        inner = value");

    held_at(&mut debuggee, &fixture.path(), inside);
    let frame = top(&mut debuggee);
    let restarted = debuggee
        .restart_frame(frame)
        .expect("the restart was arranged");
    arranged(&restarted);

    landed(&mut debuggee);
    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    // two `enter`, one `exit`: the forced return skipped the first block's
    // cleanup entirely, and only the restarted call's `with` was left normally
    assert_eq!(
        said.matches("'enter'").count(),
        2,
        "the restarted call opens the block a second time: {said}"
    );
    assert_eq!(
        said.matches("'exit'").count(),
        1,
        "the forced return is a jump out of the `with`, so its `__exit__` does \
         not run — if this ever becomes 2 the mechanism changed and every \
         document that says otherwise has to change with it: {said}"
    );
    assert!(
        said.contains("'enter', ('guarded', 1), 'enter', 'exit'"),
        "the first block was left open across the restart: {said}"
    );
}

#[test]
fn a_function_whose_only_return_is_fused_onto_a_statement_restarts() {
    // **this is the shape most functions have, and it used to be refused.**
    // cpython fuses the implicit `return None` onto the **last statement's**
    // line, so there is no *line* that is only loads and a return — moving to
    // the start of that line would run the statement. the two instructions that
    // make up the return are a perfectly clean exit all the same; they are in
    // the middle of a range, which is the only reason `f_lineno` could not name
    // them, and [`bpd_agent::linetable`] is what names them
    //
    // `implicit` ends `value = value + 7` and returns nothing. asserted on what
    // the program wrote: the statement must not have run a second time from the
    // forced exit, and the restarted call must have run the whole body again
    let fixture = Fixture::new("fused_exit", PROGRAM);
    let mut debuggee = launch(&fixture);
    let inside = line_of(PROGRAM, "    value = value + 7");

    held_at(&mut debuggee, &fixture.path(), inside);
    let frame = top(&mut debuggee);
    let restarted = debuggee
        .restart_frame(frame)
        .expect("the restart was arranged");
    let restarting = arranged(&restarted);
    assert_eq!(restarting.frame.function, "implicit", "{restarting:?}");
    // the exit is on the last statement's line, because that is the line
    // cpython fused the implicit return onto — the frame returns from partway
    // through it rather than from its start
    assert_eq!(
        restarting.exit_line,
        line_of(PROGRAM, "    value = value + 7"),
        "{restarting:?}"
    );
    // the caller binds the forced return, and is told so
    assert_eq!(restarting.disturbed, ["got"], "{restarting:?}");

    landed(&mut debuggee);
    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    assert!(
        said.contains("('implicit', 1), ('implicit', 1)"),
        "the call was not made a second time: {said}"
    );
    assert!(
        said.contains("('implicit', 1), ('implicit', 1), ('implicitly', None)"),
        "the caller bound the restarted call's own return, which is `None` \
         because `implicit` has no `return`: {said}"
    );
}

#[test]
fn the_line_table_the_forced_exit_borrows_is_put_back() {
    // **the frame is reached by lying to cpython about where a line starts, and
    // the lie has to end.** for the length of one assignment the code object's
    // line table is replaced by one carrying a line the code object does not
    // have, starting at the exit — and if it were ever left in place, every
    // traceback, every `co_lines()` and every future breakpoint binding on that
    // function would be answering out of a table bpd wrote
    //
    // asserted by the **program**, not by bpd: `reading` reads
    // `readable.__code__.co_lines()` before the call and again after the
    // restarted one, and records whether they are the same tuple. bpd saying it
    // put the table back and the table being back are different claims
    let fixture = Fixture::new("table_restored", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    read_on = value + 1"),
    );

    let frame = top(&mut debuggee);
    let restarted = debuggee
        .restart_frame(frame)
        .expect("the restart was arranged");
    assert_eq!(arranged(&restarted).frame.function, "readable");

    landed(&mut debuggee);
    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    assert!(
        said.contains("('reading', True)"),
        "the code object's line table did not come back as it was: {said}"
    );
    assert!(
        said.contains("('readable', 1), ('readable', 1), ('readable ran on', 2)"),
        "the call was not made a second time, or the forced exit ran the rest of \
         the body: {said}"
    );
}

#[test]
fn a_function_whose_every_return_returns_an_expression_is_refused_by_name() {
    // what is left after the exit became an offset rather than a line. a
    // function ending `return len(RAN)` has one `RETURN_VALUE` and the
    // instruction before it is the call — there is no sequence anywhere in it
    // that produces a value and returns without running its code, so there is
    // nothing to move to and no mechanism reaches it. known before anything is
    // attempted
    let fixture = Fixture::new("no_clean_exit", PROGRAM);
    let mut debuggee = launch(&fixture);
    let inside = line_of(PROGRAM, "    RAN.append((\"expressive\", value))");

    held_at(&mut debuggee, &fixture.path(), inside);
    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(matches!(reason, Unrestartable::NoCleanExit), "{reason:?}");
    assert!(said.contains("`expressive`"), "said {said}");
    assert!(said.contains("returns an expression"), "said {said}");
    assert!(
        said.contains("falls off its end"),
        "the refusal has to say which functions do have one, and said {said}"
    );

    // and the program is untouched by having been asked
    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    assert!(
        said.contains("('expressive', 1)")
            && !said.contains("('expressive', 1), ('expressive', 1)"),
        "a refused restart ran something: {said}"
    );
}

/// one shape: what to call it, the caller's line, what it must be refused with,
/// and the words the refusal has to carry
type Case = (
    &'static str,
    &'static str,
    fn(&Unrestartable) -> bool,
    &'static [&'static str],
);

/// every ordinary line that carries something besides the one call
///
/// data rather than a body, because the list is the point: each entry is a line
/// somebody writes without thinking about it, and each is refused for a reason
/// that names what would run twice
fn refusable_shapes() -> [Case; 8] {
    [
        (
            "attribute",
            "    got = grows(obj.attr)",
            |reason| {
                matches!(
                    reason,
                    Unrestartable::SomethingElseOnTheLine { opcode, .. } if opcode == "LOAD_ATTR"
                )
            },
            &["`LOAD_ATTR`", "property"],
        ),
        (
            "subscript",
            "    got = grows(obj[0])",
            |reason| matches!(reason, Unrestartable::SomethingElseOnTheLine { .. }),
            &["__getitem__"],
        ),
        (
            "operator",
            "    got = grows(k + 1)",
            |reason| {
                matches!(
                    reason,
                    Unrestartable::SomethingElseOnTheLine { opcode, .. } if opcode == "BINARY_OP"
                )
            },
            &["`BINARY_OP`"],
        ),
        (
            "store into an attribute",
            "    obj.slot = grows(1)",
            |reason| {
                matches!(
                    reason,
                    Unrestartable::SomethingElseOnTheLine { opcode, .. } if opcode == "STORE_ATTR"
                )
            },
            &["`STORE_ATTR`"],
        ),
        (
            "unpacked arguments",
            "    got = grows(*args)",
            |reason| {
                matches!(
                    reason,
                    Unrestartable::SomethingElseOnTheLine { opcode, .. }
                        if opcode == "CALL_FUNCTION_EX"
                )
            },
            // the one that is the call **and** is refused: unpacking iterates,
            // so a generator passed as `*args` is empty the second time and the
            // restarted call would be made with different arguments
            &["`CALL_FUNCTION_EX`", "empty by then", "positionally"],
        ),
        (
            "a call in each branch of a conditional",
            "    chosen = grows(1) if gate.open else grows(2)",
            |reason| matches!(reason, Unrestartable::MoreThanOneCall { calls: 2, .. }),
            // the shape that proves the analysis reads the **whole line**.
            // `co_lines` gives this line one range per branch, and the range the
            // caller is stopped in is `LOAD_GLOBAL, LOAD_SMALL_INT, CALL,
            // STORE_FAST` — a single clean call, which is what an earlier
            // version answered `Arranged` to while the property getter ran twice
            &["2 calls"],
        ),
        (
            "a comprehension",
            "    got = [grows(n) for n in (1,)]",
            |reason| {
                matches!(
                    reason,
                    Unrestartable::SomethingElseOnTheLine { opcode, .. }
                        if opcode == "GET_ITER" || opcode == "LOAD_FAST_AND_CLEAR"
                )
            },
            // **not** `MoreThanOneCall`: PEP 709 inlines a comprehension into
            // the caller's frame, so it has exactly one `CALL`. what refuses it
            // is the rest of the construct, and the docs said otherwise
            &["re-executes that **whole line**"],
        ),
        (
            "two calls",
            "    got = [grows(1), grows(2)]",
            |reason| matches!(reason, Unrestartable::MoreThanOneCall { calls: 2, .. }),
            &["2 calls"],
        ),
    ]
}

#[test]
fn every_shape_that_would_re_run_something_on_the_callers_line_is_refused_by_name() {
    // the rule that is most of this feature. the rewind re-executes the whole
    // line, so anything else on it runs again — and every one of these is an
    // ordinary line somebody writes without thinking about it
    for (name, caller_line, matches, wanted) in refusable_shapes() {
        let fixture = Fixture::new(&format!("refused_{}", name.replace(' ', "_")), PROGRAM);
        let mut debuggee = launch(&fixture);
        held_in_grows_called_from(&mut debuggee, &fixture, caller_line);

        let frame = top(&mut debuggee);
        let (said, reason) = refused(debuggee.restart_frame(frame));
        assert!(matches(&reason), "{name}: {reason:?}");
        for expected in wanted {
            assert!(
                said.contains(expected),
                "{name}: expected {expected:?} in {said}"
            );
        }
        // **not** a blanket "a line of its own". that was asserted of every
        // shape here, and it is the remedy for only most of them — for
        // `CALL_FUNCTION_EX` the call already **is** on a line of its own with
        // its arguments in a local, so the assertion was enforcing advice its
        // own case could not follow. each row names the remedy true of it, and
        // the rule kept is that every refusal names one
        assert!(
            said.contains("a line of its own") || said.contains("positionally"),
            "{name}: the refusal has to say what would work, and said {said}"
        );

        to_exit(&mut debuggee);
    }
}

#[test]
fn a_call_the_caller_has_no_statement_after_is_refused_by_name() {
    // the rewind can only be made from a `LINE` event — cpython answers `can
    // only jump from a 'line' trace event` to anything else — so a call the
    // caller returns straight after has nowhere to be moved from. decided from
    // the caller's own bytecode: the call line's instruction range holds the
    // `RETURN`
    let fixture = Fixture::new("last_statement", PROGRAM);
    let mut debuggee = launch(&fixture);

    held_in_grows_called_from(&mut debuggee, &fixture, "    grows(1)");

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(reason, Unrestartable::NothingRunsAfterTheCall { .. }),
        "{reason:?}"
    );
    assert!(
        said.contains("can only jump from a 'line' trace event"),
        "the refusal has to carry cpython's own rule, and said {said}"
    );
    assert!(
        said.contains("binding the result to a name"),
        "the refusal has to say what would work, and said {said}"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_generator_frame_is_refused_by_name_and_says_what_it_would_have_done() {
    // measured on 3.13, 3.14 and 3.14t: forcing a generator out makes the
    // caller's `next()` raise `StopIteration`, which leaves the caller instead
    // of reaching a line event — and the caller's `second = next(it)` passes
    // every other test this feature makes, so nothing else would catch it
    let fixture = Fixture::new("generating", PROGRAM);
    let mut debuggee = launch(&fixture);
    let yielding = line_of(PROGRAM, "    yield step_one");

    held_at(&mut debuggee, &fixture.path(), yielding);
    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            reason,
            Unrestartable::Suspendable {
                kind: bpd_core::Suspendable::Generator
            }
        ),
        "{reason:?}"
    );
    assert!(said.contains("`counter`"), "said {said}");
    assert!(said.contains("a generator"), "said {said}");
    assert!(
        said.contains("StopIteration"),
        "the refusal has to say what forcing one out really does, and said \
         {said}"
    );
    assert!(
        said.contains("set the next statement"),
        "the refusal has to name the operation that works here, and said {said}"
    );

    to_exit(&mut debuggee);
}

#[test]
fn the_outermost_frame_has_no_caller_to_run_the_call_again_and_is_refused() {
    // the entry stop holds the module frame, whose only caller is bpd's own
    // bootstrap. a restart is the caller making the call again, and there is
    // nothing above it to do that
    let fixture = Fixture::new("at_entry", PROGRAM);
    let mut debuggee = launch(&fixture);

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(matches!(reason, Unrestartable::NoCaller), "{reason:?}");
    assert!(said.contains("outermost frame"), "said {said}");
    assert!(
        said.contains("set the next statement"),
        "the refusal has to name what would work, and said {said}"
    );

    to_exit(&mut debuggee);
}

/// **the wording only** — the runtime path is not exercised anywhere
///
/// named for what it checks rather than for what it is about. no fixture reaches
/// an abandoned restart: every block shape measured on 3.13, 3.14 and 3.14t
/// accepted the rewind, and nothing here fault-injects a refusal. so this is a
/// test of two `Display` bodies, and the mechanism that produces them has no
/// integration coverage at all — which is written here rather than left for
/// somebody to infer from a name that promised more
#[test]
fn the_two_reasons_a_restart_is_abandoned_each_say_which_one_happened() {
    // the one thing a restart cannot decide in advance. everything about the
    // caller's line is read off its bytecode first, but whether cpython accepts
    // a move **to** that line from wherever the caller got to is cpython's
    // answer and it gives it at the time
    let abandoned = StopReason::RestartAbandoned {
        function: "grows".to_string(),
        wanted: 20,
        file: "/tmp/p/app.py".to_string(),
        line: 21,
        why: Abandoned::Refused {
            error: bpd_core::PythonError {
                kind: "ValueError".to_string(),
                message: "can't jump into the body of a for loop".to_string(),
                traceback: Vec::new(),
            },
        },
    };
    let StopReason::RestartAbandoned { why, .. } = &abandoned else {
        panic!("it was built as one")
    };
    let said = why.to_string();
    assert!(
        said.contains("can't jump into the body of a for loop"),
        "{said}"
    );
    assert!(
        Abandoned::CallerLeft
            .to_string()
            .contains("before it reached a line"),
        "the other half has to say which of the two it was"
    );
}

#[test]
fn a_coroutine_and_an_async_generator_are_refused_for_the_reason_a_generator_is() {
    // the experiment the prototype left open, closed by measurement. `f_back`
    // of a frame its driver sends into is whoever **resumed** it, and a
    // coroutine driven by a task has `asyncio.events.Handle._run` there —
    // rewinding that answered `InvalidStateError: __step(): already done`, which
    // is the event loop rather than the program being rewound
    //
    // a coroutine awaited **in place** does restart correctly, and is refused
    // with the rest because nothing in the frame tells the two apart. that is
    // the finding, and refusing something that would have worked is the side of
    // it this project errs on
    for (name, inside, function, kind) in [
        (
            "coroutine",
            "    settled = value",
            "waited",
            bpd_core::Suspendable::Coroutine,
        ),
        (
            "async generator",
            "    emitted = value",
            "streamed",
            bpd_core::Suspendable::AsyncGenerator,
        ),
    ] {
        let fixture = Fixture::new(&format!("suspendable_{}", name.replace(' ', "_")), PROGRAM);
        let mut debuggee = launch(&fixture);

        held_at(&mut debuggee, &fixture.path(), line_of(PROGRAM, inside));
        let frame = top(&mut debuggee);
        let (said, reason) = refused(debuggee.restart_frame(frame));
        assert!(
            matches!(reason, Unrestartable::Suspendable { kind: got } if got == kind),
            "{name}: {reason:?}"
        );
        assert!(said.contains(function), "{name}: said {said}");
        assert!(
            said.contains("Handle._run"),
            "{name}: the refusal has to carry what was measured rather than \
             only what kind of frame it is, and said {said}"
        );
        assert!(
            said.contains("set the next statement"),
            "{name}: the refusal has to name what works there, and said {said}"
        );

        to_exit(&mut debuggee);
    }
}

#[test]
fn a_call_made_from_the_module_body_is_refused_because_its_names_are_globals() {
    // a script's top level stores through its namespace mapping, and that
    // mapping **is** the global namespace — so the tail of a module-level call
    // line writes where the callee reads. if the callee reads that name, the
    // restarted call reads what bpd stored rather than what the program had,
    // and bpd cannot see what a callee reads
    //
    // this used to restart, and giving it up is deliberate: the alternative was
    // to refuse `got, G = f(), 99` and allow `kept = f(1)` on the grounds that
    // the second is *unlikely* to be read, which is the justification this
    // project does not accept. of the permitted call sites that are **in** a
    // module body it costs 520 of 1128 on 3.13 and 537 of 1156 on 3.14 — a
    // little under half, so a majority of module-level calls still restart, and
    // `a_module_body_call_that_stores_nothing_still_restarts` is one of them
    let fixture = Fixture::new("module_body", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_in_grows_called_from(&mut debuggee, &fixture, "module_level = grows(1)");

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            &reason,
            Unrestartable::TailWritesSharedState { name, .. } if name == "module_level"
        ),
        "{reason:?}"
    );
    // and it has to say **why** a module body is different, or a user reads it
    // as bpd refusing an ordinary assignment for no reason
    assert!(
        said.contains("module body") && said.contains("global namespace"),
        "said {said}"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_module_body_call_that_stores_nothing_still_restarts() {
    // the boundary of what the rule above gives up. it refuses a module-level
    // line that stores **after** the call, and a bare call statement stores
    // nothing — its tail is a `POP_TOP`. worth pinning because the refusal reads
    // broader than it is: a majority of module-level sites still restart, and
    // `f(w := 3)` does too, its store landing before the call rather than after
    let fixture = Fixture::new("module_bare", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_in_grows_called_from(&mut debuggee, &fixture, "grows(3)");

    let frame = top(&mut debuggee);
    let restarted = debuggee
        .restart_frame(frame)
        .expect("the restart was arranged");
    let restarting = arranged(&restarted);
    assert_eq!(restarting.caller.function, "<module>");
    assert!(
        restarting.disturbed.is_empty(),
        "the line binds nothing, so nothing holds the forced return: {restarting:?}"
    );

    landed(&mut debuggee);
    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    assert!(
        said.contains("('grows', 3), ('grows', 3)"),
        "the module-level call did not run again with fresh locals: {said}"
    );
}

#[test]
fn a_class_body_whose_namespace_is_not_a_dict_is_refused_by_name() {
    // a class body built through a `__prepare__` that returns a mapping of its
    // own keeps its names in that mapping, so both the read of the callee and
    // the store of the result go through it — and either runs code of the
    // program. the span meets the **read** first, which is what the sentence has
    // to describe: an earlier cut of this described the store, and was wrong
    // about the operation, the dunder and the cause all at once
    let fixture = Fixture::new("prepared_namespace", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_in_grows_called_from(&mut debuggee, &fixture, "    made = grows(1)");

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(reason, Unrestartable::NamespaceIsNotADict { .. }),
        "{reason:?}"
    );
    assert!(said.contains("OrderedDict"), "said {said}");
    // the line reads `grows` through the mapping **and** stores `made` through
    // it, and the span meets the read first. both are true of the line; what
    // matters is that the sentence describes the one it found
    //
    // **not** `contains("caller")`: the `NotRestartable` preamble always says
    // "rewinds its caller to the call", so that assertion is satisfied by
    // boilerplate and passes a message that names the wrong frame entirely.
    // the clause itself, which only the caller-side arm writes
    assert!(
        said.contains("the caller's line"),
        "the refusal has to be about the caller's line, not the frame's: {said}"
    );
    // and it is the frame's own locals rather than the globals pair — the
    // globals and builtins here are both plain dicts, and saying otherwise sent
    // somebody looking at the wrong mapping
    assert!(
        said.contains("own locals") && said.contains("__prepare__"),
        "the mapping in play is `f_locals`: {said}"
    );
    assert!(
        !said.contains("builtins"),
        "the globals pair is not what is wrong here: {said}"
    );
    assert!(
        said.contains("__getitem__") || said.contains("__missing__"),
        "the refusal names a read, so it has to name a read's dunder: {said}"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_frame_entered_from_something_that_is_not_a_call_is_refused_rather_than_asserted() {
    // a frame is not always entered from a `CALL`. **this checks `LOAD_ATTR`
    // and nothing else** — a comprehension over a custom iterator was measured
    // to stop the caller at `FOR_ITER`, and `SEND` is the same shape for an
    // `await`, and neither is driven from here. saying so rather than listing
    // them in a comment the body does not reach
    //
    // it was an assertion, on an invariant that is simply false — so an ordinary
    // shape panicked inside the debuggee and the engine lost the agent. it is a
    // refusal, and it is checked **before** anything counts the calls
    let fixture = Fixture::new("not_a_call", PROGRAM);
    let mut debuggee = launch(&fixture);
    let inside = line_of(PROGRAM, "        held = 1");

    held_at(&mut debuggee, &fixture.path(), inside);
    let stack = debuggee.the_stack(None).expect("the stack was answered");
    assert_eq!(stack.frames[0].name(), "Watched.attr");

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            reason,
            Unrestartable::NotEnteredByACall { ref opcode, .. } if opcode == "LOAD_ATTR"
        ),
        "{reason:?}"
    );
    assert!(said.contains("`LOAD_ATTR`"), "said {said}");
    // **not** `SomethingElseOnTheLine`, which is what this was. that variant's
    // sentence ends "put the call on a line of its own, with its arguments
    // already in locals" — and `got = obj.attr` already looks like that, about a
    // call that does not exist. a property getter is not entered by a call at
    // all, so there is no rearrangement of the source that makes it restartable
    assert!(
        !said.contains("put the call on a line of its own"),
        "the remedy for a shared line is not the remedy for this: {said}"
    );
    assert!(
        said.contains("set next statement"),
        "it has to say what would actually help: {said}"
    );
    // and it must not claim nothing can be written differently — the same getter
    // reached as a call **is** restartable, which is measured and said
    assert!(
        !said.contains("no way to write the line") && said.contains("fget"),
        "the shape that does restart has to be named: {said}"
    );
    // and the remedy it names has to be one that works. an earlier version
    // offered `type(obj).attr.fget(obj)`, which is **two calls** and is refused
    // as `MoreThanOneCall` — advice that swaps one refusal for another
    assert!(
        said.contains("hoisted to a line of its own"),
        "the working remedy is hoisting the `fget`, not calling it inline: {said}"
    );
    assert!(
        !said.contains("`type(obj).attr.fget(obj)` restarts"),
        "that line is two calls: {said}"
    );

    // and the program is still alive, which is the half the assertion took away
    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    assert!(
        said.contains("finished") || said.contains("property"),
        "{said}"
    );
}

#[test]
fn a_call_that_reads_back_the_name_it_stores_into_is_refused() {
    // `seed = grows(seed)`. the forced return really returns, so `seed` holds
    // bpd's value **before** the rewind — and the re-executed line would then
    // call `grows` with what bpd put there rather than with the program's value.
    // that is the same defect `f(*args)` is refused for: the second call is not
    // the call that was restarted
    let fixture = Fixture::new("reads_what_it_stores", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_in_grows_called_from(&mut debuggee, &fixture, "    seed = grows(seed)");

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            reason,
            Unrestartable::TheCallReadsWhatItStores { ref name, .. } if name == "seed"
        ),
        "{reason:?}"
    );
    assert!(said.contains("`seed`"), "said {said}");
    assert!(
        said.contains("a name the call does not read"),
        "the refusal has to say what would work, and said {said}"
    );
    // and it has to hold for the case bpd's own value is **not** involved,
    // which is the half this guard was missing: `a, x = f(x), other` reads back
    // the program's own `other` and changes the call just as completely
    assert!(
        said.contains("it does not matter whose value the tail puts there"),
        "the reason is about the read-then-write, not about whose value lands \
         in the name: {said}"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_breakpoint_the_forced_return_passes_over_is_named_rather_than_skipped_quietly() {
    // the forced return **executes** its exit line — the loads and the return
    // run — and no `LINE` event is delivered for it, because it is a jump's
    // destination. so a breakpoint there does not fire for a line the program
    // really ran, and the answer has to say so or the user watches their own
    // breakpoint be passed over
    let fixture = Fixture::new("passed_over", PROGRAM);
    let mut debuggee = launch(&fixture);
    let inside = line_of(PROGRAM, "    total = value + 1");
    let exit_line = line_of(PROGRAM, "    return total");

    // both armed: the one that stops us, and the one on the line the forced
    // return will run
    let armed = vec![
        SourceBreakpoint::at(1, fixture.path(), inside),
        SourceBreakpoint::at(2, fixture.path(), exit_line),
    ];
    debuggee
        .set_breakpoints(armed)
        .expect("the breakpoint request was answered");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        other => panic!("expected a breakpoint stop, got {other:?}"),
    }

    let frame = top(&mut debuggee);
    let restarted = debuggee
        .restart_frame(frame)
        .expect("the restart was arranged");
    let restarting = arranged(&restarted);
    assert!(
        restarting.unannounced.contains(&2),
        "the breakpoint on the exit line will not fire and has to be named, and \
         it said {restarting:?}"
    );
}

#[test]
fn a_chained_assignment_names_every_slot_the_forced_return_lands_in() {
    // `first = second = grows(1)` writes **two** names, and the instruction that
    // does it carries them as a tuple. reading only the string form of the
    // argument dropped both, so the answer said nothing was disturbed while the
    // forced return's value sat in each of them
    let fixture = Fixture::new("chained_stores", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_in_grows_called_from(&mut debuggee, &fixture, "    first = second = grows(1)");

    let frame = top(&mut debuggee);
    let restarted = debuggee
        .restart_frame(frame)
        .expect("the restart was arranged");
    let restarting = arranged(&restarted);
    let mut named = restarting.disturbed.clone();
    named.sort();
    assert_eq!(
        named,
        vec!["first".to_string(), "second".to_string()],
        "both slots hold the forced return until the restarted call finishes, \
         and it said {restarting:?}"
    );

    landed(&mut debuggee);
    to_exit(&mut debuggee);
}

#[test]
fn a_return_whose_line_has_a_dirty_range_before_it_is_refused() {
    // accepting an exit line because *some* range of it is loads and a return
    // puts the frame at a range that is not. which range a jump lands on is
    // cpython's choice — by stack depth, not by offset — so the analysis jumps
    // first and reads `f_lasti` back before deciding
    //
    // `return (side_effect(99) if total else None)` is the shape: one range of
    // that line is the call, another is the return. an earlier version answered
    // `Arranged` and the jump ran `side_effect(99)` — a side effect the debugger
    // itself made, before the restart. that is a false belief rather than an
    // error, which is the failure class this project exists against
    let fixture = Fixture::new("dirty_exit_range", PROGRAM);
    let mut debuggee = launch(&fixture);
    let inside = line_of(PROGRAM, "    total = flag");

    held_at(&mut debuggee, &fixture.path(), inside);
    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(matches!(reason, Unrestartable::NoCleanExit), "{reason:?}");
    assert!(said.contains("`charged`"), "said {said}");

    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    // the program's own record, which is the half that matters. `charged(1)`
    // runs once at module level, so one is the whole truth — a second is the
    // debugger having made a call of the program's
    assert_eq!(
        said.matches("('side_effect', 99)").count(),
        1,
        "the refused restart ran a call of the program's: {said}"
    );
}

#[test]
fn a_call_split_over_source_lines_sees_what_is_on_the_argument_line() {
    // a call past about 88 columns is what every formatter produces, and its
    // argument is attributed to the **argument's** line rather than the call's.
    // so reading "every range of the call's line" misses the `LOAD_ATTR`
    // entirely — the span from the destination to the call is what holds it
    let fixture = Fixture::new("split_call", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_in_grows_called_from(&mut debuggee, &fixture, "    fetched = grows(");

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            &reason,
            Unrestartable::SomethingElseOnTheLine { opcode, .. } if opcode == "LOAD_ATTR"
        ),
        "{reason:?}"
    );
    assert!(said.contains("`LOAD_ATTR`"), "said {said}");

    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    // its own getter, so the count is unambiguous: `wrapped` is called once and
    // the restart was refused. an `Arranged` here re-runs the getter, and the
    // getter is code of the program
    assert_eq!(
        said.matches("'split_property'").count(),
        1,
        "the property getter ran more than the program asked for: {said}"
    );
}

#[test]
fn a_call_inside_a_finally_is_restarted_rather_than_called_two_calls() {
    // cpython **duplicates** a `finally` body, so one call inside one yields two
    // `co_lines` ranges each holding a `CALL`. counting across every range of
    // the line called that two calls and refused — a false statement in a
    // refusal, which is the same category of defect as a false `Arranged`
    //
    // the span from the destination to the call holds one copy and one call
    let fixture = Fixture::new("finally_body", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_in_grows_called_from(&mut debuggee, &fixture, "        got = grows(2)");

    let frame = top(&mut debuggee);
    let restarted = debuggee
        .restart_frame(frame)
        .expect("a call inside a `finally` makes one call and is restartable");
    let restarting = arranged(&restarted);
    assert_eq!(restarting.caller.function, "protected");
    assert_eq!(restarting.disturbed, vec!["got".to_string()]);

    landed(&mut debuggee);
    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    assert!(
        said.contains("('grows', 2), ('grows', 2)"),
        "the call inside the `finally` did not run again with fresh locals: \
         {said}"
    );
}

/// stop the program on `line` of a file that is not the fixture's own script
///
/// the breakpoint is set before the file exists, so it binds when the file is
/// loaded rather than when it is asked for — which is the ordinary shape for
/// anything a program `exec`s
fn held_at_when_loaded(debuggee: &mut Debuggee, file: &Path, line: u32) {
    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, file, line)])
        .expect("the breakpoint request was answered");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        other => panic!("expected a breakpoint stop, got {other:?}"),
    }
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
}

#[test]
fn a_load_global_on_the_exit_line_is_refused_when_the_globals_are_not_a_plain_dict() {
    // the allow list justified `LOAD_GLOBAL` as "a load cannot run program
    // code". that is false: when the frame's globals mapping is a **dict
    // subclass**, `LOAD_GLOBAL` falls off cpython's exact-dict fast path and
    // calls `PyObject_GetItem` — running `__missing__`, which is the program's
    //
    // `call_line` already checks this for `STORE_NAME`. there was no equivalent
    // for a read, so a callee exiting through `return LATE` ran the program's
    // `__missing__` during the forced exit
    let fixture = Fixture::new("subclassed_globals", PROGRAM);
    let inner = fixture
        .path()
        .parent()
        .expect("a fixture is in a directory")
        .join("inner.py");
    // written before the launch, so the breakpoint binds when it is set rather
    // than waiting for a file that does not exist yet
    std::fs::write(
        &inner,
        "def callee(seed):\n    \
         RAN.append((\"callee\", seed))\n    \
         held = seed\n    \
         return LATE\n\n\n\
         def outer():\n    \
         got = callee(1)\n    \
         RAN.append((\"outer\", got))\n    \
         return got\n",
    )
    .expect("the second module was written");
    let mut debuggee = launch(&fixture);

    held_at_when_loaded(&mut debuggee, &inner, 3);
    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(reason, Unrestartable::NamespaceIsNotADict { .. }),
        "{reason:?}"
    );
    assert!(said.contains("Missing"), "said {said}");

    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    // once, from the program's own run. a second is the debugger having run the
    // program's `__missing__` on its way out
    assert_eq!(
        said.matches("('missing', 'LATE')").count(),
        1,
        "the forced exit ran the program's `__missing__`: {said}"
    );
}

#[test]
fn the_exception_copy_of_a_finally_is_refused_with_a_reason_that_is_true() {
    // cpython duplicates a `finally` body. on the exception path the caller is
    // in the **second** copy, and a span that starts at the first copy swallows
    // both — which was reported as `NothingRunsAfterTheCall`, telling the user
    // that nothing runs after the call and to bind the result to a name. three
    // statements run after it and the result is already bound to a name
    //
    // the shape is genuinely restartable and bpd cannot tell which copy a rewind
    // would land in, so it stays refused — with a reason that is true
    let fixture = Fixture::new("finally_exception_copy", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_in_grows_called_from(&mut debuggee, &fixture, "        caught_got = grows(2)");

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(reason, Unrestartable::CopiedLine { .. }),
        "{reason:?}"
    );
    assert!(
        !said.contains("binding the result to a name"),
        "the refusal offered a remedy the caller already has, and said {said}"
    );
    // it may **offer** a `finally` as one cause among several — the guard fires
    // on `co_lines` runs, and a duplicated `finally` is only one of the things
    // that produces them — but it must not assert this line is in one, and it
    // must not offer a remedy that only a `finally` has
    assert!(said.contains("2 separate runs"), "said {said}");
    assert!(
        !said.contains("move it out of the `finally`"),
        "the refusal cannot know this line is in a `finally`, and said {said}"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_load_global_that_falls_through_to_exotic_builtins_is_refused() {
    // the gate read `f_globals` and `f_locals`. cpython's `LOAD_GLOBAL` fast
    // path needs **both** globals and builtins to be exact dicts, so a plain
    // dict globals with a dict-subclass `__builtins__` takes the slow path and
    // runs the builtins mapping's `__missing__` for any name globals does not
    // hold — measured: the forced exit through `return LATE` ran it while the
    // answer said `Arranged`
    let fixture = Fixture::new("subclassed_builtins", PROGRAM);
    let inner = fixture
        .path()
        .parent()
        .expect("a fixture is in a directory")
        .join("inner_builtins.py");
    std::fs::write(
        &inner,
        "def callee(seed):\n    \
         RAN.append((\"callee\", seed))\n    \
         held = seed\n    \
         return LATE\n\n\n\
         def outer():\n    \
         got = callee(1)\n    \
         RAN.append((\"outer\", got))\n    \
         return got\n",
    )
    .expect("the second module was written");
    let mut debuggee = launch(&fixture);

    held_at_when_loaded(&mut debuggee, &inner, 3);
    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    // the **line**, not just the type name. this producer is about a line of the
    // restarted frame, and it named the first line a `LOAD_GLOBAL` appeared on —
    // a call line that could never have been an exit — rather than the `return`
    // the user has to fix
    assert!(
        matches!(reason, Unrestartable::NamespaceIsNotADict { line: 4, .. }),
        "{reason:?}"
    );
    assert!(said.contains("Missing"), "said {said}");
    // and the **sentence**. one variant with two producers carried wording
    // written for the other one, and asserting on the type name alone is
    // exactly what let four false claims through
    for wrong in [
        "the caller's line",
        "stores into",
        "__setitem__",
        "__prepare__",
    ] {
        assert!(
            !said.contains(wrong),
            "the refusal describes the other producer — {wrong:?} in {said}"
        );
    }
    assert!(
        said.contains("__missing__") || said.contains("__getitem__"),
        "the refusal has to name the dunder that really runs, and said {said}"
    );

    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    // once, from the program's own run
    assert_eq!(
        said.matches("('missing', 'LATE')").count(),
        1,
        "the forced exit ran the program's `__missing__` through its builtins: \
         {said}"
    );
}

#[test]
fn an_exit_line_that_loads_an_unbound_cell_is_refused() {
    // `EXITING` justified `LOAD_DEREF` with "cpython binds every unbound local
    // to `None` as part of the move, so even the checked load cannot raise".
    // measured on 3.13, 3.14 and 3.14t: the move binds unbound **locals** and
    // leaves unbound **cells** alone, so `return cell` raises
    //
    // and it produced a false **success**, not merely a false `Arranged`: the
    // injected `UnboundLocalError` was caught by the caller's own handler, the
    // rewind fired on that handler's first line event, and bpd reported
    // `Restarted`
    let fixture = Fixture::new("unbound_cell", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    held = seed"),
    );

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    // named, not "this function has no clean exit line" — it plainly has a
    // `return`, and the thing in the way is which name that `return` reads
    assert!(
        matches!(
            &reason,
            Unrestartable::ExitWouldRaise { name, .. } if name == "cell"
        ),
        "{reason:?}"
    );
    assert!(said.contains("`closes`"), "said {said}");
    assert!(said.contains("`cell`"), "said {said}");

    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    assert!(
        !said.contains("caller caught"),
        "the forced exit raised into the program, and its own handler swallowed \
         the evidence: {said}"
    );
    assert!(
        said.contains("('calls_closes', 2)"),
        "the program did not get its own answer: {said}"
    );
}

#[test]
fn a_frame_inside_a_finally_handling_an_exception_is_forced_out_cleanly() {
    // the hardest position to be forced out of, and the one that broke an
    // earlier mechanism. `stranded` is inside a `with`, inside a `finally`,
    // while a `ValueError` is being handled — so the frame's value stack holds
    // the context manager **and** an `Except` entry, and cpython duplicates the
    // `finally` body so the same source line exists twice in the bytecode
    //
    // the exit is an offset now, at abstract stack depth zero, and
    // `compatible_stack` accepts an empty target from anywhere — so cpython
    // unwinds all of it, hands the exception back to `tstate->exc_info` itself,
    // and the copies stop mattering. what the earlier mechanism did here was put
    // the frame back onto the *other* copy of the same line and report that
    // nothing had moved, after which the debuggee died
    //
    // asserted on the program's own record: the context manager is entered
    // twice — once before the forced exit, once by the restarted call — and left
    // once, because a forced return is a jump out of the block
    let fixture = Fixture::new("stranded_frame", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "            marker = 1"),
    );

    let frame = top(&mut debuggee);
    let restarted = debuggee
        .restart_frame(frame)
        .expect("the restart was arranged");
    assert_eq!(arranged(&restarted).frame.function, "stranded");

    landed(&mut debuggee);
    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    assert_eq!(
        said.matches("'cm_enter'").count(),
        2,
        "the restarted call did not open the block again: {said}"
    );
    assert_eq!(
        said.matches("'cm_exit'").count(),
        1,
        "the forced return is a jump out of the `with`, so the first block's \
         `__exit__` does not run: {said}"
    );
    assert!(
        said.contains("'after_stranded'"),
        "the program did not survive the restart: {said}"
    );
}

#[test]
fn an_exit_line_that_loads_a_name_bound_nowhere_is_refused() {
    // the same question as the unbound cell, asked of `LOAD_GLOBAL` and not
    // asked before. with a perfectly plain globals **and** a perfectly plain
    // builtins, `LOAD_GLOBAL` still raises `NameError` when the name is in
    // neither — so a forced exit through `return LATE_AND_UNDEFINED` injects an
    // exception into the program
    //
    // measured: bpd answered `Arranged`, the caller died of an exception bpd
    // made, and the stop that followed said `CallerLeft` — which is untrue twice
    // over, because the caller did not leave, bpd killed it
    let fixture = Fixture::new("late_global", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    kept = seed"),
    );

    let frame = top(&mut debuggee);
    // the safe candidate is taken, not the one that would raise. `return
    // LATE_AND_UNDEFINED` comes **first** in `co_lines` order, so it is the one
    // that was tried first and the one that raised into the program
    let restarted = debuggee
        .restart_frame(frame)
        .expect("this frame has a return that does not raise");
    let restarting = arranged(&restarted);
    assert_eq!(
        restarting.exit_line,
        line_of(PROGRAM, "    return kept"),
        "the frame was forced out through a `return` of a name bound nowhere: \
         {restarting:?}"
    );

    landed(&mut debuggee);
    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    assert!(
        !said.contains("('late blew up'"),
        "the forced exit raised into the program: {said}"
    );
    assert_eq!(
        said.matches("('late_global', 1)").count(),
        2,
        "the frame did not run again: {said}"
    );
}

#[test]
fn a_caller_that_catches_what_the_forced_exit_raised_is_not_reported_as_a_success() {
    // the worse half of the same defect, and the one that looks clean from
    // outside. with the call inside a `try/except BaseException`, the injected
    // `NameError` is caught by the program's own handler, the rewind fires on
    // that handler's **first** line event, and bpd reports `Restarted` with the
    // program exiting 0 — while the handler's own `RAN.append` never ran
    //
    // the interpreter's stderr is where it showed: `assigning None to 2 unbound
    // locals` reported at the `except` line, which is the rewind's assignment
    // being made from inside the handler
    let fixture = Fixture::new("caught_late_global", PROGRAM);
    let mut debuggee = launch(&fixture);
    // the second call, from the caller that has a handler
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    kept = seed"),
    );
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    kept = seed"),
    );

    let frame = top(&mut debuggee);
    let restarted = debuggee
        .restart_frame(frame)
        .expect("this frame has a return that does not raise");
    arranged(&restarted);

    landed(&mut debuggee);
    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    assert!(
        !said.contains("('late caught'"),
        "the program's own handler ran for an exception bpd made, and the rewind \
         then fired inside that handler: {said}"
    );
    assert!(
        said.contains("('caught_late_global', 2)"),
        "the program did not reach its own answer: {said}"
    );
}

#[test]
fn the_def_line_is_never_the_line_a_frame_is_forced_out_through() {
    // `RESUME` sits on the **`def`** line, not the body's — so the walk from the
    // def line's start crosses it and carries on into the body, and for a
    // function whose body is immediately a clean return that walk reaches one.
    // the def line then comes **first** in `co_lines` order, so it was the
    // candidate tried first
    //
    // measured on all three: cpython accepts the jump, lands at offset 0, and
    // re-executing `RESUME` fires a **second `PY_START`** for a frame that was
    // already started — which is a lie to every consumer of that event, one of
    // them being the restart's own landing watch
    let fixture = Fixture::new("def_line", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "def one_liner(seed):") + 1,
    );

    let frame = top(&mut debuggee);
    let restarted = debuggee
        .restart_frame(frame)
        .expect("a one-line function has a clean exit: its own return");
    let restarting = arranged(&restarted);
    assert_eq!(
        restarting.exit_line,
        line_of(PROGRAM, "def one_liner(seed):") + 1,
        "the frame was forced out through the `def` line, which re-executes \
         `RESUME`: {restarting:?}"
    );

    landed(&mut debuggee);
    to_exit(&mut debuggee);
}

#[test]
fn a_frame_whose_every_exit_would_raise_is_told_which_name() {
    // when one `return` reads a name the frame holds nothing for, another is
    // taken. when **every** one does, the refusal has to say so — answering
    // `NoCleanExit` would send somebody looking for a `return` they can see the
    // function already has
    let fixture = Fixture::new("only_late", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    stashed = seed"),
    );

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            &reason,
            Unrestartable::ExitWouldRaise { name, .. } if name == "LATE_AND_UNDEFINED"
        ),
        "{reason:?}"
    );
    assert!(said.contains("`LATE_AND_UNDEFINED`"), "said {said}");
    assert!(
        said.contains("raises"),
        "the refusal has to say what a read like that does, and said {said}"
    );
    assert!(
        !said.contains("no clean exit"),
        "the function has a `return`, and the refusal implied it does not: {said}"
    );

    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    // once, from the program's own run — the refusal did not add one
    assert_eq!(
        said.matches("'only_late raised'").count(),
        1,
        "the refused restart raised into the program: {said}"
    );
}

#[test]
fn a_name_bound_nowhere_on_the_callers_line_is_refused_as_the_callers() {
    // the same one-variant-two-producers shape as the namespace refusal, on the
    // other variant. `packed = (grows(seed), NEVER_BOUND)` puts a `LOAD_GLOBAL`
    // of a name bound nowhere on the **caller's** line — `BUILD_TUPLE` and
    // `LOAD_GLOBAL` are both on the allow list — and re-executing that line
    // would raise
    //
    // it must be described as the caller's line, not as "every line it could be
    // forced out through"
    let fixture = Fixture::new("tupled_caller", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_in_grows_called_from(
        &mut debuggee,
        &fixture,
        "    packed = (grows(seed), NEVER_BOUND)",
    );

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            &reason,
            Unrestartable::ExitWouldRaise { name, .. } if name == "NEVER_BOUND"
        ),
        "{reason:?}"
    );
    assert!(said.contains("`NEVER_BOUND`"), "said {said}");
    // **not** `contains("caller")`: the `NotRestartable` preamble always says
    // "rewinds its caller to the call", so that assertion is satisfied by
    // boilerplate and a mutant that attributed the read to the frame passed it
    assert!(
        said.contains(&format!(
            "the caller's line {} reads `NEVER_BOUND`",
            line_of(PROGRAM, "    packed = (grows(seed), NEVER_BOUND)")
        )),
        "the refusal has to name whose line, which line, and which name: {said}"
    );
    assert!(
        !said.contains("forced out through"),
        "the refusal describes the other producer: {said}"
    );

    to_exit(&mut debuggee);
}

#[test]
fn what_a_restart_says_about_cleanup_admits_the_one_that_does_run() {
    // `told()` says the forced return runs **no** cleanup and then names
    // `__exit__` and `finally`. a `__del__` is cleanup, it is not either of
    // those, and it does run: a local of the forced-out frame is finalised when
    // that frame dies — after the client has been told `Arranged`, and at a
    // moment the program never reached
    //
    // nothing in the analysis could refuse this. it is inherent to forcing any
    // frame out, which makes it a disclosure rather than a defect — and the
    // sentence was absolute
    let fixture = Fixture::new("finalised", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    summed = seed + 1"),
    );

    let frame = top(&mut debuggee);
    let restarted = debuggee
        .restart_frame(frame)
        .expect("the restart was arranged");
    let restarting = arranged(&restarted);

    let told = restarting.told().join(" | ");
    assert!(
        told.contains("__del__"),
        "the one cleanup that does run is not disclosed: {told}"
    );
    assert!(
        !told.contains("runs no cleanup"),
        "the sentence is absolute and a `__del__` falsifies it: {told}"
    );

    landed(&mut debuggee);
    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    // twice: once for the frame bpd forced out, once for the frame the restart
    // built. the first is the debugger's doing
    assert_eq!(
        said.matches("('finalised', 'held')").count(),
        2,
        "the forced-out frame's local was not finalised, so this test no longer \
         measures what it says: {said}"
    );
}

#[test]
fn what_a_restart_says_about_cleanup_does_not_name_a_closed_list() {
    // the replacement for "runs no cleanup" was "the one cleanup that **does**
    // run is `__del__`", which is the same mistake one clause over: an absolute
    // claim about a category nobody enumerated
    //
    // a suspended generator held by the forced-out frame is finalised when that
    // frame dies, which throws `GeneratorExit` into it and runs its `finally` —
    // so a `finally` of the program does run, while the sentence beside it says
    // none does
    let fixture = Fixture::new("generator_finally", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    added = seed + started"),
    );

    let frame = top(&mut debuggee);
    let restarted = debuggee
        .restart_frame(frame)
        .expect("the restart was arranged");
    let told = arranged(&restarted).told().join(" | ");
    assert!(
        !told.contains("the one cleanup"),
        "the sentence names a closed list of what runs, and a generator's \
         `finally` is not on it: {told}"
    );
    assert!(
        told.contains("last holder"),
        "what bpd knows is that anything the frame held alone is finalised — \
         say that rather than enumerating: {told}"
    );

    landed(&mut debuggee);
    to_exit(&mut debuggee);
    let said = recorded(&fixture);
    // twice: the generator the forced-out frame held, and the one the restarted
    // call built. the first `finally` is the debugger's doing
    assert_eq!(
        said.matches("'generator finally'").count(),
        2,
        "the forced-out frame's generator was not finalised, so this test no \
         longer measures what it says: {said}"
    );
}

#[test]
fn a_frame_whose_exits_are_blocked_differently_does_not_claim_they_all_read_it() {
    // `ExitWouldRaise { TheFrame }` records the **first** blocked line it meets,
    // exactly as the namespace refusal does — so "every line it could be forced
    // out through reads `{name}`" was a claim the recording shape cannot support
    //
    // `mixed_exits` has two returns: one blocked by a call, one by a name bound
    // nowhere. the first reads nothing the frame lacks
    let fixture = Fixture::new("mixed_exits", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    settled = seed"),
    );

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            &reason,
            Unrestartable::ExitWouldRaise { name, .. } if name == "MISSING_ENTIRELY"
        ),
        "{reason:?}"
    );
    assert!(
        !said.contains("every line"),
        "one of the two returns does not read that name at all: {said}"
    );
    assert!(
        said.contains("one of the lines"),
        "the refusal has to say which line it is talking about: {said}"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_caller_whose_tail_reads_an_unbound_local_is_refused_rather_than_abandoned() {
    // `LOAD_FAST_CHECK` is on `BESIDE_THE_CALL`, and it raises
    // `UnboundLocalError` for a slot that holds nothing. the exit line is safe
    // from it — `frame_lineno_set_impl` walks `co_nlocalsplus` and fills every
    // NULL slot with `None`, so the move binds them — but the caller's tail runs
    // **before** anything moves the caller: the forced return lands, the rest of
    // the line runs with it, and only then does the line event fire that the
    // rewind is made from
    //
    // so the two lists cannot share one answer about a checked load, and the
    // asymmetry is the whole of this test. before it, `packed = (grows(1),
    // spare)` with `spare` bound only on a branch answered `Arranged` and then
    // abandoned with `CallerLeft` after the tail raised — a restart that was
    // never possible, discovered by attempting it
    let fixture = Fixture::new("unbound_fast", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    step = value + 100"),
    );

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            &reason,
            Unrestartable::ExitWouldRaise { name, whose, .. }
                if name == "spare" && matches!(whose, Whose::TheCaller)
        ),
        "the caller's tail is what reads it, and the name is what to act on: \
         {reason:?}"
    );
    assert!(said.contains("`spare`"), "said {said}");

    to_exit(&mut debuggee);

    // the refusal came before anything moved, so the program has to have run
    // exactly as it would have with no debugger attached — and that is checked
    // against a real undebugged run rather than asserted. it has to be a
    // baseline: `maybe_unbound(0)` raises `UnboundLocalError` on its own, so
    // "the tail raised" is true either way and says nothing about bpd. what
    // separates the two is whether `tail_callee` was forced out on the way
    let bare = Fixture::new("unbound_fast_bare", PROGRAM);
    let run = bare.run(
        bpd_test::agent::matching_interpreter(),
        bpd_test::debuggee::Form::Script,
        &[],
    );
    assert!(run.success, "the fixture runs on its own: {}", run.stderr);
    assert_eq!(
        recorded(&fixture),
        recorded(&bare),
        "a refused restart leaves the program's own record untouched"
    );
}

#[test]
fn a_fused_store_and_load_names_only_the_slot_it_writes() {
    // `dis` gives a fused instruction a tuple `argval`, and the two halves are
    // not the same operation: `STORE_FAST_LOAD_FAST ('holds', 'spare')` writes
    // `holds` and **reads** `spare`. counting both as writes put the caller's
    // own parameter in `disturbed` — a slot the debugger never wrote, reported
    // as holding "a value the program never computed", which is the one field
    // whose whole purpose is to say which slots a client is misreading
    let fixture = Fixture::new("fused", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    lifted = value + 100"),
    );

    let frame = top(&mut debuggee);
    let arranged = match debuggee
        .restart_frame(frame)
        .expect("the restart was arranged")
    {
        Restarted::Arranged(restarting) => restarting,
        other @ Restarted::Refused { .. } => {
            panic!("expected an arranged restart, got {other:?}")
        }
    };
    assert_eq!(
        arranged.disturbed,
        vec!["holds".to_string()],
        "`echo` is stored by its own `STORE_FAST` after the fused pair and \
         `spare` is only read by it — neither takes the forced return's value"
    );

    landed(&mut debuggee);
    to_exit(&mut debuggee);
    let ran = recorded(&fixture);
    assert!(
        ran.contains("('fused_store', 102, 9)"),
        "the caller's own parameter held 9 throughout: {ran}"
    );
}

#[test]
fn a_name_the_call_reads_and_a_fused_load_names_is_not_a_name_the_line_stores() {
    // `TheCallReadsWhatItStores` compares the call's reads against `disturbed`,
    // so a name wrongly in `disturbed` becomes a refusal of a restart that is
    // sound — and one whose advice cannot be followed: it told the user to
    // "store the result under a different name" for a line that never stores
    // into the name it complained about
    //
    // `kept = fused_callee(spare); mirror = spare` reads `spare` before the call
    // and stores into `kept` and `mirror`. it stores into `spare` nowhere
    let fixture = Fixture::new("fused_reads", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    raised = value + 100"),
    );

    let frame = top(&mut debuggee);
    let arranged = match debuggee
        .restart_frame(frame)
        .expect("the restart was arranged")
    {
        Restarted::Arranged(restarting) => restarting,
        other @ Restarted::Refused { .. } => {
            panic!("the line stores into neither name the call reads: {other:?}")
        }
    };
    assert_eq!(arranged.disturbed, vec!["kept".to_string()]);

    landed(&mut debuggee);
    to_exit(&mut debuggee);
    let ran = recorded(&fixture);
    assert!(
        ran.contains("('fused_reads', 110, 9)"),
        "the restarted call ran again with the program's own `spare`: {ran}"
    );
}

#[test]
fn a_call_split_over_lines_is_not_told_it_is_in_a_finally() {
    // `CopiedLine` counts `co_lines` runs, and a duplicated `finally` body is
    // only one of the things that makes more than one. sweeping both stdlibs for
    // call sites that reach this guard, 3723 of 4161 on 3.13 and 3758 of 4219 on
    // 3.14 are not in a `finally` at all — so the sentence that named one as the
    // cause, and offered "move it out of the `finally`" as the remedy, was false
    // for the majority of the cases that reached it and impossible to act on
    let fixture = Fixture::new("split_lines", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    grown = value + 100"),
    );

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(reason, Unrestartable::CopiedLine { .. }),
        "{reason:?}"
    );
    assert!(
        !said.contains("move it out of the `finally`"),
        "there is no `finally` in this program: {said}"
    );
    assert!(
        said.contains("split over several source lines"),
        "the cause it can see has to be among the ones it names: {said}"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_starred_call_is_not_told_to_do_what_it_already_does() {
    // `CALL_FUNCTION_EX` is the **call**, not something sharing its line, so the
    // generic remedy — "put the call on a line of its own, with its arguments
    // already in locals" — describes this program exactly and refuses it anyway.
    // advice a user has already followed is worse than none: it reads as bpd
    // having misunderstood the line
    let fixture = Fixture::new("starred", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    hoisted = value + 100"),
    );

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            &reason,
            Unrestartable::SomethingElseOnTheLine { opcode, .. }
                if opcode == "CALL_FUNCTION_EX"
        ),
        "{reason:?}"
    );
    assert!(
        !said.contains("put the call on a line of its own"),
        "the call is already on a line of its own with its argument in a local: \
         {said}"
    );
    assert!(
        said.contains("iterating") && said.contains("positionally"),
        "it has to say why, and what would actually help: {said}"
    );

    to_exit(&mut debuggee);
}

#[test]
fn a_container_built_from_the_forced_return_names_the_slot_it_lands_in() {
    // `BUILD_LIST` and `BUILD_TUPLE` were modelled as destroying the call's
    // value — "it stops being reachable by a name at this point". it does not:
    // the store right after the container is exactly the name that reaches it.
    // so `box = [f(), spare]` answered `Arranged` with `disturbed` empty while
    // `box` held a list bpd had built out of its own forced `None`, and
    // `told()` emitted no note at all — a silent wrong belief in the one field
    // whose purpose is to prevent it
    let fixture = Fixture::new("boxed", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    boxed_step = value + 100"),
    );

    let frame = top(&mut debuggee);
    let arranged = match debuggee
        .restart_frame(frame)
        .expect("the restart was arranged")
    {
        Restarted::Arranged(restarting) => restarting,
        other @ Restarted::Refused { .. } => panic!("the line is one clean call: {other:?}"),
    };
    assert_eq!(
        arranged.disturbed,
        vec!["box".to_string()],
        "the container holds the forced return, and the name holds the container"
    );

    landed(&mut debuggee);
    to_exit(&mut debuggee);
}

#[test]
fn a_value_the_caller_pushed_before_the_call_is_not_a_span_bpd_gives_up_on() {
    // the walk seeds its stack with the call's return value and nothing else,
    // so anything the caller had already pushed sat below the modelled floor and
    // any pop reaching it underflowed. `first, second = spare, f(1)` does
    // exactly that — `STORE_FAST_STORE_FAST` pops twice — and was refused as
    // `SpanNotUnderstood`, whose doc blamed the allow list and the walk having
    // drifted apart. nothing had drifted: a value below the floor is one the
    // call did not produce, which is an answer rather than a gap
    let fixture = Fixture::new("under", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    under_step = value + 100"),
    );

    let frame = top(&mut debuggee);
    let arranged = match debuggee
        .restart_frame(frame)
        .expect("the restart was arranged")
    {
        Restarted::Arranged(restarting) => restarting,
        other @ Restarted::Refused { .. } => panic!("this is one clean call: {other:?}"),
    };
    // `second` takes the call's value; `first` takes `spare`, which the caller
    // pushed before the call ever ran
    assert_eq!(arranged.disturbed, vec!["second".to_string()]);

    landed(&mut debuggee);
    to_exit(&mut debuggee);
}

#[test]
fn the_disclosure_does_not_undercount_what_reading_the_bytecode_imports() {
    // the sentence both front ends give on the refusal path says "more than a
    // dozen modules". an earlier version named three — `dis`, `opcode` and
    // `_opcode` — and the real delta is 13 on 3.13 and 15 on 3.14, so a reader
    // told three had a number to act on and it was wrong
    //
    // measured against the interpreter this build's agent is for, so it cannot
    // drift with a new one
    let counted = "import sys\nbefore = set(sys.modules)\nimport dis\n\
                   print(len(set(sys.modules) - before))\n";
    let fixture = Fixture::new("counts_dis", counted);
    let run = fixture.run(
        bpd_test::agent::matching_interpreter(),
        bpd_test::debuggee::Form::Script,
        &[],
    );
    assert!(run.success, "the probe runs: {}", run.stderr);
    let gained: usize = run
        .stdout
        .trim()
        .parse()
        .expect("the probe prints one number");

    assert!(
        gained > 12,
        "the disclosure claims more than a dozen and `dis` brought {gained}"
    );
    assert!(
        bpd_core::WHAT_READING_THE_BYTECODE_COSTS.contains("more than a dozen"),
        "the claim this pins has to be the one the sentence makes"
    );
    // and it must not go back to naming a closed set
    assert!(
        !bpd_core::WHAT_READING_THE_BYTECODE_COSTS.contains("`opcode`"),
        "a list is what was wrong with it: {}",
        bpd_core::WHAT_READING_THE_BYTECODE_COSTS
    );
}

#[test]
fn a_class_body_that_declares_a_global_is_refused_for_the_store_it_makes() {
    // the producer an earlier note in `bpd_core::jump` argued could not exist.
    // it claimed a call must load its callee and that every allow-listed way to
    // do that in a namespace frame is `LOAD_NAME`, which `refuses` meets before
    // any store — so `(TheCaller, Writes)` was unreachable
    //
    // `refuses` checks globals **before** locals, and `global grows` makes the
    // load a `LOAD_GLOBAL`, which is on `THROUGH_GLOBALS` only. with the globals
    // exact and the class body's `__prepare__` mapping not, the first refusing
    // instruction is the `STORE_NAME` — so the write wording has a producer, and
    // it says `__setitem__` rather than `__getitem__`
    let fixture = Fixture::new("declared_global", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_in_grows_called_from(&mut debuggee, &fixture, "        declared = grows(1)");

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            &reason,
            Unrestartable::NamespaceIsNotADict {
                whose: Whose::TheCaller,
                access: Access::Writes,
                ..
            }
        ),
        "{reason:?}"
    );
    assert!(
        said.contains("__setitem__") && said.contains("stores into"),
        "the write wording, not the read one: {said}"
    );
    assert!(
        !said.contains("__missing__"),
        "nothing here reads through the mapping: {said}"
    );

    to_exit(&mut debuggee);
}

/// what the caller's locals really hold when the restarted call re-enters
///
/// read from inside the callee with `sys._getframe(1).f_locals`, which is the
/// program looking at itself rather than bpd reporting on it. that is the only
/// check that can catch `disturbed` being wrong, because `disturbed` is bpd's
/// own claim about exactly this
fn entered_holding(fixture: &Fixture, marker: &str, at: usize) -> String {
    let ran = recorded(fixture);
    ran.split(&format!("('{marker}'"))
        .nth(at)
        .unwrap_or_else(|| panic!("`{marker}` was not recorded {at} times: {ran}"))
        .to_string()
}

#[test]
fn a_name_that_reads_back_a_disturbed_one_is_disturbed_too() {
    // every load was modelled as pushing an untainted value, so the forced
    // return's value stopped being tracked the moment a name holding it was read
    // back. `kept3 = f(1); echo3 = spare; copy3 = kept3` named `kept3` and not
    // `copy3`, while the program itself — reading its caller's locals on the
    // restarted entry — saw both holding bpd's `None`
    let fixture = Fixture::new("aliases", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    alias_step = value + 100"),
    );

    let frame = top(&mut debuggee);
    let arranged = match debuggee
        .restart_frame(frame)
        .expect("the restart was arranged")
    {
        Restarted::Arranged(restarting) => restarting,
        other @ Restarted::Refused { .. } => panic!("one clean call: {other:?}"),
    };
    assert_eq!(
        arranged.disturbed,
        vec!["kept3".to_string(), "copy3".to_string()],
        "`copy3` reads `kept3` back, so it holds the forced return too"
    );

    landed(&mut debuggee);
    to_exit(&mut debuggee);
    // and the program's own view agrees, which is the whole point
    let second = entered_holding(&fixture, "alias_entered", 2);
    assert!(
        second.contains("('copy3', None)"),
        "the program saw `copy3` holding the forced return: {second}"
    );
}

#[test]
fn a_call_whose_argument_is_fed_back_through_another_name_is_refused() {
    // the serious half. `held3 = f(seed); seed = held3` disturbs `seed`
    // **through** `held3`, and `TheCallReadsWhatItStores` is derived from
    // `disturbed` — so with `disturbed` missing it, nothing refused, the rewind
    // re-executed the line, and the restarted call was made with bpd's forced
    // `None` instead of the program's own argument. the debuggee died with a
    // `TypeError`, under an answer of `Arranged`
    let fixture = Fixture::new("feeds_back", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    feed_step = value + 100"),
    );

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            &reason,
            Unrestartable::TheCallReadsWhatItStores { name, .. } if name == "seed"
        ),
        "{reason:?}"
    );
    assert!(said.contains("`seed`"), "said {said}");

    to_exit(&mut debuggee);
}

#[test]
fn a_slot_written_again_after_the_call_is_not_left_named() {
    // the other direction. `slot3 = f(1); slot3 = other` ends the line holding
    // the program's own value, so naming it tells a client to distrust a slot it
    // can trust. the walk never modelled a later store overwriting a disturbed
    // one
    let fixture = Fixture::new("overwritten", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    over_step = value + 100"),
    );

    let frame = top(&mut debuggee);
    let arranged = match debuggee
        .restart_frame(frame)
        .expect("the restart was arranged")
    {
        Restarted::Arranged(restarting) => restarting,
        other @ Restarted::Refused { .. } => panic!("one clean call: {other:?}"),
    };
    assert!(
        arranged.disturbed.is_empty(),
        "the line writes the slot again before the rewind: {:?}",
        arranged.disturbed
    );

    landed(&mut debuggee);
    to_exit(&mut debuggee);
}

#[test]
fn the_write_half_of_a_fused_store_is_not_a_read_the_call_makes() {
    // `LOADING` holds `STORE_FAST_LOAD_FAST`, because its load half really does
    // read a name before the call. the guard iterated **both** of its names, so
    // the **write** half counted as a read — the same split `writes()` exists
    // for, applied on the write side and missed here
    //
    // `va = spare; vb = other; vgot = victim_callee(1); va = vgot` fuses
    // `STORE_FAST va` with `LOAD_FAST other`. nothing on the line reads `va`
    // before the call, and `va` is disturbed, so the two met and bpd refused a
    // sound restart while telling the user their line calls with a value bpd put
    // there — and to "store the result under a different name", for a line with
    // no such store
    let fixture = Fixture::new("victim", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    victim_step = value + 100"),
    );

    let frame = top(&mut debuggee);
    let arranged = match debuggee
        .restart_frame(frame)
        .expect("the restart was arranged")
    {
        Restarted::Arranged(restarting) => restarting,
        other @ Restarted::Refused { .. } => {
            panic!("nothing on the line reads `va` before the call: {other:?}")
        }
    };
    assert_eq!(
        arranged.disturbed,
        vec!["vgot".to_string(), "va".to_string()],
        "both take the forced return — `va` through `vgot`"
    );

    landed(&mut debuggee);
    to_exit(&mut debuggee);
    // and the restarted call really was made with the program's own argument
    let second = entered_holding(&fixture, "victim_entered", 2);
    assert!(
        second.contains("('spare', 5)"),
        "the call ran again with the program's `spare`: {second}"
    );
}

#[test]
fn the_sweep_script_measures_the_bytecode_the_interpreter_runs() {
    // `scripts/restart_shapes.py` exists so the figures quoted in doc comments
    // can be re-derived rather than believed — and it was compiling the stdlib
    // with its own `from __future__ import annotations` inherited, which on 3.14
    // switches PEP 649 lazy annotations back off. every class body loses its
    // `MAKE_CELL __classdict__` prologue and every later offset shifts, so it
    // measured bytecode no interpreter runs and a figure went into four doc
    // comments wrong
    //
    // this does not pin a count — the count is the script's job. it pins that
    // compiling the way the script does gives the same code as importing does,
    // which is the property that was broken
    let probe = "import dis\n\
                 SRC = 'class C:\\n    x: int = 1\\n    def m(self): return 1\\n'\n\
                 inherited = compile(SRC, '<p>', 'exec')\n\
                 detached = compile(SRC, '<p>', 'exec', dont_inherit=True)\n\
                 same = [i.opname for i in dis.get_instructions(inherited)] == \\\n\
                 [i.opname for i in dis.get_instructions(detached)]\n\
                 print('same' if same else 'differs')\n";
    let fixture = Fixture::new("future_flags", probe);
    let run = fixture.run(
        bpd_test::agent::matching_interpreter(),
        bpd_test::debuggee::Form::Script,
        &[],
    );
    assert!(run.success, "the probe runs: {}", run.stderr);

    // with no future import in the probe itself the two agree, which is what
    // makes the script's `dont_inherit=True` the *safe* choice rather than a
    // cosmetic one: it is the only spelling that does not depend on what the
    // caller happens to have imported
    assert_eq!(run.stdout.trim(), "same");

    let script = std::fs::read_to_string("../../scripts/restart_shapes.py")
        .or_else(|_| std::fs::read_to_string("scripts/restart_shapes.py"))
        .expect("the sweep script is in the tree");
    assert!(
        script.contains("dont_inherit=True"),
        "the script has to compile detached from its own future flags, or the \
         figures it prints are not the ones the agent reads"
    );
}

#[test]
fn a_call_reading_a_name_the_tail_writes_is_refused_even_when_bpd_wrote_nothing() {
    // `TheCallReadsWhatItStores` compared the call's reads against `disturbed`,
    // which is the names holding **bpd's** forced return. that is only half the
    // rule. the tail also lands before the rewind, so a name it writes with the
    // *program's own* value is read back by the re-executed line just the same —
    // and the second call is then not the call that was restarted, which is the
    // exact reason `f(*args)` is refused
    //
    // `sx, y = swapped_callee(y), spare` reads `y`, then the tail binds
    // `y := spare`. bpd answered `Arranged`, reported `Restarted`, and the
    // callee ran again with `99` where every undebugged run passes `7`. nothing
    // bpd invented was involved, so `disturbed` was silent and correct
    let fixture = Fixture::new("swapped", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    swapped_step = value + 100"),
    );

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            &reason,
            Unrestartable::TheCallReadsWhatItStores { name, .. } if name == "y"
        ),
        "{reason:?}"
    );
    assert!(said.contains("`y`"), "said {said}");

    to_exit(&mut debuggee);
    // and the program ran exactly as it would have with nothing attached
    let bare = Fixture::new("swapped_bare", PROGRAM);
    let run = bare.run(
        bpd_test::agent::matching_interpreter(),
        bpd_test::debuggee::Form::Script,
        &[],
    );
    assert!(run.success, "the fixture runs on its own: {}", run.stderr);
    assert_eq!(recorded(&fixture), recorded(&bare));
}

#[test]
fn a_name_read_back_through_an_ordinary_load_is_followed_too() {
    // the walk follows the call's value out of a name it was stored into, and
    // every fixture that exercised it went through the **fused**
    // `STORE_FAST_LOAD_FAST`, whose read half has its own branch. the general
    // arm — a plain load, taking the taint of the name it reads — was covered by
    // nothing: neutering it left all 710 tests green
    //
    // a **class body** reaches it: its names are `STORE_NAME` and `LOAD_NAME`,
    // which never fuse, and unlike a module body its namespace is its own rather
    // than the globals — so it is not refused as shared state
    let fixture = Fixture::new("named_alias", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_in_grows_called_from(
        &mut debuggee,
        &fixture,
        "    aliased_kept = grows(2); aliased_copy = aliased_kept",
    );

    let frame = top(&mut debuggee);
    let arranged = match debuggee
        .restart_frame(frame)
        .expect("the restart was arranged")
    {
        Restarted::Arranged(restarting) => restarting,
        other @ Restarted::Refused { .. } => panic!("one clean call: {other:?}"),
    };
    assert_eq!(
        arranged.disturbed,
        vec!["aliased_kept".to_string(), "aliased_copy".to_string()],
        "`aliased_copy` reads `aliased_kept` back through a plain `LOAD_NAME`"
    );

    landed(&mut debuggee);
    to_exit(&mut debuggee);
}

#[test]
fn a_constant_that_happens_to_be_a_string_is_not_a_name() {
    // `Instruction::names` reads `argval`'s **type**, so a `LOAD_CONST` of a
    // string looked exactly like a read of a name — and the walk propagated the
    // call's value through it. `ca = const_callee(0); cb = 'ca'` reported `cb`
    // as holding the forced return, and `told()` said so, while `cb` holds the
    // string the program wrote there
    let fixture = Fixture::new("const_named", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    const_step = value + 100"),
    );

    let frame = top(&mut debuggee);
    let arranged = match debuggee
        .restart_frame(frame)
        .expect("the restart was arranged")
    {
        Restarted::Arranged(restarting) => restarting,
        other @ Restarted::Refused { .. } => panic!("one clean call: {other:?}"),
    };
    assert_eq!(
        arranged.disturbed,
        vec!["ca".to_string()],
        "`cb` holds the string `'ca'`, which the program computed itself"
    );

    landed(&mut debuggee);
    to_exit(&mut debuggee);
}

#[test]
fn a_tail_that_writes_a_global_the_callee_could_read_is_refused() {
    // `disturbed` and the read guard both track **names on the caller's line**.
    // neither sees state the *callee* reads that the tail writes and the
    // caller's line never reads back. `shared_got, SHARED_SEEN = reads_shared(),
    // 99` answered `Arranged` and reported `Restarted`, and the restarted call
    // read `99` where every undebugged run reads `1` — the program's own output
    // changed and nothing said so
    //
    // bpd cannot see what a callee reads, so it refuses rather than guesses.
    // measured over both stdlibs, a tail that writes a global or a cell is 143
    // of 7401 permitted sites on 3.13 and 150 of 7520 on 3.14
    let fixture = Fixture::new("writes_shared", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    shared_step = SHARED_SEEN + 100"),
    );

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            &reason,
            Unrestartable::TailWritesSharedState { name, .. } if name == "SHARED_SEEN"
        ),
        "{reason:?}"
    );
    assert!(said.contains("`SHARED_SEEN`"), "said {said}");

    to_exit(&mut debuggee);
    // and the program ran exactly as it would have with nothing attached
    let bare = Fixture::new("writes_shared_bare", PROGRAM);
    let run = bare.run(
        bpd_test::agent::matching_interpreter(),
        bpd_test::debuggee::Form::Script,
        &[],
    );
    assert!(run.success, "the fixture runs on its own: {}", run.stderr);
    assert_eq!(recorded(&fixture), recorded(&bare));
}

#[test]
fn the_refusal_names_the_read_that_takes_the_invented_value() {
    // both a `disturbed` name and a merely-written one are refusals, and they
    // are not equally informative. `wx, wy = wx, which_callee(wy)` reads both
    // `wx` and `wy` before the call, and the tail binds both — but only `wy`
    // takes bpd's forced return; `wx` is rebound to the value it already had
    //
    // taking the first match in offset order named `wx`, whose read is harmless,
    // and then asserted of it that the second call would be made with it. the
    // guard checks `disturbed` first now
    let fixture = Fixture::new("which_name", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "    which_step = value + 100"),
    );

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            &reason,
            Unrestartable::TheCallReadsWhatItStores { name, .. } if name == "wy"
        ),
        "`wy` is the argument and the name the forced return lands in: {reason:?}"
    );
    assert!(said.contains("`wy`"), "said {said}");

    to_exit(&mut debuggee);
}

#[test]
fn a_tail_that_writes_a_cell_the_closure_shares_is_refused() {
    // the `STORE_DEREF` half of `SHARED_WITH_THE_CALLEE`. it is named in the
    // refusal message, counted in the 143/150 figure and asserted in the docs —
    // and dropping `"STORE_DEREF"` from the list left the entire suite green,
    // so only the `STORE_GLOBAL` half was ever pinned
    //
    // `cell_got, cell_seen = cell_inner(), 77` writes a cell the callee closes
    // over: without the refusal the restarted call reads `77` where every
    // undebugged run reads `1`
    let fixture = Fixture::new("cell_shared", PROGRAM);
    let mut debuggee = launch(&fixture);
    held_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROGRAM, "        cell_step = cell_seen + 100"),
    );

    let frame = top(&mut debuggee);
    let (said, reason) = refused(debuggee.restart_frame(frame));
    assert!(
        matches!(
            &reason,
            Unrestartable::TailWritesSharedState { name, .. } if name == "cell_seen"
        ),
        "{reason:?}"
    );
    assert!(said.contains("cell shared with a closure"), "said {said}");

    to_exit(&mut debuggee);
    let bare = Fixture::new("cell_shared_bare", PROGRAM);
    let run = bare.run(
        bpd_test::agent::matching_interpreter(),
        bpd_test::debuggee::Form::Script,
        &[],
    );
    assert!(run.success, "the fixture runs on its own: {}", run.stderr);
    assert_eq!(recorded(&fixture), recorded(&bare));
}
