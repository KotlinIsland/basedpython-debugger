//! the capability surface, enumerated, so the parity rule can be a test
//!
//! the rule is that no capability exists in one front end and not the other. it
//! was a policy someone had to remember for as long as a capability was a
//! *method*, because rust cannot enumerate methods. as data it can be checked,
//! and this module is what a check is written against:
//!
//! - [`surface`] is one [`Request`] of every variant
//! - [`Facet`] names the capabilities that are **not** variants, because a rule
//!   that only enumerates variants misses a capability carried in a field
//! - [`Reach`] is how a front end says it gets at one, including saying that it
//!   cannot
//!
//! nothing here knows what DAP or MCP are. each adapter writes its own
//! `reach_of` — an exhaustive match with no catch-all arm, so a capability added
//! to the core is a compile error there rather than a capability that front end
//! silently does not have — and the parity test compares the two answers

use std::num::{NonZeroU32, NonZeroU64};

use crate::breakpoint::SourceBreakpoint;
use crate::frame::{FrameId, Scope};
use crate::query::{SnapshotId, StateQuery, Wanted};
use crate::script::{Budget, Script, Step};
use crate::session::{Request, Threads};
use crate::stop::StepKind;
use crate::thread::Which;
use crate::value::Detail;

/// how a front end gets at one capability
///
/// [`Reach::Unreachable`] is the variant that makes this worth having. a front
/// end whose protocol genuinely cannot express a capability says so **here**,
/// with the reason, rather than leaving a gap that reads as an oversight — and
/// the parity test can then tell a named, justified exception apart from a
/// capability nothing can reach at all
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// a request or tool of the front end's own protocol maps onto it
    Direct(&'static str),

    /// the front end makes it without being asked, and this is when
    OnItsOwn(&'static str),

    /// this front end cannot use it in this shape, because it is a composition
    ///
    /// not a gap. the capability is reachable — the *combination* is what the
    /// protocol cannot use
    Composed {
        /// the capabilities it is a composition of, by [`Request::name`]
        of: &'static [&'static str],
        /// why the composed form is unusable here
        why: &'static str,
    },

    /// this front end's protocol cannot express it at all
    ///
    /// the reason is not decoration. a capability that no front end can reach is
    /// a capability nobody has, and the only thing that separates the two is a
    /// stated reason that someone had to write down
    Unreachable {
        /// why the protocol cannot carry it
        why: &'static str,
    },
}

impl Reach {
    /// whether this front end can get at the capability at all
    pub const fn reaches(&self) -> bool {
        !matches!(self, Self::Unreachable { .. })
    }
}

/// a capability carried **inside** a [`Request`] rather than being one
///
/// the parity rule is about capabilities, and not every capability is a variant.
/// a hit condition is a field of [`SourceBreakpoint`]; the bounds on how much of
/// a value is read are a field of three requests. a front end can implement
/// every variant and still not offer either, and enumerating variants would
/// never find it
///
/// deliberately closed and deliberately short. each entry is one an adapter has
/// had to answer for, and adding one means going and answering for it in both
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Facet {
    /// a breakpoint's typed hit condition — [`crate::HitCondition`]
    HitCondition,

    /// how much of a value one request may read — [`Detail`]
    ValueBounds,
}

impl Facet {
    /// every facet, for a test that has to cover all of them
    pub const ALL: [Self; 2] = [Self::HitCondition, Self::ValueBounds];

    /// what to call this capability in a message about it
    pub const fn name(self) -> &'static str {
        match self {
            Self::HitCondition => "a breakpoint's hit condition",
            Self::ValueBounds => "the bounds on how much of a value is read",
        }
    }
}

/// one request of every variant the core defines
///
/// an adapter's `reach_of` is what makes a new variant impossible to ignore;
/// this is what a coverage test drives an adapter against
pub fn surface() -> Vec<Request> {
    let frame = FrameId { stop: 1, depth: 0 };
    vec![
        Request::SetBreakpoints {
            breakpoints: vec![SourceBreakpoint::at(1, "a.py", 1)],
        },
        Request::SetExceptionBreakpoints {
            raised: false,
            uncaught: true,
        },
        Request::Run { deadline: None },
        Request::Wait { deadline: None },
        Request::Resume { which: Which::All },
        Request::Step {
            stop: 1,
            kind: StepKind::Over,
        },
        Request::Pause,
        Request::Threads {
            settle: Threads::SETTLE,
        },
        Request::StopTheWorld {
            stop: 1,
            settle: Threads::SETTLE,
        },
        Request::Stack { stop: 1, top: None },
        Request::Variables {
            frame,
            scope: Scope::Local,
            detail: Detail::default(),
        },
        Request::Evaluate {
            frame,
            expression: "1".to_string(),
            detail: Detail::default(),
        },
        Request::SetVariable {
            frame,
            scope: Scope::Local,
            name: "x".to_string(),
            value: "1".to_string(),
            detail: Detail::default(),
        },
        Request::Query {
            stop: 1,
            query: StateQuery {
                scopes: vec![Scope::Local],
                expressions: vec![Wanted {
                    expression: "1".to_string(),
                    frame: 0,
                }],
                source: Some(2),
                ..StateQuery::default()
            },
        },
        Request::Diff {
            before: SnapshotId {
                stop: 1,
                digest: "00".to_string(),
            },
            after: SnapshotId {
                stop: 2,
                digest: "01".to_string(),
            },
        },
        Request::RunScript {
            stop: 1,
            script: Script {
                steps: vec![Step::StepOver],
                budget: Budget {
                    steps: NonZeroU32::new(1).expect("1 is not zero"),
                    wall_ms: NonZeroU64::new(1).expect("1 is not zero"),
                    bytes: NonZeroU32::new(1024).expect("1024 is not zero"),
                },
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn the_surface_holds_one_request_of_every_kind_and_no_kind_twice() {
        let names: Vec<&str> = surface().iter().map(Request::name).collect();
        let distinct: BTreeSet<&str> = names.iter().copied().collect();

        assert_eq!(
            names.len(),
            distinct.len(),
            "the surface names a capability twice: {names:?}"
        );
    }

    #[test]
    fn every_facet_is_named_once() {
        let names: BTreeSet<&str> = Facet::ALL.iter().map(|facet| facet.name()).collect();
        assert_eq!(names.len(), Facet::ALL.len(), "two facets share a name");
    }
}
