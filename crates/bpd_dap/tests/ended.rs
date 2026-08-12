//! a program bpd did not start ends with `terminated` and **no** `exited`
//!
//! DAP's `exited` event carries an `exitCode`, and it is a required field. a
//! program that connected to bpd's listener has no parent here — bpd cannot
//! reap it and never learns what it exited with — so there is no number to put
//! in it, and sending the event with a zero would be the adapter inventing the
//! one field the event exists for
//!
//! `terminated` is what the protocol has for "the session is over" and it
//! carries nothing, so it is exactly the right shape. the client is told the
//! program ended and is not told a lie about how
//!
//! the fake here is small and separate from `coverage.rs`, which drives the
//! whole request surface against a program that is still there

use std::io::{BufRead as _, BufReader, Read, Write};
use std::sync::Arc;

use bpd_core::{
    Addressed, Mode, Reported, Reporting, Request, Response, Running, SessionId, Stack, Stop,
    StopReason,
};
use bpd_dap::{Configuration, Failed, Interrupt, Launcher, ProgramOutput, Session, Started};

/// the interpreter's identity for the one thread this fake ever holds
const THREAD: u64 = 4242;

fn session() -> SessionId {
    SessionId::new(std::num::NonZeroU64::new(7).expect("7 is not zero"))
}

/// a session over a program bpd did not start
struct NotOurs {
    held: Vec<Stop>,
}

impl Session for NotOurs {
    fn dispatch(
        &mut self,
        asked: Addressed,
        _reporting: &mut dyn Reporting,
    ) -> Result<Response, Failed> {
        match asked.request {
            // DAP's `continue` is a resume and then a wait, so the two are
            // answered separately: the thread is let go, and what the program
            // then did is that it is over
            Request::Resume { .. } => {
                self.held.clear();
                Ok(Response::Resumed {
                    threads: vec![THREAD],
                })
            }
            Request::Run { .. } | Request::Wait { .. } => Ok(Response::Ran(Running::Ended {
                rebound: Vec::new(),
            })),
            Request::SetBreakpoints { .. } => Ok(Response::BreakpointsResolved {
                resolved: Vec::new(),
            }),
            // deliberately empty. what is under test is the ending, and a stack
            // of made-up frames would be a second thing to read
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

    fn interrupt(&self) -> Result<Box<dyn Interrupt>, Failed> {
        Ok(Box::new(Reaching))
    }
}

/// what reaches a program bpd did not start
///
/// a pause would go down the connection like any other request. ending it
/// cannot: bpd is not that process's parent, so there is nothing to signal and
/// nothing to reap, and it is refused by name rather than quietly doing nothing
struct Reaching;

impl Interrupt for Reaching {
    fn deliver(&mut self, request: &Request) -> Result<(), Failed> {
        Err(format!("this fake is never interrupted, and got {request:?}").into())
    }

    fn terminate(&mut self) -> Result<(), Failed> {
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
        })))
    }
}

#[test]
fn a_session_whose_exit_bpd_cannot_read_is_terminated_and_never_exited() {
    let (to_adapter, mut writes) = std::io::pipe().expect("a pipe is available");
    let (reads, from_adapter) = std::io::pipe().expect("a pipe is available");

    let served = std::thread::spawn(move || {
        bpd_dap::serve(&mut Fake, Box::new(to_adapter), Box::new(from_adapter))
    });

    let mut reader = Messages::new(reads);
    let mut seen = Vec::new();
    let mut seq = 0;
    let mut exchange = |writes: &mut std::io::PipeWriter,
                        reader: &mut Messages,
                        seen: &mut Vec<serde_json::Value>,
                        command: &str,
                        arguments: &serde_json::Value| {
        seq += 1;
        let body = serde_json::json!({
            "seq": seq, "type": "request", "command": command, "arguments": arguments,
        })
        .to_string();
        write!(writes, "Content-Length: {}\r\n\r\n{body}", body.len())
            .expect("the adapter is reading");
        writes.flush().expect("the adapter is reading");

        // read up to and including the response, so that what the adapter
        // wrote in between is in `seen` in the order it wrote it. a client that
        // sent everything at once would race its own `disconnect` against the
        // requests before it
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

    exchange(
        &mut writes,
        &mut reader,
        &mut seen,
        "initialize",
        &serde_json::json!({}),
    );
    exchange(
        &mut writes,
        &mut reader,
        &mut seen,
        "launch",
        &serde_json::json!({ "program": "/tmp/fake.py", "stopOnEntry": true }),
    );
    exchange(
        &mut writes,
        &mut reader,
        &mut seen,
        "configurationDone",
        &serde_json::json!({}),
    );
    exchange(
        &mut writes,
        &mut reader,
        &mut seen,
        "continue",
        &serde_json::json!({ "threadId": THREAD }),
    );
    // the client is done talking, and closing its end is what ends the adapter
    drop(writes);

    seen.extend(reader.collect());
    let messages = seen;
    served
        .join()
        .expect("the adapter thread ended cleanly")
        .expect("the adapter ran to the end of its input");

    let events: Vec<&str> = messages
        .iter()
        .filter(|message| message["type"] == "event")
        .filter_map(|message| message["event"].as_str())
        .collect();

    assert!(
        events.contains(&"terminated"),
        "the client has to be told the session is over, and got {events:?}"
    );
    assert!(
        !events.contains(&"exited"),
        "`exited` carries an `exitCode` and there is none to carry: bpd did not \
         start that process and never learns what it exited with. got {events:?}"
    );

    // and the reason is on the client's console rather than only in a silence
    let said: String = messages
        .iter()
        .filter(|message| message["event"] == "output")
        .filter_map(|message| message["body"]["output"].as_str())
        .collect();
    assert!(
        said.contains("did not start that process"),
        "the client is told why there is no exit code, and got {said:?}"
    );
    assert!(
        said.contains("no exit code"),
        "the client is told why there is no exit code, and got {said:?}"
    );
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
