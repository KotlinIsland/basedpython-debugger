//! a `disconnect` answers the requests it overtook, instead of dropping them
//!
//! the adapter reads its client on a thread of its own, and that is not an
//! optimisation: while the program runs the session is blocked on it, and the
//! two things a client may reasonably ask then — pause it, end it — cannot go
//! down that path. so `disconnect` is answered by the reader **immediately**,
//! ahead of everything already queued for the session, and the loop that serves
//! the session stops at its next turn without draining what is behind it
//!
//! which leaves requests nobody is going to get to. before, the process simply
//! went away and they went with it. that is not a small thing on this protocol:
//! a DAP client holds a future per request, and a stream that ends with them
//! outstanding is a client completing them with "the connection closed" rather
//! than with an answer — which is indistinguishable, from where it stands, from
//! the adapter having died. the intellij platform says exactly that out loud,
//! in a notification: *the connection to the debug adapter closed unexpectedly*
//!
//! so every one of them is refused, with why. a refusal is an **answer**: the
//! client learns its request was received and not acted on, which is a
//! different thing from the debugger vanishing. what this proves is not that
//! the session ends — `ended.rs` is about the ending — it is that nothing the
//! client said disappears on the way out

use std::io::{BufRead as _, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use bpd_core::{
    Addressed, Mode, Reported, Reporting, Request, Response, SessionId, Stop, StopReason,
    Threads,
};
use bpd_dap::{Configuration, Failed, Interrupt, Launcher, ProgramOutput, Session, Started};

/// the interpreter's identity for the one thread this fake ever holds
const THREAD: u64 = 4242;

/// a session that answers the two requests this conversation sends
///
/// it answers them so that a refusal in the transcript can only have come from
/// the adapter deciding there was nothing left to ask — which is what is under
/// test — rather than from a fake that would have refused them anyway
///
/// and it answers the **first** of them slowly, on purpose. the race this test
/// would otherwise be is the interesting part of the behaviour: whether the
/// session's loop had already taken the request behind the `disconnect`. so it
/// is not raced for — the fake says when the adapter is inside it and waits to
/// be let out, and the conversation is arranged around those two points
struct Willing {
    held: Vec<Stop>,
    /// told the moment the adapter is inside an answer
    entered: Sender<()>,
    /// released when the test has seen its `disconnect` answered
    released: Receiver<()>,
}

impl Session for Willing {
    fn dispatch(
        &mut self,
        asked: Addressed,
        _reporting: &mut dyn Reporting,
    ) -> Result<Response, Failed> {
        match asked.request {
            Request::SetBreakpoints { .. } => {
                // the test may have gone away, which is its business
                self.entered.send(()).ok();
                // the test releases this by dropping the sending end, so what
                // ends the wait is the channel closing rather than a value
                while self.released.recv().is_ok() {}
                Ok(Response::BreakpointsResolved {
                    resolved: Vec::new(),
                })
            }
            Request::Threads { .. } => Ok(Response::Threads(Threads {
                threads: Vec::new(),
                settle: Threads::SETTLE,
                mode: Mode::NonStop,
            })),
            other => Err(format!("this fake answers those two, and got {other:?}").into()),
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

    fn terminate(&mut self) -> Result<(), Failed> {
        Err("this fake started no process and is not its parent".into())
    }
}

struct Fake {
    /// handed to the one session this launches
    channels: std::sync::Mutex<Option<(Sender<()>, Receiver<()>)>>,
}

impl Launcher for Fake {
    fn launch(
        &self,
        _configuration: &Configuration,
        _output: Arc<dyn ProgramOutput>,
    ) -> Result<Started, Failed> {
        let (entered, released) = self
            .channels
            .lock()
            .expect("nothing panics holding the channels")
            .take()
            .ok_or("this fake launches once")?;
        Ok(Started::Stopped(Box::new(Willing {
            entered,
            released,
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

#[test]
fn every_request_a_disconnect_overtook_is_answered_rather_than_dropped() {
    let (to_adapter, mut writes) = std::io::pipe().expect("a pipe is available");
    let (reads, from_adapter) = std::io::pipe().expect("a pipe is available");
    let (entered, is_inside) = channel();
    let (release, released) = channel();

    let served = std::thread::spawn(move || {
        bpd_dap::serve(
            &Fake {
                channels: std::sync::Mutex::new(Some((entered, released))),
            },
            Box::new(to_adapter),
            Box::new(from_adapter),
            &bpd_dap::Reachable::Nowhere,
        )
    });

    let mut reader = Messages::new(reads);
    let mut messages = Vec::new();
    let mut seq = 0;
    let mut send = |writes: &mut std::io::PipeWriter, command: &str| -> i64 {
        seq += 1;
        ask(writes, seq, command);
        seq
    };

    // a session first, and read back to it. what follows has to be refused for
    // the reason under test rather than for there being nothing launched
    let started = send(&mut writes, "initialize");
    let launched = send(&mut writes, "launch");
    read_until(&mut reader, &mut messages, launched);

    // one request that the adapter is now busy answering
    let asked = send(&mut writes, "setBreakpoints");
    is_inside
        .recv()
        .expect("the adapter takes the request it was sent");

    // and then a client that says something else and immediately changes its
    // mind, which is what a person clicking stop looks like on the wire. the
    // `threads` cannot have been taken: the one thread that serves the session
    // is inside the answer above
    let also_asked = send(&mut writes, "threads");
    let ended = send(&mut writes, "disconnect");
    drop(writes);

    // the `disconnect` is answered by the reader rather than by the session, so
    // it arrives while the session is still inside that answer. once it has,
    // `threads` is known to be sitting in a queue nothing will get to
    read_until(&mut reader, &mut messages, ended);
    drop(release);

    messages.extend(reader.collect());
    served
        .join()
        .expect("the adapter thread ended cleanly")
        .expect("the adapter ran to the end of its input");

    let answer = |sent: i64| answer_to(&messages, sent);

    // every one of them, the one that was overtaken included
    for sent in [started, launched, asked, also_asked, ended] {
        assert_eq!(answer(sent)["request_seq"], sent);
    }

    // the one that was already being answered is answered. a `disconnect` does
    // not un-answer a question the session is part way through
    assert_eq!(
        answer(asked)["success"],
        true,
        "{}",
        serde_json::to_string(&messages).expect("the messages are json")
    );

    // and the one behind it is **refused**, with why, rather than silently
    // disappearing. a refusal is an answer: the client learns its request was
    // received and not acted on, which is a different thing from the adapter
    // having died — and "the adapter died" is the only thing a client can
    // conclude from a future that ends with the connection
    let dropped = answer(also_asked);
    assert_eq!(
        dropped["success"], false,
        "there was no session left to answer it, so success would be a claim \
         that something answered it: {dropped}"
    );
    assert!(
        dropped["message"]
            .as_str()
            .expect("a refusal says why")
            .contains("the debug session ended"),
        "the client is told why nothing answered it, and got {dropped}"
    );
}

/// send one framed request, carrying what every command in this file needs
///
/// one argument object for all of them: a `launch` reads `program` and a
/// `setBreakpoints` reads `source`, and neither minds the other being there —
/// a DAP request carries the client's own keys anyway
fn ask(writes: &mut std::io::PipeWriter, seq: i64, command: &str) {
    let body = serde_json::json!({
        "seq": seq, "type": "request", "command": command, "arguments": {
            "program": "/tmp/fake.py",
            "stopOnEntry": true,
            "source": { "path": "/tmp/fake.py" },
            "breakpoints": [],
        },
    })
    .to_string();
    write!(writes, "Content-Length: {}\r\n\r\n{body}", body.len())
        .expect("the adapter is reading");
    writes.flush().expect("the adapter is reading");
}

/// read up to and including the answer to `sent`, keeping everything on the way
fn read_until(reader: &mut Messages, messages: &mut Vec<serde_json::Value>, sent: i64) {
    loop {
        let message = reader
            .next_message()
            .unwrap_or_else(|| panic!("the adapter answers request {sent}"));
        let answered = message["type"] == "response" && message["request_seq"] == sent;
        messages.push(message);
        if answered {
            return;
        }
    }
}

/// the response to one request, or the failure that says nothing answered it
fn answer_to(messages: &[serde_json::Value], sent: i64) -> serde_json::Value {
    messages
        .iter()
        .find(|message| message["type"] == "response" && message["request_seq"] == sent)
        .unwrap_or_else(|| {
            panic!(
                "request {sent} was never answered, so a client holding a future for it \
                 learns what happened from the connection closing rather than from bpd. \
                 the adapter said {}",
                serde_json::to_string(messages).expect("the messages are json"),
            )
        })
        .clone()
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
