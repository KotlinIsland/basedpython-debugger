//! a debuggee is a **named** session, and a request can say which one it is for
//!
//! there is one session per debuggee and there is going to be more than one:
//! the ids an agent mints — a stop's number, and the frame and snapshot ids
//! built on it — all count from one in the process that minted them, so two
//! agents give the same number to different things. what tells them apart is
//! minted by the engine, which is the only thing that can see all of them
//!
//! what is checked here:
//!
//! - every stop the engine reports is named, and named with the session it came
//!   from rather than with a number that happened to be lying about
//! - a request naming a session this engine does not hold is **refused**, and a
//!   request naming none is answered by the only one — which is the same rule
//!   `only_stop` has for stops, one level up
//! - a **second** agent that presents this debuggee's token on this debuggee's
//!   own listener becomes a second session, and one that cannot present it does
//!   not become anything
//! - a session bpd did not start ends without an exit status, and says so
//!   rather than inventing one — and refuses to be terminated, because bpd is
//!   not its parent

use std::io::Write as _;
use std::net::TcpStream;
use std::num::NonZeroU64;
use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

use bpd_core::python::Capabilities;
use bpd_core::{Addressed, Request, Response, Running, SessionId, StopReason};
use bpd_engine::Debuggee;
use bpd_protocol::{TOKEN_LEN, env, frame};
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

/// the one session a debuggee holds
///
/// every test here launches one program, so a debuggee with anything but one
/// session is the test being wrong rather than something to assert about
fn the_session(debuggee: &Debuggee) -> SessionId {
    match debuggee.sessions().as_slice() {
        [only] => *only,
        open => panic!("this debuggee holds {open:?} and the test launched one program"),
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
    let session = the_session(&debuggee);

    let entry = match debuggee.held().as_slice() {
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
    let session = the_session(&debuggee);

    // one that is open is answered, which is what makes the refusal below a
    // statement about the id rather than about naming one at all
    let named = debuggee.dispatch(
        Addressed::to(session, Request::Stack { stop: 1, top: None }),
        &mut bpd_test::reporting::Unreported,
    );
    assert!(
        matches!(named, Ok(Response::Stack(_))),
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
        the_session(&first),
        the_session(&second),
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

// ---- a second session ----------------------------------------------------

/// a program that says nothing and ends, so a session over it is short
const RUNS_AND_ENDS: &str = "total = 1\ntotal += 1\n";

/// how long a wait that is expected to time out is given
///
/// it bounds the wait rather than measuring anything. the first session is
/// held at entry and has nothing to say, so what this decides is how long the
/// engine spends looking at its listener before giving up on it
const A_MOMENT: Duration = Duration::from_secs(5);

/// start a second interpreter pointed at a debuggee's own listener
///
/// exactly what the launcher does for the first one, with the endpoint and the
/// token taken off the debuggee instead of freshly bound. that is the decision
/// the design took in advance and it is what makes this a second session of the
/// same debuggee rather than a second debuggee
fn join(debuggee: &Debuggee, program: &Path) -> Child {
    let staged = bpd_test::agent::staged();
    let listener = debuggee.listener();
    Command::new(&interpreter().executable)
        .arg("-c")
        .arg("import bpd_agent; bpd_agent.main()")
        .env(
            env::ENDPOINT,
            listener
                .endpoint()
                .expect("the retained listener has an address")
                .to_string(),
        )
        .env(env::TOKEN, listener.token_hex())
        .env(env::TARGET, program)
        .env(env::FORM, env::Form::Script.as_str())
        .env("PYTHONPATH", staged.python_path())
        .spawn()
        .expect("a second interpreter was started")
}

/// wait on one session until the debuggee holds `wanted` sessions
///
/// the listener is looked at from inside a wait, which is the whole point of
/// the wait being a poll — so this drives one and reads the count back
fn until_sessions(debuggee: &mut Debuggee, at: SessionId, wanted: usize) {
    for _ in 0..10 {
        if debuggee.sessions().len() >= wanted {
            return;
        }
        match debuggee.dispatch(
            Addressed::to(
                at,
                Request::Wait {
                    deadline: Some(A_MOMENT),
                },
            ),
            &mut bpd_test::reporting::Unreported,
        ) {
            Ok(Response::Ran(Running::StillRunning { .. })) => {}
            Ok(other) => panic!("the held session had nothing to say, and answered {other:?}"),
            Err(error) => panic!("the wait failed: {error}"),
        }
    }
    panic!(
        "the debuggee holds {:?} and a second agent was started against its own \
         listener with its own token",
        debuggee.sessions()
    );
}

fn ran(debuggee: &mut Debuggee, at: SessionId, request: Request) -> Running {
    match debuggee.dispatch(
        Addressed::to(at, request),
        &mut bpd_test::reporting::Unreported,
    ) {
        Ok(Response::Ran(running)) => running,
        Ok(other) => panic!("a run was answered with {other:?}"),
        Err(error) => panic!("the session was not resumed: {error}"),
    }
}

#[test]
fn an_agent_that_presents_the_token_on_the_retained_listener_becomes_a_second_session() {
    let fixture = Fixture::new("two_lines", TWO_LINES);
    let mut debuggee = launch(&fixture);
    let first = the_session(&debuggee);

    let joining = Fixture::new("joining", RUNS_AND_ENDS);
    let mut child = join(&debuggee, &joining.path());
    until_sessions(&mut debuggee, first, 2);

    let open = debuggee.sessions();
    let second = *open
        .iter()
        .find(|id| **id != first)
        .unwrap_or_else(|| unreachable!("two sessions are open and one of them is the first"));
    assert_ne!(first, second, "two sessions were given one id");

    // a request that names none of them is refused rather than answered from
    // whichever came first. that is `only_session`'s rule and this is the first
    // time there has been anything for it to refuse
    let ambiguous = debuggee
        .dispatch(
            Addressed::unnamed(Request::Stack { stop: 1, top: None }),
            &mut bpd_test::reporting::Unreported,
        )
        .expect_err("two sessions are open and the request named neither");
    let said = ambiguous.to_string();
    assert!(said.contains("2 are open"), "said {said}");
    assert!(said.contains("name the session"), "said {said}");

    // and the second one really is a debuggee: it is held at entry, exactly as
    // the first was, and says so on its own connection
    let entry = match ran(
        &mut debuggee,
        second,
        Request::Wait {
            deadline: Some(A_MOMENT),
        },
    ) {
        Running::Stopped { stop, .. } => stop,
        other => panic!("the second agent stops at entry, and the wait answered {other:?}"),
    };
    assert_eq!(entry.reason, StopReason::Entry);
    assert_eq!(
        entry.session, second,
        "a stop is named with the connection it arrived on"
    );

    // both agents number their first stop 1, which is the whole reason a stop
    // carries a session
    assert_eq!(entry.stop, 1);
    assert_eq!(debuggee.held().len(), 2, "{:?}", debuggee.held());

    // ending it is refused by name. bpd is not that process's parent: there is
    // nothing to signal and nothing to reap, and a `terminate` that quietly did
    // nothing is one a client reads as a program that has been ended
    let refused = debuggee
        .interrupt(Some(second))
        .expect("the second session was named")
        .terminate()
        .expect_err("bpd did not start that process");
    let said = refused.to_string();
    assert!(
        said.contains("did not start that process"),
        "the refusal has to say why, and said {said}"
    );
    assert!(
        said.contains(&second.to_string()),
        "the refusal has to name the session, and said {said}"
    );

    // and when it ends, it ends as a program whose exit is not bpd's to read
    match ran(
        &mut debuggee,
        second,
        Request::Run {
            deadline: Some(A_MOMENT),
        },
    ) {
        Running::Ended { rebound } => assert!(rebound.is_empty(), "{rebound:?}"),
        Running::Exited { status, .. } => panic!(
            "bpd is not that process's parent and cannot have read {status} off \
             it. an exit status here is an invented one"
        ),
        other => panic!("the second session did not end: {other:?}"),
    }

    // the refusal afterwards says the program is over **and** that its exit is
    // not bpd's to give, which is a different sentence from an exit code
    let over = debuggee
        .dispatch(
            Addressed::to(second, Request::Stack { stop: 1, top: None }),
            &mut bpd_test::reporting::Unreported,
        )
        .expect_err("the second session's program is over");
    let said = over.to_string();
    assert!(said.contains("the program is over"), "said {said}");
    assert!(
        said.contains("not bpd's to read"),
        "the refusal has to say why there is no exit code, and said {said}"
    );

    child.wait().expect("the second interpreter was reaped");

    // the first session was never touched by any of it
    assert_eq!(debuggee.held().len(), 1, "{:?}", debuggee.held());
    match ran(
        &mut debuggee,
        first,
        Request::Run {
            deadline: Some(A_MOMENT),
        },
    ) {
        Running::Exited { status, .. } => assert!(status.success(), "{status}"),
        other => panic!("the first session did not end: {other:?}"),
    }
}

#[test]
fn a_connection_that_cannot_present_the_token_never_becomes_a_session() {
    let fixture = Fixture::new("two_lines", TWO_LINES);
    let mut debuggee = launch(&fixture);
    let first = the_session(&debuggee);

    let endpoint = debuggee
        .listener()
        .endpoint()
        .expect("the retained listener has an address");

    // three shapes of stranger: one that says nothing at all, one that says
    // something that is not a handshake, and one that presents a token of the
    // right length that is not this session's
    let silent = TcpStream::connect(endpoint).expect("the listener is open");
    let mut noise = TcpStream::connect(endpoint).expect("the listener is open");
    noise.write_all(b"hello?").expect("the stranger wrote");
    let mut wrong = TcpStream::connect(endpoint).expect("the listener is open");
    frame::write_handshake(&mut wrong, &[0x5a; TOKEN_LEN]).expect("the stranger wrote");

    // driven from inside a wait, which is where the listener is looked at
    match debuggee.dispatch(
        Addressed::to(
            first,
            Request::Wait {
                deadline: Some(A_MOMENT),
            },
        ),
        &mut bpd_test::reporting::Unreported,
    ) {
        Ok(Response::Ran(Running::StillRunning { .. })) => {}
        Ok(other) => panic!("the held session had nothing to say, and answered {other:?}"),
        Err(error) => panic!(
            "a local process opening a socket to a loopback port is not a \
             failure of the session, and the wait answered {error}"
        ),
    }

    assert_eq!(
        debuggee.sessions(),
        vec![first],
        "the session token is the only evidence there is, and none of those \
         presented it"
    );
    drop((silent, noise, wrong));

    // and the session it interrupted is untouched
    assert_eq!(debuggee.held().len(), 1);
    to_exit(debuggee);
}
