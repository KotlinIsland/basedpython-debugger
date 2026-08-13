//! the debug adapter protocol front end
//!
//! DAP is how an editor plugs into a debugger — vs code and neovim speak it and
//! have both driven this adapter, and pycharm reaches it through a plugin
//! rather than natively — and this crate is the whole of what bpd says to one.
//! it is a
//! **translation and nothing else**: a DAP request becomes a
//! [`bpd_core::Request`], the answer is rendered, and no decision about the
//! program is made on the way. a decision made here would be a decision the MCP
//! adapter makes differently, and "an agent can do everything a human can" would
//! stop being structural
//!
//! ## the dependency, and why it is only on `bpd_core`
//!
//! this crate depends on `bpd_core` and nothing else of bpd's. the session it
//! drives arrives through [`Session`] and [`Launcher`], which the `bpd` binary
//! implements over `bpd_engine`. an adapter that could reach the engine would
//! be an adapter shaped by how the agent happens to report something, and how
//! the agent reports something would become what a DAP client sees
//!
//! ## the two transports
//!
//! DAP defines two ways for a client to reach an adapter, and both end at the
//! same [`serve`]. stdio is the one an editor spawns; [`listen`] is the one a
//! client connects to, for a client that did not spawn this process. a
//! transport is where a client's bytes come from and nothing past that point
//! knows which one it was — which is why the same session assertions are run
//! over both rather than each transport having its own idea of what works
//!
//! ## where DAP's model and bpd's differ
//!
//! neither difference is papered over, because papering over one is how a
//! debugger reports something that is not true:
//!
//! - **a stop holds one thread.** DAP defaults to whole-program stops, so the
//!   adapter reports `supportsSingleThreadExecutionRequests` and sends
//!   `allThreadsStopped: false` — unless `stopTheWorld` is set, and then only
//!   when the world really did stop. a thread parked in a C call cannot be
//!   held, and when one is, the client is told rather than the claim being made
//!   anyway
//! - **a reference is not a frame id.** DAP's handle looks the same before and
//!   after a resume, so a stale one gets answered. bpd's [`bpd_core::FrameId`]
//!   carries the stop it was minted at, and [`handles`] keeps that: a reference
//!   from a stop that has ended stops resolving, and the refusal says to ask for
//!   the stack again
//! - **a hit condition is a string with no agreed meaning.** DAP carries one as
//!   free text and clients disagree about what `>5` and `5` mean, so
//!   `supportsHitConditionalBreakpoints` is not advertised and a client that
//!   sends one anyway is refused rather than guessed at

pub mod adapter;
pub mod capabilities;
pub mod configuration;
pub mod coverage;
pub mod handles;
pub mod listen;
pub mod render;
pub mod session;
pub mod wire;

pub use adapter::serve;
pub use capabilities::capabilities;
pub use configuration::{Configuration, Console};
pub use coverage::{carriage_of, reach_of, reach_of_facet, surface};
pub use listen::Listening;
pub use session::{
    Failed, Interrupt, Invocation, Launcher, ProgramOutput, Reachable, Session, Started, Stream,
    describe,
};
