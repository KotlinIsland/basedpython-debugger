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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::session::{Launcher, Reachable};
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

/// what the list of open connections is only ever held to do
const SERVING: &str = "nothing panics holding the open connections: every path \
                       through it is one push or one shutdown";

/// how often the thread watching for more connections checks whether the first
/// client's session has ended
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// how often a later connection's reader looks up to see whether the session it
/// belongs to is over
///
/// **a `shutdown` does not reliably wake a blocking read on windows**, and that
/// is what the sessions beside the first one were ended with: the first client
/// disconnected, `bpd dap` shut every extra connection down, and the threads
/// serving them stayed blocked in a read that never returned. the adapter then
/// never exited — measured on every windows job, where the suite's watchdog
/// killed it and reported the exit code that kill produces
///
/// so a later connection's socket carries a read timeout, and
/// [`UntilTheSessionEnds`] turns a timeout into the end of the stream once the
/// session is over. unix does not need it and is not harmed by it: the same
/// wake happens, a few milliseconds after the `shutdown` that already worked
const SESSION_POLL: Duration = Duration::from_millis(50);

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

/// a loopback port serving the DAP clients of one debuggee
///
/// **one listener, one token, any number of sessions.** it used to serve exactly
/// one client and turn every other connection away, and that made it impossible
/// to honour the thing it exists to make possible: `startDebugging` asks a
/// client to open a second connection, and the adapter that sent it would have
/// been the thing refusing it
///
/// a token per child was the alternative and it is not better. the connection
/// being asked for is asked for by *this* adapter, to a client that has already
/// presented this token — a second one would be a second lifetime to get wrong
/// for a boundary that is already drawn
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

    /// serve every client that presents this session's token
    ///
    /// returns when the **first** client hangs up or disconnects, exactly as
    /// the stdio transport does when its one client goes. that client is the
    /// one that launched the program, and a debuggee whose original session
    /// has ended is a program with nothing watching it — so the rest go with
    /// it rather than being left driving a process the launcher has abandoned
    ///
    /// the later connections are the ones a `startDebugging` reverse request
    /// asked for: a debugged fork is a session of the **same** debuggee, and
    /// what makes that possible is that `launcher` is shared rather than one
    /// per connection. a connection that fails to authenticate is turned away
    /// and the listener keeps waiting: if a bad connection ended the adapter,
    /// anything that could reach the port could stop the session the person was
    /// starting. `say` is where the person running `bpd` is told about each one
    ///
    /// takes `self` by value: one listener is one debuggee, and the listener is
    /// left non-blocking behind this
    pub fn serve(
        self,
        launcher: &(dyn Launcher + Sync),
        say: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<(), Error> {
        let first = loop {
            let (stream, peer) = self.listener.accept().map_err(|source| Error::Accept {
                port: self.port,
                source,
            })?;
            match self.admit(&stream) {
                Ok(admitted) => break (stream, admitted),
                Err(reason) => say(&turn_away(&stream, peer, &reason)),
            }
        };

        self.hold(launcher, say, first.0, first.1)
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

    /// serve the first client, and every later connection beside it
    fn hold(
        &self,
        launcher: &(dyn Launcher + Sync),
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

        let ended = Arc::new(AtomicBool::new(false));
        let reachable = self.reachable();
        // scoped, so `say` and `launcher` can be borrowed rather than owned:
        // what the later connections serve is a session of the **same**
        // debuggee, which is the thing the launcher holds
        std::thread::scope(|scope| {
            let more = scope.spawn(|| {
                admit_the_rest(self, &extra, &ended, launcher, say, &reachable);
            });

            let input = Cursor::new(admitted.headers).chain(admitted.buffered);
            let served =
                crate::adapter::serve(launcher, Box::new(input), Box::new(stream), &reachable);

            ended.store(true, Ordering::Relaxed);
            match more.join() {
                Ok(()) => served.map_err(|source| Error::Client { source }),
                // a panic there means a client that was told to start a session
                // was left waiting on a socket nothing answers, which is the
                // hang this exists to rule out. it is carried rather than
                // summarised
                Err(panicked) => std::panic::resume_unwind(panicked),
            }
        })
    }

    /// what a second session of this debuggee is told to connect to
    fn reachable(&self) -> Reachable {
        Reachable::At {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port: self.port,
            header: TOKEN_HEADER,
            token: self.token.clone(),
        }
    }
}

/// a later connection's reading end, which stops when the session does
///
/// every read is bounded by [`SESSION_POLL`], and a timeout is answered by
/// looking at whether the first client has gone: while it has not, the read is
/// retried and nothing is lost — the timeout took nothing off the stream —
/// and once it has, this reports the end of the stream, which is what the wire
/// reader already treats as a session ending cleanly
///
/// it exists because `shutdown` is not enough on windows. it is **not** a
/// deadline on the session: a connection with something to say is answered the
/// moment it says it, exactly as before
struct UntilTheSessionEnds {
    reading: TcpStream,
    ended: Arc<AtomicBool>,
}

impl io::Read for UntilTheSessionEnds {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.reading.read(into) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if self.ended.load(Ordering::Relaxed) {
                        return Ok(0);
                    }
                }
                answered => return answered,
            }
        }
    }
}

/// a connection that presented this session's token
struct Admitted {
    /// the first message's header bytes, exactly as they arrived
    headers: Vec<u8>,
    /// the rest of the connection, with whatever was read past the headers
    buffered: BufReader<TcpStream>,
}

/// serve every connection that arrives while the first client is being served
///
/// each is a session of the same debuggee, on a thread of its own, and each is
/// authenticated the same way the first was. a connection that cannot present
/// the token is dropped and the listener goes on waiting
///
/// **the authentication happens on the connection's own thread**, not here.
/// [`Listening::admit`] waits up to [`PRESENTATION`] for a token, and doing that
/// on the accept loop would let anything that can reach the port hold up the
/// session a `startDebugging` reverse request asked a client to start — by
/// connecting and saying nothing. that is the same slot-holding this used to
/// refuse a second client to prevent, and it is what has to stay prevented now
/// that a second client is the point
///
/// the threads are scoped to this one, so when the first client goes every
/// session it opened is waited for rather than abandoned mid-write — and it is
/// **shut down** first. a later session's thread is blocked reading its own
/// client, which would never return on its own, and a debuggee whose original
/// client has gone is a program with nothing watching it: the adapter does not
/// outlive the client that launched, and neither does anything it opened
fn admit_the_rest(
    listening: &Listening,
    listener: &TcpListener,
    ended: &Arc<AtomicBool>,
    launcher: &(dyn Launcher + Sync),
    say: &(dyn Fn(&str) + Send + Sync),
    reachable: &Reachable,
) {
    // every later connection, so that each can be shut down when the first
    // client goes. a `shutdown` is what ends the read its thread is blocked in;
    // nothing else in this process can reach that thread
    let open: std::sync::Mutex<Vec<TcpStream>> = std::sync::Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        while !ended.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, peer)) => {
                    let open = &open;
                    scope.spawn(move || {
                        // the listener this was accepted from is non-blocking,
                        // and on the BSDs — macos among them — an accepted
                        // socket **inherits** that flag where on linux it does
                        // not. a connection left non-blocking answers every
                        // read with `EAGAIN`, which the adapter reads as the
                        // client having failed the instant it connected
                        if let Err(error) = stream.set_nonblocking(false) {
                            say(&turn_away(
                                &stream,
                                peer,
                                &format!(
                                    "the connection would not go back to blocking, so \
                                     nothing could be read from it: {error}"
                                ),
                            ));
                            return;
                        }
                        match stream.try_clone() {
                            Ok(shutting) => open.lock().expect(SERVING).push(shutting),
                            Err(error) => {
                                // without a handle to shut it down, this
                                // connection would hold the adapter open after
                                // its client had gone
                                say(&turn_away(
                                    &stream,
                                    peer,
                                    &format!(
                                        "the connection could not be split, so it could \
                                         not be ended with the session: {error}"
                                    ),
                                ));
                                return;
                            }
                        }

                        let admitted = match listening.admit(&stream) {
                            Ok(admitted) => admitted,
                            Err(reason) => {
                                say(&turn_away(&stream, peer, &reason));
                                return;
                            }
                        };

                        // this connection's reads are bounded so that the end
                        // of the **first** client's session can reach a thread
                        // blocked in one. `shutdown` alone does not do that on
                        // windows, and the adapter never exited
                        // **whatever was read past the headers comes with it.**
                        // `into_inner` drops a `BufReader`'s buffer, and the
                        // first message's body is usually in it — one segment
                        // carries the whole message. taken out first, replayed
                        // below, and the session reads exactly what arrived
                        let waiting = admitted.buffered.buffer().to_vec();
                        let reading = admitted.buffered.into_inner();
                        if let Err(error) = reading.set_read_timeout(Some(SESSION_POLL)) {
                            say(&turn_away(
                                &stream,
                                peer,
                                &format!(
                                    "the connection would not take a read timeout, so \
                                     the session could not be ended with the first \
                                     client's: {error}"
                                ),
                            ));
                            return;
                        }
                        let input = Cursor::new(admitted.headers)
                            .chain(Cursor::new(waiting))
                            .chain(BufReader::new(UntilTheSessionEnds {
                                reading,
                                ended: Arc::clone(ended),
                            }));
                        let served = crate::adapter::serve(
                            launcher,
                            Box::new(input),
                            Box::new(stream),
                            reachable,
                        );
                        if let Err(error) = served {
                            // it is one session of several, so this is not the
                            // adapter's outcome. saying nothing would leave a
                            // session that ended badly invisible
                            say(&format!(
                                "the session with the client from {peer} failed: {}",
                                crate::describe(&error)
                            ));
                        }
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(error) => {
                    // nothing was accepted and nothing was refused, so the loop
                    // keeps going: an exhausted descriptor table is a state a
                    // machine gets out of, and stopping here would turn it into
                    // a port that queues silently forever
                    say(&format!(
                        "a connection could not be accepted while a client was \
                         being served: {error}"
                    ));
                    std::thread::sleep(ACCEPT_POLL);
                }
            }
        }

        // the first client has gone, so every session it opened goes with it.
        // a failure here is nothing to report: the connection is being ended
        // and one that was already closed is the request already satisfied
        for connection in open.lock().expect(SERVING).iter() {
            let shut = connection.shutdown(std::net::Shutdown::Both);
            drop(shut);
        }
    });
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
