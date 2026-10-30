//! `justMyCode` is said on the console only when the debuggee did not take it
//!
//! the filter is on unless the launch configuration turns it off, so whatever
//! is said about it is said on every session — and the console it would be said
//! on is the program's. a client shows bpd's `console` lines beside what the
//! program printed: the intellij platform prints every `output` category into
//! the session's console, `telemetry` included, and vs code's debug console is
//! where bpd's capture of the program's stdout lands. so a filter that took is
//! not news: it is the setting the client sent, which its own settings page
//! already shows, and debugpy says nothing of it either
//!
//! a filter that did **not** take is the opposite. the client was promised a
//! session that skips a library's code and is not getting one, and by the time
//! the agent answers, the request that asked for it has long been answered. the
//! console is the only channel left, and the refusal goes there
//!
//! both orders a DAP client may configure in are driven, because the filter is
//! sent from whichever of `launch` and `configurationDone` comes second, and the
//! two reach the console by different routes

use std::io::{BufRead as _, BufReader, Read, Write};
use std::sync::Arc;

use bpd_core::{Addressed, Reported, Reporting, Request, Response, SessionId, Stop, StopReason};
use bpd_dap::{Configuration, Failed, Interrupt, Launcher, ProgramOutput, Session, Started};

/// the interpreter's identity for the one thread this fake ever holds
const THREAD: u64 = 4242;

/// what the console says when the debuggee did not take the filter
///
/// the whole line, and not a part of it: what is pinned is that this is the
/// one thing said, and said as bpd's own words
const DID_NOT_TAKE: &str = "bpd: the debuggee did not take `justMyCode`, and a library's \
                            code would be stopped and stepped in\n";

/// a session whose agent answers `justMyCode` as it is told to
struct Filtering {
    held: Vec<Stop>,
    /// whether the agent reports the filter on after being asked for it
    took: bool,
}

impl Session for Filtering {
    fn dispatch(
        &mut self,
        asked: Addressed,
        _reporting: &mut dyn Reporting,
    ) -> Result<Response, Failed> {
        match asked.request {
            // the directories are there so that a notice naming them would have
            // something to name, which is what a real agent answers with
            Request::JustMyCode { on } => Ok(Response::JustMyCode(bpd_core::JustMyCode {
                on: on && self.took,
                libraries: vec!["/usr/lib/python3.14".to_string()],
            })),
            other => Err(format!("this fake answers `justMyCode`, and got {other:?}").into()),
        }
    }

    fn held(&self) -> Vec<Stop> {
        self.held.clone()
    }

    fn interrupt(&self) -> Result<Box<dyn Interrupt>, Failed> {
        Ok(Box::new(Reaching))
    }
}

/// what reaches the program: nothing, because this fake has none
struct Reaching;

impl Interrupt for Reaching {
    fn deliver(&mut self, request: &Request) -> Result<(), Failed> {
        Err(format!("this fake is never interrupted, and got {request:?}").into())
    }

    fn ask_to_end(&mut self) -> Result<bool, Failed> {
        Err("this fake started no process and is not its parent".into())
    }

    fn terminate(&mut self) -> Result<(), Failed> {
        Err("this fake started no process and is not its parent".into())
    }
}

struct Fake {
    took: bool,
}

impl Launcher for Fake {
    fn launch(
        &self,
        _configuration: &Configuration,
        _output: Arc<dyn ProgramOutput>,
    ) -> Result<Started, Failed> {
        Ok(Started::Stopped(Box::new(Filtering {
            took: self.took,
            held: vec![
                Reported {
                    stop: 1,
                    thread: THREAD,
                    reason: StopReason::Entry,
                    holding: Vec::new(),
                }
                .in_session(SessionId::new(
                    std::num::NonZeroU64::new(7).expect("7 is not zero"),
                )),
            ],
        })))
    }

    fn launch_in_terminal(
        &self,
        _configuration: &Configuration,
        _ask: &mut dyn FnMut(&bpd_dap::Invocation) -> Result<(), Failed>,
    ) -> Result<Started, Failed> {
        Err("this fake starts nothing in a terminal".into())
    }

    fn attach(&self, session: u64) -> Result<Started, Failed> {
        Err(format!("this fake holds no session {session}").into())
    }
}

/// the order a client configures in
#[derive(Debug, Clone, Copy)]
enum Order {
    /// `launch`, then `configurationDone` — vs code's, and DAP's own example
    LaunchFirst,
    /// `configurationDone`, then `launch` — the intellij platform's
    ConfiguredFirst,
}

/// every `output` event of one conversation, as category and text, in order
///
/// the program is held at its entry and never let go — `stopOnEntry` — so
/// nothing but the adapter can have said anything, and what is read is the whole
/// of what it said
fn said(took: bool, order: Order) -> Vec<(String, String)> {
    let (to_adapter, mut writes) = std::io::pipe().expect("a pipe is available");
    let (reads, from_adapter) = std::io::pipe().expect("a pipe is available");

    let served = std::thread::spawn(move || {
        bpd_dap::serve(
            &Fake { took },
            Box::new(to_adapter),
            Box::new(from_adapter),
            &bpd_dap::Transport::Stdio,
        )
    });

    let mut reader = Messages::new(reads);
    let mut seen = Vec::new();
    let mut seq = 0;
    let mut exchange = |command: &str, arguments: &serde_json::Value| {
        seq += 1;
        let body = serde_json::json!({
            "seq": seq, "type": "request", "command": command, "arguments": arguments,
        })
        .to_string();
        write!(writes, "Content-Length: {}\r\n\r\n{body}", body.len())
            .expect("the adapter is reading");
        writes.flush().expect("the adapter is reading");

        // up to and including the answer, so that a client that has heard back
        // from both requests has heard everything the second one set off
        loop {
            let message = reader
                .next_message()
                .unwrap_or_else(|| panic!("the adapter answered `{command}`"));
            let answered = message["type"] == "response" && message["command"] == command;
            seen.push(message);
            if answered {
                return;
            }
        }
    };

    let launch = serde_json::json!({ "program": "/tmp/fake.py", "stopOnEntry": true });
    exchange("initialize", &serde_json::json!({}));
    match order {
        Order::LaunchFirst => {
            exchange("launch", &launch);
            exchange("configurationDone", &serde_json::json!({}));
        }
        Order::ConfiguredFirst => {
            exchange("configurationDone", &serde_json::json!({}));
            exchange("launch", &launch);
        }
    }
    // the client is done talking, and closing its end is what ends the adapter
    drop(writes);

    seen.extend(reader.collect());
    served
        .join()
        .expect("the adapter thread ended cleanly")
        .expect("the adapter ran to the end of its input");

    let refused: Vec<&serde_json::Value> = seen
        .iter()
        .filter(|message| message["type"] == "response" && message["success"] == false)
        .collect();
    assert!(
        refused.is_empty(),
        "every request of this conversation is answered, so what is on the console \
         is the only place the filter can be reported: {refused:?}"
    );

    seen.iter()
        .filter(|message| message["type"] == "event" && message["event"] == "output")
        .map(|message| {
            (
                message["body"]["category"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                message["body"]["output"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

#[test]
fn a_filter_the_debuggee_took_is_not_said_on_the_programs_console() {
    for order in [Order::LaunchFirst, Order::ConfiguredFirst] {
        assert_eq!(
            said(true, order),
            Vec::<(String, String)>::new(),
            "configured {order:?}: the filter is on by default, so a line about it \
             taking is a line above every program's output"
        );
    }
}

#[test]
fn a_filter_the_debuggee_did_not_take_is_said_on_the_console() {
    for order in [Order::LaunchFirst, Order::ConfiguredFirst] {
        assert_eq!(
            said(false, order),
            [("console".to_string(), DID_NOT_TAKE.to_string())],
            "configured {order:?}: the client was promised a session that skips a \
             library's code, and the console is the only channel left to say it is not"
        );
    }
}

// ---- reading the adapter's side ------------------------------------------

/// the adapter's output, split back into messages
struct Messages {
    reads: BufReader<std::io::PipeReader>,
}

impl Messages {
    fn new(reads: std::io::PipeReader) -> Self {
        Self {
            reads: BufReader::new(reads),
        }
    }

    fn collect(mut self) -> Vec<serde_json::Value> {
        let mut messages = Vec::new();
        while let Some(message) = self.next_message() {
            messages.push(message);
        }
        messages
    }

    fn next_message(&mut self) -> Option<serde_json::Value> {
        let mut length = None;
        loop {
            let mut line = String::new();
            if self.reads.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                length = value.parse::<usize>().ok();
            }
        }

        let mut body = vec![0u8; length?];
        self.reads.read_exact(&mut body).ok()?;
        serde_json::from_slice(&body).ok()
    }
}
