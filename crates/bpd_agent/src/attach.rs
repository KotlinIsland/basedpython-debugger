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
#[cfg(unix)]
use std::sync::atomic::AtomicI32;
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

/// whether this process has given up the session it inherited
///
/// set in a forked child and nowhere else, so it is false for the whole of an
/// ordinary session and false everywhere on a platform that has no `fork`. a
/// fork copies the descriptors of the control connection and **not** the thread
/// that reads them, so a child that went on being a debuggee would write frames
/// into a socket another process owns, interleaved with that process's own, and
/// would wait for answers nothing can send
///
/// an atomic rather than anything larger because of where it is read: in a
/// forked child, and in [`send`] *before* the writing end is locked. the reader
/// thread writes without the GIL, so a fork can land while it holds that lock,
/// and the child's copy of it would then be held for ever by a thread that does
/// not exist. see [`crate::forks`]
static DETACHED: AtomicBool = AtomicBool::new(false);

/// the two descriptors this session's socket is reached through
///
/// [`attach`] makes two handles on one socket, one for the reader thread and
/// one for writing, and a fork copies both. the numbers are kept because a
/// forked child has to close both and can reach neither: the writing handle is
/// behind a lock it must not take, and the reading handle lives on a stack that
/// did not survive the fork
///
/// `-1` before `attach`, which is not a descriptor on any platform bpd runs on
#[cfg(unix)]
static DESCRIPTORS: [AtomicI32; 2] = [AtomicI32::new(-1), AtomicI32::new(-1)];

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

    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        DESCRIPTORS[0].store(stream.as_raw_fd(), Ordering::Relaxed);
        DESCRIPTORS[1].store(reading.as_raw_fd(), Ordering::Relaxed);
    }

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

/// whether this process has given up the session it inherited
pub(crate) fn detached() -> bool {
    DETACHED.load(Ordering::Relaxed)
}

/// give up the session this process was forked holding
///
/// `true` when this call was the one that gave it up, and `false` when it had
/// already been given up — which is a child of a child, inheriting a connection
/// its parent had already let go of
///
/// the descriptors are closed rather than left open, and that is not tidiness.
/// a socket is closed when the **last** descriptor referring to it is, so a
/// forked child that kept its copies would hold this session's connection open
/// for as long as it ran: the engine would go on waiting to read from a
/// debuggee that had exited, on a connection whose only remaining owner is a
/// process it is not debugging. every worker pool and every reloader has
/// exactly that shape
///
/// closing them here does nothing whatever to the process this one was forked
/// from. a fork copies the descriptor **table**, so the parent's own entries
/// are untouched, and no FIN is sent while it still holds them — measured
/// against a real socket, and pinned by
/// `a_fork_leaves_the_parents_session_exactly_as_it_was`, which has the parent
/// reach a breakpoint after the fork and read its own tool id back
///
/// no lock is taken, on any build. the writing end is behind one that the
/// reader thread holds without the GIL, so a fork can land while it is locked
/// and the child's copy of it would then be held by a thread that does not
/// exist. see [`crate::forks`]
#[cfg(unix)]
pub(crate) fn detach() -> bool {
    if DETACHED.swap(true, Ordering::SeqCst) {
        return false;
    }

    for descriptor in &DESCRIPTORS {
        let raw = descriptor.swap(-1, Ordering::SeqCst);
        if raw < 0 {
            continue;
        }
        // SAFETY: these are the two descriptors `attach` opened in the process
        // this one was forked from, and nothing in *this* process will close
        // either of them again. the reading handle's `TcpStream` is on the
        // stack of a thread that did not survive the fork, and the writing
        // handle's is inside a `static` whose destructor never runs and which
        // nothing reads once `DETACHED` is set — [`send`] returns before it
        // looks. so this is the only close they will get
        #[expect(
            unsafe_code,
            reason = "the owning values are unreachable in a forked child, so \
                      taking the descriptors back by number is the only way to \
                      close them — see above"
        )]
        drop(unsafe {
            use std::os::fd::FromRawFd as _;
            std::os::fd::OwnedFd::from_raw_fd(raw)
        });
    }
    true
}

/// tell the engine something
///
/// the connection is taken for the write and released, so nothing holds it
/// across a stop. a write that fails means the engine is gone, and the debuggee
/// does not carry on without it
///
/// **a detached process writes nothing.** this is the one place that is
/// enforced, because it is the only place a frame is written: the socket
/// belongs to the process this one was forked from, two writers interleaving
/// mid-frame desynchronise a length-prefixed stream, and the engine renders
/// that as a peer sending a message it does not understand. silence is not this
/// degrading quietly — the decision was taken and reported by the process that
/// owns the session, which says of every fork that bpd is not debugging the
/// child
pub(crate) fn send(message: &FromAgent) {
    if detached() {
        return;
    }

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
