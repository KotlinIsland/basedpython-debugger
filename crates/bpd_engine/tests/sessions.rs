//! a debuggee is a **named** session, and a request can say which one it is for
//!
//! there is one session per debuggee and there is going to be more than one:
//! the ids an agent mints — a stop's number, and the frame and snapshot ids
//! built on it — all count from one in the process that minted them, so two
//! agents give the same number to different things. what tells them apart is
//! minted by the engine, which is the only thing that can see all of them
//!
//! two claims are checked here, and both are about a single session because a
//! single session is all there is today:
//!
//! - every stop the engine reports is named, and named with the session it came
//!   from rather than with a number that happened to be lying about
//! - a request naming a session this engine does not hold is **refused**, and a
//!   request naming none is answered by the only one — which is the same rule
//!   `only_stop` has for stops, one level up

use std::num::NonZeroU64;

use bpd_core::python::Capabilities;
use bpd_core::{Addressed, Request, Running, SessionId, StopReason};
use bpd_engine::Debuggee;
use bpd_test::debuggee::Fixture;

/// a program with two lines, so it can be stepped once and still be there
const TWO_LINES: &str = "total = 1\ntotal += 1\nprint(total)\n";

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

fn launch(fixture: &Fixture) -> Debuggee {
    match bpd_engine::launch(
        interpreter(),
        &bpd_engine::Program::Script(fixture.path()),
        &[],
    ) {
        Ok(bpd_engine::Launched::Stopped(debuggee)) => debuggee,
        Ok(bpd_engine::Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

fn to_exit(mut debuggee: Debuggee) {
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee ran to completion")
    {
        Running::Exited { .. } => {}
        other => panic!("nothing was set, and the run answered {other:?}"),
    }
}

#[test]
fn every_stop_a_session_reports_is_named_with_that_session() {
    let fixture = Fixture::new("two_lines", TWO_LINES);
    let mut debuggee = launch(&fixture);
    let session = debuggee.session();

    let entry = match debuggee.held() {
        [entry] => entry.clone(),
        held => panic!("one thread is held at entry, and got {held:?}"),
    };
    assert_eq!(entry.reason, StopReason::Entry);
    assert_eq!(
        entry.session, session,
        "the entry stop came from this session and has to say so"
    );

    // and so does one that arrives later, off the same connection. the agent
    // does not know the number and never sends one — it is added where the
    // report arrives, which is the only place that can know which connection
    // it arrived on
    debuggee
        .the_step(bpd_core::StepKind::Over)
        .expect("the only held thread was stepped");
    let stepped = match debuggee
        .wait(&mut bpd_test::reporting::Unreported)
        .expect("the step landed")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("a step over one line answered {other:?}"),
    };
    assert_eq!(stepped.session, session);
    assert_ne!(
        stepped.stop, entry.stop,
        "a second stop is a second number, and the session is what they share"
    );

    to_exit(debuggee);
}

#[test]
fn a_request_naming_a_session_this_debuggee_is_not_is_refused() {
    let fixture = Fixture::new("two_lines", TWO_LINES);
    let mut debuggee = launch(&fixture);
    let session = debuggee.session();

    // one that is open is answered, which is what makes the refusal below a
    // statement about the id rather than about naming one at all
    let named = debuggee.dispatch(
        Addressed::to(session, Request::Stack { stop: 1, top: None }),
        &mut bpd_test::reporting::Unreported,
    );
    assert!(
        matches!(named, Ok(bpd_core::Response::Stack(_))),
        "a request naming the session it is for was answered with {named:?}"
    );

    // and one that names a session nothing has is refused rather than answered
    // from the only one there is. a client holding an id from a session that
    // has ended would otherwise be shown another program's stack
    let elsewhere = SessionId::new(
        NonZeroU64::new(session.get() + 1_000).expect("a session number plus a thousand is not 0"),
    );
    let refused = debuggee
        .dispatch(
            Addressed::to(elsewhere, Request::Stack { stop: 1, top: None }),
            &mut bpd_test::reporting::Unreported,
        )
        .expect_err("no session of this engine is that one");
    let said = refused.to_string();
    assert!(
        said.contains(&elsewhere.to_string()) && said.contains(&session.to_string()),
        "the refusal has to name what was asked for and what is held, and said \
         {said}"
    );
    assert!(
        said.contains("the stack"),
        "the refusal has to name what was asked for, and said {said}"
    );

    // the program is untouched by the refusal: it was refused before anything
    // was asked of the agent, so the thread it was about is still held
    assert_eq!(debuggee.held().len(), 1);

    to_exit(debuggee);
}

#[test]
fn two_debuggees_of_one_engine_are_never_the_same_session() {
    // the whole reason the id exists. both agents mint a stop numbered 1 for
    // their entry stop, and the pair is what tells the two apart
    let fixture = Fixture::new("two_lines", TWO_LINES);
    let first = launch(&fixture);
    let second = launch(&fixture);

    assert_ne!(
        first.session(),
        second.session(),
        "two debuggees were given one session id"
    );
    assert_eq!(
        first.held()[0].stop,
        second.held()[0].stop,
        "both agents count their stops from one, which is what the session id \
         is for"
    );
    assert_ne!(first.held()[0].session, second.held()[0].session);

    to_exit(first);
    to_exit(second);
}
