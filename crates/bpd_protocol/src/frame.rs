//! magic bytes, version negotiation, and length prefixed frames

use std::io::{self, Read, Write};

/// the framing this build speaks
///
/// bumped whenever the byte layout of the handshake or a frame header changes.
/// the message set carried inside a frame has its own versioning
pub const PROTOCOL_VERSION: u32 = 2;

/// the length of the shared secret exchanged in the handshake
///
/// the control plane listens on loopback tcp, which any local process can
/// connect to. the token is what makes that acceptable: it is generated per
/// session, passed to the agent through its environment, and a peer that cannot
/// present it is refused before it can send a single frame
pub const TOKEN_LEN: usize = 32;

/// the largest frame that will be sent or accepted
///
/// a length prefix read off a socket is attacker-shaped input even when the
/// peer is trusted, because a desynchronised stream produces an arbitrary
/// number. a bound turns that into an error instead of an allocation
pub const MAX_FRAME_LEN: u32 = 64 << 20;

/// sent by both sides before any frame, so a connection to something that is
/// not a `bpd` peer fails immediately and by name
const MAGIC: [u8; 4] = *b"bpd\x00";

/// the result type for framing operations
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// a framing failure
///
/// every variant except [`Error::Io`] means the peer is not what it claimed to
/// be, and the connection is not recoverable
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// the underlying stream failed
    #[error("the control connection failed")]
    Io(#[from] io::Error),

    /// the peer did not open with the `bpd` magic bytes
    #[error("the peer is not a bpd agent: it opened with {found:?}, expected {MAGIC:?}")]
    NotAPeer {
        /// the four bytes that were sent instead
        found: [u8; 4],
    },

    /// the peer speaks a different framing version
    #[error(
        "the peer speaks protocol version {found}, and this build speaks \
         {PROTOCOL_VERSION}. a stale bpd agent is most likely installed in the \
         debuggee's environment"
    )]
    VersionMismatch {
        /// the version the peer announced
        found: u32,
    },

    /// the peer presented the wrong session token
    ///
    /// the token is never included in the message: something on loopback is
    /// guessing, and telling it how close it got would be absurd
    #[error("the peer did not present this session's token")]
    WrongToken,

    /// the peer announced a frame larger than [`MAX_FRAME_LEN`]
    #[error("the peer announced a {announced} byte frame, and the limit is {MAX_FRAME_LEN}")]
    FrameTooLarge {
        /// the length in the frame header
        announced: u64,
    },

    /// a frame arrived whole but its contents made no sense
    #[error("{reason}")]
    Undecodable {
        /// what could not be understood
        reason: String,
    },

    /// the stream ended part way through a frame
    #[error("the control connection ended after {received} of {expected} bytes")]
    Truncated {
        /// how many bytes the frame or header needed
        expected: usize,
        /// how many arrived before the stream ended
        received: usize,
    },
}

/// how many bytes a handshake occupies on the wire
const HANDSHAKE_LEN: usize = MAGIC.len() + size_of::<u32>() + TOKEN_LEN;

/// announce this build to the peer, presenting the session token
pub fn write_handshake<W: Write>(writer: &mut W, token: &[u8; TOKEN_LEN]) -> Result<()> {
    writer.write_all(&MAGIC)?;
    writer.write_all(&PROTOCOL_VERSION.to_le_bytes())?;
    writer.write_all(token)?;
    writer.flush()?;
    Ok(())
}

/// read the peer's announcement, and refuse anything that is not an exact match
///
/// the token comparison is constant time in the length of the token, so a peer
/// on loopback cannot learn a prefix by timing the refusal
pub fn read_handshake<R: Read>(reader: &mut R, expected: &[u8; TOKEN_LEN]) -> Result<()> {
    let mut header = [0u8; HANDSHAKE_LEN];
    if !read_exact_or_eof(reader, &mut header)? {
        return Err(Error::Truncated {
            expected: header.len(),
            received: 0,
        });
    }

    let (magic, rest) = header.split_at(MAGIC.len());
    if magic != MAGIC {
        return Err(Error::NotAPeer {
            found: header[..MAGIC.len()]
                .try_into()
                .expect("the split is at MAGIC's length"),
        });
    }

    let (version, token) = rest.split_at(size_of::<u32>());
    let found = u32::from_le_bytes(version.try_into().expect("the split is at four bytes"));
    if found != PROTOCOL_VERSION {
        return Err(Error::VersionMismatch { found });
    }

    let mut difference = 0u8;
    for (presented, expected) in token.iter().zip(expected) {
        difference |= presented ^ expected;
    }
    if difference != 0 {
        return Err(Error::WrongToken);
    }

    Ok(())
}

/// write one frame, prefixed with its length
pub fn write_frame<W: Write>(writer: &mut W, payload: &[u8]) -> Result<()> {
    let announced = u64::try_from(payload.len()).expect("a slice length fits in a u64");
    if announced > u64::from(MAX_FRAME_LEN) {
        return Err(Error::FrameTooLarge { announced });
    }
    let length = u32::try_from(payload.len()).expect("the length is bounded by MAX_FRAME_LEN");

    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

/// read one frame into `buffer`, replacing whatever it held
///
/// returns `false` when the peer closed the connection cleanly *between*
/// frames, which is the only shutdown that is not an error. a close part way
/// through a frame is [`Error::Truncated`]
///
/// the caller owns the buffer so a long lived reader can reuse one allocation
pub fn read_frame_into<R: Read>(reader: &mut R, buffer: &mut Vec<u8>) -> Result<bool> {
    let mut header = [0u8; 4];
    if !read_exact_or_eof(reader, &mut header)? {
        return Ok(false);
    }

    let announced = u32::from_le_bytes(header);
    if announced > MAX_FRAME_LEN {
        return Err(Error::FrameTooLarge {
            announced: u64::from(announced),
        });
    }

    let length = usize::try_from(announced).expect("usize is at least 32 bits on every target");
    buffer.clear();
    buffer.resize(length, 0);
    if !read_exact_or_eof(reader, buffer)? {
        return Err(Error::Truncated {
            expected: length,
            received: 0,
        });
    }

    Ok(true)
}

/// fill `buffer`, distinguishing a clean end of stream from a truncated one
///
/// `Ok(false)` means nothing at all arrived. a partial read is always an error,
/// because a frame that stops half way is not a frame
fn read_exact_or_eof<R: Read>(reader: &mut R, buffer: &mut [u8]) -> Result<bool> {
    let mut received = 0;
    while received < buffer.len() {
        match reader.read(&mut buffer[received..]) {
            Ok(0) => {
                return if received == 0 {
                    Ok(false)
                } else {
                    Err(Error::Truncated {
                        expected: buffer.len(),
                        received,
                    })
                };
            }
            Ok(read) => received += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: [u8; TOKEN_LEN] = [7; TOKEN_LEN];

    #[test]
    fn a_handshake_round_trips() {
        let mut wire = Vec::new();
        write_handshake(&mut wire, &TOKEN).expect("writing to a vec cannot fail");
        read_handshake(&mut wire.as_slice(), &TOKEN).expect("this build agrees with itself");
    }

    #[test]
    fn a_peer_presenting_the_wrong_token_is_refused() {
        let mut wire = Vec::new();
        write_handshake(&mut wire, &[9; TOKEN_LEN]).expect("writing to a vec cannot fail");

        let error = read_handshake(&mut wire.as_slice(), &TOKEN)
            .expect_err("a peer without the token is not this session's agent");
        assert!(matches!(error, Error::WrongToken));
    }

    #[test]
    fn a_peer_that_is_not_bpd_is_named_as_such() {
        let mut wire = b"HTTP/1.1".to_vec();
        wire.resize(HANDSHAKE_LEN, 0);
        let error = read_handshake(&mut wire.as_slice(), &TOKEN)
            .expect_err("an http server is not a bpd agent");
        let Error::NotAPeer { found } = error else {
            panic!("expected a peer refusal, got {error:?}");
        };
        assert_eq!(&found, b"HTTP");
    }

    #[test]
    fn a_stale_agent_is_named_as_a_version_mismatch() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&MAGIC);
        wire.extend_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());

        wire.resize(HANDSHAKE_LEN, 0);
        let error = read_handshake(&mut wire.as_slice(), &TOKEN)
            .expect_err("a newer agent is still a mismatch");
        let Error::VersionMismatch { found } = error else {
            panic!("expected a version mismatch, got {error:?}");
        };
        assert_eq!(found, PROTOCOL_VERSION + 1);
    }

    #[test]
    fn frames_round_trip_in_order() {
        let payloads: [&[u8]; 3] = [b"", b"one", &[0xff; 5000]];

        let mut wire = Vec::new();
        for payload in payloads {
            write_frame(&mut wire, payload).expect("writing to a vec cannot fail");
        }

        let mut reader = wire.as_slice();
        let mut buffer = Vec::new();
        for payload in payloads {
            assert!(read_frame_into(&mut reader, &mut buffer).expect("a whole frame is present"));
            assert_eq!(buffer, payload);
        }

        assert!(
            !read_frame_into(&mut reader, &mut buffer).expect("a clean close is not an error"),
            "the stream ended between frames"
        );
    }

    #[test]
    fn a_frame_cut_short_is_not_a_clean_close() {
        let mut wire = Vec::new();
        write_frame(&mut wire, b"a whole frame").expect("writing to a vec cannot fail");
        wire.truncate(wire.len() - 1);

        let error = read_frame_into(&mut wire.as_slice(), &mut Vec::new())
            .expect_err("a frame that stops half way is not a frame");
        let Error::Truncated { expected, received } = error else {
            panic!("expected a truncation, got {error:?}");
        };
        assert_eq!(expected, "a whole frame".len());
        assert_eq!(received, expected - 1);
    }

    #[test]
    fn a_desynchronised_length_prefix_is_bounded() {
        let mut wire = (MAX_FRAME_LEN + 1).to_le_bytes().to_vec();
        wire.extend_from_slice(b"nowhere near that long");

        let error = read_frame_into(&mut wire.as_slice(), &mut Vec::new())
            .expect_err("the announced length is over the limit");
        let Error::FrameTooLarge { announced } = error else {
            panic!("expected a size refusal, got {error:?}");
        };
        assert_eq!(announced, u64::from(MAX_FRAME_LEN) + 1);
    }

    #[test]
    fn a_reused_buffer_does_not_leak_the_previous_frame() {
        let mut wire = Vec::new();
        write_frame(&mut wire, b"the longer first frame").expect("writing to a vec cannot fail");
        write_frame(&mut wire, b"short").expect("writing to a vec cannot fail");

        let mut reader = wire.as_slice();
        let mut buffer = Vec::new();
        assert!(read_frame_into(&mut reader, &mut buffer).expect("the first frame is present"));
        assert!(read_frame_into(&mut reader, &mut buffer).expect("the second frame is present"));
        assert_eq!(buffer, b"short");
    }
}
