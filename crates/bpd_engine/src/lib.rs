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

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bpd_core::{Request, Stop};
use bpd_protocol::message::{FromAgent, FromEngine};
use bpd_protocol::{TOKEN_LEN, frame, message};

pub use launch::{Debuggee, Launched, Program, launch, launch_piped};

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

    /// there is nowhere a per-user agent cache could go
    #[error("bpd has nowhere to cache the agent: {reason}")]
    NoAgentCache {
        /// what was looked at, and what it would take to fix
        reason: String,
    },

    /// the directory the agent is cached in cannot be trusted
    ///
    /// what is cached there is a shared object that gets loaded into the user's
    /// own processes, so a directory somebody else can write to is somebody
    /// else choosing what runs inside the debuggee. staging refuses rather than
    /// quietly using a temporary directory instead, because a fallback would
    /// turn a broken cache into a performance regression nobody notices
    #[error("refusing to cache the agent in `{path}`: {reason}")]
    UntrustedAgentCache {
        /// the directory that was refused
        path: PathBuf,
        /// what is wrong with it, and what it would take to fix
        reason: String,
    },

    /// the agent cache could not be read
    #[error("could not read the agent cache: `{path}`")]
    ReadAgentCache {
        /// the file or directory the failure was about
        path: PathBuf,
        /// the underlying failure
        #[source]
        source: io::Error,
    },

    /// the agent cache holds something staging never put there
    ///
    /// a cache with a surprise in it may not be the directory `bpd` thinks it
    /// is, and the one operation here that cannot be undone is deleting — so
    /// this refuses the whole of it rather than removing what it does
    /// recognise and leaving the rest
    #[error("refusing to change the agent cache `{root}`: {reason}")]
    UnexpectedInAgentCache {
        /// the cache directory
        root: PathBuf,
        /// what was found in it, and what it would take to fix
        reason: String,
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

/// the control plane, waiting for exactly one agent
#[derive(Debug)]
pub struct Listener {
    listener: TcpListener,
    token: [u8; TOKEN_LEN],
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
        getrandom::fill(&mut token).map_err(|source| Error::Listen {
            source: io::Error::other(source),
        })?;

        Ok(Self { listener, token })
    }

    /// where the agent should connect
    pub fn endpoint(&self) -> Result<SocketAddr> {
        self.listener
            .local_addr()
            .map_err(|source| Error::Listen { source })
    }

    /// this session's token, in the form the agent receives it
    pub fn token_hex(&self) -> String {
        let mut hex = String::with_capacity(TOKEN_LEN * 2);
        for byte in self.token {
            use std::fmt::Write as _;
            write!(hex, "{byte:02x}").expect("writing to a string cannot fail");
        }
        hex
    }

    /// wait for the agent, giving up if the debuggee dies or takes too long
    ///
    /// `still_running` is polled so that a debuggee which exits during startup —
    /// almost always because the agent failed to import — is reported as that,
    /// rather than as a timeout thirty seconds later
    pub fn accept(
        &self,
        mut still_running: impl FnMut() -> Result<Option<String>>,
    ) -> Result<Session> {
        let deadline = Instant::now() + ATTACH_TIMEOUT;

        loop {
            match self.listener.accept() {
                Ok((stream, _)) => return Session::attach(stream, &self.token),
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
}

impl Session {
    fn attach(mut stream: TcpStream, token: &[u8; TOKEN_LEN]) -> Result<Self> {
        stream
            .set_nonblocking(false)
            .map_err(|source| Error::Listen { source })?;

        frame::read_handshake(&mut stream, token)?;
        frame::write_handshake(&mut stream, token)?;

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
        })
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

    /// wait for the agent to report a stop, and say why it stopped
    pub fn expect_stop(&mut self) -> Result<Stop> {
        const EXPECTED: &str = "the debuggee to stop";

        match self.next_event()? {
            Some(FromAgent::Stopped { stop }) => Ok(stop),
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
    message::write(&mut writing.stream, request)?;
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
    writing: Arc<Mutex<Writing>>,
    child: Arc<Mutex<std::process::Child>>,
}

impl Interrupt {
    pub(crate) const fn new(
        writing: Arc<Mutex<Writing>>,
        child: Arc<Mutex<std::process::Child>>,
    ) -> Self {
        Self { writing, child }
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
    pub fn terminate(&mut self) -> Result<()> {
        let mut child = self.child.lock().expect(
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
