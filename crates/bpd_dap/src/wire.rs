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
}

/// the reading end of a DAP connection
#[derive(Debug)]
pub struct Reader<R> {
    input: BufReader<R>,
    headers: String,
    body: Vec<u8>,
}

impl<R: Read> Reader<R> {
    /// read from `input`
    pub fn new(input: R) -> Self {
        Self {
            input: BufReader::new(input),
            headers: String::new(),
            body: Vec::new(),
        }
    }

    /// the next message, or `None` once the client has hung up
    pub fn next_message(&mut self) -> Result<Option<Incoming>, Error> {
        let Some(length) = self.read_headers()? else {
            return Ok(None);
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

    /// the `Content-Length` of the next message, or `None` at a clean end
    fn read_headers(&mut self) -> Result<Option<usize>, Error> {
        let mut length = None;
        let mut seen = String::new();

        loop {
            self.headers.clear();
            let read = self
                .input
                .read_line(&mut self.headers)
                .map_err(|source| Error::Connection { source })?;
            if read == 0 {
                // the client hung up between messages, which is how a session
                // ends. hanging up part way through the headers is not, and is
                // what the empty-`seen` check separates
                return if seen.is_empty() {
                    Ok(None)
                } else {
                    Err(Error::NoContentLength { headers: seen })
                };
            }

            let line = self.headers.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                return match length {
                    Some(length) => Ok(Some(length)),
                    None => Err(Error::NoContentLength { headers: seen }),
                };
            }

            seen.push_str(line);
            seen.push_str("; ");

            let Some((name, value)) = line.split_once(':') else {
                return Err(Error::Header {
                    line: line.to_string(),
                });
            };
            if name.trim().eq_ignore_ascii_case(CONTENT_LENGTH) {
                let value = value.trim();
                length = Some(value.parse().map_err(|_| Error::BadContentLength {
                    value: value.to_string(),
                })?);
            }
        }
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
        write!(self.output, "Content-Length: {}\r\n\r\n", body.len())
            .map_err(|source| Error::Connection { source })?;
        self.output
            .write_all(&body)
            .map_err(|source| Error::Connection { source })?;
        self.output
            .flush()
            .map_err(|source| Error::Connection { source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(bytes: &str) -> Result<Option<Incoming>, Error> {
        Reader::new(bytes.as_bytes()).next_message()
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
