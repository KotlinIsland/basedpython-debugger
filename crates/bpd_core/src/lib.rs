//! the debug session model, shared by every `bpd` front end
//!
//! two things live here, and nothing else does:
//!
//! - the **vocabulary** that describes a debugged program — stops, breakpoints,
//!   frames, scopes, values, threads, the exceptions it raises and the refusals
//!   it earns
//! - the **capability surface**, as [`Request`] and [`Response`]: everything a
//!   client can ask of a session, named as data so it can be enumerated
//!
//! nothing in this crate knows about DAP, MCP, or any wire format. the adapters
//! are thin translations of the types defined here, which is what keeps the
//! front ends at exact feature parity. the serde derives are not a wire format
//! in that sense: they are how one definition of a type reaches the agent
//! without a second definition to convert to

pub mod breakpoint;
pub mod error;
pub mod exception;
pub mod frame;
pub mod python;
pub mod refusal;
pub mod session;
pub mod stop;
pub mod thread;
pub mod value;

pub use breakpoint::{
    Binding, Evaluation, HitCondition, LogRecord, Resolved, Site, SourceBreakpoint, Unbound,
};
pub use error::{Error, Result};
pub use exception::{PythonError, TracebackFrame};
pub use frame::{Frame, FrameId, Scope};
pub use refusal::Refusal;
pub use session::{
    ExceptionBreakpoints, Request, Response, Running, Stack, Threads, Variables, WorldStopped,
};
pub use stop::{Holding, Mode, Part, StepKind, Stop, StopReason};
pub use thread::{Progress, ThreadState, Where, Which};
pub use value::{Content, Detail, Entry, Evaluated, Omitted, Pair, Value};
