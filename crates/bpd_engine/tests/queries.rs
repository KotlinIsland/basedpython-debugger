//! the declarative state query, and the difference between two of its answers
//!
//! three claims are load bearing here and none of them can be checked by reading
//! one answer on its own:
//!
//! - **the one call and the tree walk say the same thing.** the query is
//!   composed of the requests the walk is made of, and
//!   `a_query_answers_in_one_call_what_the_tree_walk_answers_in_four` reads the
//!   same stop both ways and compares them value for value
//! - **a diff reports what the program really did.** the fixture computes two
//!   different totals and writes both to disk, so the assertion is against the
//!   program's own output rather than against what the diff claims
//! - **source is the source that is running.** the file is edited underneath a
//!   held program, and the answer has to stop showing lines rather than show the
//!   new ones
//!
//! the fixture keeps a marker line after the breakpoint, as everywhere else in
//! this suite, so no test takes the agent's word for where the program is

use std::path::Path;

use bpd_core::python::Capabilities;
use bpd_core::{
    Binding, Content, Detail, Evaluated, FrameId, LogRecord, Omitted, Resolved, Running, Scope,
    Seen, SnapshotId, Source, SourceBreakpoint, StateQuery, StopReason, Subject, Unverified,
    Wanted, WhyNot,
};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// a function called twice with different arguments, whose totals reach disk
///
/// the two stops differ in `step` and `total` and agree in `items`, so a diff
/// over them has something in every category. what the program computed is
/// written out, which is what the assertions are really against
const COUNTING: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent
RESULT = HERE / "result"


def work(step):
    total = step * 10
    items = list(range(50))
    marker = total
    return total + len(items)


def main():
    first = work(1)
    second = work(2)
    RESULT.write_text(f"{first} {second}")


main()
"#;

/// the interpreter the built agent matches, or a failure saying how to get one
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

/// nothing here is a logpoint, so a record would be one the agent invented
#[expect(
    clippy::needless_pass_by_value,
    reason = "it stands in for a `FnMut(LogRecord)` sink, which is handed the \
              record to own"
)]
fn unlogged(record: LogRecord) {
    panic!("no logpoint was set, and the agent sent {record:?}")
}

fn bound(resolved: &[Resolved]) {
    for resolution in resolved {
        if let Binding::Unbound { reason } = &resolution.binding {
            panic!("breakpoint {} did not bind: {reason}", resolution.id);
        }
    }
}

/// set one breakpoint and run to it, checking it really landed there
fn stop_at(debuggee: &mut Debuggee, file: &Path, line: u32) {
    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, file, line)])
        .expect("the breakpoint request was answered");
    bound(&resolved);
    carry_on(debuggee, line);
}

/// run to the next hit of the breakpoint that is already set
fn carry_on(debuggee: &mut Debuggee, line: u32) {
    match debuggee.run(unlogged).expect("the debuggee was resumed") {
        Running::Stopped { stop, .. } => match stop.reason {
            StopReason::Breakpoint { line: at, .. } => assert_eq!(at, line),
            other => panic!("it stopped for {other:?}"),
        },
        other => panic!("the program did not stop: {other:?}"),
    }
}

/// end the program and return what it wrote, which is the ground truth
fn finish(debuggee: &mut Debuggee, fixture: &Fixture) -> String {
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoints were cleared");
    match debuggee.run(unlogged).expect("the debuggee was resumed") {
        Running::Exited { status, .. } => assert!(status.success(), "the program failed: {status}"),
        other => panic!("the program did not finish: {other:?}"),
    }
    std::fs::read_to_string(fixture.directory().join("result"))
        .expect("the program wrote what it computed")
}

/// the whole local scope of frame 0, as a query asks for it
fn locals(detail: Detail) -> StateQuery {
    StateQuery {
        frames: 1,
        scopes: vec![Scope::Local],
        detail,
        ..StateQuery::default()
    }
}

fn text_of(value: &bpd_core::Value) -> String {
    match &value.content {
        Content::Int { text, .. } => text.clone(),
        other => panic!("expected an integer and got {other:?}"),
    }
}

#[test]
fn a_query_answers_in_one_call_what_the_tree_walk_answers_in_four() {
    let fixture = Fixture::new("counting", COUNTING);
    let mut debuggee = launch(&fixture);
    stop_at(
        &mut debuggee,
        &fixture.path(),
        line_of(COUNTING, "marker = total"),
    );

    let described = debuggee
        .the_query(StateQuery {
            frames: 2,
            scopes: vec![Scope::Local],
            expressions: vec![
                Wanted {
                    expression: "total * 2".to_string(),
                    frame: 0,
                },
                Wanted {
                    expression: "step".to_string(),
                    frame: 0,
                },
            ],
            ..StateQuery::default()
        })
        .expect("the query was answered");

    // the same stop, read the way DAP reads one: a stack, then a scope of a
    // frame, then an evaluation. the query is composed of exactly these, and an
    // answer that differed would mean there were two implementations
    let walked = debuggee.the_stack(Some(2)).expect("the stack was walked");
    assert_eq!(described.state.depth, walked.depth);
    assert_eq!(
        described
            .state
            .frames
            .iter()
            .map(|frame| frame.frame.clone())
            .collect::<Vec<_>>(),
        walked.frames,
        "the frames a query describes are the frames the walk reports"
    );

    for frame in &described.state.frames {
        let read = debuggee
            .variables(frame.frame.id, Scope::Local, Detail::default())
            .expect("the scope was read");
        let [queried] = frame.scopes.as_slice() else {
            panic!(
                "one scope was asked for and {} came back",
                frame.scopes.len()
            )
        };
        assert_eq!(queried.entries, read.entries, "frame {}", frame.frame.id);
        assert_eq!(queried.unbound, read.unbound);
        assert_eq!(queried.unreadable, read.unreadable);
        assert_eq!(queried.omitted, read.omitted);
    }

    for answer in &described.state.values {
        let evaluated = debuggee
            .evaluate(
                FrameId {
                    stop: described.state.stop,
                    depth: answer.frame,
                },
                &answer.expression,
                Detail::default(),
            )
            .expect("the expression was evaluated");
        assert_eq!(answer.result, evaluated, "`{}`", answer.expression);
    }

    // and it really is one call's worth of answers rather than a stack with a
    // scope hidden behind a second request
    let Evaluated::Value { value } = &described.state.values[0].result else {
        panic!("`total * 2` raised: {:?}", described.state.values[0])
    };
    assert_eq!(text_of(value), "20");
    assert_eq!(described.state.frames[0].frame.function, "work");
    assert_eq!(described.state.frames[1].frame.function, "main");

    assert_eq!(finish(&mut debuggee, &fixture), "60 70");
}

#[test]
fn a_budget_that_runs_out_says_which_parts_it_did_not_read() {
    let fixture = Fixture::new("counting", COUNTING);
    let mut debuggee = launch(&fixture);
    stop_at(
        &mut debuggee,
        &fixture.path(),
        line_of(COUNTING, "marker = total"),
    );

    // enough for the stack walk and the first expression, and nowhere near
    // enough for a scope of fifty items after it
    let described = debuggee
        .the_query(StateQuery {
            frames: 1,
            scopes: vec![Scope::Local, Scope::Global],
            expressions: vec![
                Wanted {
                    expression: "step".to_string(),
                    frame: 0,
                },
                Wanted {
                    expression: "total".to_string(),
                    frame: 0,
                },
            ],
            source: Some(2),
            detail: Detail {
                budget: 200,
                ..Detail::default()
            },
        })
        .expect("the query was answered");

    assert!(
        !described.state.left_out.is_empty(),
        "the budget was spent and nothing said so: {:?}",
        described.state
    );
    let said: Vec<String> = described
        .state
        .left_out
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(
        said.iter().any(|left| left.contains("global scope")),
        "the open ended part is what a spent budget cuts, and it said {said:?}"
    );
    for left in &said {
        assert!(
            left.contains("byte budget of 200"),
            "every elision says what to raise, and it said {left}"
        );
    }

    // what it did read is whole. a part is left out before it is read rather
    // than half way through, so nothing here is a fragment
    assert!(described.state.bytes > 0);
    for frame in &described.state.frames {
        for scope in &frame.scopes {
            assert!(!scope.entries.is_empty(), "a scope was read into nothing");
        }
    }
}

#[test]
fn the_source_a_query_shows_is_the_source_the_frame_is_running() {
    let fixture = Fixture::new("counting", COUNTING);
    let mut debuggee = launch(&fixture);
    let line = line_of(COUNTING, "marker = total");
    stop_at(&mut debuggee, &fixture.path(), line);

    let described = debuggee
        .the_query(StateQuery {
            frames: 1,
            source: Some(1),
            ..StateQuery::default()
        })
        .expect("the query was answered");

    let source = described.state.frames[0]
        .source
        .clone()
        .expect("the query asked for source");
    let Source::Lines {
        first, at, lines, ..
    } = source
    else {
        panic!("the file is the one the interpreter read: {source:?}")
    };
    assert_eq!(at, line);
    assert_eq!(first, line - 1);

    // against the file itself rather than against what the answer says about
    // itself
    let whole: Vec<&str> = COUNTING.lines().collect();
    let expected: Vec<String> = whole[(first - 1) as usize..(first - 1) as usize + lines.len()]
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(lines, expected);
    assert!(lines.iter().any(|text| text.contains("marker = total")));
}

#[test]
fn a_file_edited_since_the_interpreter_read_it_shows_no_lines_and_says_why() {
    let fixture = Fixture::new("counting", COUNTING);
    let mut debuggee = launch(&fixture);
    stop_at(
        &mut debuggee,
        &fixture.path(),
        line_of(COUNTING, "marker = total"),
    );

    // the program is held, and the file underneath it is edited. this is what
    // `linecache` gets wrong in every traceback: the bytes on disk are not
    // evidence of what the interpreter compiled
    std::fs::write(fixture.path(), format!("# an edit\n{COUNTING}"))
        .expect("the fixture's file is writable");

    let described = debuggee
        .the_query(StateQuery {
            frames: 1,
            source: Some(1),
            ..StateQuery::default()
        })
        .expect("the query was answered");

    let source = described.state.frames[0]
        .source
        .clone()
        .expect("the query asked for source");
    let Source::Unverified { why } = source else {
        panic!("the file is not the code that is running, and it showed lines: {source:?}")
    };
    assert!(
        matches!(why, Unverified::NotTheSameCode { .. }),
        "expected the edit to be detected, and it said {why}"
    );
    let said = why.to_string();
    assert!(said.contains("edited since"), "said {said}");
    assert!(said.contains("`work`"), "said {said}");
}

#[test]
fn a_diff_reports_the_change_the_program_really_made() {
    let fixture = Fixture::new("counting", COUNTING);
    let mut debuggee = launch(&fixture);
    let line = line_of(COUNTING, "marker = total");
    stop_at(&mut debuggee, &fixture.path(), line);

    let before = debuggee
        .the_query(locals(Detail::default()))
        .expect("the first state was read");
    carry_on(&mut debuggee, line);
    let after = debuggee
        .the_query(locals(Detail::default()))
        .expect("the second state was read");
    assert_ne!(before.id, after.id, "two different states, two ids");

    let difference = debuggee
        .diff(&before.id, &after.id)
        .expect("both states are this session's");

    let changed: Vec<(String, String, String)> = difference
        .changed
        .iter()
        .map(|change| {
            let (Seen::Value { value: one }, Seen::Value { value: other }) =
                (&change.before, &change.after)
            else {
                panic!("expected two values: {change:?}")
            };
            (change.subject.to_string(), text_of(one), text_of(other))
        })
        .collect();

    // the program computed 10 and then 20, and the proof is what it wrote to
    // disk at the end rather than what the diff says about itself
    assert!(
        changed
            .iter()
            .any(|(subject, one, other)| subject.contains("`total`")
                && one == "10"
                && other == "20"),
        "{changed:?}"
    );
    assert!(
        changed
            .iter()
            .any(|(subject, one, other)| subject.contains("`step`") && one == "1" && other == "2"),
        "{changed:?}"
    );

    // the list is the same list both times, and a diff that reported it as
    // changed would be inventing one
    let unchanged: Vec<String> = difference
        .unchanged
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(
        unchanged.iter().any(|name| name.contains("`items`")),
        "{unchanged:?}"
    );
    // `marker` is a local of the scope that holds nothing at this line, in both
    // states. that is a state a name really has, and it is not absent
    assert!(
        unchanged.iter().any(|name| name.contains("`marker`")),
        "{unchanged:?}"
    );
    assert!(
        difference.not_compared.is_empty(),
        "everything here was read whole: {:?}",
        difference.not_compared
    );

    // the ground truth: the program adds the fifty items to each total and
    // writes both out. the numbers the diff reported have to be those numbers
    let (_, one, other) = changed
        .iter()
        .find(|(subject, _, _)| subject.contains("`total`"))
        .expect("`total` is what changed");
    let total = |text: &str| text.parse::<i64>().expect("a total is a number") + 50;
    assert_eq!(
        finish(&mut debuggee, &fixture),
        format!("{} {}", total(one), total(other))
    );
}

#[test]
fn a_value_a_bound_cut_short_is_reported_as_not_compared_rather_than_unchanged() {
    let fixture = Fixture::new("counting", COUNTING);
    let mut debuggee = launch(&fixture);
    let line = line_of(COUNTING, "marker = total");
    stop_at(&mut debuggee, &fixture.path(), line);

    // three of fifty items. the list is the same at both stops, and three items
    // of it are not evidence that it is
    let cut = Detail {
        children: 3,
        ..Detail::default()
    };
    let before = debuggee.the_query(locals(cut)).expect("the first state");
    carry_on(&mut debuggee, line);
    let after = debuggee.the_query(locals(cut)).expect("the second state");

    let difference = debuggee
        .diff(&before.id, &after.id)
        .expect("both states are this session's");

    let unchanged: Vec<String> = difference
        .unchanged
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(
        !unchanged.iter().any(|name| name.contains("`items`")),
        "half a list is not evidence that the list is unchanged: {unchanged:?}"
    );

    let items = difference
        .not_compared
        .iter()
        .find(|not| matches!(&not.subject, Subject::Variable { name, .. } if name == "items"))
        .expect("`items` was cut short and has to be reported as uncomparable");
    assert!(
        matches!(items.why, WhyNot::Elided { .. }),
        "expected an elision and got {:?}",
        items.why
    );
    let said = items.to_string();
    assert!(said.contains("50 children"), "said {said}");
    assert!(said.contains("larger bound"), "said {said}");

    // and the integers beside it, which were read whole, are still compared
    assert!(
        difference
            .changed
            .iter()
            .any(|change| change.subject.to_string().contains("`total`")),
        "an elision on one name must not stop the rest being compared: {difference:?}"
    );
}

#[test]
fn a_snapshot_outlives_the_stop_it_was_taken_at_and_an_invented_id_is_refused() {
    let fixture = Fixture::new("counting", COUNTING);
    let mut debuggee = launch(&fixture);
    let line = line_of(COUNTING, "marker = total");
    stop_at(&mut debuggee, &fixture.path(), line);

    let before = debuggee
        .the_query(locals(Detail::default()))
        .expect("the first state was read");
    let first = before.state.stop;
    carry_on(&mut debuggee, line);
    let after = debuggee
        .the_query(locals(Detail::default()))
        .expect("the second state was read");

    // the stop the first was taken at has been resumed. a DAP variable
    // reference would now name whatever is at that slot; this names a reading
    // that was already taken, so it is still an answer
    let difference = debuggee
        .diff(&before.id, &after.id)
        .expect("a snapshot is a value and does not expire");
    assert!(
        difference.before.stop_has_ended,
        "the diff has to say that stop {first} has ended"
    );
    assert!(!difference.after.stop_has_ended);

    // what *did* end is asking that stop anything more, which is the existing
    // rule about frame ids and is refused by it
    let stale = debuggee
        .variables(
            FrameId {
                stop: first,
                depth: 0,
            },
            Scope::Local,
            Detail::default(),
        )
        .expect_err("that frame belongs to a stop that has ended");
    assert!(
        stale.to_string().contains("has ended"),
        "the refusal has to say what happened, and said {stale}"
    );

    // and an id this session never gave out is refused by name, with what it
    // does hold — the only way a snapshot id can fail
    let invented = SnapshotId {
        stop: 99,
        digest: "deadbeef".to_string(),
    };
    let refused = debuggee
        .diff(&invented, &after.id)
        .expect_err("this session never minted that");
    let said = refused.to_string();
    assert!(said.contains("99:deadbeef"), "said {said}");
    assert!(said.contains(&after.id.to_string()), "said {said}");

    assert_eq!(finish(&mut debuggee, &fixture), "60 70");
}

#[test]
fn the_same_state_read_twice_is_the_same_id() {
    let fixture = Fixture::new("counting", COUNTING);
    let mut debuggee = launch(&fixture);
    stop_at(
        &mut debuggee,
        &fixture.path(),
        line_of(COUNTING, "marker = total"),
    );

    // content addressing, and the reason it is worth having: an agent that
    // asked the same question twice is told it is holding one answer rather
    // than two that happen to agree
    let once = debuggee
        .the_query(locals(Detail::default()))
        .expect("the state was read");
    let twice = debuggee
        .the_query(locals(Detail::default()))
        .expect("the state was read again");
    assert_eq!(once.id, twice.id);
    assert_eq!(once.state, twice.state);

    // a different question about the same stop is a different state, and so a
    // different id
    let narrower = debuggee
        .the_query(locals(Detail {
            children: 2,
            ..Detail::default()
        }))
        .expect("the state was read a third way");
    assert_ne!(once.id, narrower.id);
    assert_eq!(once.id.stop, narrower.id.stop);

    let difference = debuggee
        .diff(&once.id, &narrower.id)
        .expect("both states are this session's");
    let items = difference
        .not_compared
        .iter()
        .find(|not| matches!(&not.subject, Subject::Variable { name, .. } if name == "items"))
        .expect("one side was cut short");
    assert!(matches!(
        items.why,
        WhyNot::Elided {
            side: bpd_core::Side::After,
            omitted: Omitted::Children { .. },
        }
    ));
}
