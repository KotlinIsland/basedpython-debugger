//! the debug session model, shared by every `bpd` front end
//!
//! nothing in this crate knows about DAP, MCP, or any wire format. the adapters
//! in `bpd_dap` and `bpd_mcp` are thin translations of the types defined here,
//! which is what keeps the two front ends at exact feature parity

pub mod error;
pub mod python;

pub use error::{Error, Result};
