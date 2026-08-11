//! the other transport DAP defines: the client connects to the adapter
//!
//! DAP has two, and stdio is only one of them. the other has the adapter
//! listening on a socket, which is what a client that did not spawn the adapter
//! needs — a script, a tooling integration, and the second session a
//! `startDebugging` reverse request asks a client to start
//!
//! ## a socket is not stdin
//!
//! a pipe has exactly one writer and whoever spawned the adapter chose it. a
//! listening socket has whoever gets there first, and **a DAP message runs the
//! debuggee's own code** — a breakpoint condition is an expression evaluated in
//! the program, so reaching this port is code execution as whoever started
//! `bpd`. two things follow, and neither is optional:
//!
//! - **loopback, and only loopback.** [`Listening::bind`] takes a port and
//!   nothing else, so there is no address for anything to widen: a wildcard
//!   bind is not expressible rather than merely defaulted away from
//! - **a token, checked before anything on the connection is acted on.**
//!   loopback is not a trust boundary — every local user reaches it, and so
//!   does every container sharing the network namespace. a browser tab is the
//!   sharper case: a page can `fetch` a same-origin-ignoring POST at
//!   `127.0.0.1` without a preflight, and this framing is HTTP shaped enough
//!   that a crafted request line and a `Content-Length` would deliver a whole
//!   DAP message. the token is what that page cannot obtain, because the same
//!   origin policy stops it reading any answer
//!
//! ## is this `bpd_protocol`'s handshake?
//!
//! it is the same *problem* — a loopback listener whose peer gets to run code —
//! and a deliberately different answer, because the peer is different.
//! `bpd_protocol` connects `bpd` to `bpd`: both ends are this build, so the
//! handshake can be magic bytes, an exact protocol version and 32 raw bytes,
//! and a peer that is not the agent is refused before it sends a frame
//!
//! here the peer is a **third party** that speaks DAP and nothing else. so the
//! token rides in the framing the transport already has — the header block is
//! `Name: value` lines and `Content-Length` was never the only one allowed —
//! and a client adds one header line rather than learning a second protocol.
//! the sizes match on purpose: 32 bytes from the operating system's randomness,
//! and a comparison that does not depend on how much of it was right
//!
//! the two tokens are separate values with separate lifetimes. this one
//! authenticates a client to the adapter; the agent's authenticates the
//! debuggee to the engine. sharing one would make a DAP client that has this
//! token able to open the debuggee's control plane directly

use std::io::{self, BufReader, Cursor, Read as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::session::Launcher;
use crate::wire::{self, TOKEN_HEADER, Writer};

/// how many bytes of randomness a session token is
///
/// the same as `bpd_protocol::TOKEN_LEN`, and not that constant: `bpd_dap`
/// depends on `bpd_core` alone, so that an adapter cannot be shaped by how the
/// engine happens to talk to the agent
const TOKEN_BYTES: usize = 32;

/// how long a connection has to present its token before it is dropped
///
/// without it a process that connects and says nothing holds the slot for as
/// long as it likes, and the client the person is waiting for is refused as
/// busy. a DAP client's first act is `initialize`, so this is generous
const PRESENTATION: Duration = Duration::from_secs(10);

/// how often the thread turning away extra connections checks whether the
/// session it is protecting has ended
const REFUSAL_POLL: Duration = Duration::from_millis(20);

/// what a second connection is told
///
/// a session **is** the connection — that is DAP's own model, and it is why a
/// second debuggee is a second session rather than a second field on a request
const BUSY: &str = "this adapter is already serving a client, and a debug \
                    session is one connection. a second session is a second \
                    `bpd dap --listen`";

/// listening could not start, or the client connection failed
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// the port could not be bound
    #[error(
        "port {port} on 127.0.0.1 could not be bound. a port that is already in \
         use is the usual reason, and `--listen 0` binds one the operating \
         system chooses"
    )]
    Bind {
        /// the port that was asked for
        port: u16,
        /// what the operating system said
        #[source]
        source: io::Error,
    },

    /// there was no randomness to mint a session token from
    #[error(
        "this session's token could not be generated, and without one there is \
         nothing separating this adapter's client from any other process on \
         this machine"
    )]
    Token {
        /// what the operating system said
        #[source]
        source: io::Error,
    },

    /// the address that was bound could not be read back
    #[error(
        "the port that was bound could not be read back, so there is nothing to \
         tell a client to connect to"
    )]
    Endpoint {
        /// what the operating system said
        #[source]
        source: io::Error,
    },

    /// a connection could not be accepted
    #[error("a connection to 127.0.0.1:{port} could not be accepted")]
    Accept {
        /// the port being listened on
        port: u16,
        /// what the operating system said
        #[source]
        source: io::Error,
    },

    /// the session with the client that was admitted failed
    #[error("the session with the client that connected failed")]
    Client {
        /// what went wrong on the connection
        #[source]
        source: wire::Error,
    },
}

/// a loopback port waiting for one DAP client
#[derive(Debug)]
pub struct Listening {
    listener: TcpListener,
    port: u16,
    token: String,
}

impl Listening {
    /// bind `port` on loopback, and mint the token a client has to present
    ///
    /// `0` binds a port the operating system chooses, which is the only way to
    /// start a listener without racing something else for a number.
    /// [`Listening::announcement`] is how the port that was really bound gets
    /// back to whoever needs it
    ///
    /// there is no address parameter, and that is the security decision rather
    /// than an omission: see the module documentation
    pub fn bind(port: u16) -> Result<Self, Error> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .map_err(|source| Error::Bind { port, source })?;
        let bound = listener
            .local_addr()
            .map_err(|source| Error::Endpoint { source })?;

        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|source| Error::Token {
            source: io::Error::other(source),
        })?;
        let mut token = String::with_capacity(TOKEN_BYTES * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            write!(token, "{byte:02x}").expect("writing to a string cannot fail");
        }

        Ok(Self {
            listener,
            port: bound.port(),
            token,
        })
    }

    /// the one line a client reads to learn where to connect and what to present
    ///
    /// json on one line, because the thing that reads it is a script and the
    /// alternative is parsing prose. it carries the token, so whoever can read
    /// this adapter's stdout is whoever can drive it — which is the same
    /// relationship a spawned adapter's stdin already has
    pub fn announcement(&self) -> String {
        serde_json::json!({
            "listening": {
                "host": Ipv4Addr::LOCALHOST.to_string(),
                "port": self.port,
                "header": TOKEN_HEADER,
                "token": self.token,
            }
        })
        .to_string()
    }

    /// serve the first client that presents this session's token
    ///
    /// returns when that client hangs up or disconnects, exactly as the stdio
    /// transport does — the transport is where a client's bytes come from, and
    /// nothing past that point knows which one it was
    ///
    /// a connection that fails to authenticate is turned away and **the
    /// listener keeps waiting**: if a bad connection ended the adapter, or held
    /// the one slot, anything that could reach the port could stop the session
    /// the person was starting. `say` is where the person running `bpd` is told
    /// about each one, since a refusal nobody sees is a session that mysteriously
    /// never starts
    ///
    /// takes `self` by value: this serves one session and the thread that turns
    /// away extra connections leaves the listener non-blocking behind it, so a
    /// second call would spin rather than wait. one listener is one session, and
    /// the type says so rather than a comment saying so
    pub fn serve(
        self,
        launcher: &mut dyn Launcher,
        say: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<(), Error> {
        loop {
            let (stream, peer) = self.listener.accept().map_err(|source| Error::Accept {
                port: self.port,
                source,
            })?;

            match self.admit(&stream) {
                Ok(admitted) => return self.hold(launcher, say, stream, admitted),
                Err(reason) => {
                    say(&turn_away(&stream, peer, &reason));
                }
            }
        }
    }

    /// read the first message's headers and check the token on them
    ///
    /// the headers come back so the caller can put them in front of the rest of
    /// the stream: authenticating costs the client nothing, and the message it
    /// authenticated with is the `initialize` it was going to send anyway
    fn admit(&self, stream: &TcpStream) -> Result<Admitted, String> {
        let reading = stream
            .try_clone()
            .map_err(|error| format!("the connection could not be split to read it: {error}"))?;
        reading
            .set_read_timeout(Some(PRESENTATION))
            .map_err(|error| format!("the connection would not take a read timeout: {error}"))?;

        let mut buffered = BufReader::new(reading);
        let headers = wire::authenticate(&mut buffered, &self.token).map_err(|error| {
            match &error {
                // a connection that presents nothing is indistinguishable from
                // one that is slow, right up until it is not. saying which
                // deadline it missed is what makes that actionable
                wire::Error::Connection { source }
                    if matches!(
                        source.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    format!(
                        "the connection presented no `{TOKEN_HEADER}` header within \
                         {PRESENTATION:?}, so it was dropped and this adapter went back \
                         to waiting"
                    )
                }
                _ => crate::describe(&error),
            }
        })?;

        // the session spends most of its life waiting for the program, and a
        // read that timed out would end it. the deadline was for the handshake
        // and it is over
        buffered
            .get_ref()
            .set_read_timeout(None)
            .map_err(|error| format!("the connection's read timeout would not clear: {error}"))?;

        Ok(Admitted { headers, buffered })
    }

    /// serve the admitted client, turning away everything else that connects
    fn hold(
        &self,
        launcher: &mut dyn Launcher,
        say: &(dyn Fn(&str) + Send + Sync),
        stream: TcpStream,
        admitted: Admitted,
    ) -> Result<(), Error> {
        let extra = self.listener.try_clone().map_err(|source| Error::Accept {
            port: self.port,
            source,
        })?;
        extra
            .set_nonblocking(true)
            .map_err(|source| Error::Accept {
                port: self.port,
                source,
            })?;

        let ended = AtomicBool::new(false);
        // scoped, so `say` can be borrowed rather than owned: what a refusal is
        // told to is the caller's stderr, and the caller outlives this
        std::thread::scope(|scope| {
            let refusing = scope.spawn(|| refuse_the_rest(&extra, &ended, say));

            let input = Cursor::new(admitted.headers).chain(admitted.buffered);
            let served = crate::adapter::serve(launcher, Box::new(input), Box::new(stream));

            ended.store(true, Ordering::Relaxed);
            match refusing.join() {
                Ok(()) => served.map_err(|source| Error::Client { source }),
                // a panic there means a second client was left waiting on a
                // socket nothing answers, which is the hang this exists to
                // rule out. it is carried rather than summarised
                Err(panicked) => std::panic::resume_unwind(panicked),
            }
        })
    }
}

/// a connection that presented this session's token
struct Admitted {
    /// the first message's header bytes, exactly as they arrived
    headers: Vec<u8>,
    /// the rest of the connection, with whatever was read past the headers
    buffered: BufReader<TcpStream>,
}

/// turn away every connection that arrives while a client is being served
///
/// a queue would look exactly like a hang from the other end, and a client
/// waiting on a socket that will never answer has nothing to report to whoever
/// is waiting on it
fn refuse_the_rest(listener: &TcpListener, ended: &AtomicBool, say: &(dyn Fn(&str) + Send + Sync)) {
    while !ended.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => say(&turn_away(&stream, peer, BUSY)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(REFUSAL_POLL);
            }
            Err(error) => {
                // nothing was refused and nothing was accepted, so the loop
                // keeps going: an exhausted descriptor table is a state a
                // machine gets out of, and stopping here would turn it into a
                // port that queues silently forever
                say(&format!(
                    "a connection could not be accepted while a client was being \
                     served, and it was not turned away: {error}"
                ));
                std::thread::sleep(REFUSAL_POLL);
            }
        }
    }
}

/// tell a connection why it is not being served, and describe that for the log
///
/// the reason goes out as an `output` event, which is the only message DAP
/// lets an adapter send unprompted — there is no request to answer, because a
/// refused connection has not sent one this adapter would act on. the stream is
/// closed by the drop that follows
fn turn_away(stream: &TcpStream, peer: SocketAddr, reason: &str) -> String {
    let told = Writer::new(stream).event(
        "output",
        &serde_json::json!({ "category": "console", "output": format!("{reason}\n") }),
    );
    match told {
        Ok(()) => format!("refused a connection from {peer}: {reason}"),
        Err(error) => format!(
            "refused a connection from {peer}: {reason} — and it hung up before it \
             could be told: {error}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_announcement_names_the_port_that_was_really_bound() {
        let listening = Listening::bind(0).expect("loopback binds an arbitrary port");
        let announced: serde_json::Value =
            serde_json::from_str(&listening.announcement()).expect("the announcement is json");

        assert_eq!(announced["listening"]["host"], "127.0.0.1");
        assert_eq!(announced["listening"]["header"], TOKEN_HEADER);

        // the whole point of binding zero: the number that comes back is the one
        // a client can connect to, so nothing has to guess and then race
        let port = announced["listening"]["port"]
            .as_u64()
            .expect("a port is a number");
        assert_ne!(
            port, 0,
            "a client cannot connect to the port that means any"
        );
        assert_eq!(
            u16::try_from(port).expect("a port is sixteen bits"),
            listening
                .listener
                .local_addr()
                .expect("a bound listener has an address")
                .port()
        );
    }

    #[test]
    fn two_listeners_do_not_share_a_token() {
        let one = Listening::bind(0).expect("loopback binds an arbitrary port");
        let other = Listening::bind(0).expect("loopback binds an arbitrary port");
        assert_ne!(
            one.token, other.token,
            "a token minted per session is what stops one session's client \
             driving another's"
        );
        assert_eq!(one.token.len(), TOKEN_BYTES * 2, "hex is two per byte");
    }

    #[test]
    fn a_bound_port_that_is_taken_names_the_port_rather_than_the_errno() {
        let taken = Listening::bind(0).expect("loopback binds an arbitrary port");
        let error = Listening::bind(taken.port).expect_err("that port is this test's");

        let said = error.to_string();
        assert!(said.contains(&taken.port.to_string()), "said {said}");
        assert!(
            said.contains("--listen 0"),
            "the refusal has to say what to do instead, and said {said}"
        );
    }
}
