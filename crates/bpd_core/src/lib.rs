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
//! [`parity`] is the second of those two enumerated — the surface, the
//! capabilities carried inside it, and the vocabulary a front end uses to say
//! how it reaches one. it is about this crate's own enum rather than about any
//! protocol, which is why it is here and not in an adapter that would then be
//! the only one it fitted
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
pub mod parity;
pub mod python;
pub mod refusal;
pub mod script;
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
pub use parity::{Facet, Reach, surface};
pub use refusal::Refusal;
pub use script::{
    Answered, At, Bound, Budget, Did, Disarmed, Halted, Landed, Outcome, Place, Predicate, Record,
    Script, Step, Transcript,
};
pub use session::{
    ExceptionBreakpoints, Reporting, Request, Response, Running, Stack, Threads, Variables,
    WorldStopped, exit_code, only_stop,
};
pub use stop::{Holding, Mode, Part, StepKind, Stop, StopReason};
pub use thread::{Progress, ThreadState, Where, Which};
pub use value::{Content, Detail, Entry, Evaluated, Omitted, Pair, Value};
