//! DAP's framing, and the envelope every message shares
//!
//! the framing is the one from the language server protocol: `Content-Length`,
//! a blank line, and exactly that many bytes of json. it is transport only —
//! nothing here knows what a debug session is
//!
//! every failure in this module names the byte sequence that caused it. a
//! framing error that says "parse error" leaves whoever is looking at it with a
//! socket and no idea which end got it wrong

use std::io::{BufRead, BufReader, Read, Write};

/// the header that carries a message's length
///
/// compared case-insensitively, because the specification inherits HTTP's
/// header rules and a client is entitled to spell it `content-length`
const CONTENT_LENGTH: &str = "content-length";

/// the header a client presents this session's token in
///
/// a header rather than a field of the `initialize` request, because it
/// authenticates the **connection** and so has to be checked before anything
/// that arrived on it is acted on. the framing is the language server
/// protocol's, whose header block is `Name: value` lines and which says nothing
/// about `Content-Length` being the only one — see [`authenticate`], and
/// `docs/development/dap.md` for why a socket needs one at all
pub const TOKEN_HEADER: &str = "x-bpd-token";

/// the most a header block may occupy before the connection is refused
///
/// a peer that streams header lines and never sends the blank one would
/// otherwise grow this process's memory until it died. eight kilobytes is
/// enormous for two headers
const MAX_HEADER_BYTES: usize = 8 << 10;

/// a message that could not be read from or written to the client
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// the connection to the client failed
    #[error("the connection to the debug adapter client failed")]
    Connection {
        /// the underlying failure
        #[source]
        source: std::io::Error,
    },

    /// a header line was not `Name: value`
    #[error("`{line}` is not a `Name: value` header line")]
    Header {
        /// the line as it arrived
        line: String,
    },

    /// a message arrived with no `Content-Length`
    #[error(
        "a message arrived with no `Content-Length` header, so there is no way \
         to know where it ends. the headers were: {headers}"
    )]
    NoContentLength {
        /// the headers that did arrive
        headers: String,
    },

    /// the `Content-Length` was not a length
    #[error("`Content-Length: {value}` is not a number of bytes")]
    BadContentLength {
        /// the value as it arrived
        value: String,
    },

    /// the connection ended in the middle of a message
    #[error(
        "the client hung up {read} bytes into a message it said was {expected} \
         bytes long"
    )]
    Truncated {
        /// how many bytes arrived
        read: usize,
        /// how many were promised
        expected: usize,
    },

    /// the headers went on past the bound without ending
    #[error(
        "a message's headers passed {MAX_HEADER_BYTES} bytes without the blank \
         line that ends them. the headers began: {headers}"
    )]
    HeadersTooLong {
        /// the headers as far as they were read
        headers: String,
    },

    /// the connection closed before it presented anything
    #[error("the connection closed before it presented a `{TOKEN_HEADER}` header")]
    Silent,

    /// the connection sent a message carrying no token
    ///
    /// this adapter is listening on a socket when it asks for one, and a
    /// message that reaches it is a message that runs the debuggee's own code —
    /// a breakpoint condition is an expression evaluated in the program
    #[error(
        "the connection sent a message with no `{TOKEN_HEADER}` header, and a \
         socket needs one: reaching this port is running code as whoever \
         started bpd. the token is the `token` field `bpd dap --listen` printed \
         when it bound"
    )]
    NoToken,

    /// the connection presented a token that is not this session's
    ///
    /// what was presented is never quoted back. something on loopback is
    /// guessing, and telling it how close it got would be absurd
    #[error(
        "the connection presented an `{TOKEN_HEADER}` that is not this \
         session's token. the token is the `token` field `bpd dap --listen` \
         printed when it bound"
    )]
    WrongToken,

    /// the body was not a DAP message
    #[error("a {length} byte message body was not a DAP message: {text}")]
    Body {
        /// how long it was
        length: usize,
        /// the body, as far as it can be shown
        text: String,
        /// what serde said about it
        #[source]
        source: serde_json::Error,
    },
}

/// how much of an unparsable body is quoted back
///
/// enough to identify the message, short enough that a client sending a
/// megabyte of nonsense does not put a megabyte in a log
const QUOTED: usize = 400;

/// one message from a DAP client
///
/// only `request` is ever sent to an adapter, and a message of any other type
/// is answered rather than ignored — see [`Incoming::kind`]
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Incoming {
    /// the client's sequence number, which every answer names
    pub seq: i64,
    /// `request`, in every message an adapter is meant to receive
    #[serde(rename = "type")]
    pub kind: String,
    /// which request it is
    pub command: Option<String>,
    /// the arguments, still as json because each command has its own shape
    #[serde(default)]
    pub arguments: serde_json::Value,
    /// which of **this adapter's** requests a response answers
    ///
    /// only a response carries one. DAP is not one-directional: an adapter can
    /// ask a client for something, and the answer names the `seq` the question
    /// went out with — which is the only way to tell one answer from another
    /// when two questions are outstanding
    pub request_seq: Option<i64>,
    /// whether the client did what a reverse request asked
    pub success: Option<bool>,
    /// why it did not, when it did not
    pub message: Option<String>,
}

/// one message's header block, as it arrived
#[derive(Debug, Default)]
struct Block {
    /// how long the body is, when a `Content-Length` was given
    length: Option<usize>,
    /// the token the connection presented, when one was given
    token: Option<String>,
    /// every header line, for a failure to quote back
    seen: String,
}

/// read one header block, leaving its bytes in `raw`
///
/// `raw` is byte exact and the caller owns it, which is what lets
/// [`authenticate`] check a connection's token and then hand the untouched
/// message on to the framing. `Ok(None)` means nothing at all arrived, which is
/// the one clean way for a connection to end
fn read_header_block<R: BufRead>(input: &mut R, raw: &mut Vec<u8>) -> Result<Option<Block>, Error> {
    raw.clear();
    let mut block = Block::default();

    loop {
        let start = raw.len();
        let read = match input.read_until(b'\n', raw) {
            Ok(read) => read,
            // **the client is gone, said the way windows says it.** a process
            // that exits closes its socket on unix and the read below sees
            // `0`; windows resets it, and the same event arrives here as
            // `ECONNRESET`. read as a failure, the session ended in an error
            // and `bpd dap` exited non-zero every time a client hung up —
            // which is how every session on that platform ends
            //
            // it becomes `0` rather than an answer of its own, so the `start`
            // check below decides what it means: between messages it is the
            // end of the session, and part way through the headers it is the
            // truncation it already was
            Err(error) if bpd_core::peer_is_gone(&error) => 0,
            Err(source) => return Err(Error::Connection { source }),
        };
        if read == 0 {
            // the peer hung up between messages, which is how a session ends.
            // hanging up part way through the headers is not, and is what the
            // `start` check separates
            return if start == 0 {
                Ok(None)
            } else {
                Err(Error::NoContentLength {
                    headers: block.seen,
                })
            };
        }
        if raw.len() > MAX_HEADER_BYTES {
            return Err(Error::HeadersTooLong {
                headers: block.seen,
            });
        }

        let line = String::from_utf8_lossy(&raw[start..]);
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return Ok(Some(block));
        }

        block.seen.push_str(line);
        block.seen.push_str("; ");

        let Some((name, value)) = line.split_once(':') else {
            return Err(Error::Header {
                line: line.to_string(),
            });
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case(CONTENT_LENGTH) {
            block.length = Some(value.parse().map_err(|_| Error::BadContentLength {
                value: value.to_string(),
            })?);
        } else if name.eq_ignore_ascii_case(TOKEN_HEADER) {
            block.token = Some(value.to_string());
        }
    }
}

/// check the token a connection presents, without consuming its first message
///
/// the header block it read comes back, so the caller can put those bytes in
/// front of the rest of the stream and let [`Reader`] frame the message whole.
/// nothing on the connection is acted on before this returns
///
/// the comparison is constant time across the token's bytes. the **length** is
/// compared first and separately, which leaks only how long this build's tokens
/// are — a compiled-in constant that is already public
pub fn authenticate<R: BufRead>(input: &mut R, expected: &str) -> Result<Vec<u8>, Error> {
    assert!(
        !expected.is_empty(),
        "a session token is what separates this adapter's client from any other \
         local process, and an empty one separates nothing"
    );

    let mut raw = Vec::new();
    let Some(block) = read_header_block(input, &mut raw)? else {
        return Err(Error::Silent);
    };
    let Some(presented) = block.token else {
        return Err(Error::NoToken);
    };

    if presented.len() != expected.len() {
        return Err(Error::WrongToken);
    }
    let mut difference = 0u8;
    for (presented, expected) in presented.bytes().zip(expected.bytes()) {
        difference |= presented ^ expected;
    }
    if difference != 0 {
        return Err(Error::WrongToken);
    }

    Ok(raw)
}

/// the reading end of a DAP connection
#[derive(Debug)]
pub struct Reader<R> {
    input: BufReader<R>,
    headers: Vec<u8>,
    body: Vec<u8>,
}

impl<R: Read> Reader<R> {
    /// read from `input`
    pub fn new(input: R) -> Self {
        Self {
            input: BufReader::new(input),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// the next message, or `None` once the client has hung up
    pub fn next_message(&mut self) -> Result<Option<Incoming>, Error> {
        let Some(block) = read_header_block(&mut self.input, &mut self.headers)? else {
            return Ok(None);
        };
        let Some(length) = block.length else {
            return Err(Error::NoContentLength {
                headers: block.seen,
            });
        };

        self.body.clear();
        self.body.resize(length, 0);
        let mut read = 0;
        while read < length {
            let taken = self
                .input
                .read(&mut self.body[read..])
                .map_err(|source| Error::Connection { source })?;
            if taken == 0 {
                return Err(Error::Truncated {
                    read,
                    expected: length,
                });
            }
            read += taken;
        }

        let text = String::from_utf8_lossy(&self.body);
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|source| Error::Body {
                length,
                text: text.chars().take(QUOTED).collect(),
                source,
            })
    }
}

/// the writing end of a DAP connection
///
/// it owns the outgoing sequence numbers, which is why an adapter shares one of
/// these rather than writing to the stream from two places: two messages
/// interleaved on the wire are two broken messages, and two sequence numbers
/// handed out at once are one message the client cannot correlate
#[derive(Debug)]
pub struct Writer<W> {
    output: W,
    seq: i64,
}

impl<W: Write> Writer<W> {
    /// write to `output`
    pub const fn new(output: W) -> Self {
        Self { output, seq: 0 }
    }

    /// answer a request
    pub fn respond(
        &mut self,
        request: &Incoming,
        body: Option<serde_json::Value>,
    ) -> Result<(), Error> {
        let mut message = serde_json::json!({
            "type": "response",
            "request_seq": request.seq,
            "success": true,
            "command": request.command.clone().unwrap_or_default(),
        });
        if let Some(body) = body {
            message["body"] = body;
        }
        self.send(message)
    }

    /// refuse a request, with the reason the client should show
    pub fn refuse(&mut self, request: &Incoming, reason: &str) -> Result<(), Error> {
        self.send(serde_json::json!({
            "type": "response",
            "request_seq": request.seq,
            "success": false,
            "command": request.command.clone().unwrap_or_default(),
            "message": reason,
            "body": { "error": {
                "id": 1,
                "format": "{reason}",
                "variables": { "reason": reason },
                "showUser": true,
            } },
        }))
    }

    /// ask the **client** for something, and return the `seq` it went out with
    ///
    /// DAP is not one-directional: a handful of requests go the other way, and
    /// this adapter sends two. `startDebugging` is how the spec answers a
    /// debuggee that became two processes — it asks the client to start a whole
    /// second debug session, because a DAP session **is** a connection and there
    /// is nowhere on one to put a second program. `runInTerminal` is how it
    /// answers a debuggee that needs a terminal, which an adapter cannot make
    ///
    /// the answer to either arrives on the same stream as the client's own
    /// requests, carrying this `seq` as its `request_seq`. whether anything
    /// waits for it is the caller's: nothing waits for a `startDebugging`,
    /// because a client that started the session has already done the only
    /// thing that was asked of it, and a `runInTerminal` is waited for, because
    /// what it answers is whether the program was started at all
    pub fn request(&mut self, command: &str, arguments: &serde_json::Value) -> Result<i64, Error> {
        self.send(serde_json::json!({
            "type": "request",
            "command": command,
            "arguments": arguments,
        }))?;
        Ok(self.seq)
    }

    /// say something that answers nothing
    pub fn event(&mut self, event: &str, body: &serde_json::Value) -> Result<(), Error> {
        self.send(serde_json::json!({
            "type": "event",
            "event": event,
            "body": body,
        }))
    }

    fn send(&mut self, mut message: serde_json::Value) -> Result<(), Error> {
        self.seq += 1;
        message["seq"] = self.seq.into();

        let body = serde_json::to_vec(&message)
            .expect("a message built from json values and strings serialises");

        // **a client that is gone is not a failed write.** on unix the last
        // message to a client whose process has exited goes into a socket
        // buffer nobody will ever read and this returns `Ok`; windows resets
        // the socket instead, and the same write comes back `ECONNRESET`. read
        // as a failure it ended the session in an error and `bpd dap` exited
        // non-zero — after a session that had gone perfectly, every time,
        // because hanging up is how a client ends one
        //
        // there is nothing to deliver and nowhere to deliver it, so this says
        // what unix says. the session then ends the way it always does: the
        // next read finds the client gone and answers `None`
        let written = write!(self.output, "Content-Length: {}\r\n\r\n", body.len())
            .and_then(|()| self.output.write_all(&body))
            .and_then(|()| self.output.flush());
        match written {
            Ok(()) => Ok(()),
            Err(gone) if bpd_core::peer_is_gone(&gone) => Ok(()),
            Err(source) => Err(Error::Connection { source }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(bytes: &str) -> Result<Option<Incoming>, Error> {
        Reader::new(bytes.as_bytes()).next_message()
    }

    /// a client that serves what it has and is then gone, the way windows says
    /// it: `ECONNRESET` rather than a closed socket
    struct Reset {
        sends: Vec<u8>,
        sent: usize,
    }

    impl Read for Reset {
        fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
            if self.sent == self.sends.len() {
                return Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
            }
            let taking = into.len().min(self.sends.len() - self.sent);
            into[..taking].copy_from_slice(&self.sends[self.sent..self.sent + taking]);
            self.sent += taking;
            Ok(taking)
        }
    }

    #[test]
    fn a_client_that_reset_between_messages_ended_its_session() {
        // what every `bpd dap` session on windows does when the client hangs
        // up. read as a failure it made the adapter exit non-zero at the end of
        // a session that had gone perfectly
        let ended = Reader::new(Reset {
            sends: Vec::new(),
            sent: 0,
        })
        .next_message()
        .expect("a client that is gone between messages ended its session");
        assert!(ended.is_none(), "and there is no message to answer");
    }

    #[test]
    fn a_client_that_reset_inside_its_headers_is_not_an_ending() {
        // the half that must not go with it: a header block that stops half way
        // is a client that died mid-message, and saying the session ended
        // cleanly would lose the one it was sending
        let refused = Reader::new(Reset {
            sends: b"Content-Length: 12\r\n".to_vec(),
            sent: 0,
        })
        .next_message();
        assert!(
            matches!(refused, Err(Error::NoContentLength { .. })),
            "a header block that stops half way is not a session ending: {refused:?}"
        );
    }

    /// one request to answer, so the writer has something to write
    const DISCONNECT: &str = r#"{"seq":1,"type":"request","command":"disconnect"}"#;

    /// a client that has gone: every write to it is `ECONNRESET`
    struct Gone;

    impl Write for Gone {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
        }
    }

    /// and one that is simply broken
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn answering_a_client_that_has_gone_is_not_a_failed_session() {
        // what windows gives for the last write of every session. unix puts the
        // same bytes in a buffer nobody reads and calls it a success, and the
        // session ends on the next read either way
        let request = read(&format!(
            "Content-Length: {}\r\n\r\n{DISCONNECT}",
            DISCONNECT.len()
        ))
        .expect("the frame is well formed")
        .expect("there is a message");
        Writer::new(Gone)
            .respond(&request, None)
            .expect("a client that is gone is not a failed write");
    }

    #[test]
    fn a_connection_that_is_broken_rather_than_gone_still_fails() {
        // the half that must not go with it: an error that is not the peer
        // having left is a connection that stopped working, and a session that
        // swallowed it would answer nobody and say nothing
        let request = read(&format!(
            "Content-Length: {}\r\n\r\n{DISCONNECT}",
            DISCONNECT.len()
        ))
        .expect("the frame is well formed")
        .expect("there is a message");
        let refused = Writer::new(Broken).respond(&request, None);
        assert!(
            matches!(refused, Err(Error::Connection { .. })),
            "a broken connection is still a failure: {refused:?}"
        );
    }

    #[test]
    fn a_framed_request_reads_back_as_itself() {
        let body = r#"{"seq":3,"type":"request","command":"next","arguments":{"threadId":7}}"#;
        let message = read(&format!("Content-Length: {}\r\n\r\n{body}", body.len()))
            .expect("the frame is well formed")
            .expect("there is a message");

        assert_eq!(message.seq, 3);
        assert_eq!(message.command.as_deref(), Some("next"));
        assert_eq!(message.arguments["threadId"], 7);
    }

    #[test]
    fn the_header_name_is_matched_the_way_http_matches_one() {
        let body = "{\"seq\":1,\"type\":\"request\",\"command\":\"threads\"}";
        let message = read(&format!(
            "content-length:{}\r\nContent-Type: application/json\r\n\r\n{body}",
            body.len()
        ))
        .expect("the frame is well formed")
        .expect("there is a message");

        assert_eq!(message.command.as_deref(), Some("threads"));
    }

    #[test]
    fn a_clean_hang_up_is_the_end_and_a_dirty_one_is_a_failure() {
        assert!(read("").expect("nothing arrived at all").is_none());

        let error = read("Content-Length: 12\r\n").expect_err("the headers never ended");
        assert!(
            error.to_string().contains("Content-Length"),
            "the failure has to name what was missing, and said {error}"
        );
    }

    #[test]
    fn a_body_shorter_than_its_header_promised_is_named_as_that() {
        let error = read("Content-Length: 40\r\n\r\n{\"seq\":1}").expect_err("the body was short");
        let said = error.to_string();
        assert!(said.contains("9 bytes into"), "said {said}");
        assert!(said.contains("40"), "said {said}");
    }

    #[test]
    fn an_unreadable_body_is_quoted_back() {
        let error = read("Content-Length: 5\r\n\r\nnot!!").expect_err("the body is not json");
        assert!(error.to_string().contains("not!!"), "said {error}");
    }

    #[test]
    fn a_length_that_is_not_a_number_says_what_it_was() {
        let error = read("Content-Length: soon\r\n\r\n").expect_err("`soon` is not a length");
        assert!(error.to_string().contains("soon"), "said {error}");
    }

    /// a token of the shape [`crate::listen`] mints, so the tests below compare
    /// something the same length as the real thing
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn presenting(header: &str) -> Result<Vec<u8>, Error> {
        let body = r#"{"seq":1,"type":"request","command":"initialize"}"#;
        let framed = format!("Content-Length: {}\r\n{header}\r\n{body}", body.len());
        authenticate(&mut BufReader::new(framed.as_bytes()), TOKEN)
    }

    #[test]
    fn an_authenticated_connection_keeps_the_message_it_was_authenticated_by() {
        let raw = presenting(&format!("X-Bpd-Token: {TOKEN}\r\n"))
            .expect("the connection presented this session's token");

        // the point of handing the bytes back: the framing reads the first
        // message whole, so authenticating costs the client nothing
        let mut replayed = Vec::new();
        replayed.extend_from_slice(&raw);
        replayed
            .extend_from_slice(br#"{"seq":1,"type":"request","command":"initialize"}"#.as_slice());
        let message = Reader::new(replayed.as_slice())
            .next_message()
            .expect("the frame is whole")
            .expect("there is a message");
        assert_eq!(message.command.as_deref(), Some("initialize"));
    }

    #[test]
    fn the_token_header_is_matched_the_way_every_other_header_is() {
        presenting(&format!("x-BPD-token:   {TOKEN}  \r\n"))
            .expect("a header name is case insensitive and a value is trimmed");
    }

    #[test]
    fn a_connection_with_no_token_or_the_wrong_one_is_refused_and_told_where_to_get_it() {
        let missing = presenting("").expect_err("nothing presented a token");
        assert!(matches!(missing, Error::NoToken), "got {missing:?}");
        assert!(
            missing.to_string().contains("--listen"),
            "the refusal has to say where the token comes from, and said {missing}"
        );

        let wrong = presenting("X-Bpd-Token: 0000\r\n").expect_err("that is not the token");
        assert!(matches!(wrong, Error::WrongToken), "got {wrong:?}");

        // one byte out, and the same length, which is the case a length check
        // alone would let through
        let near = format!("X-Bpd-Token: {}0\r\n", &TOKEN[..TOKEN.len() - 1]);
        let close = presenting(&near).expect_err("one byte out is out");
        assert!(matches!(close, Error::WrongToken), "got {close:?}");
        assert!(
            !close.to_string().contains(&TOKEN[..8]),
            "a refusal must not quote the token back at whoever is guessing: {close}"
        );
    }

    #[test]
    fn a_connection_that_says_nothing_at_all_is_named_as_that() {
        let silent = authenticate(&mut BufReader::new("".as_bytes()), TOKEN)
            .expect_err("a peer that connects and hangs up presented nothing");
        assert!(matches!(silent, Error::Silent), "got {silent:?}");
    }

    #[test]
    fn headers_that_never_end_are_bounded_rather_than_read_forever() {
        let endless = "X-Filler: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n".repeat(1000);
        let error = read(&endless).expect_err("the header block never ends");
        assert!(
            matches!(error, Error::HeadersTooLong { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn what_is_written_is_framed_the_way_it_is_read() {
        let mut written = Vec::new();
        let mut writer = Writer::new(&mut written);
        writer
            .event("initialized", &serde_json::json!({}))
            .expect("a vector accepts every write");
        writer
            .event("terminated", &serde_json::json!({}))
            .expect("a vector accepts every write");

        let text = String::from_utf8(written).expect("json is utf8");
        assert!(text.starts_with("Content-Length: "), "wrote {text}");

        // the sequence numbers are the adapter's own and count from one, so a
        // client can order two events that answer nothing
        assert!(text.contains(r#""seq":1"#), "wrote {text}");
        assert!(text.contains(r#""seq":2"#), "wrote {text}");
    }
}
