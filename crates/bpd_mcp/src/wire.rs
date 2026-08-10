//! JSON-RPC 2.0, one message per line, on stdin and stdout
//!
//! this is transport only — nothing here knows what a debug session is. MCP's
//! stdio transport is newline delimited, so a message may not contain a raw
//! newline; `serde_json` writes compact json, which never does
//!
//! every failure names the line that caused it. a framing error that says
//! "parse error" leaves whoever is looking at it with a pipe and no idea which
//! end got it wrong

use std::io::{BufRead, BufReader, Read, Write};

/// the version of JSON-RPC every message declares
const JSONRPC: &str = "2.0";

/// a message that could not be read from or written to the client
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// the connection to the client failed
    #[error("the connection to the MCP client failed")]
    Connection {
        /// the underlying failure
        #[source]
        source: std::io::Error,
    },

    /// a line was not a json value
    #[error("a {length} byte line was not json: {text}")]
    NotJson {
        /// how long it was
        length: usize,
        /// the line, as far as it can be shown
        text: String,
        /// what serde said about it
        #[source]
        source: serde_json::Error,
    },
}

/// how much of an unparsable line is quoted back
///
/// enough to identify the message, short enough that a client sending a
/// megabyte of nonsense does not put a megabyte in a log
const QUOTED: usize = 400;

/// what arrived on the connection
///
/// a **request** carries an id and is answered; a **notification** does not and
/// is not. the two are separated here rather than in the server, because
/// answering a notification is a protocol violation and not answering a request
/// is a client waiting for ever
#[derive(Debug, Clone)]
pub enum Incoming {
    /// a request, which is answered by naming its id
    Request {
        /// the client's id, echoed on the answer. a string or a number
        id: serde_json::Value,
        /// which method
        method: String,
        /// the arguments, still as json because each method has its own shape
        params: serde_json::Value,
    },

    /// a notification, which is not answered
    Notification {
        /// which method
        method: String,
    },

    /// something that is not a JSON-RPC message this server can act on
    ///
    /// kept as a case rather than dropped. a client whose messages are being
    /// silently ignored looks exactly like a server that has hung
    Unusable {
        /// the id to refuse under, when there was one
        id: Option<serde_json::Value>,
        /// what is wrong with it
        reason: String,
    },
}

/// the reading end of an MCP connection
#[derive(Debug)]
pub struct Reader<R> {
    input: BufReader<R>,
    line: String,
}

impl<R: Read> Reader<R> {
    /// read from `input`
    pub fn new(input: R) -> Self {
        Self {
            input: BufReader::new(input),
            line: String::new(),
        }
    }

    /// the next message, or `None` once the client has hung up
    pub fn next_message(&mut self) -> Result<Option<Incoming>, Error> {
        loop {
            self.line.clear();
            let read = self
                .input
                .read_line(&mut self.line)
                .map_err(|source| Error::Connection { source })?;
            if read == 0 {
                return Ok(None);
            }

            let text = self.line.trim();
            if text.is_empty() {
                continue;
            }

            let message: serde_json::Value =
                serde_json::from_str(text).map_err(|source| Error::NotJson {
                    length: text.len(),
                    text: text.chars().take(QUOTED).collect(),
                    source,
                })?;
            return Ok(Some(classify(&message)));
        }
    }
}

/// decide what one json value is, and refuse it by name when it is nothing
fn classify(message: &serde_json::Value) -> Incoming {
    if message.is_array() {
        // JSON-RPC allows a batch and MCP's 2025-06-18 revision removed it. a
        // server that answered one anyway would be answering in a shape the
        // client's own library has stopped being able to read
        return Incoming::Unusable {
            id: None,
            reason: "this is a JSON-RPC batch. MCP removed batching in its \
                     2025-06-18 revision, so bpd does not accept one — send \
                     each request on its own line"
                .to_string(),
        };
    }

    let id = message.get("id").filter(|id| !id.is_null()).cloned();

    let version = message.get("jsonrpc").and_then(serde_json::Value::as_str);
    if version != Some(JSONRPC) {
        return Incoming::Unusable {
            id,
            reason: match version {
                Some(found) => format!(
                    "this message declares JSON-RPC {found}, and MCP is \
                     JSON-RPC {JSONRPC}"
                ),
                None => format!(
                    "this message has no `jsonrpc` field. every MCP message \
                     declares `\"jsonrpc\": \"{JSONRPC}\"`"
                ),
            },
        };
    }

    let Some(method) = message.get("method").and_then(serde_json::Value::as_str) else {
        // a message with an id and no method is a *response*, and nothing sends
        // this server one: it makes no requests of the client, because it
        // declares no capability that would let it
        return Incoming::Unusable {
            id,
            reason: "this message names no `method`. bpd's MCP server makes no \
                     requests of a client, so there is nothing here that a \
                     response could be answering"
                .to_string(),
        };
    };

    let params = message
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    match id {
        Some(id) => Incoming::Request {
            id,
            method: method.to_string(),
            params,
        },
        None => Incoming::Notification {
            method: method.to_string(),
        },
    }
}

/// the JSON-RPC error codes this server uses
///
/// only the ones it can really produce. a code that is never sent would be a
/// case a client is invited to handle and never sees
pub mod code {
    /// the method is not one this server implements
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// the parameters are not usable
    pub const INVALID_PARAMS: i64 = -32602;
    /// the message is not a valid JSON-RPC request
    pub const INVALID_REQUEST: i64 = -32600;
}

/// the writing end of an MCP connection
#[derive(Debug)]
pub struct Writer<W> {
    output: W,
}

impl<W: Write> Writer<W> {
    /// write to `output`
    pub const fn new(output: W) -> Self {
        Self { output }
    }

    /// answer a request
    pub fn result(
        &mut self,
        id: &serde_json::Value,
        result: serde_json::Value,
    ) -> Result<(), Error> {
        let mut message = serde_json::json!({ "jsonrpc": JSONRPC, "id": id });
        message["result"] = result;
        self.send(&message)
    }

    /// refuse a request, with the reason the client should show
    pub fn failure(
        &mut self,
        id: &serde_json::Value,
        code: i64,
        message: &str,
    ) -> Result<(), Error> {
        self.send(&serde_json::json!({
            "jsonrpc": JSONRPC,
            "id": id,
            "error": { "code": code, "message": message },
        }))
    }

    fn send(&mut self, message: &serde_json::Value) -> Result<(), Error> {
        let body = serde_json::to_string(message)
            .expect("a message built from json values and strings serialises");
        debug_assert!(
            !body.contains('\n'),
            "the stdio transport is newline delimited and a message carried one"
        );
        writeln!(self.output, "{body}").map_err(|source| Error::Connection { source })?;
        self.output
            .flush()
            .map_err(|source| Error::Connection { source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Option<Incoming> {
        Reader::new(text.as_bytes())
            .next_message()
            .expect("the line is json")
    }

    #[test]
    fn a_request_and_a_notification_are_told_apart_by_their_id() {
        let request = read(r#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}"#)
            .expect("there is a message");
        let Incoming::Request { id, method, .. } = request else {
            panic!("expected a request, got {request:?}")
        };
        assert_eq!(id, 4);
        assert_eq!(method, "tools/list");

        let notification = read(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .expect("there is a message");
        assert!(
            matches!(notification, Incoming::Notification { .. }),
            "a message with no id is a notification and got {notification:?}"
        );
    }

    #[test]
    fn a_message_that_is_not_this_protocol_is_refused_by_name_rather_than_dropped() {
        let cases = [
            (r#"{"id":1,"method":"ping"}"#, "no `jsonrpc` field"),
            (
                r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
                "JSON-RPC 1.0",
            ),
            (r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#, "batch"),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
                "names no `method`",
            ),
        ];

        for (text, expected) in cases {
            let message = read(text).expect("there is a message");
            let Incoming::Unusable { reason, .. } = message else {
                panic!("expected {text} to be refused, got {message:?}")
            };
            assert!(
                reason.contains(expected),
                "expected {expected:?} in {reason:?}"
            );
        }
    }

    #[test]
    fn a_line_that_is_not_json_is_quoted_back() {
        let error = Reader::new("not!! json\n".as_bytes())
            .next_message()
            .expect_err("the line is not json");
        assert!(error.to_string().contains("not!! json"), "said {error}");
    }

    #[test]
    fn a_clean_hang_up_is_the_end_and_blank_lines_are_skipped() {
        assert!(read("").is_none(), "nothing arrived at all");
        let after_blanks = read("\n\n{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n");
        assert!(
            matches!(after_blanks, Some(Incoming::Notification { .. })),
            "blank lines are not messages and got {after_blanks:?}"
        );
    }

    #[test]
    fn every_message_written_is_one_line() {
        let mut written = Vec::new();
        let mut writer = Writer::new(&mut written);
        writer
            .result(&serde_json::json!(1), serde_json::json!({ "a": "b\nc" }))
            .expect("a vector accepts every write");
        writer
            .failure(&serde_json::json!("x"), code::INVALID_PARAMS, "no")
            .expect("a vector accepts every write");

        let text = String::from_utf8(written).expect("json is utf8");
        assert_eq!(
            text.lines().count(),
            2,
            "the transport is newline delimited and this wrote {text:?}"
        );
    }
}
