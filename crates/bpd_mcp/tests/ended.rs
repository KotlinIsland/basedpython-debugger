//! a program bpd did not start ends **without** an exit code, and says so
//!
//! there are two ways a debuggee can be over and they are not the same fact. a
//! program bpd launched is reaped by bpd, so bpd knows what it exited with. a
//! program that connected to bpd's listener has no parent here: bpd cannot reap
//! it and never learns its status
//!
//! rendering the second as the first — `"outcome": "exited", "exit_code": 0` —
//! would be the server inventing the one number the caller is asking about. so
//! it is its own outcome, it carries no `exit_code` field at all, and the
//! refusals afterwards say the program is over *and* why there is no number
//!
//! the fake here is deliberately tiny and separate from `coverage.rs`: that one
//! drives the whole tool surface against a program that is still there, and a
//! session that ended would change every answer in it

use std::sync::Arc;

use bpd_core::{
    Addressed, Exit, Joined, Mode, Reported, Reporting, Request, Response, Running, SessionId,
    Stack, Stop, StopReason,
};
use bpd_mcp::{Configuration, Failed, Launcher, ProgramOutput, Session, Started};

/// the thread the fake's one stop holds
const THREAD: u64 = 7;

fn session() -> SessionId {
    SessionId::new(std::num::NonZeroU64::new(1).unwrap_or_else(|| unreachable!("1 is not zero")))
}

/// a session over a program bpd did not start
///
/// it is held at entry, it can be resumed exactly once, and after that it is
/// over with no exit status — which is the whole of what this file is about
struct NotOurs {
    held: Vec<Stop>,
    /// whether the program has ended, which it has once it was resumed
    over: bool,
}

impl Session for NotOurs {
    fn dispatch(
        &mut self,
        asked: Addressed,
        _reporting: &mut dyn Reporting,
    ) -> Result<Response, Failed> {
        match asked.request {
            Request::Run { .. } | Request::Wait { .. } => {
                self.held.clear();
                self.over = true;
                Ok(Response::Ran(Running::Ended {
                    rebound: Vec::new(),
                }))
            }
            // `launch` answers with the frames of the entry stop, so this has
            // to be here. it is deliberately empty: what is under test is the
            // ending, and a stack of made-up frames would be a second thing to
            // read
            Request::Stack { .. } => Ok(Response::Stack(Stack {
                frames: Vec::new(),
                depth: 0,
                mode: Mode::NonStop,
            })),
            other => {
                Err(format!("this fake answers a resume and a stack, and got {other:?}").into())
            }
        }
    }

    fn held(&self) -> Vec<Stop> {
        self.held.clone()
    }

    /// one session, over a process bpd did not start
    fn sessions(&self) -> Vec<Joined> {
        vec![Joined {
            session: session(),
            ours: false,
            held: self.held.clone(),
            exit: self.ended(None),
        }]
    }

    fn ended(&self, _session: Option<SessionId>) -> Option<Exit> {
        // the point of the whole file: over, and with no number to give
        self.over.then_some(Exit::Unknown)
    }

    fn terminate(&mut self, _session: Option<SessionId>) -> Result<(), Failed> {
        Err("bpd did not start that process and is not its parent".into())
    }
}

struct Fake;

impl Launcher for Fake {
    fn launch(
        &mut self,
        _configuration: &Configuration,
        _output: Arc<dyn ProgramOutput>,
    ) -> Result<Started, Failed> {
        Ok(Started::Stopped(Box::new(NotOurs {
            held: vec![
                Reported {
                    stop: 1,
                    thread: THREAD,
                    reason: StopReason::Entry,
                    holding: Vec::new(),
                }
                .in_session(session()),
            ],
            over: false,
        })))
    }
}

#[test]
fn a_session_whose_exit_bpd_cannot_read_is_never_rendered_as_an_exit_code() {
    let mut client = Client::start();
    client.ask(
        "initialize",
        &serde_json::json!({ "protocolVersion": bpd_mcp::PROTOCOL_VERSION }),
    );
    client.notify("notifications/initialized");
    client.call(
        "launch",
        &serde_json::json!({ "program": "/tmp/fake.py", "python": "python3" }),
    );

    let ended = client.call("continue_", &serde_json::json!({ "deadline_ms": 1000 }));
    assert_eq!(
        ended["outcome"], "ended",
        "a program whose exit bpd cannot read is its own outcome, and got {ended}"
    );
    assert!(
        ended.get("exit_code").is_none(),
        "there is no exit code and the answer must not carry the field at all — \
         a null would read as one that was measured. got {ended}"
    );
    let note = ended["note"]
        .as_str()
        .expect("the outcome says why there is no exit code");
    assert!(note.contains("did not start"), "said {note}");
    assert!(note.contains("not its parent"), "said {note}");

    // and the refusal afterwards is the one that says the program is over, not
    // the one that invites holding a thread
    let refused = client.failure("stack", &serde_json::json!({}));
    assert!(
        refused.contains("the program is over"),
        "the refusal has to say the program ended, and said {refused}"
    );
    assert!(
        refused.contains("not bpd's to read"),
        "and why there is no exit code with it, and said {refused}"
    );
    assert!(
        !refused.contains("pausing it"),
        "there is nothing left to pause, and it said {refused}"
    );

    client.finish();
}

// ---- the client ----------------------------------------------------------

/// enough of an MCP client to drive the server over a pipe
struct Client {
    writes: std::io::PipeWriter,
    reads: std::io::BufReader<std::io::PipeReader>,
    served: Option<std::thread::JoinHandle<()>>,
    seq: u64,
}

impl Client {
    fn start() -> Self {
        let (to_server, client_writes) = std::io::pipe().expect("a pipe is available");
        let (client_reads, from_server) = std::io::pipe().expect("a pipe is available");
        let served = std::thread::spawn(move || {
            bpd_mcp::serve(&mut Fake, Box::new(to_server), Box::new(from_server))
                .expect("the server ran to the end of its input");
        });
        Self {
            writes: client_writes,
            reads: std::io::BufReader::new(client_reads),
            served: Some(served),
            seq: 0,
        }
    }

    fn ask(&mut self, method: &str, params: &serde_json::Value) -> serde_json::Value {
        use std::io::Write as _;
        self.seq += 1;
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.seq,
            "method": method,
            "params": params,
        });
        writeln!(self.writes, "{request}").expect("the server is reading");
        self.next_message()
    }

    fn notify(&mut self, method: &str) {
        use std::io::Write as _;
        let note = serde_json::json!({ "jsonrpc": "2.0", "method": method });
        writeln!(self.writes, "{note}").expect("the server is reading");
    }

    /// call a tool and read the json it answered with
    fn call(&mut self, tool: &str, arguments: &serde_json::Value) -> serde_json::Value {
        let answered = self.ask(
            "tools/call",
            &serde_json::json!({ "name": tool, "arguments": arguments }),
        );
        let result = &answered["result"];
        assert_ne!(
            result["isError"],
            serde_json::Value::Bool(true),
            "`{tool}` was refused: {answered}"
        );
        serde_json::from_str(text_of(tool, &answered)).expect("a tool answers json")
    }

    /// call a tool that is expected to be refused, and read what it said
    fn failure(&mut self, tool: &str, arguments: &serde_json::Value) -> String {
        let answered = self.ask(
            "tools/call",
            &serde_json::json!({ "name": tool, "arguments": arguments }),
        );
        assert_eq!(
            answered["result"]["isError"],
            serde_json::Value::Bool(true),
            "`{tool}` was answered rather than refused: {answered}"
        );
        text_of(tool, &answered).to_string()
    }

    fn next_message(&mut self) -> serde_json::Value {
        use std::io::BufRead as _;
        let mut line = String::new();
        let read = self
            .reads
            .read_line(&mut line)
            .expect("the server wrote a line");
        assert!(read > 0, "the server closed its output");
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("`{line}` is not json: {error}"))
    }

    fn finish(mut self) {
        drop(std::mem::replace(&mut self.writes, {
            let (_read, write) = std::io::pipe().expect("a pipe is available");
            write
        }));
        if let Some(served) = self.served.take() {
            served.join().expect("the server thread ended cleanly");
        }
    }
}

fn text_of<'a>(tool: &str, answered: &'a serde_json::Value) -> &'a str {
    answered["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("`{tool}` answered {answered}"))
}
