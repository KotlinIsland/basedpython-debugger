//! how every capability of the core reaches an agent
//!
//! the sibling of `bpd_dap::coverage`, and the reason the parity rule is a test
//! rather than a review habit. [`reach_of`] matches [`Request`] with **no
//! catch-all arm**, so a variant added to the core is a compile error here
//! rather than a capability an agent silently does not have, and
//! [`reach_of_facet`] does the same for the capabilities carried inside a
//! request
//!
//! `crates/bpd_mcp/tests/coverage.rs` drives the server through a whole session
//! and checks this table against what the session was really asked.
//! `crates/bpd/tests/parity.rs` is the two-sided half, and compares it against
//! DAP's answer

use bpd_core::parity::{Facet, Reach};
use bpd_core::session::Request;

pub use bpd_core::parity::surface;

/// how one capability reaches an agent
///
/// the match is exhaustive and has no catch-all arm, which is the whole point
/// of the function
pub const fn reach_of(request: &Request) -> Reach {
    match request {
        Request::SetBreakpoints { .. } => Reach::Direct("set_breakpoints"),
        Request::SetExceptionBreakpoints { .. } => Reach::Direct("set_exception_breakpoints"),
        Request::DebugChildren { .. } => Reach::Direct("debug_children"),

        // the composed form DAP cannot use is the one an agent wants: the
        // answer to `continue_` **is** the stop it produced, so there is no
        // event to correlate and nothing to poll
        Request::Run { .. } => Reach::Direct("continue_"),

        Request::Wait { .. } => Reach::Direct(
            "wait, and the tail of every other control tool — a step, a pause \
             and a launch all end by waiting for what the program did",
        ),

        Request::Resume { .. } => Reach::Direct("resume"),
        Request::Step { .. } => Reach::Direct("step_over, step_in and step_out"),
        Request::Pause => Reach::Direct("pause"),
        Request::Threads { .. } => Reach::Direct("threads"),
        Request::StopTheWorld { .. } => Reach::Direct("stop_the_world"),

        Request::Stack { .. } => Reach::Direct(
            "stack, and the `frames` of every control tool — an agent that had \
             to ask again for where it stopped would be paying the round trip \
             this interface exists to remove",
        ),

        Request::Variables { .. } => Reach::Direct("variables"),
        Request::TemplateContext { .. } => Reach::Direct("template_context"),
        Request::Evaluate { .. } => Reach::Direct("evaluate"),
        Request::SetVariable { .. } => Reach::Direct("set_variable"),
        Request::SetNextStatement { .. } => Reach::Direct("set_next_statement"),
        Request::RestartFrame { .. } => Reach::Direct("restart_frame"),
        Request::ReplaceCode { .. } => Reach::Direct("replace_code"),

        // the shape this front end exists for, in one call: an agent says what
        // it wants to know and is answered with it, instead of walking a tree
        Request::Query { .. } => Reach::Direct("state"),
        Request::Diff { .. } => Reach::Direct("diff"),

        // the shape this whole front end exists for, taken one step further: an
        // MCP tool takes JSON Schema input, so a tree of steps goes across as
        // itself and the schema is the documentation an agent reads
        Request::RunScript { .. } => Reach::Direct("run_script"),
    }
}

/// how one capability carried inside a request reaches an agent
///
/// exhaustive, for the reason [`reach_of`] is
pub const fn reach_of_facet(facet: Facet) -> Reach {
    match facet {
        // the capability DAP has no route for. an MCP tool takes JSON Schema
        // input, so the typed form goes across as itself and there is no
        // convention to guess at
        Facet::HitCondition => Reach::Direct(
            "the `hits` object of a breakpoint in `set_breakpoints`, which \
             carries the kind and the count as themselves",
        ),

        Facet::ValueBounds => Reach::Direct(
            "the `detail` object of `variables`, `evaluate`, `set_variable` and \
             `state`, which is per call rather than per session",
        ),

        // MCP has no push, so a second session has to be **learnable**: the
        // `sessions` tool is how an agent finds out one exists, and the
        // `session` argument is how it says which one a call is for. it is
        // optional everywhere, because naming none still means the only session
        // there is
        Facet::Session => Reach::Direct(
            "sessions, which lists them, and the optional `session` argument of \
             every tool that is about one. a tool that is about a stop needs \
             neither: the stop carries the session it was reported from",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tools;

    #[test]
    fn every_tool_a_reach_names_is_a_tool_this_server_offers() {
        // the table is prose, and prose drifts. every name in it that looks like
        // a tool has to be one, or the coverage test is checking a claim about
        // something that does not exist
        let offered: Vec<&str> = tools().iter().map(|tool| tool.name).collect();

        for request in surface() {
            let (Reach::Direct(how) | Reach::OnItsOwn(how)) = reach_of(&request) else {
                continue;
            };
            let first = how
                .split([',', ' '])
                .next()
                .expect("a split always yields one piece");
            assert!(
                offered.contains(&first),
                "`{}` is said to be reached through `{first}`, which is not a \
                 tool. what is offered: {offered:?}",
                request.name()
            );
        }
    }
}
