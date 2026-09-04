//! what the platform says when the thing on the other end is gone
//!
//! one fact, in one place, because two transports need it and they are in
//! different crates: the agent's control connection in `bpd_protocol`, and the
//! DAP wire in `bpd_dap`
//!
//! ## the same event, said three ways
//!
//! a process that exits closes its sockets on unix, so a reader sees `Ok(0)` —
//! an ordinary end of stream. **windows resets them**, so the same reader sees
//! `ECONNRESET`, and a writer or a peek sees it too. a pipe nobody holds open is
//! `BrokenPipe` on both
//!
//! read as failures rather than as an ending, they cost this project four
//! rounds of ci: a debuggee that had run to the end and printed everything came
//! back as `the control connection failed`, and every `bpd dap` session on
//! windows ended with the adapter exiting non-zero after the client hung up
//!
//! **it is not a licence to ignore io errors.** what makes it safe is *where*
//! it is applied: at a message boundary it is the end, and part way through a
//! message it stays the truncation it is. the position decides, never the errno

use std::io;

/// whether an io failure means the peer is **gone** rather than that something
/// went wrong
///
/// the three kinds that mean there is no peer any more. anything else — a
/// permission, a timeout, unreadable data — is a failure and stays one
#[must_use]
pub fn peer_is_gone(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_ways_a_peer_says_it_is_gone_are_one_event() {
        for kind in [
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::BrokenPipe,
        ] {
            assert!(
                peer_is_gone(&io::Error::from(kind)),
                "{kind:?} is the peer being gone"
            );
        }
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidData,
            io::ErrorKind::TimedOut,
            io::ErrorKind::WouldBlock,
        ] {
            assert!(
                !peer_is_gone(&io::Error::from(kind)),
                "{kind:?} is something going wrong, not a session ending"
            );
        }
    }
}
