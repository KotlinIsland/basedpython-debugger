//! owns debuggee processes and the control plane to their agents
//!
//! the engine runs out of process. it binds a listener, launches an interpreter
//! with the agent staged on its path, and the agent connects **back** — which
//! avoids a race over who binds first and means the debuggee never has to listen
//! for anything
//!
//! the transport is loopback tcp rather than a unix socket, because a named pipe
//! on windows is a second implementation of the same thing and this one is
//! portable. what makes loopback acceptable is the session token: it is
//! generated per launch, handed to the agent through its environment, and the
//! handshake refuses a peer that cannot present it. any local process can
//! *connect*; none can join the session

pub mod agent;
pub mod cache;
pub mod launch;
mod mapping;

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bpd_core::{Request, SessionId, Stop};
use bpd_protocol::message::{FromAgent, FromEngine};
use bpd_protocol::{TOKEN_LEN, frame, message};

pub use launch::{
    Debuggee, Forwarders, Invocation, Launched, Program, launch, launch_in_terminal, launch_piped,
};

/// the result type for engine operations
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// a failure the engine reports rather than works around
///
/// only the ones that describe `bpd`'s own machinery: a socket, a process, an
/// artifact that could not be found, an interval that does not fit the wire.
/// everything that describes the **program** — nothing held, a stop that is
/// ambiguous, a refusal the agent gave a reason for — is a
/// [`bpd_core::Error`], because a front end that depends on `bpd_core` alone
/// still has to render it
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// the session refused, for a reason that is about the program
    ///
    /// carried rather than restated: an adapter renders it out of `bpd_core`
    /// without knowing an engine exists
    #[error(transparent)]
    Session(#[from] bpd_core::Error),

    /// the agent build could not be found
    #[error("{reason}")]
    LocateAgent {
        /// what was looked for, and where
        reason: String,
    },

    /// the agent build could not be put where the interpreter can import it
    #[error("could not stage the agent: `{path}`")]
    StageAgent {
        /// the file or directory the failure was about
        path: PathBuf,
        /// the underlying failure
        #[source]
        source: io::Error,
    },

    /// there is nowhere a per-user cache could go
    #[error("bpd has nowhere to cache what it stages: {reason}")]
    NoCacheHome {
        /// what was looked at, and what it would take to fix
        reason: String,
    },

    /// a directory `bpd` stages into cannot be trusted
    ///
    /// what is cached there is loaded into the user's own processes — a shared
    /// object in one cache, a `sitecustomize` a child imports in the other — so
    /// a directory somebody else can write to is somebody else choosing what
    /// runs inside the debuggee. staging refuses rather than quietly using a
    /// temporary directory instead, because a fallback would turn a broken
    /// cache into a performance regression nobody notices
    #[error("refusing to cache in `{path}`: {reason}")]
    UntrustedCache {
        /// the directory that was refused
        path: PathBuf,
        /// what is wrong with it, and what it would take to fix
        reason: String,
    },

    /// a staging cache could not be read
    #[error("could not read the cache: `{path}`")]
    ReadCache {
        /// the file or directory the failure was about
        path: PathBuf,
        /// the underlying failure
        #[source]
        source: io::Error,
    },

    /// a staging cache holds something staging never put there
    ///
    /// a cache with a surprise in it may not be the directory `bpd` thinks it
    /// is, and the one operation here that cannot be undone is deleting — so
    /// this refuses the whole of it rather than removing what it does
    /// recognise and leaving the rest
    #[error("refusing to change the cache `{root}`: {reason}")]
    UnexpectedInCache {
        /// the cache directory
        root: PathBuf,
        /// what was found in it, and what it would take to fix
        reason: String,
    },

    /// there is a basedpython source map beside the program and it cannot be used
    ///
    /// found rather than asked for, and still fatal. the map is only ever there
    /// because `by` put it there, so a program running out of a directory that
    /// has one **is** a transpiled program — and running it while ignoring a map
    /// that cannot be trusted would report every `.by` location wrongly, or
    /// report none at all without saying why
    #[error("the basedpython build this program runs out of cannot be mapped")]
    SourceMap {
        /// what is wrong with it
        #[source]
        source: bpd_core::source_map::MapError,
    },

    /// a `.by`, or a remap, was asked of a program that is not a basedpython build
    ///
    /// bpd finds a map rather than being told where one is: a program running out
    /// of a directory holding `_by_sourcemap.py` is running that build. a session
    /// without one has no `.by` in it anywhere, so there is nothing to translate a
    /// `.by` path through and no map to read again — and answering about whatever
    /// python sits beside it would be answering a question nobody asked
    #[error(
        "this program is not running out of a basedpython build, so there is no source map to \
         {wanted}. only a program `by run` transpiled has one"
    )]
    NotABasedpythonBuild {
        /// what was being attempted, written as the end of that sentence
        wanted: &'static str,
    },

    /// a `.by` the build's map does not describe
    ///
    /// refused rather than answered about whichever python sits nearest it. the
    /// answer would be about a file the user did not edit, and a replacement made
    /// against one of those is worse than a replacement that did not happen
    #[error(
        "`{}` is not a file of the basedpython build this program runs out of: {reason}",
        file.display()
    )]
    NotInTheBuild {
        /// the `.by` that was named
        file: PathBuf,
        /// what the map said about it
        reason: bpd_core::source_map::Unmapped,
    },

    /// a source map arrived after breakpoints had already been resolved
    ///
    /// the map decides what a `.by` breakpoint binds to, so a set resolved
    /// before it arrived was resolved by a different rule. rather than leave two
    /// answers about one breakpoint standing, this refuses — install the map
    /// before the first set, which is what `bpd by` does at launch
    #[error(
        "a basedpython source map was installed after {resolved} breakpoints had already been \
         resolved. those were resolved without it, so they would disagree with everything resolved \
         after — install the map before the first breakpoint set"
    )]
    MapAfterBreakpoints {
        /// how many breakpoints had already been answered
        resolved: usize,
    },

    /// the control plane could not be brought up
    #[error("could not listen for the agent on loopback")]
    Listen {
        /// the underlying failure
        #[source]
        source: io::Error,
    },

    /// the debuggee could not be started
    #[error("could not start `{interpreter}`")]
    Spawn {
        /// the interpreter that was asked for
        interpreter: PathBuf,
        /// the underlying failure
        #[source]
        source: io::Error,
    },

    /// the debuggee exited before its agent connected
    ///
    /// the usual cause is the agent failing to import, so the debuggee's own
    /// stderr is the useful part and it has already gone to the terminal
    #[error(
        "the debuggee exited with {status} before its agent connected — the \
         agent did not load, and the debuggee's own output says why"
    )]
    ExitedBeforeAttach {
        /// how the debuggee exited
        status: String,
    },

    /// the agent did not connect in time, and the debuggee is still running
    #[error(
        "the agent did not connect within {}s, and the debuggee is still \
         running. it was launched with the agent on its path but never \
         announced itself",
        timeout.as_secs()
    )]
    AttachTimeout {
        /// how long was waited
        timeout: Duration,
    },

    /// whoever was asked to start the debuggee says it did not
    ///
    /// nothing is waited for after this. a wait for an agent that was never
    /// launched is a timeout thirty seconds later with a cause nobody would
    /// find, and the thing that was asked has just said what went wrong
    #[error("the program was not started")]
    NotStarted {
        /// what the thing that was asked said
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// the debuggee was started in somebody else's terminal and never connected
    ///
    /// the sibling of [`Self::AttachTimeout`] for a launch bpd did not spawn.
    /// it is a **different** message rather than the same one, because the
    /// thing to go and look at is different: there is no child to poll, so bpd
    /// cannot tell a program that failed to start from one that is slow — and
    /// the terminal the client opened is where the interpreter said which
    #[error(
        "the program was started in a terminal the client owns and no agent \
         connected within {}s. bpd is not that process's parent, so it cannot \
         say whether the interpreter ever ran — the terminal the client opened \
         is where the interpreter's own output is, and a command that failed to \
         start says why there",
        timeout.as_secs()
    )]
    NoAgentFromTerminal {
        /// how long was waited
        timeout: Duration,
    },

    /// the debuggee in somebody else's terminal ended before it was held
    ///
    /// what a program that does not compile does on that path: the agent
    /// connects, fails to build the program, and the interpreter prints the
    /// `SyntaxError` where it was started. there is no exit code to report with
    /// it — bpd is not that process's parent
    #[error(
        "the program started in a terminal the client owns ended before it \
         reached its first statement. bpd is not that process's parent, so \
         there is no exit code to report — the terminal the client opened has \
         the interpreter's own words for what went wrong, a `SyntaxError` among \
         them"
    )]
    EndedInTerminal,

    /// the control connection failed
    #[error("the control connection to the agent failed")]
    Control {
        /// the underlying framing failure
        #[source]
        source: frame::Error,
    },

    /// the agent hung up when a message was expected
    #[error("the agent disconnected while the engine was waiting for {expected}")]
    AgentGone {
        /// what the engine was waiting for
        expected: &'static str,
    },

    /// an interval was asked for that does not fit the wire
    ///
    /// the protocol carries milliseconds as a `u32`. a longer wait is refused
    /// rather than silently truncated, because a settle interval that quietly
    /// became a different one would make every "still" it reported mean
    /// something the caller did not ask for
    #[error(
        "an interval of {settle:?} does not fit the protocol, which carries \
         milliseconds as a 32 bit number — under 49 days"
    )]
    SettleTooLong {
        /// what was asked for
        settle: Duration,
    },

    /// a request that has to be waited for was sent on an [`Interrupt`]
    ///
    /// an interrupt reaches a program that is **running**, and everything the
    /// agent answers is answered on a thread it is already holding. so there is
    /// exactly one request it can carry, and the rest are refused here rather
    /// than written into a socket nobody will answer them on
    #[error(
        "{request} cannot be sent to a running program: the agent answers a \
         request on a thread it is holding, and there is none. a pause is the \
         only request that reaches a running program, because arming one is how \
         a thread becomes held"
    )]
    NotAnInterrupt {
        /// what was asked for
        request: &'static str,
    },

    /// the agent said something the engine was not waiting for
    ///
    /// reachable only from an agent newer than this engine, which the handshake
    /// already refuses — so this is a bug rather than a configuration, and it
    /// says so rather than being absorbed
    #[error("the agent sent {event} while the engine was waiting for {expected}")]
    UnexpectedEvent {
        /// what arrived
        event: String,
        /// what was being waited for
        expected: &'static str,
    },
}

impl From<frame::Error> for Error {
    fn from(source: frame::Error) -> Self {
        Self::Control { source }
    }
}

/// how long the engine waits for an agent to announce itself
///
/// generous, because a cold interpreter importing a large extension on a loaded
/// machine is slow, and a timeout that fires early looks exactly like a bug in
/// the agent
const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);

/// how often the wait for an agent checks whether the debuggee died instead
const ATTACH_POLL: Duration = Duration::from_millis(5);

/// how long a connection that arrived on a live listener has to handshake
///
/// short on purpose, and shorter than [`ATTACH_TIMEOUT`], because the two are
/// not the same situation. at launch nothing else is happening and a cold
/// interpreter is allowed to be slow. a connection arriving mid-session lands
/// in the middle of a wait on a program that is running, and a peer that
/// connects and says nothing must not be able to hold that wait up
const HANDSHAKE_PATIENCE: Duration = Duration::from_secs(2);

/// how many sessions this engine has minted an id for
///
/// the engine is where a session id comes from, because uniqueness is a
/// property of the thing that can see every session and an agent can see only
/// itself. one counter for the whole process rather than one per listener: two
/// listeners with their own counters would both mint a first session, which is
/// the collision this exists to remove
static SESSIONS: AtomicU64 = AtomicU64::new(0);

/// a secret in the form the agent receives it
fn hex(secret: &[u8; TOKEN_LEN]) -> String {
    let mut written = String::with_capacity(TOKEN_LEN * 2);
    for byte in secret {
        use std::fmt::Write as _;
        write!(written, "{byte:02x}").expect("writing to a string cannot fail");
    }
    written
}

/// name a session nothing else will be named
fn mint_session() -> SessionId {
    let minted = SESSIONS.fetch_add(1, Ordering::Relaxed) + 1;
    SessionId::new(
        NonZeroU64::new(minted)
            .expect("the count is incremented before it is used, so it is not 0"),
    )
}

/// the control plane, and the door every later session comes through
#[derive(Debug)]
pub struct Listener {
    socket: TcpListener,
    token: [u8; TOKEN_LEN],
    /// what a child that was **`exec`'d** presents instead
    ///
    /// a second secret rather than a second port. it exists because of where it
    /// has to live: an `exec`'d child inherits nothing but the environment, so
    /// the token it presents is readable by every descendant of the debuggee
    /// and by anything that can read that process's environment. the session
    /// token is taken back out of the environment before the program runs and
    /// must stay out — a peer holding it could write frames into the session
    /// bpd is already answering
    ///
    /// so this one's whole power is to *open* a session. it is minted per
    /// debuggee and is **not** rotated per child: what fixes a child's
    /// environment is `subprocess` building the block before the audit event is
    /// raised, and rewriting it there is the undocumented path
    /// [child processes](../../../docs/development/subprocesses.md) rules out
    child: [u8; TOKEN_LEN],
}

impl Listener {
    /// bind loopback on a port the operating system chooses
    pub fn bind() -> Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .map_err(|source| Error::Listen { source })?;
        listener
            .set_nonblocking(true)
            .map_err(|source| Error::Listen { source })?;

        let mut token = [0u8; TOKEN_LEN];
        let mut child = [0u8; TOKEN_LEN];
        for secret in [&mut token, &mut child] {
            getrandom::fill(secret).map_err(|source| Error::Listen {
                source: io::Error::other(source),
            })?;
        }

        Ok(Self {
            socket: listener,
            token,
            child,
        })
    }

    /// where the agent should connect
    pub fn endpoint(&self) -> Result<SocketAddr> {
        self.socket
            .local_addr()
            .map_err(|source| Error::Listen { source })
    }

    /// this session's token, in the form the agent receives it
    pub fn token_hex(&self) -> String {
        hex(&self.token)
    }

    /// the token an `exec`'d child of this debuggee presents
    ///
    /// handed to the agent at launch and put into the program's environment
    /// only when child debugging is asked for — see [`Self::child`]
    pub fn child_token_hex(&self) -> String {
        hex(&self.child)
    }

    /// a connection that has arrived and handshaked, if one has
    ///
    /// what makes a **second** agent possible: the listener outlives the launch
    /// that bound it, and this is how the engine looks at it without giving up
    /// the wait it is in
    ///
    /// a connection that arrives here is not assumed to be anything. the
    /// session token is the whole of the evidence, the handshake is where it is
    /// checked, and a peer that cannot present it is dropped — `Ok(None)`,
    /// exactly as if nothing had connected. that is not a failure being
    /// swallowed: any local process can open a socket to a loopback port, and
    /// one that could not answer the handshake has said nothing about the
    /// program for the debugger to report
    ///
    /// the handshake itself is given [`HANDSHAKE_PATIENCE`] and no more. a peer
    /// that connects and then says nothing would otherwise hold up the wait
    /// this is called from for as long as it liked
    /// either token is enough here, and only here: a **forked** child inherited
    /// the session token in memory, and an `exec`'d one read the child token
    /// out of its environment. both are this debuggee's, and what a connection
    /// becomes is the same either way
    pub fn arrived(&self) -> Result<Option<Session>> {
        let stream = match self.socket.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(source) => return Err(Error::Listen { source }),
        };
        Ok(Session::attach(stream, &[self.token, self.child], HANDSHAKE_PATIENCE).ok())
    }

    /// wait for the agent, giving up if the debuggee dies or takes too long
    ///
    /// `still_running` is polled so that a debuggee which exits during startup —
    /// almost always because the agent failed to import — is reported as that,
    /// rather than as a timeout thirty seconds later
    ///
    /// the **session** token and no other. the first agent is the one bpd
    /// launched, holding what bpd put in its environment; a peer presenting the
    /// child token here would be a child claiming to be the program bpd started
    pub fn accept(
        &self,
        mut still_running: impl FnMut() -> Result<Option<String>>,
    ) -> Result<Session> {
        let deadline = Instant::now() + ATTACH_TIMEOUT;

        loop {
            match self.socket.accept() {
                Ok((stream, _)) => {
                    return Session::attach(stream, &[self.token], ATTACH_TIMEOUT);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(source) => return Err(Error::Listen { source }),
            }

            if let Some(status) = still_running()? {
                return Err(Error::ExitedBeforeAttach { status });
            }
            if Instant::now() >= deadline {
                return Err(Error::AttachTimeout {
                    timeout: ATTACH_TIMEOUT,
                });
            }
            std::thread::sleep(ATTACH_POLL);
        }
    }
}

/// the writing end of a control connection, and what has gone down it
///
/// behind a mutex and shared with every [`Interrupt`] taken from the session,
/// because a frame written by two threads at once is two half frames. the count
/// lives with the stream for the same reason: it is a statement about what was
/// sent, and it has to be incremented by whoever sent it
#[derive(Debug)]
pub(crate) struct Writing {
    stream: TcpStream,
    requests: u64,
}

/// the control connection to one attached agent
///
/// the reading end belongs to the session and the writing end is shared, so
/// that a pause can be delivered to a program the session is already waiting on
#[derive(Debug)]
pub struct Session {
    reading: TcpStream,
    writing: Arc<Mutex<Writing>>,
    buffer: Vec<u8>,
    /// what this connection's debuggee is called, for as long as it lasts
    ///
    /// minted here, on the connection, because the connection is the session: a
    /// second agent is a second connection, and the engine is what sees both.
    /// every stop that arrives here is named with it, which is the only place a
    /// [`Stop`] gets one
    id: SessionId,
}

impl Session {
    /// complete the handshake and take the connection, or refuse it
    ///
    /// `patience` bounds the handshake alone. it is a parameter rather than a
    /// constant because the two callers are in different situations, and both
    /// are stated where they call — see [`Listener::arrived`]
    ///
    /// `tokens` is what this door accepts. the reply is written with the one
    /// the peer actually presented, so a child that opened with the child token
    /// is answered with it rather than being told a secret it did not have
    fn attach(
        mut stream: TcpStream,
        tokens: &[[u8; TOKEN_LEN]],
        patience: Duration,
    ) -> Result<Self> {
        stream
            .set_nonblocking(false)
            .map_err(|source| Error::Listen { source })?;
        stream
            .set_read_timeout(Some(patience))
            .map_err(|source| Error::Listen { source })?;

        let presented = frame::read_handshake_among(&mut stream, tokens)?;
        frame::write_handshake(&mut stream, &tokens[presented])?;

        // the session reads this connection for as long as the program runs,
        // and the deadline above was about the handshake and nothing else
        stream
            .set_read_timeout(None)
            .map_err(|source| Error::Listen { source })?;

        // one handle for reading and one for writing, on the same socket. the
        // session blocks on the reading end for as long as the program runs,
        // and a pause has to be able to reach the agent while it does
        let reading = stream
            .try_clone()
            .map_err(|source| Error::Listen { source })?;

        Ok(Self {
            reading,
            writing: Arc::new(Mutex::new(Writing {
                stream,
                requests: 0,
            })),
            buffer: Vec::new(),
            id: mint_session(),
        })
    }

    /// what this session is called
    pub const fn id(&self) -> SessionId {
        self.id
    }

    fn writing(&self) -> MutexGuard<'_, Writing> {
        self.writing.lock().expect(
            "nothing panics holding the writing end: every path through it is a frame write",
        )
    }

    /// how many requests the engine has sent the agent since it attached
    ///
    /// the agent reads the control connection only while it is stopped, so this
    /// number is also how many times the debuggee has waited on the debugger. a
    /// feature that claims to cost no round trips is a feature this can be
    /// counted against
    pub fn requests_sent(&self) -> u64 {
        self.writing().requests
    }

    /// the next thing the agent has to say, or `None` once it has hung up
    pub fn next_event(&mut self) -> Result<Option<FromAgent>> {
        Ok(message::read(&mut self.reading, &mut self.buffer)?)
    }

    /// whether the agent has begun saying something by `deadline`
    ///
    /// this **peeks** rather than reading, and that is the whole of why it is
    /// written this way. a read timeout that fired part way through a frame
    /// would leave the stream desynchronised, and the next four bytes read as a
    /// length prefix would be the middle of a message — which the frame limit
    /// turns into a failure and could as easily turn into a 60 MiB allocation.
    /// one byte visible means a frame has started, and it is then read to its
    /// end with the timeout off
    ///
    /// `false` means the deadline passed with the agent silent. it says nothing
    /// about what the program is doing: a program that is running answers
    /// nothing, because everything the agent answers it answers on a thread it
    /// is holding
    pub fn readable_by(&mut self, deadline: Instant) -> Result<bool> {
        let readable = self.peek_until(deadline);
        // whatever came of it, the stream goes back to blocking before anything
        // reads a frame off it
        self.reading
            .set_read_timeout(None)
            .map_err(|source| Error::Control {
                source: frame::Error::Io(source),
            })?;
        readable
    }

    fn peek_until(&mut self, deadline: Instant) -> Result<bool> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            self.reading
                .set_read_timeout(Some(left))
                .map_err(|source| Error::Control {
                    source: frame::Error::Io(source),
                })?;

            let mut first = [0u8; 1];
            match self.reading.peek(&mut first) {
                // zero bytes is the agent having hung up, which `next_event`
                // reports as the end of the session rather than as a timeout
                Ok(_) => return Ok(true),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(source) => {
                    return Err(Error::Control {
                        source: frame::Error::Io(source),
                    });
                }
            }
        }
    }

    /// whether the agent has closed this connection
    ///
    /// what a session with **no child** has instead of an exit status. bpd is
    /// not that process's parent, so there is nothing to reap and the only
    /// thing it can observe about the program being over is that the agent
    /// stopped being on the other end
    ///
    /// a non-blocking peek rather than a read, so it says what is true now and
    /// takes nothing off the stream: zero bytes with the socket readable is the
    /// peer having closed, and anything else — bytes waiting, or nothing yet —
    /// is a connection that is still there
    pub fn hung_up(&self) -> Result<bool> {
        self.reading
            .set_nonblocking(true)
            .map_err(|source| Error::Control {
                source: frame::Error::Io(source),
            })?;

        let mut first = [0u8; 1];
        let closed = match self.reading.peek(&mut first) {
            Ok(0) => Ok(true),
            Ok(_) => Ok(false),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            Err(source) => Err(Error::Control {
                source: frame::Error::Io(source),
            }),
        };

        self.reading
            .set_nonblocking(false)
            .map_err(|source| Error::Control {
                source: frame::Error::Io(source),
            })?;
        closed
    }

    /// wait for the agent to report a stop, and say why it stopped
    pub fn expect_stop(&mut self) -> Result<Stop> {
        const EXPECTED: &str = "the debuggee to stop";

        match self.next_event()? {
            Some(FromAgent::Stopped { stop }) => Ok(stop.in_session(self.id)),
            Some(other) => Err(Error::UnexpectedEvent {
                event: format!("{other:?}"),
                expected: EXPECTED,
            }),
            None => Err(Error::AgentGone { expected: EXPECTED }),
        }
    }

    /// tell the agent to do something
    pub fn send(&mut self, request: &FromEngine) -> Result<()> {
        write_to(&mut self.writing(), request)
    }

    /// the shared writing end, for a handle that reaches a running program
    pub(crate) fn writer(&self) -> Arc<Mutex<Writing>> {
        Arc::clone(&self.writing)
    }
}

fn write_to(writing: &mut Writing, request: &FromEngine) -> Result<()> {
    // a write that fails because the peer is gone is **the peer being gone**,
    // which the read side already reports as [`Error::AgentGone`]. it is one
    // event arriving on whichever end reaches it first: unix takes the last
    // write into a buffer nobody will read and says so on the next read, and
    // windows resets the socket when the process exits, so the write is where
    // it lands. reported as a transport failure it made a debuggee that had run
    // to the end and printed everything come back as `the control connection
    // failed`, on every windows job
    if let Err(error) = message::write(&mut writing.stream, request) {
        return Err(if error.is_peer_gone() {
            Error::AgentGone {
                expected: "the agent to still be there to take a request",
            }
        } else {
            error.into()
        });
    }
    writing.requests += 1;
    Ok(())
}

/// a handle that reaches a debuggee while the engine is waiting for it
///
/// every other request the engine makes is answered on a thread the agent is
/// already holding, so it is sent and waited for on the same thread. the two
/// things that are about a program which is **running** cannot be: a
/// [`Request::Pause`] exists precisely for one, and ending it is the answer
/// when it will not stop on its own. a front end blocked waiting for the
/// program is exactly the front end that needs both, so they are on a handle of
/// their own that can be moved to another thread
///
/// a pause is *delivered*, not asked: the acknowledgement comes back on the
/// reading end, where the wait is, and reaches the caller through
/// [`bpd_core::Reporting::pausing`]
#[derive(Debug)]
pub struct Interrupt {
    session: SessionId,
    writing: Arc<Mutex<Writing>>,
    /// `None` when bpd did not start this process — see [`Self::terminate`]
    child: Option<Arc<Mutex<std::process::Child>>>,
}

impl Interrupt {
    pub(crate) const fn new(
        session: SessionId,
        writing: Arc<Mutex<Writing>>,
        child: Option<Arc<Mutex<std::process::Child>>>,
    ) -> Self {
        Self {
            session,
            writing,
            child,
        }
    }

    /// send a request without waiting for the answer to it
    ///
    /// only [`Request::Pause`] can be sent this way, and anything else is
    /// refused rather than written: the agent answers on a thread it is
    /// holding, so a request sent to a running program would be answered
    /// whenever it next happened to stop
    pub fn deliver(&mut self, request: &Request) -> Result<()> {
        match request {
            Request::Pause => write_to(
                &mut self.writing.lock().expect(
                    "nothing panics holding the writing end: every path through it is a frame write",
                ),
                &FromEngine::Pause,
            ),
            other => Err(Error::NotAnInterrupt {
                request: other.name(),
            }),
        }
    }

    /// end the debuggee, whatever it is doing
    ///
    /// the last resort rather than a resume: a program that is running cannot
    /// be asked anything, so a client that wants to be finished with one has
    /// nothing else to say. the agent is not told, because there is no thread
    /// of the debuggee's waiting to be told
    ///
    /// # errors
    ///
    /// when bpd did not start the process. ending one is signalling the child
    /// bpd holds and reaping it, and a session that arrived on bpd's listener
    /// has no child — bpd is not its parent, so there is nothing to signal and
    /// nothing to wait on. it is **refused by name**, because a `terminate`
    /// that quietly did nothing is one a client reads as a program that has
    /// been ended
    pub fn terminate(&mut self) -> Result<()> {
        let Some(held) = self.child.as_ref() else {
            return Err(bpd_core::Error::NotOurProcess {
                session: self.session,
            }
            .into());
        };
        let mut child = held.lock().expect(
            "nothing panics holding the debuggee: every path through it is a kill or a wait",
        );
        // std refuses to signal a child it has already reaped, which is what a
        // client disconnecting from a program that finished on its own does.
        // that is the request already satisfied rather than a failure
        match child.kill() {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::InvalidInput => return Ok(()),
            Err(source) => {
                return Err(Error::Spawn {
                    interpreter: PathBuf::from("the debuggee"),
                    source,
                });
            }
        }
        child.wait().map_err(|source| Error::Spawn {
            interpreter: PathBuf::from("the debuggee"),
            source,
        })?;
        Ok(())
    }
}
