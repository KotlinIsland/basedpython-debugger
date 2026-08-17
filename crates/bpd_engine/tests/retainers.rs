//! what is holding an object, and how
//!
//! the walk is `gc.get_referrers`, and three things about it were measured
//! before any of this was built: it needs no `unsafe`, it is linear and cheap
//! — 9.8 ms on an 805,000-object heap — and it leaves the heap exactly the size
//! it found it
//!
//! everything here drives a real interpreter, because what is under test is what
//! the collector really says

use bpd_core::python::Capabilities;
use bpd_core::{Retainers, Running, SourceBreakpoint};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

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

/// a program holding one object four ways at once
///
/// each holder is a different shape, because *where inside* a holder the object
/// sits is the half of the answer that is worth having — "a dict holds it" is
/// almost nothing next to "the value under `'session'`"
const PROGRAM: &str = r"class Holder:
    def __init__(self, thing):
        self.thing = thing


def main():
    target = ['the object being asked about']
    holder = Holder(target)
    in_a_list = [target]
    in_a_dict = {'session': target}
    here = 1              # the breakpoint
    return holder, in_a_list, in_a_dict


main()
";

fn asked(debuggee: &mut Debuggee, expression: &str, fixture: &Fixture) -> Retainers {
    let here = line_of(PROGRAM, "here = 1");
    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), here)])
        .expect("the breakpoint was answered");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {
            let frame = debuggee
                .the_stack(Some(1))
                .expect("the stack was answered")
                .frames[0]
                .id;
            debuggee
                .what_holds(frame, expression)
                .expect("the retainer walk was answered")
        }
        other => panic!("the breakpoint never stopped it: {other:?}"),
    }
}

#[test]
fn every_holder_is_found_and_says_where_inside_itself_the_object_sits() {
    let fixture = Fixture::new("program", PROGRAM);
    let mut debuggee = launch(&fixture);
    let found = asked(&mut debuggee, "target", &fixture);

    let through: Vec<String> = found
        .found
        .iter()
        .filter_map(|retainer| retainer.through.clone())
        .collect();

    // the three shapes, each named by where inside it the object is. a report
    // that said only "a dict holds it" would be true and nearly useless
    assert!(
        through
            .iter()
            .any(|where_| where_.contains("attribute `thing`")),
        "the object's attribute holder was not named: {:#?}",
        found.found
    );
    assert!(
        through.iter().any(|where_| where_ == "index 0"),
        "the list holder did not say which index: {:#?}",
        found.found
    );
    assert!(
        through
            .iter()
            .any(|where_| where_.contains("the value under `session`")),
        "the dict holder did not say which key: {:#?}",
        found.found
    );

    // the frame's own local is **not** among them, and that is cpython's
    // behaviour rather than bpd filtering: measured on 3.13, 3.14 and 3.15, a
    // frame does not appear as a retainer of its own local even materialised
    // with its `f_locals` read. it is asserted so that a release which changes
    // it is noticed here rather than in a report nobody can explain
    assert!(
        !found.found.iter().any(|retainer| retainer.kind == "frame"),
        "a frame appeared as a retainer, which changes what this answer means: \
         {:#?}",
        found.found
    );
}

#[test]
fn the_answer_says_what_the_walk_cannot_see_every_time() {
    // the part that stops this being a narrower question's answer. a list of
    // holders reads as "these are the holders", and this walk cannot see two
    // whole categories — one of which is bpd itself
    let fixture = Fixture::new("program", PROGRAM);
    let mut debuggee = launch(&fixture);
    let found = asked(&mut debuggee, "target", &fixture);

    assert!(
        found.coverage.untracked.contains("does not track"),
        "the coverage has to name the untracked hole: {:?}",
        found.coverage
    );
    assert!(
        found.coverage.not_python.contains("bpd's own"),
        "the coverage has to say the debugger is among what it cannot see, \
         because it is holding the object while answering: {:?}",
        found.coverage
    );
}

#[test]
fn an_object_nothing_holds_is_an_empty_answer_rather_than_a_missing_one() {
    // a literal is retained by the frame evaluating it and by nothing the walk
    // can see, so the honest answer is "none found" with the coverage that says
    // why that is not the same as "nothing holds it"
    let fixture = Fixture::new("program", PROGRAM);
    let mut debuggee = launch(&fixture);
    let found = asked(&mut debuggee, "['made right here']", &fixture);

    assert!(
        found.found.is_empty(),
        "nothing the collector tracks holds a list built by the expression \
         itself: {:#?}",
        found.found
    );
    assert!(
        !found.coverage.not_python.is_empty(),
        "and the answer still has to say what it could not see"
    );
}

/// a program holding one object in the two shapes that have no position
///
/// the target is a plain instance rather than a list, because a set holds only
/// what it can hash — which is exactly why the existing program never reaches
/// this path
const UNORDERED: &str = r"class Thing:
    pass


def main():
    target = Thing()
    in_a_set = {target}
    in_a_frozen = frozenset({target})
    here = 1              # the breakpoint
    return in_a_set, in_a_frozen


main()
";

#[test]
fn a_holder_with_no_order_is_not_given_a_position_it_does_not_have() {
    // a set's iteration order is its hash table's, and it moves when the table
    // is resized. "index 3" of a set is a location the program does not have —
    // read as a list's index it is a false statement about where the object is,
    // and it is not stable enough to be true twice
    let fixture = Fixture::new("program", UNORDERED);
    let here = line_of(UNORDERED, "here = 1");
    let mut debuggee = launch(&fixture);

    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), here)])
        .expect("the breakpoint was answered");
    let found = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {
            let frame = debuggee
                .the_stack(Some(1))
                .expect("the stack was answered")
                .frames[0]
                .id;
            debuggee
                .what_holds(frame, "target")
                .expect("the retainer walk was answered")
        }
        other => panic!("the breakpoint never stopped it: {other:?}"),
    };

    for retainer in &found.found {
        if retainer.kind == "set" || retainer.kind == "frozenset" {
            let inside = retainer.through.as_deref().unwrap_or("");
            assert!(
                !inside.starts_with("index"),
                "a {} was reported as `{inside}`, and a set has no index: {:#?}",
                retainer.kind,
                found.found
            );
            assert_eq!(
                inside, "an element of it",
                "a holder with no order says the object is in it and refuses to \
                 say where: {:#?}",
                found.found
            );
        }
    }

    // and the shapes really were reached, so this cannot pass by finding none
    let unordered = found
        .found
        .iter()
        .filter(|retainer| retainer.kind == "set" || retainer.kind == "frozenset")
        .count();
    assert_eq!(
        unordered, 2,
        "the program holds it in a set and a frozenset, and the walk found \
         {unordered} of them: {:#?}",
        found.found
    );
}

/// a program whose holders notice being looked at
///
/// `__len__` on the instance and `__hash__` on the dict key are the two hooks a
/// retainer walk can trip without meaning to. the list records every call, and
/// is cleared just before the stop so only the walk's own calls are in it
const WATCHFUL: &str = r"called = []


class Watchful:
    def __init__(self, thing):
        self.thing = thing

    def __len__(self):
        called.append('len')
        return 0


class Key:
    def __hash__(self):
        called.append('hash')
        return 1

    def __eq__(self, other):
        called.append('eq')
        return self is other


def main():
    target = ['the object being asked about']
    watchful = Watchful(target)
    keyed = {Key(): target}
    called.clear()
    here = 1              # the breakpoint
    return watchful, keyed


main()
";

#[test]
fn asking_what_holds_an_object_runs_none_of_the_program_to_answer() {
    // the rule the module states about `repr` and `dir`, which `len` and a dict
    // lookup break just as thoroughly: `__len__`, `__hash__` and `__eq__` are
    // the program's code, and running them to answer a question about the
    // program can mutate the heap being asked about — or raise, and fail a walk
    // for a reason that has nothing to do with what was asked
    let fixture = Fixture::new("program", WATCHFUL);
    let here = line_of(WATCHFUL, "here = 1");
    let mut debuggee = launch(&fixture);

    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), here)])
        .expect("the breakpoint was answered");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        other => panic!("the breakpoint never stopped it: {other:?}"),
    }

    let frame = debuggee
        .the_stack(Some(1))
        .expect("the stack was answered")
        .frames[0]
        .id;
    let found = debuggee
        .what_holds(frame, "target")
        .expect("the retainer walk was answered");

    let called = match debuggee
        .evaluate(frame, "repr(called)", bpd_core::Detail::default())
        .expect("the evaluation was answered")
    {
        bpd_core::Evaluated::Value { value } => match value.content {
            bpd_core::Content::Str { text, .. } => text,
            other => panic!("`repr` makes a string, and this is {other:?}"),
        },
        bpd_core::Evaluated::Raised { error } => panic!("`repr(called)` raised {error:?}"),
    };

    assert_eq!(
        called.trim_matches('\''),
        "[]",
        "the walk ran the program's own code to describe it — {called} — while \
         answering: {:#?}",
        found.found
    );
}

/// a program holding one object in containers that lie when asked to iterate
///
/// a list subclass with an `__iter__` of its own is ordinary in framework code,
/// and it is the case that tells "read the storage" apart from "ask the object"
const LYING: &str = r"class Registry(list):
    def __iter__(self):
        return iter(['nothing', 'here'])


class Watchful(set):
    def __iter__(self):
        raise AssertionError('the walk asked a set to iterate itself')


def main():
    target = ['the object being asked about']
    registry = Registry([1, target, 3])
    here = 1              # the breakpoint
    return registry


main()
";

#[test]
fn a_container_that_lies_about_its_contents_is_read_through_its_storage() {
    // the rule this is an instance of: a question about the program is answered
    // from what the interpreter itself indexes, never by asking the object. a
    // subclass with its own `__iter__` would otherwise decide what the debugger
    // reports about it — and an earlier fix for that used an exact type check,
    // which stopped running the program's code by refusing to answer at all
    let fixture = Fixture::new("program", LYING);
    let here = line_of(LYING, "here = 1");
    let mut debuggee = launch(&fixture);

    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), here)])
        .expect("the breakpoint was answered");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        other => panic!("the breakpoint never stopped it: {other:?}"),
    }
    let frame = debuggee
        .the_stack(Some(1))
        .expect("the stack was answered")
        .frames[0]
        .id;
    let found = debuggee
        .what_holds(frame, "target")
        .expect("the retainer walk was answered");

    let registry = found
        .found
        .iter()
        .find(|retainer| retainer.kind == "Registry")
        .unwrap_or_else(|| {
            panic!(
                "the subclass holds it and was not found: {:#?}",
                found.found
            )
        });

    // the real position, out of the storage. the object's own iterator yields
    // two strings and would have said the target is not in it at all
    assert_eq!(
        registry.through.as_deref(),
        Some("index 1"),
        "a list subclass is answered from `PyList_GET_ITEM`, which is where the \
         interpreter itself looks: {registry:#?}"
    );
}
