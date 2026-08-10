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
    Binding, Content, Entry, Evaluated, Evaluation, Facet, Frame, FrameId, Mode, Reach, Reporting,
    Request, Resolved, Response, Running, Site, Stack, Stop, StopReason, Threads, Value, Variables,
    WorldStopped,
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
    let frame = stack["body"]["stackFrames"][0]["id"].clone();

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
            held: vec![stop(1, StopReason::Entry)],
            remaining: vec![stop(
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

fn stop(number: u64, reason: StopReason) -> Stop {
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

impl Session for FakeSession {
    fn held(&self) -> Vec<Stop> {
        self.held.clone()
    }

    fn interrupt(&self) -> Result<Box<dyn Interrupt>, Failed> {
        Ok(Box::new(FakeInterrupt {
            asked: Arc::clone(&self.asked),
        }))
    }

    fn dispatch(
        &mut self,
        request: Request,
        _reporting: &mut dyn Reporting,
    ) -> Result<Response, Failed> {
        {
            let mut recorder = self.asked.lock().expect("the recorder is not poisoned");
            recorder.requests.push(request.name());
            match &request {
                Request::Variables { detail, .. }
                | Request::Evaluate { detail, .. }
                | Request::SetVariable { detail, .. } => recorder.details.push(*detail),
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
            Request::Stack { stop, .. } => Response::Stack(Stack {
                frames: vec![Frame {
                    id: FrameId { stop, depth: 0 },
                    file: "/tmp/fake.py".to_string(),
                    line: 3,
                    function: "main".to_string(),
                    first_line: 1,
                }],
                depth: 1,
                mode: Mode::NonStop,
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
            Request::Evaluate { .. } | Request::SetVariable { .. } => {
                Response::Evaluated(Evaluated::Value {
                    value: integer("9"),
                })
            }
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
