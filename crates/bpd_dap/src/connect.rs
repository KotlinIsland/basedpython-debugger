//! the adapter connecting out to a client that listens
//!
//! [`crate::listen`] is DAP's other transport the usual way round: the adapter
//! listens and the client connects. it binds loopback and nothing else, and on
//! a run target that is exactly what cannot be reached — docker publishes a
//! container's port by connecting to the container's `0.0.0.0`, so a listener
//! on the container's `127.0.0.1` answers the editor with a reset. measured
//! that way by the plugin: loopback reset, the wildcard reached. widening the
//! bind is the one thing `listen` is built not to be able to do
//!
//! so the direction turns round. the **client** listens, on the machine the
//! person is at, and tells the adapter where; the adapter dials out and serves
//! that one connection exactly as it serves a `--listen` client. bpd still
//! listens on nothing but loopback — on nothing at all, here
//!
//! ## the token goes the other way too
//!
//! a listening client has the problem a listening adapter has: whoever reaches
//! the port first is served, and what a debug adapter says is not harmless to
//! its client — a `runInTerminal` reverse request asks it to start a process.
//! so the client chooses a token and hands it to the adapter, and the adapter
//! **presents it first**: the first message on the connection is this
//! adapter's, a `bpd/connected` event carrying [`wire::TOKEN_HEADER`] in its header
//! block — the header a `--listen` client puts on its first message, put on the
//! adapter's. a client reads that one message, compares the token, and hands
//! the rest of the stream to its DAP machinery; a connection that presents the
//! wrong one, or none, it closes
//!
//! the token never goes on a command line, where `ps` shows it to every user:
//! it is read from [`TOKEN_VARIABLE`], and the program bpd starts does not
//! inherit it
//!
//! ## a client that is not there
//!
//! dialling is bounded by [`PATIENCE`], and so is waiting for the client's
//! first message once connected. a connection the client closes without
//! sending anything is a client that turned this adapter away — a wrong token
//! is answered that way — and all three are [`Error`]s naming the address,
//! never a hang

use std::io::{self, Read};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::session::{Launcher, Reachable};
use crate::wire::{self, Writer};

/// the environment variable a connecting adapter reads its token from
pub const TOKEN_VARIABLE: &str = bpd_core::DAP_TOKEN_VARIABLE;

/// the fewest characters a token may have
///
/// what `--listen` mints is 32 bytes of the operating system's randomness,
/// written as 64 hex digits. a client choosing its own is held to the same
/// bytes' worth of hex at the least, so that a token cannot be weaker than the
/// one it stands in for
pub const TOKEN_MINIMUM: usize = 32;

/// how long the connection may take, and then the client's first message
const PATIENCE: Duration = Duration::from_secs(10);

/// connecting to the client failed, or it would not be served
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// the token is not one that can be presented
    #[error(
        "`{TOKEN_VARIABLE}` {why}. the client chooses the token, at least \
         {TOKEN_MINIMUM} characters of printable ascii with no spaces, and hands \
         it over in that variable rather than on the command line, where any user \
         of this machine could read it"
    )]
    Token {
        /// what is wrong with it
        why: &'static str,
    },

    /// the address names nothing
    #[error("`{address}` could not be resolved to an address to connect to")]
    Resolve {
        /// the address as given
        address: String,
        /// what the resolver said, when it said anything
        #[source]
        source: Option<io::Error>,
    },

    /// nothing at the address accepted the connection in time
    #[error(
        "nothing at `{address}` accepted a connection within {PATIENCE:?}. the \
         client listens there and hands that address to bpd, so it is not \
         listening yet, or the address does not reach it from here"
    )]
    Connect {
        /// the address as given
        address: String,
        /// what the last attempt was answered with
        #[source]
        source: io::Error,
    },

    /// the client hung up, or said nothing, before it sent a request
    #[error(
        "the client at `{address}` closed the connection without sending a \
         request, within {PATIENCE:?} of bpd presenting its token. a client that \
         does not accept the token it is shown closes the connection that way"
    )]
    Unanswered {
        /// the address as given
        address: String,
    },

    /// the session with the client failed
    #[error("the session with the client at `{address}` failed")]
    Client {
        /// the address as given
        address: String,
        /// what went wrong on the connection
        #[source]
        source: wire::Error,
    },
}

impl Error {
    /// whether this is the client not being there, or not having this adapter
    ///
    /// what `bpd dap --connect` exits with a status of its own for: whoever
    /// started it can tell a client that never answered from a session that ran
    pub const fn unreached(&self) -> bool {
        matches!(
            self,
            Self::Resolve { .. } | Self::Connect { .. } | Self::Unanswered { .. }
        )
    }
}

/// check a token before anything is dialled
///
/// it goes into a header line, so it can hold nothing that ends one, and it
/// is held to [`TOKEN_MINIMUM`]
///
/// # errors
///
/// when it is absent, too short, or holds a character a header cannot carry
pub fn token(value: Option<&str>) -> Result<String, Error> {
    let Some(value) = value else {
        return Err(Error::Token { why: "is not set" });
    };
    if value.len() < TOKEN_MINIMUM {
        return Err(Error::Token {
            why: "is shorter than a token may be",
        });
    }
    if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(Error::Token {
            why: "holds a character that is not printable ascii",
        });
    }
    Ok(value.to_string())
}

/// dial the client at `address`, present `token`, and serve the one session
///
/// what it returns is what the program the client launched exited with, when
/// bpd read one — see [`crate::serve`]. it is **one** session: a debugged fork
/// would need a second connection, and nothing here can ask for one, so
/// `debugChildren` is refused the way it is on stdio
///
/// # errors
///
/// see [`Error`]
pub fn serve(
    address: &str,
    token: &str,
    launcher: &dyn Launcher,
) -> Result<Option<std::process::ExitStatus>, Error> {
    let stream = dial(address)?;
    let failed = |source| Error::Client {
        address: address.to_string(),
        source,
    };
    let connection = |source| failed(wire::Error::Connection { source });

    stream.set_nodelay(true).map_err(connection)?;
    let reading = stream.try_clone().map_err(connection)?;
    reading
        .set_read_timeout(Some(PATIENCE))
        .map_err(connection)?;

    let writing: Box<dyn io::Write + Send> = Box::new(stream);
    let mut output = Writer::new(writing);
    output
        .presenting(
            token,
            "bpd/connected",
            &serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }),
        )
        .map_err(failed)?;

    let spoke = Arc::new(AtomicBool::new(false));
    let input = FirstWordWithin {
        reading,
        spoke: Arc::clone(&spoke),
    };
    let served =
        crate::adapter::serve_writing(launcher, Box::new(input), output, &Reachable::Nowhere);

    if !spoke.load(Ordering::Relaxed) {
        return Err(Error::Unanswered {
            address: address.to_string(),
        });
    }
    served.map_err(failed)
}

/// resolve `address` and connect to the first of its addresses that answers
fn dial(address: &str) -> Result<TcpStream, Error> {
    let resolved: Vec<SocketAddr> = address
        .to_socket_addrs()
        .map_err(|source| Error::Resolve {
            address: address.to_string(),
            source: Some(source),
        })?
        .collect();
    let mut last = None;
    for candidate in resolved {
        match TcpStream::connect_timeout(&candidate, PATIENCE) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(match last {
        Some(source) => Error::Connect {
            address: address.to_string(),
            source,
        },
        None => Error::Resolve {
            address: address.to_string(),
            source: None,
        },
    })
}

/// the client's side of the connection, bounded until it first says something
///
/// a client that accepts the connection and then says nothing would hold bpd —
/// and under `by run` the whole run — for ever. so the first read carries
/// [`PATIENCE`], and the bound comes off once the client has spoken: from then
/// on the session spends most of its life waiting for the program, and a read
/// that timed out would end it
struct FirstWordWithin {
    reading: TcpStream,
    spoke: Arc<AtomicBool>,
}

impl Read for FirstWordWithin {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        let read = self.reading.read(into)?;
        if read > 0 && !self.spoke.swap(true, Ordering::Relaxed) {
            self.reading.set_read_timeout(None)?;
        }
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_one_a_header_can_carry_and_no_weaker_than_a_minted_one() {
        assert!(matches!(token(None), Err(Error::Token { .. })));
        assert!(matches!(token(Some("short")), Err(Error::Token { .. })));
        let spaced = format!("{} {}", "a".repeat(20), "b".repeat(20));
        assert!(matches!(token(Some(&spaced)), Err(Error::Token { .. })));
        let broken = format!("{}\r\nX-Other: 1", "a".repeat(40));
        assert!(matches!(token(Some(&broken)), Err(Error::Token { .. })));
        let fine = "0123456789abcdef".repeat(4);
        assert_eq!(token(Some(&fine)).expect("64 hex digits"), fine);
    }

    #[test]
    fn nothing_listening_is_refused_by_name_rather_than_waited_on() {
        // bind and drop, so the port is one nothing is listening on
        let free = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("loopback binds an arbitrary port");
        let error = dial(&free.to_string()).expect_err("nothing accepts there");
        assert!(matches!(error, Error::Connect { .. }), "{error}");
        assert!(error.unreached());
    }
}
