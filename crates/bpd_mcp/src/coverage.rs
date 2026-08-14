//! how every capability of the core reaches an agent
//!
//! the sibling of `bpd_dap::coverage`, and the reason the parity rule is a test
//! rather than a review habit. [`reach_of`] matches [`Request`] with **no
//! catch-all arm**, so a variant added to the core is a compile error here
//! rather than a capability an agent silently does not have, and
//! [`reach_of_facet`] does the same for the capabilities carried inside a
//! request
//!
//! [`carriage_of`] is the same thing for the other direction — what the debugger
//! **says** without being asked. that half was held by `bpd_core::Reporting`,
//! which has no default bodies, so an implementation had to exist and an empty
//! one satisfied it. as [`bpd_core::Told`] it is data, and the same exhaustive
//! match applies
//!
//! `crates/bpd_mcp/tests/coverage.rs` drives the server through a whole session
//! and checks these tables against what the session was really asked and what the
//! client was really told. `crates/bpd/tests/parity.rs` is the two-sided half,
//! and compares them against DAP's answers

use bpd_core::parity::{Carried, Facet, Reach, Told};
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

        // an agent reasoning about what a branch will do is the same reader
        // an editor's inlay hint is, and it needs the same second half: not
        // what a name holds, but how far past this line that can be carried
        Request::Facts { .. } => Reach::Direct("facts"),

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
        // an argument of the tool that carries the replacement itself. the
        // report comes back in the answer's notes and **above** the changes,
        // because it is what the changes have to be read against
        Facet::LiveReplacement => Reach::Direct(
            "`even_under_a_live_frame` on `replace_code`, with every frame still \
             on the old code in the answer",
        ),

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

        // an MCP answer is JSON, so both locations go on the frame as
        // themselves. `generated` is the pair an agent can act on — set a
        // breakpoint in it, read that file — and `mapped` is the sentence, the
        // same one DAP puts on a source's `origin`
        Facet::GeneratedLocation => Reach::Direct(
            "the `mapped` and `generated` fields of a frame in stack and in \
             state, which carry the sentence `bpd_core::Mapping` renders and \
             the generated file and line themselves",
        ),

        // the one facet an MCP tool cannot carry, and the only one that is not
        // a payload: everything else here is a shape, and JSON Schema carries
        // any shape. a terminal is a thing the client has to **have**
        Facet::Terminal => Reach::Unreachable {
            why: "there is no terminal on this side to give. an MCP client is an \
                  agent — it reads the program's output out of a tool's answer, \
                  and there is no terminal in that picture for a debuggee to run \
                  on, no keystrokes to deliver to one and nothing that would \
                  render an escape sequence. DAP's `runInTerminal` works because \
                  it asks a client that **owns** a terminal to make one, and the \
                  equivalent here would be this server opening a pseudo-terminal \
                  and calling it the agent's — which is `isatty()` answering \
                  `True` about a thing that is not a terminal, in a debugger \
                  whose whole rule is that what it reports is true. an agent \
                  that needs a program to consume input gives it a file, an \
                  argument or the environment",
        },
    }
}

/// how one thing the debugger says reaches an agent
///
/// exhaustive, for the reason [`reach_of`] is: a fact added to the core is a
/// compile error here rather than a fact an agent silently never hears
///
/// every entry is [`Carried::Pulled`], and that is the whole asymmetry between
/// the two front ends. MCP has no push — this server writes nothing that is not
/// an answer to something the client asked — so a fact that arrives while the
/// program is running is kept and handed over on the next answer instead of
/// being sent when it happens. it is a route rather than a gap, and it is the
/// one that has to be watched: a server that kept a fact and never handed it
/// over would look exactly like this until somebody read what an answer really
/// carried, which is what `crates/bpd_mcp/tests/coverage.rs` does
pub const fn carriage_of(told: Told) -> Carried {
    match told {
        Told::Logged => Carried::Pulled(
            "the `logged` key of the answer to whichever call the program was \
             running during, with the breakpoint, the file, the line, the thread \
             and which qualifying hit it was",
        ),

        Told::Pausing => Carried::Pulled(
            "`pause_armed_while_running` under the `logged` key, which is the \
             threads that were running python when the pause went on",
        ),

        // its own key rather than part of the logs. an agent that found a child
        // under `logged` would reasonably read it as a logpoint having fired
        Told::Spawned => Carried::Pulled(
            "`spawned.started` on the answer to the call the program was running \
             during, one entry per child, each saying what bpd can tell about it \
             being python and, under `taking_up`, whether bpd was asked to take \
             it up as a session of its own",
        ),

        Told::BlindSpot => Carried::Pulled(
            "`spawned.cannot_see`, beside the children rather than instead of \
             them — an agent that read `started: []` without it would conclude \
             there were none — with `silence_is_not_evidence` as the field to act \
             on",
        ),

        Told::Attached => Carried::Pulled(
            "`attached.sessions` on the next answer, and the `sessions` tool \
             afterwards. both, because MCP has no push and a **held** process \
             nothing was told about is a hung program: the answer is what makes \
             it news, and the tool is what makes it learnable by an agent that \
             was not listening",
        ),

        Told::Stopped => Carried::Pulled(
            "`outcome: stopped` on the answer to the control tool that let the \
             program run, with the stop, the thread, the reason and the frames — \
             one call, one answer, and no event to correlate",
        ),

        // `output_complete` is the half that would otherwise go missing: an
        // agent that read `exited` as the end of the program's output would
        // attribute a child's later lines to a run that had already ended
        Told::Exited => Carried::Pulled(
            "`outcome: exited`, carrying `exit_code` and `output_complete` — with \
             a `note` saying what still holds the stream when it is false",
        ),

        Told::Finishing => Carried::Pulled(
            "`outcome: finishing`, carrying `held` — the threads that have to be \
             resumed before the interpreter can finalize",
        ),

        // a separate outcome from `exited`, and deliberately without an
        // `exit_code` field rather than with a null one: the program is over and
        // the number is not bpd's to give
        Told::Ended => Carried::Pulled(
            "`outcome: ended`, deliberately with no `exit_code` field at all — a \
             null would read as a number that was measured",
        ),

        // the outcome DAP has nowhere to put, and the one this front end most
        // needs: every control tool here blocks until the program stops again,
        // so a deadline that passes is what the call returns
        Told::StillRunning => Carried::Pulled(
            "`outcome: timed_out`, carrying `waited_ms` and what is held now. it \
             is never rendered as a stop: nothing was held and nothing was read \
             off the program, so no location is reported for it, not even a \
             sampled one",
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
