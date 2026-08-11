//! every capability of the core is reachable through the adapter
//!
//! the parity rule says no capability exists in one adapter and not the other.
//! the two-sided test of it arrives with the MCP adapter, because with one
//! adapter there is nothing to compare against. this is the one-sided half, and
//! it is the half that fails when a capability is added: `bpd_dap::reach_of`
//! matches `Request` with no catch-all arm, so a new variant does not compile
//! until someone says how DAP gets at it — and then this asserts the claim is
//! true by **driving the adapter** and watching what the session is asked
//!
//! the session here is a fake, deliberately. what this test is about is the
//! adapter's own translation: which DAP request becomes which `Request`, and
//! whether the table beside it is honest. that a request really stops a real
//! interpreter is `crates/bpd/tests/dap.rs`, against a real one

use std::collections::BTreeSet;
use std::io::{BufRead as _, BufReader, Read, Write};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};

use bpd_core::{
    At, Binding, Content, Did, Entry, Evaluated, Evaluation, Facet, Frame, FrameId, Mode, Outcome,
    Reach, Record, Reporting, Request, Resolved, Response, Running, Site, Stack, Stop, StopReason,
    Threads, Value, Variables, WorldStopped,
};
use bpd_dap::{
    Configuration, Failed, Interrupt, Launcher, ProgramOutput, Session, Started, reach_of,
    reach_of_facet, surface,
};

/// the interpreter's identity for the one thread this fake ever holds
const THREAD: u64 = 4242;

/// what the session was asked, and what the requests carried
///
/// the names alone answer "was this capability reached at all". a capability
/// carried *inside* a request needs the payload too: the launch configuration
/// is the only route DAP has to `bpd_core::Detail`, and a table that claimed it
/// without checking would be claiming a mapping nobody had made
#[derive(Debug, Default)]
struct Recorder {
    requests: Vec<&'static str>,
    details: Vec<bpd_core::Detail>,
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
    drive(&asked);
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

/// the child the fake session reports while the program runs
fn started_a_child() -> bpd_core::Spawn {
    bpd_core::Spawn {
        event: "_posixsubprocess.fork_exec".to_string(),
        executable: Some("/usr/bin/python3.14".to_string()),
        arguments: vec!["/usr/bin/python3.14".to_string(), "worker.py".to_string()],
        verdict: bpd_core::Verdict::ThisInterpreter,
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
                .is_some_and(|text| text.contains("worker.py"))
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

/// run one whole DAP conversation against the fake session
///
/// one conversation rather than one per test: the point of it is coverage of
/// the whole surface, and three tests reading three different parts of the same
/// transcript is what that looks like
#[expect(
    clippy::too_many_lines,
    reason = "it is a transcript. splitting a conversation into helpers hides \
              the order the messages go in, which is the thing under test"
)]
fn drive(asked: &Asked) -> Transcript {
    let (to_adapter, mut client_writes) = std::io::pipe().expect("a pipe is available");
    let (client_reads, from_adapter) = std::io::pipe().expect("a pipe is available");

    let served = std::thread::spawn({
        let asked = Arc::clone(asked);
        move || {
            bpd_dap::serve(
                &mut Fake { asked },
                Box::new(to_adapter),
                Box::new(from_adapter),
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
        serde_json::json!({}),
    );
    answer(
        &mut client_writes,
        &mut reader,
        "launch",
        serde_json::json!({
            "program": "/tmp/fake.py",
            "stopOnEntry": true,
            "stopTheWorld": true,
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
            "breakpoints": [ { "line": 3, "condition": "total > 1" } ],
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
    // the whole of a stop in one call, and the difference between two of them.
    // DAP's own way of reading state is the tree walk above, and it keeps it —
    // this is the same capability an agent's front end has, which is what the
    // parity rule requires
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

    fn refusals(&self) -> Vec<String> {
        self.messages
            .iter()
            .filter(|message| message["type"] == "response" && message["success"] == false)
            .filter(|message| message["command"] != "variables")
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
}

/// the state the fake keeps across a session
struct FakeSession {
    asked: Asked,
    held: Vec<Stop>,
    /// stops still to come, innermost first
    remaining: Vec<Stop>,
}

impl Launcher for Fake {
    fn launch(
        &mut self,
        _configuration: &Configuration,
        _output: Arc<dyn ProgramOutput>,
    ) -> Result<Started, Failed> {
        Ok(Started::Stopped(Box::new(FakeSession {
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
        })))
    }
}

fn stop_at(number: u64, reason: StopReason) -> Stop {
    Stop {
        stop: number,
        thread: THREAD,
        reason,
        holding: Vec::new(),
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
        request: Request,
        reporting: &mut dyn Reporting,
    ) -> Result<Response, Failed> {
        // a program starts a child while it runs, so a wait is where one is
        // reported. the fake does it on every wait, which is what makes the
        // adapter's route for it part of this conversation rather than
        // something only a real interpreter ever exercises
        if matches!(request, Request::Wait { .. }) {
            reporting.spawned(started_a_child());
        }
        {
            let mut recorder = self.asked.lock().expect("the recorder is not poisoned");
            recorder.requests.push(request.name());
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
                    None => Running::Exited {
                        status: exited_with(0),
                        rebound: Vec::new(),
                    },
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

/// an exit status, which has no portable constructor
fn exited_with(code: i32) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        ExitStatus::from_raw(code << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt as _;
        ExitStatus::from_raw(code.unsigned_abs())
    }
}
