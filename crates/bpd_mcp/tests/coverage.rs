//! every capability of the core is reachable through a tool, and a control tool
//! really does return the stop it produced
//!
//! the session here is a fake, deliberately. what is under test is the server's
//! own translation: which tool becomes which `Request`, what the answer looks
//! like, and whether the table beside it is honest. that a tool really stops a
//! real interpreter is `crates/bpd/tests/mcp.rs`, against a real one
//!
//! the fake is also how the **timeout** shape is exercised without waiting for
//! one: it answers the last wait with `Running::StillRunning`, which is what an
//! engine whose deadline passed returns. that a real deadline really passes is
//! the other test's job

use std::collections::BTreeSet;
use std::io::{BufRead as _, BufReader, Write};
use std::sync::{Arc, Mutex};

use bpd_core::{
    Addressed, At, Binding, Content, Detail, Did, Entry, Evaluated, Evaluation, Exit, Facet, Frame,
    FrameId, HitCondition, Mode, Outcome, Reach, Record, Reported, Reporting, Request, Resolved,
    Response, Running, SessionId, Site, SourceBreakpoint, Stack, Stop, StopReason, Threads, Value,
    Variables, WorldStopped,
};
use bpd_mcp::{
    Configuration, Failed, Launcher, ProgramOutput, Session, Started, reach_of, reach_of_facet,
    surface, tools,
};

/// the interpreter's identity for the one thread this fake ever holds
const THREAD: u64 = 4242;

/// the session this fake's stops are reported from
///
/// the engine mints one per debuggee and a fake has no engine, so this stands
/// in for one. deliberately not 1: what is under test is that the server
/// addresses a request to the session the **stop** came from, and a number that
/// could be a default would not show it
fn session() -> SessionId {
    SessionId::new(std::num::NonZeroU64::new(7).expect("7 is not zero"))
}

/// what the session was asked, and what the requests carried
///
/// the names alone answer "was this capability reached at all". a capability
/// carried *inside* a request needs the payload: a hit condition is a field of a
/// breakpoint, and the bounds on a value read are a field of three requests
#[derive(Debug, Default)]
struct Recorder {
    requests: Vec<&'static str>,
    breakpoints: Vec<SourceBreakpoint>,
    details: Vec<Detail>,
    /// what every request was addressed to, beside what it asked for
    ///
    /// naming the session is a capability like any other, and one the server
    /// makes on its own — so the only way to check the table is honest is to
    /// look at what really arrived
    addressed: Vec<(&'static str, Option<SessionId>)>,
}

type Asked = Arc<Mutex<Recorder>>;

#[test]
fn every_capability_of_the_core_is_reachable_through_a_tool() {
    let asked = Asked::default();
    let transcript = drive(&asked);

    let failed = transcript.failures();
    assert!(failed.is_empty(), "the server refused: {failed:?}");

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
            Reach::Direct(tool) => assert!(
                asked.contains(name),
                "`{name}` is said to be reachable through `{tool}`, and driving \
                 the server never asked for it. what was asked: {asked:?}"
            ),
            Reach::OnItsOwn(when) => assert!(
                asked.contains(name),
                "`{name}` is said to be made by the server {when}, and driving \
                 it never asked for it. what was asked: {asked:?}"
            ),
            Reach::Composed { of, .. } => {
                for part in of {
                    assert!(
                        asked.contains(part),
                        "`{name}` is said to be a composition of `{part}`, and \
                         driving the server never asked for that either"
                    );
                }
            }
            Reach::Unreachable { why } => assert!(
                !asked.contains(name),
                "`{name}` is said to be out of an agent's reach — {why} — and \
                 driving the server asked for it anyway"
            ),
        }
    }
}

#[test]
fn every_capability_carried_inside_a_request_reaches_the_session() {
    let asked = Asked::default();
    drive(&asked);
    let recorded = asked.lock().expect("the recorder is not poisoned");

    for facet in Facet::ALL {
        assert!(
            reach_of_facet(facet).reaches(),
            "`{}` is said to be out of an agent's reach, and an MCP tool takes \
             JSON Schema input — there is nothing it cannot carry",
            facet.name()
        );
    }

    // the capability DAP has no route for at all. the conversation asked for
    // every third qualifying hit, and a session that was handed `None` would
    // mean the tool parsed it and dropped it
    let hits: Vec<Option<HitCondition>> = recorded
        .breakpoints
        .iter()
        .map(|breakpoint| breakpoint.hits)
        .collect();
    assert!(
        hits.iter().any(|hits| matches!(
            hits,
            Some(HitCondition::Every { count }) if count.get() == 3
        )),
        "the hit condition never reached the session: {hits:?}"
    );

    // and the bounds on a value read, which reach DAP only through the launch
    // configuration and reach an agent per call
    assert!(
        !recorded.details.is_empty(),
        "nothing read a value, so the bounds reached nothing"
    );
    for detail in &recorded.details {
        assert_eq!(
            detail.children, 7,
            "the tool call asked for 7 children per container and the session \
             was asked for {detail:?}"
        );
    }

    // and the capability the server reaches on its own: no tool takes a session
    // argument, because there is one session — so the server addresses every
    // request it makes, and one about a stop goes to the session that stop was
    // reported from
    assert!(
        recorded
            .addressed
            .iter()
            .any(|(_, addressed)| *addressed == Some(session())),
        "nothing the server asked for named the session its stops came from: \
         {:?}",
        recorded.addressed
    );

    // and nothing was ever addressed anywhere else. this fake holds one
    // session, so a request naming another would be the server inventing one
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
fn a_control_tool_returns_the_stop_it_produced_and_nothing_arrives_as_an_event() {
    let transcript = drive(&Asked::default());

    // the headline claim: a step is one call and one answer. every message the
    // server wrote answers something the client asked, so there is no event
    // stream to correlate and nothing to poll
    for message in &transcript.messages {
        assert!(
            message.get("id").is_some(),
            "this server writes nothing but answers, and wrote {message}"
        );
    }

    let stepped = transcript.result_of("step_over");
    assert_eq!(stepped["outcome"], "stopped", "step_over gave {stepped}");
    assert_eq!(stepped["stop"], 2);
    // the frames come with it. an agent that had to ask again for where it
    // stopped would be paying the round trip this interface exists to remove
    // and every frame says which kind it is. frame 0 of this stack is a django
    // template frame, which the interpreter has no frame for at all — an agent
    // that read it as python would go looking for a `.html` file's locals
    assert_eq!(stepped["frames"][0]["kind"], "template");
    assert_eq!(stepped["frames"][0]["node"], "VariableNode");
    assert_eq!(stepped["frames"][0]["python_frame"], 1);
    assert_eq!(stepped["frames"][0]["frame"], 0);
    assert_eq!(stepped["frames"][1]["kind"], "python");
    assert_eq!(stepped["frames"][1]["function"], "main");
}

#[test]
fn a_deadline_that_passes_is_a_timeout_and_is_never_dressed_as_a_stop() {
    let transcript = drive(&Asked::default());
    let timed_out = transcript.result_of("wait");

    assert_eq!(
        timed_out["outcome"], "timed_out",
        "the wait gave {timed_out}"
    );
    assert!(
        timed_out.get("frames").is_none() && timed_out.get("thread").is_none(),
        "a timeout carries no location at all, and carried {timed_out}"
    );
    let note = timed_out["note"]
        .as_str()
        .expect("a timeout says what it is");
    assert!(note.contains("still running"), "said {note}");
    assert!(
        note.contains("pause"),
        "a timeout has to say what can be done instead, and said {note}"
    );
}

#[test]
fn an_argument_a_tool_does_not_take_is_refused_with_the_name_of_it() {
    let asked = Asked::default();
    let transcript = drive_with(&asked, &[("wait", serde_json::json!({ "deadlineMs": 5 }))]);

    let refused = transcript
        .failures()
        .into_iter()
        .next()
        .expect("`deadlineMs` is not an argument of `wait`");
    assert!(refused.contains("deadlineMs"), "said {refused}");
    assert!(
        refused.contains("tools/list"),
        "a refusal has to say where the truth is, and said {refused}"
    );
}

#[test]
fn a_method_this_server_does_not_implement_is_refused_by_name() {
    let transcript = drive(&Asked::default());
    let refused = transcript
        .error_of(&serde_json::json!(9_001))
        .expect("`completion/complete` is not implemented");
    assert_eq!(refused["code"], -32601);
    let message = refused["message"].as_str().expect("an error says why");
    assert!(
        message.contains("tools/call"),
        "a refusal has to name what does exist, and said {message}"
    );
}

#[test]
fn what_the_program_printed_comes_back_on_the_answer_that_let_it_run() {
    let transcript = drive(&Asked::default());

    // the server's stdout is the protocol, so the debuggee's cannot be. a
    // program whose output vanished would be a debugger that swallowed the
    // evidence
    let ran = transcript.result_of("continue_");
    let said = ran["output"]["text"]
        .as_str()
        .expect("the program printed something and the answer carries it");
    assert!(said.contains("the program said this"), "carried {ran}");
    assert!(said.contains("[stdout]"), "which stream it was on: {ran}");
}

// ---- the conversation ----------------------------------------------------

/// run one whole MCP session against the fake, plus any extra calls
///
/// one conversation rather than one per test: the point of it is coverage of
/// the whole surface, and several tests reading different parts of the same
/// transcript is what that looks like
fn drive(asked: &Asked) -> Transcript {
    drive_with(asked, &[])
}

#[expect(
    clippy::too_many_lines,
    reason = "it is one conversation covering the whole tool surface, and \
              splitting it would put the calls and the order they are answered \
              in in two places"
)]
fn drive_with(asked: &Asked, extra: &[(&str, serde_json::Value)]) -> Transcript {
    let (to_server, client_writes) = std::io::pipe().expect("a pipe is available");
    let (client_reads, from_server) = std::io::pipe().expect("a pipe is available");

    let served = std::thread::spawn({
        let asked = Arc::clone(asked);
        move || {
            bpd_mcp::serve(
                &mut Fake { asked },
                Box::new(to_server),
                Box::new(from_server),
            )
        }
    });

    let mut client = Client {
        writes: client_writes,
        reads: Messages::new(client_reads),
        seq: 0,
    };

    client.ask(
        "initialize",
        &serde_json::json!({ "protocolVersion": bpd_mcp::PROTOCOL_VERSION }),
    );
    client.notify("notifications/initialized");
    client.ask("tools/list", &serde_json::json!({}));

    client.call(
        "launch",
        &serde_json::json!({ "program": "/tmp/fake.py", "python": "python3" }),
    );
    client.call(
        "set_breakpoints",
        &serde_json::json!({
            "breakpoints": [ {
                "file": "/tmp/fake.py",
                "line": 3,
                "condition": "total > 1",
                // the capability DAP has no route for
                "hits": { "hits": "every", "count": 3 },
            } ],
        }),
    );
    client.call(
        "set_exception_breakpoints",
        &serde_json::json!({ "uncaught": true }),
    );

    // what a forked child of the program does, and the tool that makes a second
    // session learnable. both name the session explicitly, which is what puts
    // the `session` argument's route through this conversation rather than
    // leaving it to a test that needs a real fork
    client.call(
        "debug_children",
        &serde_json::json!({ "on": true, "session": session().get() }),
    );
    client.call("sessions", &serde_json::json!({}));
    client.call("threads", &serde_json::json!({ "settle_ms": 10 }));
    client.call("stop_the_world", &serde_json::json!({}));
    client.call("stack", &serde_json::json!({}));
    client.call(
        "variables",
        &serde_json::json!({ "scope": "local", "detail": { "children": 7 } }),
    );
    client.call(
        "template_context",
        &serde_json::json!({ "frame": 0, "detail": { "children": 7 } }),
    );
    client.call(
        "evaluate",
        &serde_json::json!({ "expression": "total + 1", "detail": { "children": 7 } }),
    );
    client.call(
        "set_variable",
        &serde_json::json!({
            "scope": "local", "name": "total", "value": "9",
            "detail": { "children": 7 },
        }),
    );

    // moving where the program will carry on from, and re-entering the frame
    // from the top. neither resumes anything: the thread is still held after
    // both, which is why the conversation goes on asking about the same stop
    client.call(
        "set_next_statement",
        &serde_json::json!({ "frame": 1, "line": 2 }),
    );
    client.call("restart_frame", &serde_json::json!({ "frame": 1 }));

    // making the process run the file that is on disk. it is about the process
    // rather than about a held thread, so it names neither a stop nor a frame
    client.call(
        "replace_code",
        &serde_json::json!({ "file": "/tmp/fake.py" }),
    );

    // the whole of a stop in one call, and the difference between two of them.
    // the id comes back on the answer and is what a `diff` is written against
    let described = client.call(
        "state",
        &serde_json::json!({
            "frames": 1,
            "scopes": ["local"],
            "expressions": [ { "expression": "total + 1" } ],
            "source": 2,
            "detail": { "children": 7 },
        }),
    );
    let described: serde_json::Value = serde_json::from_str(
        described["result"]["content"][0]["text"]
            .as_str()
            .expect("`state` answered with text"),
    )
    .expect("`state` answered with json");
    let id = described["snapshot"]
        .as_str()
        .expect("a state carries the id it is kept under")
        .to_string();
    client.call("diff", &serde_json::json!({ "before": id, "after": id }));

    // a whole investigation in one call: a tree of steps, and a budget on all
    // three axes because there is no default for one
    client.call(
        "run_script",
        &serde_json::json!({
            "steps": [
                { "step": "log", "note": "starting" },
                {
                    "step": "if",
                    "predicate": { "expression": "total > 1" },
                    "then": [ { "step": "step_over" } ],
                    "otherwise": [],
                },
            ],
            "budget": { "steps": 8, "wall_ms": 500, "bytes": 4096 },
        }),
    );
    client.call("step_over", &serde_json::json!({ "deadline_ms": 500 }));
    client.call("step_in", &serde_json::json!({ "deadline_ms": 500 }));
    client.call("step_out", &serde_json::json!({ "deadline_ms": 500 }));
    client.call("continue_", &serde_json::json!({ "deadline_ms": 500 }));
    client.call("pause", &serde_json::json!({ "deadline_ms": 500 }));
    client.call("resume", &serde_json::json!({}));
    // the fake has no stops left, so this is the timeout shape
    client.call("wait", &serde_json::json!({ "deadline_ms": 500 }));

    for (tool, arguments) in extra {
        client.call(tool, arguments);
    }

    // a method this server does not implement, under an id the test can find
    client.under(&serde_json::json!(9_001), "completion/complete");
    client.call("terminate", &serde_json::json!({}));

    let Client { writes, reads, .. } = client;
    drop(writes);
    served
        .join()
        .expect("the server did not panic")
        .expect("the server served the whole conversation");

    Transcript {
        messages: reads.seen,
    }
}

/// an MCP client, talking to the server over a pipe
struct Client {
    writes: std::io::PipeWriter,
    reads: Messages,
    seq: i64,
}

impl Client {
    /// send a request and read until its answer arrives
    fn ask(&mut self, method: &str, params: &serde_json::Value) -> serde_json::Value {
        self.seq += 1;
        let id = serde_json::json!(self.seq);
        self.write(&serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        }));
        self.reads.answer_to(&id)
    }

    /// call one tool
    fn call(&mut self, tool: &str, arguments: &serde_json::Value) -> serde_json::Value {
        self.ask(
            "tools/call",
            &serde_json::json!({ "name": tool, "arguments": arguments }),
        )
    }

    /// send a request under an id of the caller's choosing
    fn under(&mut self, id: &serde_json::Value, method: &str) -> serde_json::Value {
        self.write(&serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": {},
        }));
        self.reads.answer_to(id)
    }

    /// send something that is not answered
    fn notify(&mut self, method: &str) {
        self.write(&serde_json::json!({ "jsonrpc": "2.0", "method": method }));
    }

    fn write(&mut self, message: &serde_json::Value) {
        writeln!(self.writes, "{message}").expect("the server is reading");
        self.writes.flush().expect("the server is reading");
    }
}

/// everything the server said
struct Transcript {
    messages: Vec<serde_json::Value>,
}

impl Transcript {
    /// the parsed result of the first successful call of one tool
    fn result_of(&self, tool: &str) -> serde_json::Value {
        let index = self.call_index(tool);
        let message = self
            .messages
            .get(index)
            .unwrap_or_else(|| panic!("nothing answered `{tool}`"));
        assert_eq!(
            message["result"]["isError"], false,
            "`{tool}` failed: {message}"
        );
        let text = message["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("`{tool}` answered with no text: {message}"));
        serde_json::from_str(text)
            .unwrap_or_else(|error| panic!("`{tool}` answered with {text}: {error}"))
    }

    /// which message answered the first call of one tool
    ///
    /// the calls go out in order and the answers come back in order, so the nth
    /// `tools/call` answer is the nth tool. the transcript does not carry the
    /// request, so this counts the tool results rather than searching them
    fn call_index(&self, tool: &str) -> usize {
        let order = tool_order();
        let wanted = order
            .iter()
            .position(|name| *name == tool)
            .unwrap_or_else(|| panic!("`{tool}` is not in the conversation"));
        self.messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message["result"]["content"].is_array())
            .map(|(index, _)| index)
            .nth(wanted)
            .unwrap_or_else(|| panic!("only {} tools were answered", self.messages.len()))
    }

    /// every tool call that reported a failure, with what it said
    fn failures(&self) -> Vec<String> {
        self.messages
            .iter()
            .filter(|message| message["result"]["isError"] == true)
            .map(|message| {
                message["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    /// the JSON-RPC error answered under one id
    fn error_of(&self, id: &serde_json::Value) -> Option<serde_json::Value> {
        self.messages
            .iter()
            .find(|message| &message["id"] == id && message.get("error").is_some())
            .map(|message| message["error"].clone())
    }
}

/// the tools the conversation calls, in order
fn tool_order() -> Vec<&'static str> {
    vec![
        "launch",
        "set_breakpoints",
        "set_exception_breakpoints",
        "debug_children",
        "sessions",
        "threads",
        "stop_the_world",
        "stack",
        "variables",
        "template_context",
        "evaluate",
        "set_variable",
        "set_next_statement",
        "restart_frame",
        "replace_code",
        "state",
        "diff",
        "run_script",
        "step_over",
        "step_in",
        "step_out",
        "continue_",
        "pause",
        "resume",
        "wait",
    ]
}

/// the child the fake session reports while the program runs
fn started_a_child() -> bpd_core::Spawn {
    bpd_core::Spawn {
        event: "_posixsubprocess.fork_exec".to_string(),
        executable: Some("/usr/bin/python3.14".to_string()),
        arguments: vec!["/usr/bin/python3.14".to_string(), "worker.py".to_string()],
        verdict: bpd_core::Verdict::ThisInterpreter,
    }
}

/// the blind spot the fake session announces while the program runs
fn cannot_see_a_child() -> bpd_core::Blindspot {
    bpd_core::Blindspot::MultiprocessingSpawn {
        interpreter: "3.13".to_string(),
    }
}

#[test]
fn a_blind_spot_is_carried_beside_the_children_rather_than_instead_of_them() {
    // parity with DAP, where the same fact arrives as an `important` output
    // event. an agent that read `started` without this would conclude from an
    // empty list that the program has no children — which is exactly what this
    // message exists to stop
    let stepped = drive(&Asked::default()).result_of("step_over");
    let unseen = &stepped["spawned"]["cannot_see"][0];

    assert_eq!(
        unseen["silence_is_not_evidence"], true,
        "the one field an agent has to act on was not there: {stepped}"
    );
    assert!(
        unseen["says"]
            .as_str()
            .is_some_and(|says| says.contains("3.14")),
        "the message has to name the release where this is visible: {unseen}"
    );
    assert!(
        stepped["spawned"]["started"].is_array(),
        "the blind spot goes beside the children, not instead of them: {stepped}"
    );
}

#[test]
fn a_child_the_program_started_is_carried_on_the_answer_that_saw_it() {
    // parity with DAP, where the same fact arrives as an `output` event. MCP
    // has no event stream — the server writes nothing that is not an answer —
    // so it rides on the answer of the call the program was running during
    let stepped = drive(&Asked::default()).result_of("step_over");
    let spawned = &stepped["spawned"];
    assert!(
        spawned.is_object(),
        "the program started a child during this call and the answer was {stepped}"
    );

    let started = &spawned["started"][0];
    assert_eq!(started["arguments"][1], "worker.py");
    assert_eq!(started["event"], "_posixsubprocess.fork_exec");

    // its own key rather than under `logged`. an agent that found it there
    // would reasonably read it as a logpoint having fired
    assert!(
        stepped["logged"].is_null(),
        "a child was reported as a log record: {stepped}"
    );

    // the two fields an agent decides on. `certain` is why the verdict is not a
    // boolean — bpd reads an argument vector, so it can be sure a child runs
    // this interpreter and cannot be sure what a launcher will do
    assert_eq!(started["certain"], true);
    assert_eq!(
        started["debugged"], false,
        "an agent that assumed the child was being debugged would set \
         breakpoints in it and wait for stops that never come"
    );
    assert!(
        started["says"]
            .as_str()
            .is_some_and(|says| says.contains("not debugging it")),
        "the sentence a person is shown has to be here too, or an agent and a \
         human reading the same session read different things: {started}"
    );
}

#[test]
fn the_conversation_calls_every_tool_this_server_offers() {
    // a tool nobody calls is a tool nothing checks. `terminate` is called last
    // and is not in the ordered list, because the extra calls come before it
    let called: BTreeSet<&str> = tool_order().into_iter().chain(["terminate"]).collect();
    let offered: BTreeSet<&str> = tools().iter().map(|tool| tool.name).collect();
    assert_eq!(
        offered.difference(&called).collect::<Vec<_>>(),
        Vec::<&&str>::new(),
        "a tool is offered and the coverage conversation never calls it"
    );
}

/// the messages the server writes, read one line at a time
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

    fn answer_to(&mut self, id: &serde_json::Value) -> serde_json::Value {
        loop {
            let mut line = String::new();
            let read = self
                .input
                .read_line(&mut line)
                .expect("the server is writing");
            assert_ne!(
                read, 0,
                "the server hung up mid-conversation: {:#?}",
                self.seen
            );
            let message: serde_json::Value = serde_json::from_str(line.trim())
                .unwrap_or_else(|error| panic!("the server wrote {line:?}: {error}"));
            self.seen.push(message.clone());
            if &message["id"] == id {
                return message;
            }
        }
    }
}

// ---- the fake session -----------------------------------------------------

/// a session that answers everything and records what it was asked
struct Fake {
    asked: Asked,
}

struct FakeSession {
    asked: Asked,
    held: Vec<Stop>,
    /// stops still to come, innermost first
    remaining: Vec<Stop>,
    output: Arc<dyn ProgramOutput>,
}

impl Launcher for Fake {
    fn launch(
        &mut self,
        _configuration: &Configuration,
        output: Arc<dyn ProgramOutput>,
    ) -> Result<Started, Failed> {
        Ok(Started::Stopped(Box::new(FakeSession {
            asked: Arc::clone(&self.asked),
            held: vec![stop_at(1, StopReason::Entry)],
            // innermost first, so `pop` hands them out in order
            remaining: vec![
                stop_at(
                    6,
                    StopReason::Paused {
                        file: "/tmp/fake.py".to_string(),
                        line: 8,
                    },
                ),
                stop_at(
                    5,
                    StopReason::Breakpoint {
                        breakpoints: vec![1],
                        file: "/tmp/fake.py".to_string(),
                        line: 3,
                    },
                ),
                stepped(4, bpd_core::StepKind::Out, 7),
                stepped(3, bpd_core::StepKind::In, 6),
                stepped(2, bpd_core::StepKind::Over, 4),
            ],
            output,
        })))
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

fn stepped(number: u64, kind: bpd_core::StepKind, line: u32) -> Stop {
    stop_at(
        number,
        StopReason::Stepped {
            kind,
            file: "/tmp/fake.py".to_string(),
            line,
        },
    )
}

/// what a fake session answers a debug script with
///
/// the shape rather than the substance: that a script really drives a real
/// interpreter is `crates/bpd_engine/tests/scripts.rs`. what is under test here
/// is that the tool reaches the session with the tree the client wrote, and
/// that the whole transcript comes back
fn transcript(stop: u64, script: &bpd_core::Script) -> bpd_core::Transcript {
    bpd_core::Transcript {
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
    }
}

/// what a fake session answers a state query with
///
/// the shape rather than the substance: that a query really reads a real
/// interpreter, and that it agrees with the tree walk, is
/// `crates/bpd_engine/tests/queries.rs`. what is under test here is that the
/// tool reaches the session with the query the client wrote, and that the whole
/// answer including its id comes back
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

fn integer(text: &str) -> Value {
    Value {
        kind: "int".to_string(),
        content: Content::Int {
            text: text.to_string(),
            omitted: None,
        },
    }
}

impl Session for FakeSession {
    fn held(&self) -> Vec<Stop> {
        self.held.clone()
    }

    /// one session, over a program bpd started
    fn sessions(&self) -> Vec<bpd_core::Joined> {
        vec![bpd_core::Joined {
            session: session(),
            ours: true,
            held: self.held.clone(),
            exit: None,
        }]
    }

    /// this fake's program never ends: the whole conversation it drives happens
    /// against one that is there, and a fake that claimed an exit would answer
    /// every refusal with the wrong one of the two reasons
    fn ended(&self, _session: Option<SessionId>) -> Option<Exit> {
        None
    }

    fn terminate(&mut self, _session: Option<SessionId>) -> Result<(), Failed> {
        self.held.clear();
        Ok(())
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
        self.record(&request);
        self.asked
            .lock()
            .expect("the recorder is not poisoned")
            .addressed
            .push((request.name(), session));

        // a program starts a child while it runs, so a wait is where one is
        // reported. the fake does it on every wait, which puts the server's
        // route for it in this conversation rather than leaving it to a test
        // that needs a real interpreter
        if matches!(request, Request::Wait { .. }) {
            reporting.spawned(started_a_child());
            reporting.blind_to(cannot_see_a_child());
        }

        Ok(match request {
            Request::SetBreakpoints { breakpoints } => Response::BreakpointsResolved {
                resolved: breakpoints
                    .iter()
                    .map(|breakpoint| Resolved {
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
                self.output
                    .wrote(bpd_mcp::Stream::Stdout, "the program said this\n");
                self.held.clear();
                Response::Ran(self.next_stop())
            }
            Request::Wait { .. } => Response::Ran(self.next_stop()),
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
            Request::Stack { stop, .. } => Response::Stack(stack(stop)),
            Request::TemplateContext { .. } => Response::TemplateContext(template_context()),
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
            Request::Evaluate { .. } | Request::SetVariable { .. } => {
                Response::Evaluated(Evaluated::Value {
                    value: integer("9"),
                })
            }
            // both jumps answer the same shape, and the fake answers the
            // interesting one: a breakpoint on the destination that will not
            // fire for this pass, and a local the move bound to `None`. that a
            // real interpreter really moves is
            // `crates/bpd_engine/tests/jumps.rs`
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
            Request::RestartFrame { .. } => Response::Jumped(bpd_core::Jumped {
                at: bpd_core::Where {
                    file: "/tmp/fake.py".to_string(),
                    line: 1,
                    function: "main".to_string(),
                },
                outcome: bpd_core::Jump::Moved {
                    from: 3,
                    bound_to_none: Vec::new(),
                    unannounced: Vec::new(),
                },
                mode: Mode::NonStop,
            }),
            // the fake applies, so that what the renderer says about a
            // replacement that really happened is under test: which functions
            // moved, how many objects held each, and which breakpoints had to be
            // bound again. the refused shape is what the DAP coverage drives, and
            // that a real interpreter really replaces code is
            // `crates/bpd_engine/tests/replacement.rs`
            Request::ReplaceCode { file } => Response::Replaced(bpd_core::Replaced {
                file,
                outcome: bpd_core::Replacement::Applied {
                    changed: vec![bpd_core::Rebound {
                        function: "main".to_string(),
                        was_at: 2,
                        now_at: 5,
                        objects: 2,
                    }],
                    unchanged: vec!["<module>".to_string()],
                    rebound: vec![Resolved {
                        id: 1,
                        binding: Binding::Bound {
                            line: 6,
                            sites: vec![Site {
                                qualname: "main".to_string(),
                                first_line: 5,
                                offset: 4,
                            }],
                            evaluation: Evaluation::Always,
                        },
                    }],
                },
                mode: Mode::NonStop,
            }),
            Request::RunScript { stop, script } => Response::Transcript(transcript(stop, &script)),
            Request::Query { stop, query } => Response::State(snapshot(stop, &query)),
            Request::Diff { before, after } => Response::Difference(bpd_core::difference(
                &snapshot(before.stop, &bpd_core::StateQuery::default()),
                &snapshot(after.stop, &bpd_core::StateQuery::default()),
                &self.held.iter().map(|stop| stop.stop).collect::<Vec<u64>>(),
            )),
        })
    }
}

/// one frame of each kind
///
/// an agent has to be able to tell a frame the interpreter really has from one
/// bpd synthesised over a django template node, and only a stack with both in
/// it shows that it can
fn stack(stop: u64) -> Stack {
    Stack {
        frames: vec![
            Frame {
                id: FrameId { stop, depth: 0 },
                file: "/tmp/page.html".to_string(),
                line: 2,
                kind: bpd_core::FrameKind::Template {
                    node: "VariableNode".to_string(),
                    python: FrameId { stop, depth: 1 },
                },
            },
            Frame {
                id: FrameId { stop, depth: 1 },
                file: "/tmp/fake.py".to_string(),
                line: 3,
                kind: bpd_core::FrameKind::Python {
                    function: "main".to_string(),
                    first_line: 1,
                },
            },
        ],
        depth: 2,
        mode: Mode::NonStop,
    }
}

/// two layers holding the same name, which is what a rendering has to show
fn template_context() -> bpd_core::TemplateContext {
    bpd_core::TemplateContext {
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
    }
}

impl FakeSession {
    /// note what was asked, for the assertions that are about the asking
    fn record(&self, request: &Request) {
        let mut recorder = self.asked.lock().expect("the recorder is not poisoned");
        recorder.requests.push(request.name());
        match request {
            Request::SetBreakpoints { breakpoints } => {
                recorder.breakpoints.extend(breakpoints.iter().cloned());
            }
            Request::Variables { detail, .. }
            | Request::Evaluate { detail, .. }
            | Request::SetVariable { detail, .. } => recorder.details.push(*detail),
            Request::Query { query, .. } => recorder.details.push(query.detail),
            _ => {}
        }
    }

    /// the next stop, or the timeout shape once there are none left
    fn next_stop(&mut self) -> Running {
        match self.remaining.pop() {
            Some(next) => {
                self.held.push(next.clone());
                Running::Stopped {
                    stop: next,
                    rebound: Vec::new(),
                }
            }
            // what an engine whose deadline passed answers. the program is
            // running, so there is nothing to say about where it is
            None => Running::StillRunning {
                waited: std::time::Duration::from_millis(500),
                rebound: Vec::new(),
            },
        }
    }
}
