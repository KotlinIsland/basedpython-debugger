//! the control connection to the engine
//!
//! this module is transport only: it connects, it holds the socket, and it
//! serialises access to it. what a stop *means* is [`crate::session`]'s
//! problem, because deciding it needs the interpreter and this does not

use std::io;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use bpd_protocol::message::{FromAgent, FromEngine};
use bpd_protocol::{TOKEN_LEN, frame, message};

/// the exit code used when the debugger disappears mid-session
///
/// borrowed from `EX_SOFTWARE`. any exit code is a lie of some kind here; what
/// matters is that the debuggee does **not** carry on running unobserved after
/// the thing that was supposed to be watching it has gone
const ENGINE_LOST: i32 = 70;

/// the live control connection, or nothing before `attach`
static ATTACHED: Mutex<Option<Attached>> = Mutex::new(None);

#[derive(Debug)]
struct Attached {
    stream: TcpStream,
    buffer: Vec<u8>,
    target: PathBuf,
    stopped_at_entry: bool,
}

/// connect to the engine and complete the handshake
pub(crate) fn attach(endpoint: &str, token_hex: &str, target: PathBuf) -> io::Result<()> {
    let token = decode_token(token_hex)?;
    let mut stream = TcpStream::connect(endpoint)?;

    // the agent announces itself first: an engine that is listening for
    // something else finds out before it has sent anything
    frame::write_handshake(&mut stream, &token).map_err(|error| framing(&error))?;
    frame::read_handshake(&mut stream, &token).map_err(|error| framing(&error))?;

    let mut attached = lock();
    *attached = Some(Attached {
        stream,
        buffer: Vec::new(),
        target,
        stopped_at_entry: false,
    });
    Ok(())
}

fn lock() -> MutexGuard<'static, Option<Attached>> {
    ATTACHED
        .lock()
        .expect("the attach lock is released by `Held` on every path, including the ones that exit")
}

/// the program this agent was asked to run
pub(crate) fn target() -> Option<PathBuf> {
    lock().as_ref().map(|attached| attached.target.clone())
}

/// whether the entry stop has already happened
pub(crate) fn has_stopped_at_entry() -> bool {
    lock()
        .as_ref()
        .is_some_and(|attached| attached.stopped_at_entry)
}

/// exclusive use of the control connection
///
/// held for the whole of a stop, so the engine sees one conversation at a time.
/// a second thread reaching a breakpoint blocks here until the first is
/// resumed, and then reports its own stop — which is the honest ordering, not a
/// claim that both were held
#[derive(Debug)]
pub(crate) struct Held {
    guard: MutexGuard<'static, Option<Attached>>,
}

/// take the control connection
pub(crate) fn hold() -> Held {
    Held { guard: lock() }
}

/// record that the entry stop has happened, so it happens once
pub(crate) fn mark_stopped_at_entry() {
    let mut attached = lock();
    let Some(attached) = attached.as_mut() else {
        unreachable!("the entry stop cannot be reached before `attach` installed the connection");
    };
    attached.stopped_at_entry = true;
}

impl Held {
    fn attached(&mut self) -> &mut Attached {
        let Some(attached) = self.guard.as_mut() else {
            unreachable!("nothing holds the control connection before `attach` installed it");
        };
        attached
    }

    /// tell the engine something
    ///
    /// a write that fails means the engine is gone, and the debuggee does not
    /// carry on without it
    pub(crate) fn send(&mut self, message: &FromAgent) {
        let attached = self.attached();
        if let Err(error) = message::write(&mut attached.stream, message) {
            lost(&format!("the control connection failed: {error}"));
        }
    }

    /// wait for the engine's next request
    pub(crate) fn receive(&mut self) -> FromEngine {
        let attached = self.attached();
        match message::read::<_, FromEngine>(&mut attached.stream, &mut attached.buffer) {
            Ok(Some(request)) => request,
            Ok(None) => {
                lost("the debugger closed the control connection while the program was stopped")
            }
            Err(error) => lost(&format!("the control connection failed: {error}")),
        }
    }
}

/// the debugger is gone, and the debuggee must not carry on without it
///
/// written and exited rather than raised: there is nothing on the other end to
/// receive an error, and an exception here would unwind into whatever user
/// frame happened to be running and could be caught by it. a program that
/// swallowed the loss of its debugger and kept going is the outcome this
/// prevents
#[expect(
    clippy::print_stderr,
    clippy::exit,
    reason = "the debuggee has no other channel left, and continuing is not an \
              option — see above"
)]
pub(crate) fn lost(reason: &str) -> ! {
    eprintln!("bpd: {reason}. the program was stopped and is not being resumed");
    std::process::exit(ENGINE_LOST);
}

fn framing(error: &frame::Error) -> io::Error {
    io::Error::other(error.to_string())
}

fn decode_token(hex: &str) -> io::Result<[u8; TOKEN_LEN]> {
    if hex.len() != TOKEN_LEN * 2 {
        return Err(io::Error::other(format!(
            "the session token is {} characters, expected {}",
            hex.len(),
            TOKEN_LEN * 2
        )));
    }

    let mut token = [0u8; TOKEN_LEN];
    for (byte, pair) in token.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        let text = std::str::from_utf8(pair).map_err(io::Error::other)?;
        *byte = u8::from_str_radix(text, 16).map_err(io::Error::other)?;
    }
    Ok(token)
}
