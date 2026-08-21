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
//! how it reaches one. it enumerates what the debugger **says** as well, because
//! [`Reporting`] having no default bodies forces an implementation to exist and
//! is satisfied by an empty one. it is about this crate's own enums rather than
//! about any protocol, which is why it is here and not in an adapter that would
//! then be the only one it fitted
//!
//! nothing in this crate knows about DAP, MCP, or any wire format. the adapters
//! are thin translations of the types defined here, which is what keeps the
//! front ends at exact feature parity. the serde derives are not a wire format
//! in that sense: they are how one definition of a type reaches the agent
//! without a second definition to convert to

pub mod breakpoint;
pub mod error;
pub mod exception;
pub mod fact;
pub mod frame;
pub mod jump;
pub mod parity;
pub mod python;
pub mod query;
pub mod refusal;
pub mod replace;
pub mod script;
pub mod session;
pub mod source_map;
pub mod spawn;
pub mod stop;
pub mod thread;
pub mod value;

pub use breakpoint::{
    Binding, Evaluation, HitCondition, LogRecord, NoArming, Resolved, Site, SourceBreakpoint,
    Unbound,
};
pub use error::{Error, Result};
pub use exception::{PythonError, TracebackFrame};
pub use fact::{Class, Fact, Facts, Limit, Mutation, Observed, Silence, Silent, Stability};
pub use frame::{
    Coverage, Depth, Frame, FrameId, FrameKind, Kept, Retainer, Retainers, Scheduling, Scope,
    TASK_NOT_SEEN, Trail, Visited,
};
pub use jump::{
    Access, Address, Again, Blocked, Jump, Jumped, Reset, Restarted, Restarting, Suspendable,
    Through, Unresettable, Unrestartable, WHAT_READING_THE_BYTECODE_COSTS, Whose,
};
pub use parity::{Carried, Facet, Naming, Reach, Told, ran, ran_as, say, surface};
pub use query::{
    Answer, Appeared, Changed, Difference, FrameState, Frames, Moved, NotCompared, NotRead,
    QueryPart, ScopeState, Seen, Side, Snapshot, SnapshotId, Source, State, StateQuery, Subject,
    Taken, Unverified, Wanted, WhyNot, difference,
};
pub use refusal::Refusal;
pub use replace::{
    Divergence, LiveFrame, Rebound, Replaced, Replacement, StillRunning, Unreplaceable,
};
pub use script::{
    Answered, At, Bound, Budget, Did, Disarmed, Halted, Landed, Outcome, Place, Predicate, Record,
    Script, Step, Transcript,
};
pub use session::{
    Addressed, ContextLayer, ExceptionBreakpoints, Exit, Forwarded, Joined, Reporting, Request,
    Response, Running, SessionId, Shadowed, Stack, TemplateContext, Threads, Variables,
    WorldStopped, exit_code, only_session, only_stop,
};
pub use source_map::{Located, MapError, MappedFile, Mapping, SourceMap, Unmapped};
pub use spawn::{Blindspot, Spawn, Verdict};
pub use stop::{Abandoned, Holding, Mode, Part, Reported, StepKind, Stop, StopReason};
pub use thread::{Progress, ThreadState, Where, Which};
pub use value::{Content, Detail, Entry, Evaluated, Omitted, Pair, Value};
