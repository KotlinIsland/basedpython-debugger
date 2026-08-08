//! what the engine and the agent say to each other
//!
//! kept in its own module so a change to the message set cannot change the
//! framing, and encoded as json because a captured session being readable is
//! worth more than the bytes at this frequency — the framing benchmark puts the
//! whole envelope at a few nanoseconds, so the encoding is not where the cost is
//!
//! there is no request id. one would be a field that is parsed and never read
//! until there are two requests in flight at once, and the first thing to need
//! it is the concurrency that arrives with breakpoints

use std::io::{Read, Write};

use crate::frame::{self, Result};

/// why the debuggee stopped
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopReason {
    /// stopped before the first statement of the program, having run nothing
    Entry,
}

/// what the agent tells the engine
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FromAgent {
    /// the agent is attached and every thread is held
    Stopped {
        /// why it stopped
        reason: StopReason,
    },
}

/// what the engine tells the agent
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FromEngine {
    /// let the debuggee run
    Resume,
}

/// encode a message and write it as one frame
pub fn write<W: Write, M: serde::Serialize>(writer: &mut W, message: &M) -> Result<()> {
    let encoded = serde_json::to_vec(message).map_err(|source| frame::Error::Undecodable {
        reason: format!("this build could not encode a message it produced: {source}"),
    })?;
    frame::write_frame(writer, &encoded)
}

/// read one frame and decode it
///
/// returns `None` when the peer closed the connection cleanly between frames
pub fn read<R: Read, M: serde::de::DeserializeOwned>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
) -> Result<Option<M>> {
    if !frame::read_frame_into(reader, buffer)? {
        return Ok(None);
    }

    serde_json::from_slice(buffer)
        .map(Some)
        .map_err(|source| frame::Error::Undecodable {
            // the payload itself is not quoted: it can be a whole object graph,
            // and an error that dumps one is an error nobody reads
            reason: format!("the peer sent a message this build does not understand: {source}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_event_round_trips() {
        let sent = FromAgent::Stopped {
            reason: StopReason::Entry,
        };

        let mut wire = Vec::new();
        write(&mut wire, &sent).expect("writing to a vec cannot fail");

        let received: Option<FromAgent> =
            read(&mut wire.as_slice(), &mut Vec::new()).expect("the frame is whole");
        assert_eq!(received, Some(sent));
    }

    #[test]
    fn an_engine_request_round_trips() {
        let mut wire = Vec::new();
        write(&mut wire, &FromEngine::Resume).expect("writing to a vec cannot fail");

        let received: Option<FromEngine> =
            read(&mut wire.as_slice(), &mut Vec::new()).expect("the frame is whole");
        assert_eq!(received, Some(FromEngine::Resume));
    }

    #[test]
    fn a_clean_close_is_not_a_message() {
        let received: Option<FromAgent> =
            read(&mut [].as_slice(), &mut Vec::new()).expect("a clean close is not an error");
        assert_eq!(received, None);
    }

    #[test]
    fn a_message_this_build_does_not_understand_is_named_as_such() {
        let mut wire = Vec::new();
        frame::write_frame(&mut wire, br#"{"event":"invented_by_a_newer_agent"}"#)
            .expect("writing to a vec cannot fail");

        let error = read::<_, FromAgent>(&mut wire.as_slice(), &mut Vec::new())
            .expect_err("the tag is not one this build has");
        assert!(
            error.to_string().contains("does not understand"),
            "the refusal must say what happened, got {error}"
        );
    }

    #[test]
    fn a_frame_that_is_not_json_is_refused_rather_than_guessed_at() {
        let mut wire = Vec::new();
        frame::write_frame(&mut wire, b"\xff\xfe not json at all")
            .expect("writing to a vec cannot fail");

        read::<_, FromEngine>(&mut wire.as_slice(), &mut Vec::new())
            .expect_err("a desynchronised stream must not decode");
    }
}
