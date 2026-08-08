//! the control connection, and the stop it serves
//!
//! the agent holds the GIL while it is stopped. at an entry stop that is a
//! complete stop of the whole program, because no user thread exists yet — the
//! program has run nothing. it is **not** sufficient for a breakpoint, where
//! other threads are already running and holding the GIL only stops the ones
//! that want it. real stop coordination is its own piece of work

use std::io;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Mutex;

use bpd_protocol::message::{FromAgent, FromEngine, StopReason};
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

    let mut attached = ATTACHED.lock().expect("the attach lock is never poisoned");
    *attached = Some(Attached {
        stream,
        buffer: Vec::new(),
        target,
        stopped_at_entry: false,
    });
    Ok(())
}

/// the program this agent was asked to run
pub(crate) fn target() -> Option<PathBuf> {
    ATTACHED
        .lock()
        .expect("the attach lock is never poisoned")
        .as_ref()
        .map(|attached| attached.target.clone())
}

/// whether the entry stop has already happened
pub(crate) fn has_stopped_at_entry() -> bool {
    ATTACHED
        .lock()
        .expect("the attach lock is never poisoned")
        .as_ref()
        .is_some_and(|attached| attached.stopped_at_entry)
}

/// report the entry stop and block until the engine resumes
///
/// on losing the engine this exits the process rather than continuing. carrying
/// on would leave a program running that its user believes is stopped, which is
/// the exact failure this project exists to prevent
pub(crate) fn stop_at_entry() {
    let mut guard = ATTACHED.lock().expect("the attach lock is never poisoned");
    let Some(attached) = guard.as_mut() else {
        unreachable!("the entry stop cannot be reached before attach installed the connection");
    };
    attached.stopped_at_entry = true;

    let exchange = message::write(
        &mut attached.stream,
        &FromAgent::Stopped {
            reason: StopReason::Entry,
        },
    )
    .and_then(|()| message::read::<_, FromEngine>(&mut attached.stream, &mut attached.buffer));

    match exchange {
        Ok(Some(FromEngine::Resume)) => {}
        // `FromEngine` is non-exhaustive, so a newer engine could ask for
        // something this build cannot do. carrying on regardless would resume a
        // program whose debugger asked for the opposite
        Ok(Some(other)) => lost(&format!(
            "the debugger asked for {other:?}, which this agent does not \
             understand"
        )),
        Ok(None) => {
            lost("the debugger closed the control connection while the program was stopped")
        }
        Err(error) => lost(&format!("the control connection failed: {error}")),
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
fn lost(reason: &str) -> ! {
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
