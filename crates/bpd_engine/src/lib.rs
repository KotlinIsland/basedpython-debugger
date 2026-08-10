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
pub mod launch;

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bpd_protocol::message::{FromAgent, FromEngine, Refusal, Stop};
use bpd_protocol::{TOKEN_LEN, frame, message};

pub use launch::{Debuggee, Launched, Running, Stack, Threads, Variables, WorldStopped, launch};

/// the result type for engine operations
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// a failure the engine reports rather than works around
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// the agent build could not be found
    #[error("{reason}")]
    LocateAgent {
        /// what was looked for, and where
        reason: String,
    },

    /// the agent build could not be copied where the interpreter can import it
    #[error("could not stage the agent from `{path}`")]
    StageAgent {
        /// the artifact being staged
        path: PathBuf,
        /// the underlying failure
        #[source]
        source: io::Error,
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

    /// two breakpoints in one request claimed the same id
    ///
    /// the id is how every later report — a rebinding, a stop — names which
    /// breakpoint it is about. sharing one would mean the client is given one
    /// answer for two questions and cannot tell which it belongs to
    #[error(
        "two breakpoints in the same request both have id {id}. an id names one \
         breakpoint in every report about it, so it has to be unique within a set"
    )]
    DuplicateBreakpointId {
        /// the id that was used twice
        id: u32,
    },

    /// something was asked of a debuggee with no thread held
    ///
    /// the agent answers on a thread it is holding and at no other time, so a
    /// request made to a program with nothing held would be answered whenever
    /// it next happened to stop. that is not an answer, and waiting for it
    /// looks exactly like a hang
    #[error("no thread of the debuggee is held, so it cannot be asked for {wanted}")]
    NotStopped {
        /// what was asked for
        wanted: &'static str,
    },

    /// a request that is about one stop was made while several were held
    ///
    /// a stop holds one thread and there can be more than one of them at a
    /// time. answering from whichever happened to be first would be answering
    /// about a thread the caller did not name
    #[error(
        "{wanted} is about one held thread and {} are held: {held:?}. name the \
         stop it is about",
        held.len()
    )]
    AmbiguousStop {
        /// what was asked for
        wanted: &'static str,
        /// the stops that are held
        held: Vec<u64>,
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

    /// the agent will not answer the request, and said why
    ///
    /// not a failure of the engine or of the transport: the agent understood
    /// the request and refused it, because answering would have meant guessing
    /// what was meant
    #[error("{reason}")]
    Refused {
        /// what stood in the way
        reason: Refusal,
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

/// the control connection to one attached agent
#[derive(Debug)]
pub struct Session {
    stream: TcpStream,
    buffer: Vec<u8>,
    requests: u64,
}

impl Session {
    fn attach(stream: TcpStream, token: &[u8; TOKEN_LEN]) -> Result<Self> {
        stream
            .set_nonblocking(false)
            .map_err(|source| Error::Listen { source })?;

        let mut session = Self {
            stream,
            buffer: Vec::new(),
            requests: 0,
        };
        frame::read_handshake(&mut session.stream, token)?;
        frame::write_handshake(&mut session.stream, token)?;
        Ok(session)
    }

    /// how many requests the engine has sent the agent since it attached
    ///
    /// the agent reads the control connection only while it is stopped, so this
    /// number is also how many times the debuggee has waited on the debugger. a
    /// feature that claims to cost no round trips is a feature this can be
    /// counted against
    pub const fn requests_sent(&self) -> u64 {
        self.requests
    }

    /// the next thing the agent has to say, or `None` once it has hung up
    pub fn next_event(&mut self) -> Result<Option<FromAgent>> {
        Ok(message::read(&mut self.stream, &mut self.buffer)?)
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
        message::write(&mut self.stream, request)?;
        self.requests += 1;
        Ok(())
    }
}
