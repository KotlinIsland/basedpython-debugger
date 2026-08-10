//! the model context protocol front end
//!
//! MCP is what agents already speak, and its tool model matches how an agent
//! works: call a thing, get the answer, decide what to do. this crate is the
//! whole of what bpd says to one, and it is a **translation and nothing else** —
//! an MCP tool call becomes a [`bpd_core::Request`], the answer is rendered, and
//! no decision about the program is made on the way
//!
//! ## the dependency, and why it is only on `bpd_core`
//!
//! this crate depends on `bpd_core` and nothing else of bpd's, exactly as
//! `bpd_dap` does. the session it drives arrives through [`Session`] and
//! [`Launcher`], which the `bpd` binary implements over `bpd_engine`. an adapter
//! that could reach the engine would be an adapter shaped by how the agent
//! happens to report something, and how the agent reports something would become
//! what an agent using bpd sees
//!
//! ## what is different from DAP, and why
//!
//! DAP is right for an editor and wrong for an agent, and the difference is one
//! thing: **the answer arrives as an event.** `next` returns an
//! acknowledgement, and where it stopped arrives later on a stream the client
//! has to correlate. every step becomes a state machine
//!
//! so here every control tool — `continue_`, `step_over`, `step_in`,
//! `step_out`, `wait`, `pause` — blocks until the program stops again and
//! **returns the stop it produced**. one call, one answer
//!
//! that only works with a **deadline**, which every one of them requires. when
//! it passes, the answer says the program is still running. it does not say
//! where: everything the agent inside the debuggee answers, it answers on a
//! thread it is holding, so a program with nothing held cannot be asked
//! anything at all — not even what its threads are doing. a debugger that
//! reported a location there would be reporting a state it did not read
//!
//! ## the parity rule
//!
//! no capability exists in one adapter and not the other. [`reach_of`] says how
//! each is reached here, `bpd_dap::reach_of` says how each is reached there, and
//! `crates/bpd/tests/parity.rs` compares the two — which is the only place both
//! adapters are visible at once, since neither may depend on the other

pub mod coverage;
pub mod prompts;
pub mod render;
pub mod resources;
pub mod server;
pub mod session;
pub mod tools;
pub mod wire;

pub use coverage::{reach_of, reach_of_facet, surface};
pub use prompts::{Prompt, prompts};
pub use resources::{Resource, resources};
pub use server::{PROTOCOL_VERSION, serve};
pub use session::{
    Configuration, Failed, Launcher, ProgramOutput, Session, Started, Stream, describe,
};
pub use tools::{Tool, tools};
