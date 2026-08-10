//! how every capability of the core reaches a DAP client
//!
//! the parity rule is that no capability exists in one adapter and not the
//! other. the *test* of that rule needs two adapters and arrives with the
//! second one. this is the half that can be written now, and it is the half
//! that bites when someone adds a capability: [`reach_of`] matches
//! [`Request`] with **no catch-all arm**, so a variant added to the core is a
//! compile error here rather than a capability DAP silently does not have
//!
//! `crates/bpd_dap/tests/coverage.rs` is the other half. it drives the adapter
//! with a real DAP conversation, records every request the session was actually
//! asked, and checks this table against what happened — so an entry that claims
//! a mapping the adapter does not make fails rather than reading well

use bpd_core::{Detail, FrameId, Request, Scope, SourceBreakpoint, StepKind, Threads, Which};

/// how a DAP client gets at one capability
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// a DAP request maps onto it
    Command(&'static str),

    /// the adapter makes it without being asked, and this is when
    OnItsOwn(&'static str),

    /// DAP cannot use this form of it, because it is a composition
    ///
    /// not a gap. the capability is reachable — the *combination* is what an
    /// event driven protocol cannot use, because it would mean not answering a
    /// request until the program next stopped
    Composed {
        /// the capabilities it is a composition of, by [`Request::name`]
        of: &'static [&'static str],
        /// why the composed form is unusable here
        why: &'static str,
    },
}

/// how one capability reaches a DAP client
///
/// the match is exhaustive and has no catch-all arm, which is the whole point
/// of the function
pub const fn reach_of(request: &Request) -> Reach {
    match request {
        Request::SetBreakpoints { .. } => Reach::Command("setBreakpoints"),
        Request::SetExceptionBreakpoints { .. } => Reach::Command("setExceptionBreakpoints"),

        Request::Run => Reach::Composed {
            of: &["resuming a thread", "waiting for the program"],
            why: "a `continue` has to be answered before the program stops \
                  again, and a run does not return until it does. the adapter \
                  resumes, answers, and then waits — which is the same two \
                  things in the order DAP needs them",
        },

        Request::Wait => Reach::OnItsOwn(
            "whenever no thread is held, which is what the adapter does between \
             a resume and the `stopped` event that follows it",
        ),

        Request::Resume { .. } => Reach::Command("continue"),
        Request::Step { .. } => Reach::Command("next, stepIn and stepOut"),
        Request::Pause => Reach::Command("pause"),
        Request::Threads { .. } => Reach::Command("threads"),

        Request::StopTheWorld { .. } => Reach::OnItsOwn(
            "at every stop, when `stopTheWorld` is set in the launch \
             configuration. it is what makes `allThreadsStopped` true on a \
             `stopped` event, and DAP has no request for it because DAP has no \
             concept of a program that is only partly held",
        ),

        Request::Stack { .. } => Reach::Command("stackTrace"),
        Request::Variables { .. } => Reach::Command("variables"),
        Request::Evaluate { .. } => Reach::Command("evaluate"),
        Request::SetVariable { .. } => Reach::Command("setVariable"),
    }
}

/// one request of every variant the core defines
///
/// [`reach_of`] is what makes a new variant impossible to ignore; this list is
/// what the coverage test drives the adapter against
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
        Request::Run,
        Request::Wait,
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
    fn every_composition_is_composed_of_capabilities_that_are_themselves_reachable() {
        let reachable: BTreeSet<&str> = surface()
            .iter()
            .filter(|request| !matches!(reach_of(request), Reach::Composed { .. }))
            .map(Request::name)
            .collect();

        for request in surface() {
            if let Reach::Composed { of, .. } = reach_of(&request) {
                for part in of {
                    assert!(
                        reachable.contains(part),
                        "`{}` is said to be a composition of `{part}`, which is \
                         not itself reachable",
                        request.name()
                    );
                }
            }
        }
    }
}
