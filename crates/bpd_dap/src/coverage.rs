//! how every capability of the core reaches a DAP client
//!
//! the parity rule is that no capability exists in one adapter and not the
//! other. [`reach_of`] matches [`Request`] with **no catch-all arm**, so a
//! variant added to the core is a compile error here rather than a capability
//! DAP silently does not have, and [`reach_of_facet`] does the same for the
//! capabilities that are carried *inside* a request rather than being one
//!
//! two tests read this table. `crates/bpd_dap/tests/coverage.rs` drives the
//! adapter with a real DAP conversation and checks the table against what the
//! session was actually asked — so an entry that claims a mapping the adapter
//! does not make fails rather than reading well. `crates/bpd/tests/parity.rs`
//! is the two-sided half, and compares this against the MCP adapter's answer

use bpd_core::parity::{Facet, Reach};
use bpd_core::session::Request;

pub use bpd_core::parity::surface;

/// how one capability reaches a DAP client
///
/// the match is exhaustive and has no catch-all arm, which is the whole point
/// of the function
pub const fn reach_of(request: &Request) -> Reach {
    match request {
        Request::SetBreakpoints { .. } => Reach::Direct("setBreakpoints"),
        Request::SetExceptionBreakpoints { .. } => Reach::Direct("setExceptionBreakpoints"),

        Request::Run { .. } => Reach::Composed {
            of: &["resuming a thread", "waiting for the program"],
            why: "a `continue` has to be answered before the program stops \
                  again, and a run does not return until it does. the adapter \
                  resumes, answers, and then waits — which is the same two \
                  things in the order DAP needs them",
        },

        Request::Wait { .. } => Reach::OnItsOwn(
            "whenever no thread is held, which is what the adapter does between \
             a resume and the `stopped` event that follows it. it never carries \
             a deadline: the answer to a DAP `continue` has already been sent, \
             so there is nothing waiting on the wait and nothing a timeout \
             would be the answer to",
        ),

        Request::Resume { .. } => Reach::Direct("continue"),
        Request::Step { .. } => Reach::Direct("next, stepIn and stepOut"),
        Request::Pause => Reach::Direct("pause"),
        Request::Threads { .. } => Reach::Direct("threads"),

        Request::StopTheWorld { .. } => Reach::OnItsOwn(
            "at every stop, when `stopTheWorld` is set in the launch \
             configuration. it is what makes `allThreadsStopped` true on a \
             `stopped` event, and DAP has no request for it because DAP has no \
             concept of a program that is only partly held",
        ),

        // DAP has no request of its own for a whole investigation and will not
        // grow one, so this is an extension — which the protocol provides for,
        // and which a client sends with its own `customRequest`. the answer is
        // the transcript, in the response body, because an editor given only
        // where a script ended cannot tell why any more than an agent can
        Request::RunScript { .. } => Reach::Direct("bpd/runScript, a custom request"),

        // DAP's own way of reading state is the tree walk, and it keeps it. the
        // one call form is an extension for the same reason a script is: the
        // capability is the core's, and "the whole state at this stop" is a
        // thing a person wants as much as an agent does
        Request::Query { .. } => Reach::Direct("bpd/state, a custom request"),
        Request::Diff { .. } => Reach::Direct("bpd/diff, a custom request"),

        Request::Stack { .. } => Reach::Direct("stackTrace"),
        Request::Variables { .. } => Reach::Direct("variables"),
        // a template frame's `scopes` are the layers of its django context, one
        // DAP scope each, and `variables` on one reads that layer. DAP has no
        // idea what a template is and does not need one: a stack of dicts maps
        // onto a list of scopes exactly
        Request::TemplateContext { .. } => Reach::Direct("scopes, then variables"),
        Request::Evaluate { .. } => Reach::Direct("evaluate"),
        Request::SetVariable { .. } => Reach::Direct("setVariable"),

        // `goto` carries a target id rather than a line, so a client asks
        // `gotoTargets` for one first. that round trip is DAP's mechanics rather
        // than a capability of the core: the adapter mints a target for the
        // location the client asked about, and only when it is the file the held
        // thread is executing — a line number means nothing without the file it
        // is in, and cpython would accept the same number in another file
        Request::SetNextStatement { .. } => Reach::Direct("goto, after gotoTargets"),
        Request::RestartFrame { .. } => Reach::Direct("restartFrame"),
    }
}

/// how one capability carried inside a request reaches a DAP client
///
/// exhaustive, for the reason [`reach_of`] is
pub const fn reach_of_facet(facet: Facet) -> Reach {
    match facet {
        // the one capability of the core that DAP cannot carry, and the reason
        // is DAP's rather than bpd's: `hitCondition` is free text whose meaning
        // is a per-client convention. `>5`, `=5`, `%5` and a bare `5` are read
        // differently by different debuggers, so there is nothing to map
        // `HitCondition` onto that would mean the same thing to two clients
        Facet::HitCondition => Reach::Unreachable {
            why: "DAP carries a hit condition as a **string** whose meaning is a \
                  per-client convention — `>5`, `=5`, `%5` and a bare `5` are \
                  read differently by different debuggers. \
                  `bpd_core::HitCondition` is deliberately not that string, so \
                  `supportsHitConditionalBreakpoints` is not advertised and a \
                  client that sends one anyway is refused with the reason \
                  rather than answered on whichever convention bpd guessed",
        },

        // a DAP session **is** the connection. the spec's answer to a second
        // debuggee is the `startDebugging` reverse request, which has the
        // client start a whole second session of its own — so there is no
        // field on a DAP request for a session id and a client never writes
        // one. the adapter addresses every request it makes instead
        Facet::Session => Reach::OnItsOwn(
            "on every request it makes: one that is about a stop is addressed \
             to the session that stop was reported from, which the stop \
             carries, and one that is about the program names none — which is \
             the only session a connection serves",
        ),

        // DAP has nowhere on a request to carry them, so they come from the
        // launch configuration instead — one setting for the session rather
        // than one per read. that is a narrower way to reach it, not a gap
        Facet::ValueBounds => Reach::Direct(
            "the `variables` object of the launch configuration, every field of \
             which is a field of `bpd_core::Detail` — and per call on the \
             `detail` of a `bpd/state`, which is an extension and so has \
             somewhere to carry one",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

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

    #[test]
    fn a_capability_dap_cannot_carry_says_why_rather_than_being_left_out() {
        let Reach::Unreachable { why } = reach_of_facet(Facet::HitCondition) else {
            panic!("DAP has no route for a hit condition, and the table says it has")
        };
        assert!(
            why.contains("per-client convention"),
            "an unreachable capability has to say what stands in the way, and \
             said {why}"
        );
    }
}
