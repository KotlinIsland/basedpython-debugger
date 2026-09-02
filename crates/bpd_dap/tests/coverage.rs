//! every capability of the core is reachable through the adapter, and
//! everything the debugger says reaches the client
//!
//! the parity rule says no capability exists in one adapter and not the other.
//! the two-sided test of it arrives with the MCP adapter, because with one
//! adapter there is nothing to compare against. this is the one-sided half, and
//! it is the half that fails when a capability is added: `bpd_dap::reach_of`
//! matches `Request` with no catch-all arm, so a new variant does not compile
//! until someone says how DAP gets at it — and then this asserts the claim is
//! true by **driving the adapter** and watching what the session is asked
//!
//! the other direction is here too, and it is the harder one. what the debugger
//! *says* was held by `bpd_core::Reporting`, a trait with no default bodies —
//! which forces an implementation to exist and is satisfied by an empty one. so
//! the fake session says one of everything, the conversation runs to each of the
//! two ways a program can end, and `shown` says what would prove each reached
//! the client. an adapter that took a report and dropped it passes every other
//! test in this file and fails that one
//!
//! the session here is a fake, deliberately. what this test is about is the
//! adapter's own translation: which DAP request becomes which `Request`, and
//! whether the table beside it is honest. that a request really stops a real
//! interpreter is `crates/bpd/tests/dap.rs`, against a real one

use std::collections::BTreeSet;
use std::io::{BufRead as _, BufReader, Read, Write};
use std::sync::{Arc, Mutex};

use bpd_core::parity::mark;
use bpd_core::{
    Addressed, At, Binding, Carried, Content, Did, Entry, Evaluated, Evaluation, Facet, Frame,
    FrameId, Mode, Outcome, Reach, Record, Reported, Reporting, Request, Resolved, Response,
    Running, SessionId, Site, SourceBreakpoint, Stack, Stop, StopReason, Threads, Told, Value,
    Variables, WorldStopped, ran_as, say,
};
use bpd_dap::{
    Configuration, Failed, Interrupt, Launcher, ProgramOutput, Session, Started, carriage_of,
    reach_of, reach_of_facet, surface,
};

/// the interpreter's identity for the one thread this fake ever holds
const THREAD: u64 = 4242;

/// the `.by` the fake's mapped frame is reported in
const MAPPED_TO: &str = "/src/demo.by";

/// the generated python that mapped frame is really running
///
/// the location a front end has to be able to show. a client that was given
/// only `demo.by:11` has no way to check the debugger against the interpreter
const GENERATED_FROM: &str = "/tmp/.by-build/demo.py";

/// a variable the fake's invocation carries, to be found in what was asked for
///
/// the environment is not decoration on a `runInTerminal`: it is how the agent
/// learns where to connect and what token to present, so a client handed the
/// argument vector without it would start an interpreter that never becomes a
/// debuggee
const TERMINAL_MARK: &str = "BPD_ENDPOINT";

/// the session this fake's stops are reported from
///
/// the engine mints one per debuggee and a fake has no engine, so this stands
/// in for one. deliberately not 1: what is under test is that the adapter
/// addresses a request to the session the **stop** came from, and a number that
/// could be a default would not show it
fn session() -> SessionId {
    SessionId::new(std::num::NonZeroU64::new(7).expect("7 is not zero"))
}

/// what the session was asked, and what the requests carried
///
/// the names alone answer "was this capability reached at all". a capability
/// carried *inside* a request needs the payload too: the launch configuration
/// is the only route DAP has to `bpd_core::Detail`, and a table that claimed it
/// without checking would be claiming a mapping nobody had made
#[derive(Debug, Default)]
struct Recorder {
    requests: Vec<&'static str>,
    /// the breakpoint set as it really arrived
    ///
    /// a capability carried *inside* a request needs the payload to be checked:
    /// the names alone say a `setBreakpoints` happened, not that the `after` on
    /// one of them survived the translation
    breakpoints: Vec<SourceBreakpoint>,
    details: Vec<bpd_core::Detail>,
    /// what every request was addressed to, beside what it asked for
    ///
    /// naming the session is a capability like any other, and one the adapter
    /// makes on its own — so the only way to check the table is honest is to
    /// look at what really arrived
    addressed: Vec<(&'static str, Option<SessionId>)>,
}

/// the recorder, shared with the fake session
type Asked = Arc<Mutex<Recorder>>;

#[test]
fn every_capability_of_the_core_is_reachable_through_a_dap_request() {
    let asked = Asked::default();
    let client = drive(&asked);

    // a refusal would mean the conversation never got as far as the request it
    // was written to exercise, and the coverage below would be measuring less
    // than it reads as
    let refused = client.refusals();
    assert!(refused.is_empty(), "the adapter refused: {refused:?}");

    let asked: BTreeSet<&str> = asked
        .lock()
        .expect("the recorder is not poisoned")
        .requests
        .iter()
        .copied()
        .collect();

    for request in surface() {
        let name = request.name();
        match reach_of(&request) {
            Reach::Direct(command) => assert!(
                asked.contains(name),
                "`{name}` is said to be reachable through DAP's `{command}`, and \
                 driving the adapter never asked for it. what was asked: {asked:?}"
            ),
            Reach::OnItsOwn(when) => assert!(
                asked.contains(name),
                "`{name}` is said to be made by the adapter {when}, and driving \
                 it never asked for it. what was asked: {asked:?}"
            ),
            Reach::Unreachable { why } => assert!(
                !asked.contains(name),
                "`{name}` is said to be unreachable from DAP — {why} — and \
                 driving the adapter asked for it anyway"
            ),
            Reach::Composed { of, .. } => {
                assert!(
                    !asked.contains(name),
                    "`{name}` is said to be unusable in this shape, and the \
                     adapter asked for it anyway"
                );
                for part in of {
                    assert!(
                        asked.contains(part),
                        "`{name}` is said to be a composition of `{part}`, and \
                         driving the adapter never asked for that either"
                    );
                }
            }
        }
    }
}

#[test]
fn every_capability_carried_inside_a_request_is_reached_or_says_why_not() {
    let asked = Asked::default();
    let client = drive(&asked);
    let recorded = asked.lock().expect("the recorder is not poisoned");

    for facet in Facet::ALL {
        match reach_of_facet(facet) {
            Reach::Direct(_) | Reach::OnItsOwn(_) | Reach::Composed { .. } => {}
            Reach::Unreachable { why } => assert!(
                !why.is_empty(),
                "`{}` is said to be out of DAP's reach and no reason is given",
                facet.name()
            ),
        }
    }

    // three of the four DAP reaches are read off this conversation below.
    // `Facet::Terminal` is the fourth and is read off a conversation of its own,
    // in `a_client_that_owns_a_terminal_is_asked_to_start_the_program_in_it`:
    // asking for one changes what a launch **is**, so it is a different launch
    // rather than another request in this one

    // where the task was created, read off the console rather than the table
    // that claims it. an editor that walked the frames and dropped this would
    // show a stack that is true and says nothing about who scheduled it
    let scheduled: Vec<&serde_json::Value> = client
        .events("output")
        .into_iter()
        .filter(|event| {
            event["body"]["output"]
                .as_str()
                .is_some_and(|text| text.contains("inside an asyncio task, scheduled at"))
        })
        .collect();
    assert!(
        !scheduled.is_empty(),
        "`{}` is said to reach a client and the console never said it",
        Facet::Scheduling.name()
    );
    for event in scheduled {
        let said = event["body"]["output"].as_str().unwrap_or_default();
        assert!(
            said.contains("schedule"),
            "the line has to name the frame that scheduled it, and said {said}"
        );
    }

    // the sequence, read off what really arrived at the session. the adapter
    // resolves `after: {path, line}` to whatever id that breakpoint holds now,
    // and one that parsed the field and dropped it would send an ordinary
    // breakpoint and look identical here without this
    let sequenced: Vec<Option<u32>> = recorded
        .breakpoints
        .iter()
        .map(|breakpoint| breakpoint.after)
        .collect();
    assert!(
        sequenced.iter().any(Option::is_some),
        "`{}` never reached the session: {sequenced:?}",
        Facet::Sequenced.name()
    );
    // and it named the breakpoint on line 3, which is the one the conversation
    // pointed at — an `after` resolved to the wrong id is worse than none
    let gate = recorded
        .breakpoints
        .iter()
        .find(|breakpoint| breakpoint.line == 3)
        .map(|breakpoint| breakpoint.id);
    assert_eq!(
        sequenced.iter().flatten().copied().next(),
        gate,
        "`after` resolved to an id that is not the breakpoint on line 3"
    );

    // the claim under test: DAP has nowhere on a request to put the bounds on a
    // value read, so the launch configuration is the route — and the
    // conversation set `children` to 7 there. a session asked for the default
    // 100 instead would mean the configuration was parsed and not used
    assert!(
        !recorded.details.is_empty(),
        "nothing read a value, so the launch configuration's bounds reached nothing"
    );
    for detail in &recorded.details {
        assert_eq!(
            detail.children, 7,
            "the launch configuration asked for 7 children per container and \
             the session was asked for {detail:?}"
        );
    }

    // and the location a mapped frame is really at. the frame said `demo.by:11`
    // and a client shown only that cannot check the debugger against the
    // interpreter — `origin` is DAP's own field for where a source came from,
    // and the generated file and line have to be in it
    let origins: Vec<String> = client
        .frames()
        .iter()
        .filter_map(|frame| frame["source"]["origin"].as_str().map(ToString::to_string))
        .collect();
    assert!(
        origins
            .iter()
            .any(|origin| origin.contains(GENERATED_FROM) && origin.contains("14")),
        "the stack reported `{MAPPED_TO}` and no source said where the \
         interpreter really is: {origins:?}"
    );

    // and the capability the adapter reaches on its own: a DAP request has no
    // field for a session, so the adapter puts one on every request it makes.
    // a request about a stop goes to the session that stop was reported from
    assert!(
        recorded
            .addressed
            .iter()
            .any(|(_, addressed)| *addressed == Some(session())),
        "nothing the adapter asked for named the session its stops came from: \
         {:?}",
        recorded.addressed
    );

    // and nothing was ever addressed anywhere else. this fake holds one
    // session, so a request naming another would be the adapter inventing one
    for (name, addressed) in &recorded.addressed {
        if let Some(named) = addressed {
            assert_eq!(
                *named,
                session(),
                "`{name}` was addressed to {named}, and the only session there \
                 is is {}",
                session()
            );
        }
    }
}

#[test]
fn a_client_that_owns_a_terminal_is_asked_to_start_the_program_in_it() {
    // the claim `reach_of_facet` makes about `Facet::Terminal`, read against
    // what the adapter really sent. an adapter that took `console` and launched
    // on the debug console anyway would pass every other test in this file
    let asked = terminal_launch(true);

    let request = asked
        .messages
        .iter()
        .find(|message| message["type"] == "request" && message["command"] == "runInTerminal")
        .unwrap_or_else(|| {
            panic!(
                "`console` asked for a terminal and the client was never asked \
                 for one: {:?}",
                asked.messages
            )
        });
    assert_eq!(
        request["arguments"]["kind"], "integrated",
        "the client decides where a terminal goes, and the word goes through: {request}"
    );

    // the whole command line, and the environment with it. either half missing
    // is an interpreter that starts and never becomes a debuggee
    let arguments = request["arguments"]["args"]
        .as_array()
        .unwrap_or_else(|| panic!("the client is handed an argument vector: {request}"));
    assert!(
        arguments.iter().any(|argument| argument
            .as_str()
            .is_some_and(|word| word.contains("bpd_agent"))),
        "the command line the client was handed does not start the agent: {request}"
    );
    assert_eq!(
        request["arguments"]["env"][TERMINAL_MARK], "127.0.0.1:4711",
        "the environment the agent reads did not go with it: {request}"
    );
    assert_eq!(
        request["arguments"]["cwd"], "/tmp",
        "a client that ran the command somewhere else would run a different \
         program for a relative path: {request}"
    );

    // and the launch was answered only after the client said it had started it
    let launched = asked
        .messages
        .iter()
        .position(|message| message["type"] == "response" && message["command"] == "launch")
        .unwrap_or_else(|| panic!("the launch was never answered: {:?}", asked.messages));
    let requested = asked
        .messages
        .iter()
        .position(|message| message["type"] == "request" && message["command"] == "runInTerminal")
        .expect("the request was found above");
    assert!(
        requested < launched,
        "the launch was answered before the client had been asked to start the \
         program, so bpd would have been waiting for an agent it had not asked \
         anybody to start"
    );
    assert_eq!(
        asked.messages[launched]["success"], true,
        "the client started the program and the launch was refused anyway: {:?}",
        asked.messages[launched]
    );
}

#[test]
fn a_client_with_no_terminal_is_refused_at_launch_and_asked_for_nothing() {
    // `supportsRunInTerminalRequest` is a **client** capability. a client
    // without it cannot be asked, so a launch that asked anyway would sit
    // waiting for an agent nothing was ever going to start — which is a hang
    // with no cause, and the reason this is refused at the one moment nothing
    // has happened yet
    let asked = terminal_launch(false);

    assert!(
        !asked
            .messages
            .iter()
            .any(|message| message["command"] == "runInTerminal"),
        "a client that cannot answer it was asked anyway: {:?}",
        asked.messages
    );

    let launched = asked
        .messages
        .iter()
        .find(|message| message["type"] == "response" && message["command"] == "launch")
        .unwrap_or_else(|| panic!("the launch was never answered: {:?}", asked.messages));
    assert_eq!(launched["success"], false, "{launched}");
    let said = launched["message"]
        .as_str()
        .unwrap_or_else(|| panic!("a refusal says why: {launched}"));
    assert!(
        said.contains("runInTerminal"),
        "the refusal names what the client is missing, and said {said:?}"
    );
    assert!(
        said.contains("take `console` out"),
        "and what to do instead, and said {said:?}"
    );
}

#[test]
fn a_replacement_under_a_live_frame_that_is_not_a_boolean_is_refused_rather_than_guessed_at() {
    // its own test rather than a line in the shared conversation, because that
    // conversation is read by a test asserting nothing in it was refused — and
    // this is a refusal on purpose
    //
    // what it is protecting is not tidiness. `evenUnderALiveFrame` decides
    // whether the process ends up running two versions of one function, and an
    // adapter that read `"yes"` as truthy would make that trade on a typo
    let client = drive(&Asked::default());

    // the console half of the claim in `reach_of_facet`, read rather than
    // believed. the body carries the report because serde does that for free;
    // the console line is what a user watching a debug console sees, and it is
    // the same place a refusal's reason goes — succeeding here costs what
    // failing usually saves, so it belongs in the same stream
    let told: Vec<&serde_json::Value> = client
        .events("output")
        .into_iter()
        .filter(|event| {
            event["body"]["output"]
                .as_str()
                // the applied case by its own words. "two versions of one
                // function" alone matches the **refusal** in the same
                // conversation, which says the same thing about why it refused
                // — an assertion that cannot tell those apart passes with the
                // report it is checking for deleted
                .is_some_and(|text| text.contains("bpd replaced the code under a live frame"))
        })
        .collect();
    assert!(
        !told.is_empty(),
        "a replacement was applied under a live frame and the console never \
         said what it cost"
    );
    for event in told {
        assert_eq!(
            event["body"]["category"], "important",
            "DAP has a category for exactly this and it was not used: {event}"
        );
    }

    let said = client
        .refusal_of("bpd/replaceCode")
        .expect("a non-boolean was read as an answer rather than refused");
    assert!(
        said.contains("two versions of one function"),
        "the refusal has to say what the flag would have cost, or a client \
         cannot tell it from a schema quibble. it said {said}"
    );
}

#[test]
fn a_stop_that_ends_takes_its_references_with_it() {
    let asked = Asked::default();
    let client = drive(&asked);

    // the conversation asks for the same `variablesReference` again after the
    // thread it belonged to has been stepped. DAP's handle looks the same
    // before and after a resume, and answering one is how a debugger reports a
    // frame the program has already left
    let stale = client
        .refusal_of("variables")
        .expect("the stale reference was refused");
    assert!(
        stale.contains("ask for the stack again"),
        "the refusal has to say what to do instead, and said {stale}"
    );
}

#[test]
fn a_blind_spot_reaches_the_client_where_a_collapsed_console_cannot_hide_it() {
    // the message that stops silence being evidence. it is the one notice a
    // client must not file beside ordinary console chatter, because everything
    // else bpd says is a positive claim and this one is about an absence
    let client = drive(&Asked::default());

    let said: Vec<&serde_json::Value> = client
        .events("output")
        .into_iter()
        .filter(|event| {
            event["body"]["output"]
                .as_str()
                .is_some_and(|text| text.contains(mark::BLIND_TO))
        })
        .collect();
    assert!(
        !said.is_empty(),
        "the interpreter has a blind spot and the client was never told"
    );

    for event in said {
        assert_eq!(
            event["body"]["category"], "important",
            "DAP has a category for exactly this and it was not used: {event}"
        );
    }
}

#[test]
fn a_child_the_program_started_reaches_the_client_as_the_debuggers_own_words() {
    let client = drive(&Asked::default());

    let said: Vec<&serde_json::Value> = client
        .events("output")
        .into_iter()
        .filter(|event| {
            event["body"]["output"]
                .as_str()
                .is_some_and(|text| text.contains(mark::CHILD))
        })
        .collect();
    assert!(
        !said.is_empty(),
        "the program started a child and the client was never told"
    );

    for event in said {
        // `console` and not `stdout`. the program did not write this — bpd did
        // — and a client that showed it among the program's own output would
        // be putting words in the debuggee's mouth
        assert_eq!(
            event["body"]["category"], "console",
            "a notice of bpd's shown as program output: {event}"
        );
        // no `source` and no `line`. the hook runs on whatever thread made the
        // child and sees what the program asked the operating system for, not
        // where it was asked — a location invented from the frame that happened
        // to be running is a location nobody can act on
        assert!(
            event["body"]["source"].is_null() && event["body"]["line"].is_null(),
            "the notice claimed a place in the program: {event}"
        );
        // the child in this conversation is one bpd was told to take up, and a
        // client told otherwise reads `bpd is not debugging it` and then gets a
        // `startDebugging` for the same child. DAP has one place to put it —
        // the sentence — so that is where it is looked for
        let output = event["body"]["output"]
            .as_str()
            .unwrap_or_else(|| unreachable!("the events were filtered on this string"));
        assert!(
            output.contains(mark::TAKING_UP),
            "the child is being taken up as a session of its own and the \
             console said: {output}"
        );
        assert!(
            !output.contains("not debugging"),
            "the same fact, from the other side: {output}"
        );
    }
}

#[test]
fn the_thread_model_reaches_the_client_on_every_stop() {
    let client = drive(&Asked::default());
    let stopped = client.events("stopped");
    assert!(!stopped.is_empty(), "nothing stopped");

    // this conversation sets `stopTheWorld`, and the fake holds nothing in a C
    // call — so the world really did stop and the client is entitled to be told
    for event in &stopped {
        assert_eq!(
            event["body"]["allThreadsStopped"], true,
            "the world was stopped and the client was told otherwise: {event}"
        );
    }
}

#[test]
fn everything_the_debugger_says_really_reaches_the_client() {
    // the half of the parity rule a trait could not hold. `bpd_core::Reporting`
    // has no default bodies, so an implementation of it has to exist — and an
    // empty one satisfies that. what fails an empty one is this: the fake says
    // one of everything, and the claim in `bpd_dap::carriage_of` is read against
    // what the client was really sent
    let told = everything_said();

    for said in Told::ALL {
        match carriage_of(said) {
            Carried::Pushed(where_it_goes) => assert!(
                shown(said, &told),
                "`{}` is said to reach a client as {where_it_goes}, and driving \
                 the adapter never sent it",
                said.name()
            ),
            // never DAP's answer, and `crates/bpd/tests/parity.rs` is where that
            // is stated. a fact held back for a request a DAP client is not
            // obliged to make is a fact nobody is told
            Carried::Pulled(where_it_goes) => panic!(
                "DAP holds `{}` back for {where_it_goes}, and it has an event \
                 stream",
                said.name()
            ),
            Carried::Nowhere { why } => assert!(
                !shown(said, &told),
                "`{}` is said to reach no DAP client — {why} — and driving the \
                 adapter sent it anyway",
                said.name()
            ),
        }
    }
}

/// what the client is sent across both ways the program can end
///
/// two conversations rather than one, because the two outcomes that end a
/// program cannot both happen to it. the messages are read together: a fact is
/// carried if it turned up in either, and each check below names something
/// distinctive enough that the two cannot be confused
fn everything_said() -> Transcript {
    let mut messages = drive(&Asked::default()).messages;
    messages.extend(drive_until(&Asked::default(), Told::Ended).messages);
    Transcript { messages }
}

/// what would show one thing the debugger says reached a DAP client
///
/// exhaustive and with no catch-all arm, so a fact added to the core does not
/// compile here until someone says what its arrival looks like. it is
/// deliberately not the table in `bpd_dap::coverage`: that one says what the
/// adapter *would* send, and this is read against what it really sent — saying
/// it is reached is not the same as reaching it
fn shown(said: Told, told: &Transcript) -> bool {
    match said {
        // the program's own words, written where the program would have written
        // them
        Told::Logged => told.output("stdout", mark::LOGGED),
        Told::Pausing => told.output("console", &mark::RUNNING.to_string()),
        Told::Spawned => told.output("console", mark::CHILD),
        Told::BlindSpot => told.output("important", mark::BLIND_TO),
        // a reverse request rather than an event, because the client has to
        // start a session for the held child rather than merely hear about it
        Told::Attached => told.messages.iter().any(|message| {
            message["type"] == "request"
                && message["command"] == "startDebugging"
                && message.to_string().contains(&mark::JOINED.to_string())
        }),
        Told::Stopped => !told.events("stopped").is_empty(),
        // the code **and** the state of the program's output. the exit `ran`
        // makes is one whose output is still being written, and an adapter that
        // sent the code alone would leave a client reading every later line as
        // part of a run that had already ended
        Told::Exited => {
            told.events("exited").iter().any(|event| {
                event["body"]["exitCode"] == serde_json::json!(i64::from(mark::EXIT_CODE))
            }) && told.output("console", mark::STILL_WRITING)
        }
        Told::Finishing => told.output("console", &mark::HELD_AT_THE_END.to_string()),
        // `terminated` alone is not evidence: an exit produces one too. the
        // reason on the console is the part that only this outcome writes
        Told::Ended => {
            !told.events("terminated").is_empty() && told.output("console", "not bpd's to read")
        }
        // nothing at all, which is what `carriage_of` says. the fake really does
        // answer a wait with it, so a client that started being told would show
        // the wait's own length turning up somewhere
        Told::StillRunning => told
            .messages
            .iter()
            .any(|message| message.to_string().contains(&mark::WAITED_MS.to_string())),
    }
}

/// run one whole DAP conversation against the fake session
///
/// one conversation rather than one per test: the point of it is coverage of
/// the whole surface, and three tests reading three different parts of the same
/// transcript is what that looks like
fn drive(asked: &Asked) -> Transcript {
    drive_until(asked, Told::Exited)
}

/// launch asking for a terminal, as a client that can answer for one and as one
/// that cannot
///
/// a conversation of its own rather than a branch of the one above, because the
/// launch is the whole of what differs: the client is asked to start the
/// program, and everything after the answer is the session the other
/// conversation drives at length
///
/// the client here **answers** the reverse request, which nothing else in this
/// file does. what that proves is the order — the launch is not answered until
/// the program has been started, so bpd never waits for an agent nobody was
/// asked to start
fn terminal_launch(advertised: bool) -> Transcript {
    let (to_adapter, mut client_writes) = std::io::pipe().expect("a pipe is available");
    let (client_reads, from_adapter) = std::io::pipe().expect("a pipe is available");

    let served = std::thread::spawn(move || {
        bpd_dap::serve(
            &Fake {
                asked: Asked::default(),
                ending: Told::Exited,
            },
            Box::new(to_adapter),
            Box::new(from_adapter),
            &bpd_dap::Reachable::Nowhere,
        )
    });

    let mut reader = Messages::new(client_reads);
    let mut seq = 0;
    let send = |writer: &mut std::io::PipeWriter, message: serde_json::Value| {
        let body = message.to_string();
        write!(writer, "Content-Length: {}\r\n\r\n{body}", body.len())
            .expect("the adapter is reading");
        writer.flush().expect("the adapter is reading");
    };

    seq += 1;
    send(
        &mut client_writes,
        serde_json::json!({
            "seq": seq, "type": "request", "command": "initialize",
            "arguments": { "supportsRunInTerminalRequest": advertised },
        }),
    );
    reader.response_to(seq);

    seq += 1;
    let launch = seq;
    send(
        &mut client_writes,
        serde_json::json!({
            "seq": launch, "type": "request", "command": "launch",
            "arguments": { "program": "/tmp/fake.py", "console": "integratedTerminal" },
        }),
    );

    // read until the launch is answered, answering the reverse request on the
    // way if one arrives. a client that waited for its own answer first would
    // deadlock against an adapter waiting for it, which is the shape of this
    // exchange and the reason it is driven rather than assumed
    loop {
        let message = reader.next_message();
        let answering = message["type"] == "request" && message["command"] == "runInTerminal";
        let sequence = message["seq"]
            .as_i64()
            .expect("every message carries a seq");
        let answered = message["type"] == "response" && message["request_seq"] == launch;
        reader.seen.push(message);
        if answering {
            send(
                &mut client_writes,
                serde_json::json!({
                    "type": "response", "seq": 0, "request_seq": sequence,
                    "success": true, "command": "runInTerminal",
                    // the pid of the shell the client started it in. bpd reads
                    // neither: it is not that process's parent, and a number a
                    // client reported is not something a debugger may treat as
                    // a process it can see
                    "body": { "shellProcessId": 4242 },
                }),
            );
        }
        if answered {
            break;
        }
    }

    seq += 1;
    send(
        &mut client_writes,
        serde_json::json!({
            "seq": seq, "type": "request", "command": "disconnect", "arguments": {},
        }),
    );
    reader.response_to(seq);
    drop(client_writes);

    served
        .join()
        .expect("the adapter did not panic")
        .expect("the adapter served the whole conversation");

    Transcript {
        messages: reader.seen,
    }
}

/// the same conversation, run to one of the two ways a program can end
///
/// `ending` is what the fake answers the last wait with. a program bpd started
/// exits and bpd reads the status; one that connected to bpd's listener is over
/// and bpd is not its parent, so there is no status to read — and the two are
/// told to the client differently, which is the whole of why both are driven
#[expect(
    clippy::too_many_lines,
    reason = "it is a transcript. splitting a conversation into helpers hides \
              the order the messages go in, which is the thing under test"
)]
fn drive_until(asked: &Asked, ending: Told) -> Transcript {
    let (to_adapter, mut client_writes) = std::io::pipe().expect("a pipe is available");
    let (client_reads, from_adapter) = std::io::pipe().expect("a pipe is available");

    let served = std::thread::spawn({
        let asked = Arc::clone(asked);
        move || {
            // reachable, because `debugChildren` is refused on a transport a
            // second session cannot arrive on — and this conversation asks for
            // it, which is the only route DAP has to that capability
            bpd_dap::serve(
                &Fake { asked, ending },
                Box::new(to_adapter),
                Box::new(from_adapter),
                &bpd_dap::Reachable::At {
                    host: "127.0.0.1".to_string(),
                    port: 4711,
                    header: "X-Bpd-Token",
                    token: "00".repeat(32),
                },
            )
        }
    });

    let mut reader = Messages::new(client_reads);
    let mut seq = 0;
    let mut send =
        |writer: &mut std::io::PipeWriter, command: &str, arguments: serde_json::Value| {
            seq += 1;
            let body = serde_json::json!({
                "seq": seq, "type": "request", "command": command, "arguments": arguments,
            })
            .to_string();
            write!(writer, "Content-Length: {}\r\n\r\n{body}", body.len())
                .expect("the adapter is reading");
            writer.flush().expect("the adapter is reading");
            seq
        };

    let mut answer = |writer: &mut std::io::PipeWriter,
                      reader: &mut Messages,
                      command: &str,
                      arguments: serde_json::Value| {
        let sent = send(writer, command, arguments);
        reader.response_to(sent)
    };

    answer(
        &mut client_writes,
        &mut reader,
        "initialize",
        // one of the two **client** capabilities this adapter reads — the other
        // is `supportsRunInTerminalRequest`, which this conversation does not
        // ask for and `terminal_launch` does. without it a debugged fork is
        // refused, because a client that cannot start the session
        // `startDebugging` asks for would leave the child held
        serde_json::json!({ "supportsStartDebuggingRequest": true }),
    );
    answer(
        &mut client_writes,
        &mut reader,
        "launch",
        serde_json::json!({
            "program": "/tmp/fake.py",
            "stopOnEntry": true,
            "stopTheWorld": true,
            // the only route DAP has to `Request::DebugChildren`, which the
            // adapter sends at `configurationDone`
            "debugChildren": true,
            // the only route DAP has to `bpd_core::Detail`, which is a
            // capability of the core that no DAP *request* can carry
            "variables": { "children": 7 },
        }),
    );
    answer(
        &mut client_writes,
        &mut reader,
        "setBreakpoints",
        serde_json::json!({
            "source": { "path": "/tmp/fake.py" },
            "breakpoints": [
                { "line": 3, "condition": "total > 1" },
                // and one that waits for the first, named by **file and line**
                // rather than by id: this adapter re-mints breakpoint ids on
                // every `setBreakpoints`, so an id from an earlier response is
                // already stale
                { "line": 9, "after": { "path": "/tmp/fake.py", "line": 3 } },
            ],
        }),
    );
    answer(
        &mut client_writes,
        &mut reader,
        "setExceptionBreakpoints",
        serde_json::json!({ "filters": ["uncaught"] }),
    );
    answer(
        &mut client_writes,
        &mut reader,
        "configurationDone",
        serde_json::json!({}),
    );

    let threads = answer(
        &mut client_writes,
        &mut reader,
        "threads",
        serde_json::json!({}),
    );
    let thread = threads["body"]["threads"][0]["id"].clone();

    let stack = answer(
        &mut client_writes,
        &mut reader,
        "stackTrace",
        serde_json::json!({ "threadId": thread }),
    );
    // frame 0 is the django template frame and frame 1 is the python one it is
    // rendered by. both are driven, because what a client may do with the two
    // is different and the adapter decides which from the frame's own kind
    let template_frame = stack["body"]["stackFrames"][0]["id"].clone();
    let frame = stack["body"]["stackFrames"][1]["id"].clone();

    let layers = answer(
        &mut client_writes,
        &mut reader,
        "scopes",
        serde_json::json!({ "frameId": template_frame }),
    );
    let layer = layers["body"]["scopes"][0]["variablesReference"].clone();
    answer(
        &mut client_writes,
        &mut reader,
        "variables",
        serde_json::json!({ "variablesReference": layer }),
    );
    answer(
        &mut client_writes,
        &mut reader,
        "evaluate",
        serde_json::json!({ "expression": "greeting|upper", "frameId": template_frame }),
    );

    let scopes = answer(
        &mut client_writes,
        &mut reader,
        "scopes",
        serde_json::json!({ "frameId": frame }),
    );
    let local = scopes["body"]["scopes"][0]["variablesReference"].clone();

    answer(
        &mut client_writes,
        &mut reader,
        "variables",
        serde_json::json!({ "variablesReference": local }),
    );
    answer(
        &mut client_writes,
        &mut reader,
        "setVariable",
        serde_json::json!({ "variablesReference": local, "name": "total", "value": "9" }),
    );
    answer(
        &mut client_writes,
        &mut reader,
        "evaluate",
        serde_json::json!({ "expression": "total + 1", "frameId": frame }),
    );
    // moving where the program will carry on from. `goto` carries a target
    // rather than a line, and `gotoTargets` is where one comes from — so the
    // two are driven together, and the target is used against the thread it
    // was minted for
    let targets = answer(
        &mut client_writes,
        &mut reader,
        "gotoTargets",
        // the file the fake's *python* frame is running. frame 0 of that stack
        // is a django template frame, and a template line is not a place the
        // interpreter can be moved to
        serde_json::json!({ "source": { "path": "/tmp/fake.py" }, "line": 1 }),
    );
    let target = targets["body"]["targets"][0]["id"].clone();
    assert!(
        target.is_number(),
        "a held thread executing the file the client asked about has a target, \
         and the adapter offered {targets}"
    );
    answer(
        &mut client_writes,
        &mut reader,
        "goto",
        serde_json::json!({ "threadId": thread, "targetId": target }),
    );
    // **twice, and the order matters.** the first is a client that has never
    // heard of `bpd/restarting`, which is every client by default: it gets the
    // narration on the console. adding the opt-in below without this one removed
    // the only exercise the console path had, and deleting that path went
    // unnoticed
    answer(
        &mut client_writes,
        &mut reader,
        "restartFrame",
        serde_json::json!({ "frameId": frame }),
    );
    // and then a client that opts in, which is the path the console does **not**
    // take — the sentence saying the forced return runs no cleanup used to exist
    // only on the console side, so the clients most likely to act on it were the
    // ones told least
    answer(
        &mut client_writes,
        &mut reader,
        "bpd/understands",
        serde_json::json!({ "events": ["bpd/restarting"] }),
    );
    answer(
        &mut client_writes,
        &mut reader,
        "restartFrame",
        serde_json::json!({ "frameId": frame }),
    );

    // what is provable about a frame's names, which `variables` has nowhere to
    // put: a DAP `Variable` carries a value and has no field for how long that
    // value can be relied on
    let proved = answer(
        &mut client_writes,
        &mut reader,
        "bpd/facts",
        serde_json::json!({
            "frameId": frame,
            "names": ["total", "self.limit"],
            "limit": { "depth": 3 },
        }),
    );
    assert_eq!(
        proved["body"]["proved"][0]["name"], "total",
        "the answer names the names it was asked about: {proved}"
    );
    assert_eq!(
        proved["body"]["proved"][0]["stability"]["stability"], "permanent",
        "every fact carries how long it stays true, which is the half a \
         `variables` answer cannot carry: {proved}"
    );

    // making the process run the file that is on disk. DAP's own `restart`
    // throws the process away, which is the opposite of this, so it is an
    // extension — and the parity rule is why it is here at all: an editor is
    // where the edit that makes it worth having was made
    let replaced = answer(
        &mut client_writes,
        &mut reader,
        "bpd/replaceCode",
        serde_json::json!({ "file": "/tmp/fake.py" }),
    );
    assert_eq!(
        replaced["body"]["files"][0]["outcome"]["replaced"], "refused",
        "the fake refuses, and the adapter has to carry the whole answer rather \
         than a yes or a no: {replaced}"
    );

    // the same request with the guarantee traded away, which is the only route
    // to a live frame being applied rather than refused. an editor that carried
    // the flag and dropped the report would make that trade for its user in
    // silence, so what is read here is the report and not the table claiming it
    let traded = answer(
        &mut client_writes,
        &mut reader,
        "bpd/replaceCode",
        serde_json::json!({ "file": "/tmp/fake.py", "evenUnderALiveFrame": true }),
    );
    assert_eq!(
        traded["body"]["files"][0]["outcome"]["replaced"], "applied",
        "the flag is what turns a live frame from a refusal into a report, and \
         the answer was {traded}"
    );
    let left_behind = traded["body"]["files"][0]["outcome"]["still_running"]
        .as_array()
        .unwrap_or_else(|| panic!("the report of what it cost is missing: {traded}"));
    assert!(
        !left_behind.is_empty(),
        "the replacement was applied under a live frame and named no frame: \
         {traded}"
    );
    assert_eq!(
        left_behind[0]["function"], "main",
        "the frame has to be named, or nobody can tell which code is doubled: \
         {traded}"
    );

    // and a value that is not a boolean, refused on purpose. `refusals` exempts
    // this one by what it says rather than by its command, so an *unintended*
    // `bpd/replaceCode` refusal is still visible
    answer(
        &mut client_writes,
        &mut reader,
        "bpd/replaceCode",
        serde_json::json!({ "file": "/tmp/fake.py", "evenUnderALiveFrame": "yes" }),
    );

    // the whole of a stop in one call, and the difference between two of them.
    // DAP's own way of reading state is the tree walk above, and it keeps it —
    // this is the same capability an agent's front end has, which is what the
    // parity rule requires
    // the one mode that turns off what makes bpd fast, and the window it fills.
    // deliberately not DAP's `stepBack`, which means "put the program back" —
    // a trail says only where it went
    answer(
        &mut client_writes,
        &mut reader,
        "bpd/record",
        serde_json::json!({ "on": true }),
    );
    let went = answer(
        &mut client_writes,
        &mut reader,
        "bpd/trail",
        serde_json::json!({}),
    );
    assert_eq!(
        went["body"]["dropped"], 12,
        "the window's edge has to reach the client, or its oldest entry reads \
         as where the recording began: {went}"
    );

    // why an object is still alive — a custom request, because DAP's model of
    // state is a tree walked downwards from a frame and this is asked upwards
    // from an object
    let holding = answer(
        &mut client_writes,
        &mut reader,
        "bpd/retainers",
        serde_json::json!({ "frameId": frame, "expression": "total" }),
    );
    assert!(
        holding["body"]["coverage"]["not_python"]
            .as_str()
            .is_some_and(|said| said.contains("bpd's own")),
        "the answer has to say the debugger is among what the walk cannot see: \
         {holding}"
    );

    let described = answer(
        &mut client_writes,
        &mut reader,
        "bpd/state",
        serde_json::json!({
            "threadId": thread,
            "query": {
                "frames": 1,
                "scopes": ["local"],
                "expressions": [ { "expression": "total + 1" } ],
                "detail": { "children": 7 },
            },
        }),
    );
    let id = described["body"]["id"]
        .as_str()
        .expect("a state carries the id it is kept under")
        .to_string();
    answer(
        &mut client_writes,
        &mut reader,
        "bpd/diff",
        serde_json::json!({ "before": id, "after": id }),
    );

    // a whole investigation in one call. DAP has no request of its own for
    // this and never will, so it is an extension — and the parity rule is why
    // it exists here at all: a capability an agent has and a person does not is
    // the thing that rule prevents
    answer(
        &mut client_writes,
        &mut reader,
        "bpd/runScript",
        serde_json::json!({
            "threadId": thread,
            "steps": [
                { "step": "log", "note": "starting" },
                {
                    "step": "while",
                    "predicate": { "expression": "total > 1" },
                    "limit": 3,
                    "body": [ { "step": "step_over" } ],
                },
            ],
            "budget": { "steps": 8, "wall_ms": 500, "bytes": 4096 },
        }),
    );

    answer(
        &mut client_writes,
        &mut reader,
        "pause",
        serde_json::json!({}),
    );

    answer(
        &mut client_writes,
        &mut reader,
        "next",
        serde_json::json!({ "threadId": thread }),
    );

    // the thread has been stepped, so the reference from the stop it was held
    // at names a frame that has moved
    answer(
        &mut client_writes,
        &mut reader,
        "variables",
        serde_json::json!({ "variablesReference": local }),
    );

    answer(
        &mut client_writes,
        &mut reader,
        "continue",
        serde_json::json!({ "threadId": thread }),
    );
    // the program has been let go, and what it does next arrives on the
    // connection rather than as the answer to anything. reading to the end of it
    // is what puts every outcome the waiting produced in the transcript: a
    // client that hung up first would be measuring the race between its own
    // `disconnect` and the adapter's wait loop
    reader.until_event("terminated");
    answer(
        &mut client_writes,
        &mut reader,
        "disconnect",
        serde_json::json!({}),
    );

    drop(client_writes);
    served
        .join()
        .expect("the adapter did not panic")
        .expect("the adapter served the whole conversation");

    Transcript {
        messages: reader.seen,
    }
}

/// everything the adapter said
struct Transcript {
    messages: Vec<serde_json::Value>,
}

impl Transcript {
    fn events(&self, event: &str) -> Vec<&serde_json::Value> {
        self.messages
            .iter()
            .filter(|message| message["type"] == "event" && message["event"] == event)
            .collect()
    }

    /// whether an `output` event of one category said something
    ///
    /// the category is part of it rather than beside it. DAP's categories are
    /// how a client decides where a line goes, and a notice of bpd's shown as
    /// the program's own output is a claim about the debuggee that is not true
    fn output(&self, category: &str, said: &str) -> bool {
        self.events("output").into_iter().any(|event| {
            event["body"]["category"] == category
                && event["body"]["output"]
                    .as_str()
                    .is_some_and(|text| text.contains(said))
        })
    }

    /// every stack frame the adapter reported, across every `stackTrace`
    fn frames(&self) -> Vec<&serde_json::Value> {
        self.messages
            .iter()
            .filter(|message| message["command"] == "stackTrace")
            .filter_map(|message| message["body"]["stackFrames"].as_array())
            .flatten()
            .collect()
    }

    fn refusals(&self) -> Vec<String> {
        self.messages
            .iter()
            .filter(|message| message["type"] == "response" && message["success"] == false)
            .filter(|message| message["command"] != "variables")
            // the one refusal the conversation makes on purpose, exempted by
            // what it says rather than by its command: a `bpd/replaceCode`
            // refused for any *other* reason is a conversation that did not
            // reach what it was written to exercise, and stays visible
            .filter(|message| !message.to_string().contains("evenUnderALiveFrame"))
            .map(ToString::to_string)
            .collect()
    }

    fn refusal_of(&self, command: &str) -> Option<String> {
        self.messages
            .iter()
            .find(|message| {
                message["type"] == "response"
                    && message["success"] == false
                    && message["command"] == command
            })
            .map(|message| message["message"].as_str().unwrap_or_default().to_string())
    }
}

/// the framed messages the adapter writes, read one at a time
struct Messages {
    input: BufReader<std::io::PipeReader>,
    seen: Vec<serde_json::Value>,
}

impl Messages {
    fn new(input: std::io::PipeReader) -> Self {
        Self {
            input: BufReader::new(input),
            seen: Vec::new(),
        }
    }

    /// read until an event of one name arrives, keeping everything on the way
    fn until_event(&mut self, event: &str) {
        loop {
            let message = self.next_message();
            let arrived = message["type"] == "event" && message["event"] == event;
            self.seen.push(message);
            if arrived {
                return;
            }
        }
    }

    /// read until the answer to `seq` arrives, keeping everything on the way
    fn response_to(&mut self, seq: i64) -> serde_json::Value {
        loop {
            let message = self.next_message();
            self.seen.push(message.clone());
            if message["type"] == "response" && message["request_seq"] == seq {
                return message;
            }
        }
    }

    fn next_message(&mut self) -> serde_json::Value {
        let mut length = None;
        loop {
            let mut line = String::new();
            let read = self
                .input
                .read_line(&mut line)
                .expect("the adapter is writing");
            assert_ne!(read, 0, "the adapter hung up mid-conversation");
            let line = line.trim_end_matches(['\r', '\n']).to_string();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                length = Some(value.parse().expect("a length is a number"));
            }
        }

        let mut body = vec![0; length.expect("every message carries its length")];
        self.input
            .read_exact(&mut body)
            .expect("the adapter wrote what it promised");
        serde_json::from_slice(&body).expect("the adapter writes json")
    }
}

/// a session that answers everything and records what it was asked
///
/// it holds one thread, hands out one frame, and has two stops in it: the entry
/// stop, and the one a step lands at
struct Fake {
    asked: Asked,
    /// which of the two ways a program can end this one ends
    ending: Told,
}

/// the state the fake keeps across a session
struct FakeSession {
    asked: Asked,
    held: Vec<Stop>,
    /// stops still to come, innermost first
    remaining: Vec<Stop>,
    /// what a wait is answered with once there are no stops left, in order
    ///
    /// the last one repeats, because the adapter goes on waiting until the
    /// program is over and the two outcomes that end one are the last entry
    over: Vec<Told>,
    /// whether the reports have been made, which happens once
    said: bool,
}

impl Fake {
    /// the session this fake hands out, however it was asked for
    fn session(&self, ending: Told) -> Started {
        Started::Stopped(Box::new(FakeSession {
            asked: Arc::clone(&self.asked),
            held: vec![stop_at(1, StopReason::Entry)],
            remaining: vec![stop_at(
                2,
                StopReason::Stepped {
                    kind: bpd_core::StepKind::Over,
                    file: "/tmp/fake.py".to_string(),
                    line: 4,
                },
            )],
            // a deadline that passes and a program that runs out with threads
            // still held are both things a real one does before it ends, and
            // neither ends it — so both come before whichever ending this
            // conversation is driving
            over: vec![Told::StillRunning, Told::Finishing, ending],
            said: false,
        }))
    }
}

impl Launcher for Fake {
    fn launch(
        &self,
        _configuration: &Configuration,
        _output: Arc<dyn ProgramOutput>,
    ) -> Result<Started, Failed> {
        Ok(self.session(self.ending))
    }

    /// the same session, with the client asked to start the program
    ///
    /// what the fake is for here is the **asking**: the invocation is recorded
    /// so that the conversation can read what the client was handed, and the
    /// session that comes back is the one every other test in this file drives.
    /// that a real interpreter really starts on a real terminal this way is
    /// `crates/bpd/tests/dap.rs`
    fn launch_in_terminal(
        &self,
        _configuration: &Configuration,
        ask: &mut dyn FnMut(&bpd_dap::Invocation) -> Result<(), Failed>,
    ) -> Result<Started, Failed> {
        let invocation = bpd_dap::Invocation {
            arguments: vec![
                "python3".to_string(),
                "-c".to_string(),
                "import bpd_agent; bpd_agent.main()".to_string(),
            ],
            env: vec![(TERMINAL_MARK.to_string(), "127.0.0.1:4711".to_string())],
            directory: "/tmp".to_string(),
        };
        ask(&invocation)?;
        Ok(self.session(self.ending))
    }

    /// this fake launches, and nothing takes up a session of it
    ///
    /// a second session only exists when a program forked under a debugger, and
    /// this fake has no program at all — so an `attach` naming one is a request
    /// for something that was never there
    fn attach(&self, session: u64) -> Result<Started, Failed> {
        Err(format!("this fake holds no session {session}").into())
    }
}

fn stop_at(number: u64, reason: StopReason) -> Stop {
    Reported {
        stop: number,
        thread: THREAD,
        reason,
        holding: Vec::new(),
    }
    .in_session(session())
}

fn integer(text: &str) -> Value {
    Value {
        kind: "int".to_string(),
        content: Content::Int {
            text: text.to_string(),
            omitted: None,
        },
    }
}

/// what a fake session answers a state query with
fn snapshot(stop: u64, query: &bpd_core::StateQuery) -> bpd_core::Snapshot {
    bpd_core::Snapshot {
        id: bpd_core::SnapshotId {
            stop,
            digest: format!("{stop}{stop}ff"),
        },
        state: bpd_core::State {
            stop,
            thread: THREAD,
            reason: StopReason::Entry,
            frames: vec![bpd_core::FrameState {
                frame: Frame {
                    id: FrameId { stop, depth: 0 },
                    file: "/tmp/fake.py".to_string(),
                    line: 3,
                    mapping: None,
                    kind: bpd_core::FrameKind::Python {
                        function: "main".to_string(),
                        first_line: 1,
                    },
                },
                source: None,
                scopes: query
                    .scopes
                    .iter()
                    .map(|scope| bpd_core::ScopeState {
                        scope: *scope,
                        entries: vec![Entry {
                            name: "total".to_string(),
                            value: integer("1"),
                        }],
                        unbound: Vec::new(),
                        unreadable: Vec::new(),
                        omitted: Vec::new(),
                    })
                    .collect(),
            }],
            depth: 1,
            values: query
                .expressions
                .iter()
                .map(|wanted| bpd_core::Answer {
                    expression: wanted.expression.clone(),
                    frame: wanted.frame,
                    result: Evaluated::Value {
                        value: integer("9"),
                    },
                })
                .collect(),
            left_out: Vec::new(),
            mode: Mode::NonStop,
            bytes: 120,
        },
    }
}

impl Session for FakeSession {
    fn held(&self) -> Vec<Stop> {
        self.held.clone()
    }

    fn interrupt(&self) -> Result<Box<dyn Interrupt>, Failed> {
        Ok(Box::new(FakeInterrupt {
            asked: Arc::clone(&self.asked),
        }))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per capability of the core, which is what makes a \
                  capability the fake does not answer a compile error here too"
    )]
    fn dispatch(
        &mut self,
        asked: Addressed,
        reporting: &mut dyn Reporting,
    ) -> Result<Response, Failed> {
        let Addressed { session, request } = asked;

        // everything a running program says arrives while it is running, so a
        // wait is where all of it is reported. the fake says one of each, once,
        // which is what puts the adapter's route for every one of them in this
        // conversation rather than leaving it to a test that needs a real
        // interpreter to fork
        if matches!(request, Request::Wait { .. }) && !self.said {
            self.said = true;
            say(reporting);
        }
        {
            let mut recorder = self.asked.lock().expect("the recorder is not poisoned");
            recorder.requests.push(request.name());
            recorder.addressed.push((request.name(), session));
            if let Request::SetBreakpoints { breakpoints } = &request {
                recorder.breakpoints.clone_from(breakpoints);
            }
            match &request {
                Request::Variables { detail, .. }
                | Request::Evaluate { detail, .. }
                | Request::SetVariable { detail, .. } => recorder.details.push(*detail),
                Request::Query { query, .. } => recorder.details.push(query.detail),
                _ => {}
            }
        }

        Ok(match request {
            Request::SetBreakpoints { breakpoints } => Response::BreakpointsResolved {
                resolved: breakpoints
                    .iter()
                    .map(|breakpoint| Resolved {
                        // the answer says what the request asked for, so a
                        // sequenced breakpoint comes back waiting — an adapter
                        // that dropped that half would show a solid breakpoint
                        // on a line the interpreter is not watching
                        waiting_for: breakpoint.after,
                        id: breakpoint.id,
                        binding: Binding::Bound {
                            line: breakpoint.line,
                            sites: vec![Site {
                                qualname: "main".to_string(),
                                first_line: 1,
                                offset: 0,
                            }],
                            evaluation: Evaluation::Expression,
                        },
                    })
                    .collect(),
            },
            Request::SetExceptionBreakpoints { raised, uncaught } => {
                Response::ExceptionBreakpoints(bpd_core::ExceptionBreakpoints { raised, uncaught })
            }
            Request::DebugChildren { on } => Response::DebuggingChildren { on },
            Request::Run { .. } => {
                unreachable!("DAP resumes and waits, and never composes the two")
            }
            Request::Wait { .. } => {
                let running = match self.remaining.pop() {
                    Some(next) => {
                        self.held.push(next.clone());
                        Running::Stopped {
                            stop: next,
                            rebound: Vec::new(),
                        }
                    }
                    None => ran_as(self.next_outcome()),
                };
                Response::Ran(running)
            }
            Request::Resume { .. } | Request::Step { .. } => {
                self.held.clear();
                Response::Resumed {
                    threads: vec![THREAD],
                }
            }
            Request::Pause => Response::Pausing {
                running: vec![THREAD],
            },
            Request::Threads { settle } => Response::Threads(Threads {
                threads: vec![bpd_core::ThreadState {
                    thread: THREAD,
                    held: self.held.first().map(|stop| stop.stop),
                    at: None,
                    progress: bpd_core::Progress::Held,
                }],
                settle,
                mode: Mode::NonStop,
            }),
            Request::StopTheWorld { .. } => Response::WorldStopped(WorldStopped {
                held: vec![THREAD],
                native: Vec::new(),
            }),
            // two frames, and deliberately one of each kind: a client has to be
            // able to tell a frame the interpreter has from one bpd synthesised
            // over a django template node, and the only place that shows is a
            // stack with both in it
            Request::Stack { stop, .. } => Response::Stack(Stack {
                // the fake's stack is inside a task, because an empty
                // `scheduled_by` is the half a front end can drop with nothing
                // failing — every ordinary stack has one
                in_a_task: true,
                scheduling_cut: false,
                // the fake's stack is inside a task, because an empty
                // `scheduled_by` is the half a front end can drop with nothing
                // failing — every ordinary stack has one
                scheduled_by: vec![bpd_core::Scheduling {
                    file: "/src/app.py".to_string(),
                    line: 42,
                    function: "schedule".to_string(),
                }],
                frames: vec![
                    Frame {
                        id: FrameId { stop, depth: 0 },
                        file: "/tmp/page.html".to_string(),
                        line: 2,
                        mapping: None,
                        kind: bpd_core::FrameKind::Template {
                            node: "VariableNode".to_string(),
                            python: FrameId { stop, depth: 1 },
                        },
                    },
                    Frame {
                        id: FrameId { stop, depth: 1 },
                        file: "/tmp/fake.py".to_string(),
                        line: 3,
                        mapping: None,
                        kind: bpd_core::FrameKind::Python {
                            function: "main".to_string(),
                            first_line: 1,
                        },
                    },
                    // a frame of a basedpython build, reported at the `.by`
                    // line it came from. the generated location is the fact a
                    // client has to be able to reach — see `MAPPED_TO`
                    Frame {
                        id: FrameId { stop, depth: 2 },
                        file: MAPPED_TO.to_string(),
                        line: 11,
                        mapping: Some(bpd_core::Mapping::FromSource {
                            generated: bpd_core::Located {
                                file: std::path::PathBuf::from(GENERATED_FROM),
                                line: 14,
                            },
                        }),
                        kind: bpd_core::FrameKind::Python {
                            function: "main".to_string(),
                            first_line: 1,
                        },
                    },
                ],
                depth: 3,
                mode: Mode::NonStop,
            }),
            // the shape rather than the substance: that a script really drives
            // a real interpreter is `crates/bpd_engine/tests/scripts.rs`. what
            // is under test here is that a DAP client can reach the capability
            // at all, and gets the whole transcript back
            Request::RunScript { stop, script } => Response::Transcript(bpd_core::Transcript {
                at_most: script.at_most(),
                bytes: 120,
                records: vec![Record {
                    step: "1".to_string(),
                    at: At::of(&stop_at(stop, StopReason::Entry)),
                    did: Did::Logged {
                        note: "the fake ran it".to_string(),
                    },
                }],
                rebound: Vec::new(),
                outcome: Outcome::Ran,
            }),
            Request::Variables { .. } => Response::Variables(Variables {
                entries: vec![Entry {
                    name: "total".to_string(),
                    value: integer("1"),
                }],
                unbound: Vec::new(),
                unreadable: Vec::new(),
                omitted: Vec::new(),
                mode: Mode::NonStop,
            }),
            Request::Facts { names, .. } => Response::Facts(bpd_core::Facts {
                proved: names
                    .iter()
                    .map(|name| bpd_core::Fact {
                        name: name.clone(),
                        scope: bpd_core::Scope::Local,
                        observed: bpd_core::Observed::IsInt {
                            text: "1".to_string(),
                        },
                        stability: bpd_core::Stability::Permanent,
                    })
                    .collect(),
                silent: Vec::new(),
                mode: Mode::NonStop,
            }),
            Request::TemplateContext { .. } => {
                Response::TemplateContext(bpd_core::TemplateContext {
                    layers: vec![
                        bpd_core::ContextLayer {
                            index: 0,
                            entries: vec![Entry {
                                name: "greeting".to_string(),
                                value: integer("1"),
                            }],
                            omitted: Vec::new(),
                        },
                        bpd_core::ContextLayer {
                            index: 1,
                            entries: vec![Entry {
                                name: "greeting".to_string(),
                                value: integer("2"),
                            }],
                            omitted: Vec::new(),
                        },
                    ],
                    mode: Mode::NonStop,
                })
            }
            Request::Evaluate { .. } | Request::SetVariable { .. } => {
                Response::Evaluated(Evaluated::Value {
                    value: integer("9"),
                })
            }
            // both jumps answer the same shape, and the fake answers the
            // interesting one: a breakpoint on the destination that will not
            // fire, and a local the move bound to `None`. that a real
            // interpreter really moves is `crates/bpd_engine/tests/jumps.rs`
            Request::SetNextStatement { line, .. } => Response::Jumped(bpd_core::Jumped {
                at: bpd_core::Where {
                    file: "/tmp/fake.py".to_string(),
                    line,
                    function: "main".to_string(),
                },
                outcome: bpd_core::Jump::Moved {
                    from: 3,
                    bound_to_none: vec!["total".to_string()],
                    unannounced: vec![1],
                },
                mode: Mode::NonStop,
            }),
            // arranged rather than refused, because that is the branch that
            // ends the stop: the adapter has to answer and then **not** send a
            // `stopped` event, and a fake that only ever refused would let a
            // build which sent one anyway pass
            Request::RestartFrame { .. } => {
                Response::Restarted(bpd_core::Restarted::Arranged(bpd_core::Restarting {
                    frame: bpd_core::Where {
                        file: "/tmp/fake.py".to_string(),
                        line: 3,
                        function: "work".to_string(),
                    },
                    exit_line: 5,
                    caller: bpd_core::Where {
                        file: "/tmp/fake.py".to_string(),
                        line: 1,
                        function: "main".to_string(),
                    },
                    // a frame that really was inside a block and really does
                    // hold a finaliser, because both notes are conditional now
                    // and a fixture that was neither would assert nothing
                    inside_a_block: true,
                    finalising: vec!["conn".to_string()],
                    disturbed: vec!["got".to_string()],
                    bound_to_none: vec!["later".to_string()],
                    unannounced: vec![1],
                    mode: Mode::NonStop,
                }))
            }
            // the fake refuses, because a refusal is the answer with something
            // in it: it carries *every* reason rather than the first, and the
            // adapter has to hand the whole of it over. that a real interpreter
            // really replaces code is
            // `crates/bpd_engine/tests/replacement.rs`
            Request::Record { on, .. } => Response::Recording {
                on,
                held: 3,
                // a window that has dropped entries, because that is the half a
                // front end can leave out with nothing failing — a trail whose
                // start is not where the recording began reads as the whole run
                dropped: 12,
            },
            Request::Trail => Response::Trail(bpd_core::Trail {
                went: vec![bpd_core::Visited {
                    file: "/src/app.py".to_string(),
                    line: 12,
                    function: "handle".to_string(),
                    thread: THREAD,
                    held: Some(bpd_core::Kept::whole(Vec::new())),
                }],
                dropped: 12,
                recording: true,
                window: 100_000,
            }),
            Request::Retainers { .. } => Response::Retainers(bpd_core::Retainers {
                of: "a list holding 1".to_string(),
                found: vec![bpd_core::Retainer {
                    kind: "dict".to_string(),
                    described: "a dict holding 3".to_string(),
                    through: Some("the value under `session`".to_string()),
                }],
                // always carried, because a front end that dropped it would
                // turn this into a narrower question's answer without failing
                coverage: bpd_core::Coverage {
                    untracked: "objects the collector does not track never appear here".to_string(),
                    not_python: "a reference held by C or rust cannot be found, bpd's own included"
                        .to_string(),
                    mode: Mode::NonStop,
                },
            }),
            Request::ReplaceCode {
                files,
                remap,
                even_under_a_live_frame,
            } if even_under_a_live_frame =>
            // the trade, taken. a front end that carried the flag and dropped
            // the report would make it for its user in silence, so the fake
            // answers with the one thing that says it was made
            {
                Response::Replaced(bpd_core::Replacements {
                    files: files
                        .into_iter()
                        .map(|file| bpd_core::Replaced {
                            file,
                            outcome: bpd_core::Replacement::Applied {
                                changed: Vec::new(),
                                unchanged: Vec::new(),
                                still_running: vec![bpd_core::StillRunning {
                                    function: "main".to_string(),
                                    frame: bpd_core::LiveFrame::Thread {
                                        thread: THREAD,
                                        line: 3,
                                        held: Some(1),
                                    },
                                }],
                            },
                        })
                        .collect(),
                    rebound: Vec::new(),
                    mode: Some(Mode::NonStop),
                    // the other half of what a front end has to carry: a remap
                    // that happened and was not reported is a client believing
                    // its `.by` lines came out of the table it last saw
                    remapped: remap.then(|| bpd_core::Remapped {
                        directory: std::path::PathBuf::from("/tmp/build"),
                        files: 2,
                        breakpoints: 1,
                    }),
                })
            }
            Request::ReplaceCode { files, .. } => {
                Response::Replaced(bpd_core::Replacements {
                    files: files
                        .into_iter()
                        .map(|file| bpd_core::Replaced {
                            file,
                            outcome: bpd_core::Replacement::Refused {
                                because: vec![bpd_core::Unreplaceable::Running {
                                    function: "main".to_string(),
                                    frame: bpd_core::LiveFrame::Thread {
                                        thread: THREAD,
                                        line: 3,
                                        held: Some(1),
                                    },
                                }],
                            },
                        })
                        .collect(),
                    rebound: Vec::new(),
                    mode: Some(Mode::NonStop),
                    // nothing is remapped when nothing was applied, which is the
                    // rule the engine keeps and the fake has to keep with it
                    remapped: None,
                })
            }
            // the same two capabilities an agent's front end reaches, reached by
            // an editor. that a query really reads a real interpreter is
            // `crates/bpd_engine/tests/queries.rs`
            Request::Query { stop, query } => Response::State(snapshot(stop, &query)),
            Request::Diff { before, after } => Response::Difference(bpd_core::difference(
                &snapshot(before.stop, &bpd_core::StateQuery::default()),
                &snapshot(after.stop, &bpd_core::StateQuery::default()),
                &self.held.iter().map(|stop| stop.stop).collect::<Vec<u64>>(),
            )),
        })
    }
}

impl FakeSession {
    /// the next thing a wait is answered with once there are no stops left
    ///
    /// the last one repeats. the adapter waits for as long as the program is
    /// there, and what makes it stop is one of the two endings — which is the
    /// last entry, and the one this conversation was run for
    fn next_outcome(&mut self) -> Told {
        if self.over.len() > 1 {
            self.over.remove(0)
        } else {
            self.over[0]
        }
    }
}

struct FakeInterrupt {
    asked: Asked,
}

impl Interrupt for FakeInterrupt {
    fn deliver(&mut self, request: &Request) -> Result<(), Failed> {
        self.asked
            .lock()
            .expect("the recorder is not poisoned")
            .requests
            .push(request.name());
        Ok(())
    }

    fn terminate(&mut self) -> Result<(), Failed> {
        Ok(())
    }
}

#[test]
fn a_client_that_opts_into_the_restart_event_is_told_what_the_restart_really_did() {
    // the event path skips the console, so every sentence the adapter has about
    // a restart has to be **on** the event. the one saying the forced return
    // runs no cleanup was console-only, which left the clients most likely to
    // act on it as the ones told least
    //
    // nothing else in this file names `bpd/restarting`, so without this the gap
    // reopens silently
    let told = everything_said();
    let events = told.events("bpd/restarting");
    assert!(
        !events.is_empty(),
        "the conversation opts into the event and restarts a frame"
    );
    let event = events[0];

    assert_eq!(
        event["body"]["restarting"]["restarted"], "arranged",
        "the tag is what tells an arranged restart from a refused one, and the \
         two leave the thread in different places: {event}"
    );
    let notes = event["body"]["notes"]
        .as_array()
        .unwrap_or_else(|| panic!("the event carries the notes: {event}"));
    let said = notes
        .iter()
        .filter_map(|note| note.as_str())
        .collect::<Vec<_>>()
        .join(" | ");

    for wanted in [
        // that the thread is gone, which is the whole difference from a `goto`
        "the thread has been let go",
        // the **whole** line, not only the call
        "the **whole** line",
        // and the one that used to be console-only
        "no block cleanup",
        "no `__exit__`",
        // the cleanup that does run, which the sentence above used to deny
        "`__del__`",
    ] {
        assert!(said.contains(wanted), "expected {wanted:?} in {said:?}");
    }

    // and the **console** carries the same sentence for the restart made before
    // the opt-in. both paths, one conversation, so neither can be deleted
    // without a test going red
    assert!(
        told.output("console", "no block cleanup"),
        "a client that never heard of the event was not narrated at"
    );
    assert!(
        told.output("console", "`__del__`"),
        "the console path lost the one cleanup that does run"
    );
    assert!(
        told.output("console", "the **whole** line"),
        "the console narration lost what really runs again"
    );
}
