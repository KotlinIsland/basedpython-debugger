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
//!
//! ## it is not on the process while it forks
//!
//! since 3.12 cpython counts the process's **operating system** threads at
//! `os.fork()` and raises a `DeprecationWarning` when there is more than one —
//! and a program can put that in its own data with
//! `warnings.catch_warnings(record=True)`, not only on its stderr. this thread
//! is registered with nothing, so `threading.active_count()` and
//! `threading.enumerate()` are the same under bpd as without it, and the count
//! cpython takes is the one place its existence shows
//!
//! so it does not exist across a fork. `os.register_at_fork(before=…)` stands
//! it down and `after_in_parent=…` starts it again, which is
//! [`crate::forks`]'s doing and [`stand_down`] and [`resume_reading`]'s work.
//! the count cpython takes is after every `before` handler has run and before
//! every `after_in_parent` one does — measured on 3.13, 3.14, 3.15 and a
//! free-threaded 3.14, from both sides: a thread stopped in a `before` handler
//! is not counted, and one started in an `after_in_parent` handler is not
//! either

use std::io;
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;

use bpd_protocol::message::{FromAgent, FromEngine};
use bpd_protocol::{TOKEN_LEN, frame, message};

use crate::cells::ForkCell;
use crate::stops;

/// the exit code used when the debugger disappears mid-session
///
/// borrowed from `EX_SOFTWARE`. any exit code is a lie of some kind here; what
/// matters is that the debuggee does **not** carry on running unobserved after
/// the thing that was supposed to be watching it has gone
const ENGINE_LOST: i32 = 70;

/// the writing end, or nothing before `attach`
///
/// in a [`ForkCell`] because a program thread can be inside [`send`], holding
/// it across a socket write, while another thread of the same process calls
/// `os.fork()` — there is no GIL on a free-threaded build to keep the two
/// apart. the child's copy would then be locked by a thread the fork did not
/// keep
static WRITER: ForkCell<Option<TcpStream>> = ForkCell::new(no_writer);

/// what the writing end is before `attach`, and in a forked child
const fn no_writer() -> Option<TcpStream> {
    None
}

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
/// forked child, and in [`send`] *before* the writing end is locked. a fork
/// handler that had to take a lock to find out whether this process still owns
/// a session would be waiting on whatever a thread the fork did not keep was
/// holding. see [`crate::forks`] and [`crate::cells`]
static DETACHED: AtomicBool = AtomicBool::new(false);

/// every descriptor this session opened in the debuggee
///
/// [`attach`] makes two handles on one socket, one for the reader thread and
/// one for writing, and a socket pair the reader thread is woken through. a
/// fork copies all four. the numbers are kept because a forked child has to
/// close all four and can reach none of them: the writing handle is behind a
/// lock it must not take, the wakeup's writing half is behind another, and the
/// reading handles live in a `static` a detached process must not read
///
/// `-1` before `attach`, which is not a descriptor on any platform bpd runs on
#[cfg(unix)]
static DESCRIPTORS: [AtomicI32; 4] = [
    AtomicI32::new(-1),
    AtomicI32::new(-1),
    AtomicI32::new(-1),
    AtomicI32::new(-1),
];

/// the reading end of the control connection, and the wakeup beside it
///
/// what a reader thread is handed, and what it hands back when it stands down.
/// it is one value because the two are read together and a reader that had one
/// without the other could not be stood down
struct Reading {
    /// the frames arrive here
    stream: TcpStream,
    /// the reading half of the pair [`stand_down`] writes a byte into
    ///
    /// non blocking, because it is drained rather than waited on: the wait is
    /// the `poll` over both descriptors
    #[cfg(unix)]
    wakeup: UnixStream,
}

/// who is reading the control connection, and what is keeping them off it
///
/// **nothing under this lock needs the interpreter.** that is what makes it
/// safe to hold while a python thread is inside `os.fork()` with the GIL: a
/// thread holding it always makes progress, so a thread waiting for it cannot
/// be waiting on something that is waiting for the GIL
struct Reader {
    /// how many forks are between their `before` handler and their
    /// `after_in_parent` one
    ///
    /// a count rather than a flag because two threads can fork at once. the
    /// thread is put back when the **last** of them is through, so a fork that
    /// starts while another is in flight still finds it gone
    #[cfg(unix)]
    forking: usize,
    /// the writing half of the wakeup pair, or nothing before [`attach`]
    #[cfg(unix)]
    waker: Option<UnixStream>,
    /// the reading end while no thread holds it
    idle: Option<Reading>,
    /// the thread that holds it, which hands it back when it stands down
    running: Option<JoinHandle<Reading>>,
}

/// in a [`ForkCell`] for the reason [`WRITER`] is: [`stand_down`] takes this,
/// and the `forking` count above exists because two threads can fork at once —
/// so a fork can land while a *concurrent* fork holds it, and the child's copy
/// would be locked by neither of them
static READER: ForkCell<Reader> = ForkCell::new(no_reader);

/// what the reader is before `attach`, and in a forked child
const fn no_reader() -> Reader {
    Reader {
        #[cfg(unix)]
        forking: 0,
        #[cfg(unix)]
        waker: None,
        idle: None,
        running: None,
    }
}

/// where the engine listens, and what this session presents to it
///
/// kept for the life of the process so that a **forked child** can open a
/// connection of its own. a fork inherits memory, so nothing about debugging one
/// has to go through the environment — which is why this is the only channel a
/// child needs and why the parity guarantee is untouched by it
///
/// a [`OnceLock`] rather than anything larger because of where it is read: in a
/// fork handler, where taking a lock a thread the fork did not keep was holding
/// would wait for ever. reading one is an atomic load
static ENGINE: OnceLock<Engine> = OnceLock::new();

/// the endpoint and token [`attach`] was given
struct Engine {
    endpoint: String,
    token: [u8; TOKEN_LEN],
}

/// connect to the engine, complete the handshake, and start reading
pub(crate) fn attach(endpoint: &str, token_hex: &str) -> io::Result<()> {
    let token = decode_token(token_hex)?;
    ENGINE
        .set(Engine {
            endpoint: endpoint.to_string(),
            token,
        })
        .unwrap_or_else(|_| unreachable!("the agent's entry point attaches once"));
    connect(endpoint, &token)
}

/// open a control connection of this process's own, and start reading it
///
/// the same work for the first connection and for the one a forked child makes:
/// there is one description of what a session's transport is, rather than one
/// that is correct and one that has to be kept in step with it
fn connect(endpoint: &str, token: &[u8; TOKEN_LEN]) -> io::Result<()> {
    let mut stream = TcpStream::connect(endpoint)?;

    // the agent announces itself first: an engine that is listening for
    // something else finds out before it has sent anything
    frame::write_handshake(&mut stream, token).map_err(|error| framing(&error))?;
    frame::read_handshake(&mut stream, token).map_err(|error| framing(&error))?;

    // one handle for reading and one for writing, on the same socket. the
    // reader is blocked on the reading one whenever the session is idle, and a
    // held thread has to be able to answer while it is
    let reading = stream.try_clone()?;

    #[cfg(unix)]
    let (waker, wakeup) = {
        let (waker, wakeup) = UnixStream::pair()?;
        wakeup.set_nonblocking(true)?;
        (waker, wakeup)
    };

    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        DESCRIPTORS[0].store(stream.as_raw_fd(), Ordering::Relaxed);
        DESCRIPTORS[1].store(reading.as_raw_fd(), Ordering::Relaxed);
        DESCRIPTORS[2].store(waker.as_raw_fd(), Ordering::Relaxed);
        DESCRIPTORS[3].store(wakeup.as_raw_fd(), Ordering::Relaxed);
    }

    *writer() = Some(stream);

    let mut reader = reader();
    #[cfg(unix)]
    {
        reader.waker = Some(waker);
    }
    reader.idle = Some(Reading {
        stream: reading,
        #[cfg(unix)]
        wakeup,
    });
    start_reading(&mut reader)
}

fn lock<T>(mutex: &'static Mutex<T>) -> MutexGuard<'static, T> {
    mutex
        .lock()
        .expect("nothing panics while holding an agent lock: every path through one is a send, a receive, or a field read")
}

fn writer() -> MutexGuard<'static, Option<TcpStream>> {
    lock(WRITER.get())
}

fn reader() -> MutexGuard<'static, Reader> {
    lock(READER.get())
}

/// hand the reading end to a thread of its own
///
/// the caller holds the lock for the whole transition, so there is never a
/// moment at which the connection has two readers or none unaccounted for
fn start_reading(reader: &mut Reader) -> io::Result<()> {
    let reading = reader
        .idle
        .take()
        .unwrap_or_else(|| unreachable!("the reading end is idle whenever no thread holds it"));

    // the reading end goes with the closure, so a spawn that fails closes it.
    // that is the right end of a bad choice: the alternative is a session whose
    // connection is open and unread, which looks exactly like a debuggee that
    // is busy. every caller of this reports the failure and stops
    let handle = std::thread::Builder::new()
        .name("bpd-control".to_string())
        .spawn(move || read_requests(reading))?;
    reader.running = Some(handle);
    Ok(())
}

/// take the reader thread off the process, because it is about to fork
///
/// called from `os.register_at_fork(before=…)`, on the thread that is forking,
/// with the GIL held. **the GIL is not given back**: everything here is a
/// socket write and a join, none of it needs the interpreter, and giving it
/// back would put a wait for the GIL inside `os.fork()` that a bare run does
/// not have
///
/// the thread is joined rather than signalled and left, because `join` is
/// `pthread_join` and that is the only thing that says the operating system
/// thread has gone — which is what cpython counts
///
/// it stands down **between frames**, so what has arrived and not been read
/// stays in the kernel's receive buffer for the next reader. a request that
/// arrives inside the window is therefore delayed and never lost, and the
/// length-prefixed stream cannot desynchronise: no reader ever holds half a
/// frame. a thread this session is holding at a breakpoint stays held across
/// the fork and is resumed by the request that was waiting
#[cfg(unix)]
pub(crate) fn stand_down() {
    // a forked child gave the session up and never started a reader, so there
    // is nothing here to take off the process. going on would count a fork the
    // matching `after_in_parent` handler will not count back, because that half
    // returns here too
    if detached() {
        return;
    }

    let mut reader = reader();
    reader.forking += 1;
    let Some(handle) = reader.running.take() else {
        // another fork is already in flight and has taken it off
        return;
    };

    {
        use std::io::Write as _;
        let waker = reader
            .waker
            .as_mut()
            .unwrap_or_else(|| unreachable!("`attach` installs the waker before the first reader"));
        if let Err(error) = waker.write_all(&[0]) {
            fatal(&format!(
                "the reader of the control connection could not be told to \
                 stand down for a fork: {error}. it cannot be joined, and a \
                 fork with it still on the process would change what the \
                 program records"
            ));
        }
    }

    match handle.join() {
        Ok(mut reading) => {
            reading.drain_wakeup();
            reader.idle = Some(reading);
        }
        // a panic in the agent is a broken invariant, and this is the one place
        // it would otherwise be swallowed: the thread is gone either way, so
        // the fork would succeed and the session would be dead
        Err(_) => fatal(
            "the reader of the control connection panicked. the session cannot \
             be answered and the program is not being left to run undebugged",
        ),
    }
}

/// put the reader thread back, now that the fork is over
///
/// called from `os.register_at_fork(after_in_parent=…)`, in the process that
/// did the forking. the child registers nothing here — it has given the session
/// up and there is nothing for it to read
#[cfg(unix)]
pub(crate) fn resume_reading() {
    if detached() {
        return;
    }

    let mut reader = reader();
    assert!(
        reader.forking > 0,
        "every fork's `after_in_parent` handler follows its own `before` one"
    );
    reader.forking -= 1;
    if reader.forking > 0 {
        // another fork is still in flight, and the thread it is waiting to be
        // rid of is this one
        return;
    }

    if let Err(error) = start_reading(&mut reader) {
        fatal(&format!(
            "the reader of the control connection could not be started again \
             after a fork: {error}. the session is over — nothing can answer a \
             stop or deliver a resume — and the program is not being left to \
             run undebugged"
        ));
    }
}

/// whether this process has ever opened a session of its own
///
/// what makes the `sitecustomize` an `exec`'d child is entered through
/// **idempotent**, which the design requires of it: the directory holding that
/// file is on the debuggee's own `sys.path` as well, so a program that imports
/// `sitecustomize` by hand reaches the child entry point in a process that
/// already has a session. it answers `true` there and the entry point returns
///
/// it is [`ENGINE`] rather than a flag of its own because that cell is set by
/// the one call that opens a session, so there is nothing to keep in step
#[cfg(unix)]
pub(crate) fn attached() -> bool {
    ENGINE.get().is_some()
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
/// the wakeup pair goes the same way and for the same reason: it is two more
/// descriptors this session opened in a process that is no longer a debuggee
///
/// **no lock is taken, on any build.** the reader thread is not on the process
/// while it forks — `os.register_at_fork(before=…)` stands it down — so it is
/// not the thread that could be holding one. what could is a thread of the
/// **program's**: one inside [`send`] holds the writing end across a socket
/// write, and one inside [`crate::stops::enter`] holds the stop registry. on a
/// gil build `os.fork()` holds the GIL and so do those, which keeps them apart;
/// on a free-threaded build nothing does, and a first-class target does not get
/// an argument that holds on one build
///
/// so the three cells are **replaced** rather than emptied, with an atomic
/// store each — see [`crate::cells`], which is also where the abandoned cells
/// are accounted for. the numbered closes above are why they must not be
/// dropped: what they hold owns these descriptor numbers, and a later `close`
/// of a number this process has recycled would close a file the program opened
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
        // SAFETY: these are the descriptors `attach` opened in the process this
        // one was forked from, and nothing in *this* process will close any of
        // them again. the values that own them are in cells this call is about
        // to abandon, and an abandoned cell is never freed — so nothing will
        // ever drop a `TcpStream` or a `UnixStream` for one of these numbers,
        // and this is the only close they will get
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

    // and now the cells those values live in, so that this process can use the
    // writing end, the reader and the stop registry again without waiting on a
    // lock a thread the fork did not keep was holding. it does not take any of
    // them to do it
    WRITER.abandon();
    READER.abandon();
    stops::abandon();
    true
}

/// open a session of this forked child's own, on the endpoint it inherited
///
/// called from `os.register_at_fork(after_in_child=…)`, **after** [`detach`] has
/// closed the four descriptors this process inherited and replaced the three
/// cells they lived in. so what this opens is a fifth descriptor rather than a
/// second owner of the parent's, and there is no instant at which this process
/// holds a writable handle on a socket it does not own
///
/// the endpoint and the token come out of [`ENGINE`], which is inherited memory.
/// nothing goes through the environment, nothing is written to disk, and a
/// program that reads its own `os.environ` and its own `sys.path` sees exactly
/// what it would have — the fork's whole advantage over an exec
///
/// the engine keeps the listener the first agent attached on open for the life
/// of the debuggee, and a peer that presents that debuggee's token becomes a
/// second session of it. so this needs no new token and no new port: a second
/// token would be a second lifetime to get wrong, and this connection is
/// authenticated by exactly the thing the first one was
///
/// **[`DETACHED`] is cleared before the connection is made**, and the ordering
/// is safe for a reason that is about the process rather than about the code: a
/// fork keeps only the calling thread, this *is* that thread, and it is inside a
/// fork handler. there is no other thread in this process that could reach
/// [`send`] in the window, and the reader that could route a request into one
/// does not exist until the last line
#[cfg(unix)]
pub(crate) fn reattach() -> io::Result<()> {
    let engine = ENGINE
        .get()
        .unwrap_or_else(|| unreachable!("a forked child inherits the endpoint `attach` stored"));
    DETACHED.store(false, Ordering::SeqCst);
    match connect(&engine.endpoint, &engine.token) {
        Ok(()) => Ok(()),
        Err(error) => {
            // back to what a child gets with child debugging off. it is not a
            // debuggee, so nothing of it may write to a connection it does not
            // have — and the caller says so on this process's own stderr
            DETACHED.store(true, Ordering::SeqCst);
            Err(error)
        }
    }
}

/// where the engine listens, for a message about not having reached it
#[cfg(unix)]
pub(crate) fn endpoint() -> &'static str {
    ENGINE
        .get()
        .map_or("the engine", |engine| engine.endpoint.as_str())
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

/// what the reader thread does next
#[cfg(unix)]
enum Next {
    /// a frame has begun to arrive
    Frame,
    /// the process is about to fork, and this thread must not be on it
    StandDown,
}

#[cfg(unix)]
impl Reading {
    /// wait until there is a frame to read, or until this thread is to go
    ///
    /// standing down **wins** over a frame that has begun to arrive: whatever
    /// the kernel is holding stays there, and the reader started after the fork
    /// reads it. that is what makes the window lossless — the alternative is a
    /// reader that stands down owning part of a frame, and a length-prefixed
    /// stream cannot be resumed from the middle
    fn awaited(&mut self) -> Next {
        use std::os::fd::AsFd as _;

        use rustix::event::{PollFd, PollFlags, poll};

        loop {
            let mut watched = [
                PollFd::from_borrowed_fd(self.wakeup.as_fd(), PollFlags::IN),
                PollFd::from_borrowed_fd(self.stream.as_fd(), PollFlags::IN),
            ];
            match poll(&mut watched, None) {
                Ok(_) => {}
                // a signal the *program* installed a handler for arrives on
                // whichever thread the operating system picks, and this thread
                // is as eligible as any. it is not a failure of the connection
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => lost(&format!("the control connection failed: {error}")),
            }

            let wakeup = watched[0].revents();
            let stream = watched[1].revents();

            if wakeup.intersects(PollFlags::IN) {
                return Next::StandDown;
            }

            // an end of stream and an error are the reading path's to report,
            // in the words it already has for them
            if !stream.is_empty() {
                return Next::Frame;
            }
        }
    }

    /// take back every byte that was written to wake a reader
    ///
    /// the invariant it keeps is that the wakeup is **empty whenever the
    /// reading end is idle**, established by [`attach`] making a fresh pair and
    /// re-established here. a byte left behind would stand the next reader down
    /// the instant it started, with no fork in flight to join it — and then
    /// nothing would be reading the connection and nothing would have said so
    ///
    /// it is done by the thread that joined the reader rather than by the
    /// reader itself, because a reader can also return when the session ends,
    /// on a path that never looked at the wakeup at all
    fn drain_wakeup(&mut self) {
        use std::io::Read as _;

        let mut swallowed = [0u8; 8];
        loop {
            match self.wakeup.read(&mut swallowed) {
                Ok(read) if read > 0 => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                // the writing half lives in a `static` for the life of the
                // process, so there is no end of stream to reach here and no
                // error left that is about anything but the pair itself
                other => fatal(&format!(
                    "the wakeup of the control connection's reader could not be \
                     read back ({other:?}). the next reader would stand down \
                     the moment it started, and the session would go unread"
                )),
            }
        }
    }
}

/// read requests for as long as this thread holds the connection, and hand each
/// to the thread it names
///
/// it gives the reading end back rather than closing it. on unix that is a
/// stand-down for a fork, and the connection outlives this thread by design; on
/// every platform it is also how a session that has ended lets go
fn read_requests(mut reading: Reading) -> Reading {
    let mut buffer = Vec::new();
    loop {
        #[cfg(unix)]
        if matches!(reading.awaited(), Next::StandDown) {
            return reading;
        }

        match message::read::<_, FromEngine>(&mut reading.stream, &mut buffer) {
            Ok(Some(request)) => stops::route(request),
            Ok(None) => {
                if FINISHED.load(Ordering::Relaxed) {
                    return reading;
                }
                lost("the debugger closed the control connection while the program was running");
            }
            Err(error) => {
                if FINISHED.load(Ordering::Relaxed) {
                    return reading;
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
