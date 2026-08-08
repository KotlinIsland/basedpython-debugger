//! the control plane framing between the `bpd` engine and the in-process agent
//!
//! this is the transport layer only — magic bytes, version negotiation and
//! length prefixed frames. the messages carried inside a frame are defined
//! elsewhere, so that a change to the message set cannot change the framing
//!
//! version negotiation is an **exact match**. the engine and the agent are
//! built and shipped together, so a mismatch means a stale agent extension is
//! sitting in the debuggee's environment. that is a real and confusing failure
//! mode, and it is worth a clear error at connect time rather than a decoding
//! failure ten frames later. there is no compatibility ladder

pub mod env;
pub mod frame;
pub mod message;

pub use frame::{Error, MAX_FRAME_LEN, PROTOCOL_VERSION, Result, TOKEN_LEN};
