//! the control connection to the engine
//!
//! this module is transport only: it connects, it holds the socket, and it
//! serialises access to it. what a stop *means* is [`crate::stops`]'s problem,
//! because deciding it needs the interpreter and this does not
//!
//! ## why a thread of its own reads it
//!
//! a stop holds one thread and leaves the rest of the program running, so
//! several threads can be held at once and the engine has to be able to talk to
//! any of them. if a held thread read the connection itself, a second held
//! thread would have to wait for the first to be finished with it — and a
//! thread waiting on the debugger is a thread that is not running, which is the
//! opposite of what the model promises
//!
//! so a rust thread owns the reading end and routes each request to the thread
//! it is addressed to. it never touches the interpreter: everything that needs
//! the GIL is answered on a python thread that bpd is already holding, because
//! evaluating an expression anywhere else would run the program's code on the
//! wrong thread and quietly report another thread's `threading.current_thread()`

use std::io;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use bpd_protocol::message::{FromAgent, FromEngine};
use bpd_protocol::{TOKEN_LEN, frame, message};

use crate::stops;

/// the exit code used when the debugger disappears mid-session
///
/// borrowed from `EX_SOFTWARE`. any exit code is a lie of some kind here; what
/// matters is that the debuggee does **not** carry on running unobserved after
/// the thing that was supposed to be watching it has gone
const ENGINE_LOST: i32 = 70;

/// the writing end, or nothing before `attach`
static WRITER: Mutex<Option<TcpStream>> = Mutex::new(None);

/// whether the program has finished
///
/// the reader thread treats a closed connection as the debugger vanishing,
/// which is right while the program is running and wrong once it has ended —
/// the engine closes its end when the session is over, and exiting 70 for that
/// would report a failure where there was none
static FINISHED: AtomicBool = AtomicBool::new(false);

/// connect to the engine, complete the handshake, and start reading
pub(crate) fn attach(endpoint: &str, token_hex: &str) -> io::Result<()> {
    let token = decode_token(token_hex)?;
    let mut stream = TcpStream::connect(endpoint)?;

    // the agent announces itself first: an engine that is listening for
    // something else finds out before it has sent anything
    frame::write_handshake(&mut stream, &token).map_err(|error| framing(&error))?;
    frame::read_handshake(&mut stream, &token).map_err(|error| framing(&error))?;

    // one handle for reading and one for writing, on the same socket. the
    // reader blocks for the whole life of the session, and a held thread has to
    // be able to answer while it does
    let reading = stream.try_clone()?;
    *writer() = Some(stream);

    std::thread::Builder::new()
        .name("bpd-control".to_string())
        .spawn(move || read_requests(reading))?;
    Ok(())
}

fn lock<T>(mutex: &'static Mutex<T>) -> MutexGuard<'static, T> {
    mutex
        .lock()
        .expect("nothing panics while holding an agent lock: every path through one is a send, a receive, or a field read")
}

fn writer() -> MutexGuard<'static, Option<TcpStream>> {
    lock(&WRITER)
}

/// the program has ended, so a connection that closes now is not a loss
pub(crate) fn mark_finished() {
    FINISHED.store(true, Ordering::Relaxed);
}

/// tell the engine something
///
/// the connection is taken for the write and released, so nothing holds it
/// across a stop. a write that fails means the engine is gone, and the debuggee
/// does not carry on without it
pub(crate) fn send(message: &FromAgent) {
    let mut writer = writer();
    let Some(stream) = writer.as_mut() else {
        unreachable!("nothing sends on the control connection before `attach` installed it");
    };
    if let Err(error) = message::write(stream, message) {
        drop(writer);
        lost(&format!("the control connection failed: {error}"));
    }
}

/// read requests for the life of the session and hand each to the thread it
/// names
fn read_requests(mut stream: TcpStream) {
    let mut buffer = Vec::new();
    loop {
        match message::read::<_, FromEngine>(&mut stream, &mut buffer) {
            Ok(Some(request)) => stops::route(request),
            Ok(None) => {
                if FINISHED.load(Ordering::Relaxed) {
                    return;
                }
                lost("the debugger closed the control connection while the program was running");
            }
            Err(error) => {
                if FINISHED.load(Ordering::Relaxed) {
                    return;
                }
                lost(&format!("the control connection failed: {error}"));
            }
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
pub(crate) fn lost(reason: &str) -> ! {
    fatal(&format!(
        "{reason}. the program was stopped and is not being resumed"
    ));
}

/// the agent cannot do what it was asked and cannot say so over the connection
///
/// the same exit as [`lost`], and for the same reason: there is nothing on the
/// other end that would receive an error, and an exception raised here would
/// unwind into whatever user frame happened to be running
#[expect(
    clippy::print_stderr,
    clippy::exit,
    reason = "the debuggee has no other channel left, and continuing is not an \
              option — see above"
)]
pub(crate) fn fatal(reason: &str) -> ! {
    eprintln!("bpd: {reason}");
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
