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
