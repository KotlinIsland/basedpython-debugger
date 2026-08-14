//! how every capability of the core reaches a DAP client
//!
//! the parity rule is that no capability exists in one adapter and not the
//! other. [`reach_of`] matches [`Request`] with **no catch-all arm**, so a
//! variant added to the core is a compile error here rather than a capability
//! DAP silently does not have, and [`reach_of_facet`] does the same for the
//! capabilities that are carried *inside* a request rather than being one
//!
//! [`carriage_of`] is the same thing for the other direction — what the debugger
//! **says** without being asked. that half was held by `bpd_core::Reporting`,
//! which has no default bodies, so an implementation had to exist and an empty
//! one satisfied it. as [`bpd_core::Told`] it is data, and the same exhaustive
//! match applies
//!
//! two tests read these tables. `crates/bpd_dap/tests/coverage.rs` drives the
//! adapter with a real DAP conversation and checks them against what the session
//! was actually asked and what the client was actually told — so an entry that
//! claims a mapping the adapter does not make fails rather than reading well.
//! `crates/bpd/tests/parity.rs` is the two-sided half, and compares this against
//! the MCP adapter's answer

use bpd_core::parity::{Carried, Facet, Reach, Told};
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

        // DAP has no request for it and does not need one: it is a property of
        // the session rather than something asked mid-flight, and the launch
        // configuration is where a DAP session's properties are. the adapter
        // sends it at `configurationDone`, before the program has run a line,
        // because a program can fork in its first statement
        Request::DebugChildren { .. } => Reach::OnItsOwn(
            "at `configurationDone`, when `debugChildren` is set in the launch \
             configuration. it is refused up front unless the client supports \
             the `startDebugging` reverse request and this adapter is reachable \
             by a second connection, because a debugged fork stops and a client \
             that cannot take it up would leave it held",
        ),

        Request::Run { .. } => Reach::Composed {
            of: &["resuming a thread", "waiting for the program"],
            why: "a `continue` has to be answered before the program stops \
                  again, and a run does not return until it does. the adapter \
                  resumes, answers, and then waits — which is the same two \
                  things in the order DAP needs them",
        },

        Request::Wait { .. } => Reach::OnItsOwn(
            "whenever no thread is held, which is what the adapter does between \
             a resume and the `stopped` event that follows it. it carries a \
             deadline and nothing is reported when one passes: the answer to a \
             DAP `continue` has already been sent, so there is nothing waiting \
             on the wait and nothing a timeout would be the answer to. the \
             deadline is there because two connections serve two sessions of one \
             debuggee, and a wait that blocked in the engine until the program \
             stopped would hold it against the other",
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

        // DAP has no request for replacing a running process's code, and its
        // `restart` is the opposite thing — it throws the process away. so this
        // is an extension, for the reason a script is: the capability is the
        // core's, and an editor is where somebody edits the file that makes it
        // worth having
        Request::ReplaceCode { .. } => Reach::Direct("bpd/replaceCode, a custom request"),
    }
}

/// how one capability carried inside a request reaches a DAP client
///
/// exhaustive, for the reason [`reach_of`] is
pub const fn reach_of_facet(facet: Facet) -> Reach {
    match facet {
        // a boolean on the custom request that carries the replacement itself,
        // and the report comes back two ways: in the body, whole, and as a
        // console line per frame — the same place a refusal's reason goes,
        // because succeeding here costs the same thing failing usually saves
        Facet::LiveReplacement => Reach::Direct(
            "`evenUnderALiveFrame` on `bpd/replaceCode`, with every frame still \
             on the old code in the answer and on the console",
        ),

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

        // a DAP session **is** the connection, so a client never writes a
        // session id on a request and there is no field for one. the spec's
        // answer to a second debuggee is the `startDebugging` reverse request:
        // the adapter asks the client to start a second session, and that
        // session's connection is what names it — in the configuration the
        // reverse request handed over, which the client sends straight back on
        // its `attach`
        Facet::Session => Reach::OnItsOwn(
            "on every request it makes: one that is about a stop is addressed \
             to the session that stop was reported from, which the stop \
             carries, and one that is about the program names the session this \
             connection serves. which session that is comes from the \
             `startDebugging` reverse request for a second one, and from the \
             launch for the first",
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

        // DAP has a field for this and its own spec names the case: `origin`
        // is "the origin of this source. for example, 'internal module',
        // 'inlined content from source map'". a frame of a basedpython build
        // is reported at its `.by` line, and this is where it says where the
        // interpreter really is
        Facet::GeneratedLocation => Reach::Direct(
            "the `origin` of a stack frame's `source`, which carries the \
             sentence `bpd_core::Mapping` renders — either the generated file \
             and line the interpreter is at, or, for generated python no `.by` \
             line is behind, the map's own reason there is none",
        ),

        // the reverse request exists for exactly this, and it is the only
        // honest way an adapter gives a debuggee a terminal: the client owns
        // one and makes it, where an adapter has none to give. a pseudo-terminal
        // in front of a debug console would be `isatty()` answering `True`
        // about a thing that delivers no keystrokes and has no size
        Facet::Terminal => Reach::Direct(
            "the `runInTerminal` reverse request, asked for by `console` in the \
             launch configuration — the client is handed the argument vector \
             and the environment bpd would have spawned, and the agent connects \
             back from the terminal exactly as it does from a process bpd \
             started. it is refused at `launch` unless the client advertised \
             `supportsRunInTerminalRequest`",
        ),
    }
}

/// how one thing the debugger says reaches a DAP client
///
/// exhaustive, for the reason [`reach_of`] is: a fact added to the core is a
/// compile error here rather than a fact DAP silently swallows
///
/// every entry is [`Carried::Pushed`] but one. DAP has an event stream, so the
/// adapter says a thing when it happens rather than keeping it for an answer
/// somebody may never ask for
pub const fn carriage_of(told: Told) -> Carried {
    match told {
        Told::Logged => Carried::Pushed(
            "an `output` event on the `stdout` category, carrying the source and \
             the line the logpoint is on. `stdout` because a logpoint's message \
             is the program's own words, written where the program would have \
             written them",
        ),

        Told::Pausing => Carried::Pushed(
            "an `output` event on the `console` category, naming the threads that \
             were running python when the pause went on — and saying that nothing \
             is going to arrive when there were none",
        ),

        // `console` and not `stdout`. the program did not write this, bpd did,
        // and a client that filed it among the program's own output would be
        // putting words in the debuggee's mouth
        Told::Spawned => Carried::Pushed(
            "an `output` event on the `console` category, carrying the core's \
             whole sentence — what bpd can tell about the child being python, \
             and whether bpd was asked to take it up as a session of its own — \
             with no `source` and no `line`: the audit hook sees what the \
             program asked the operating system for and not where it was asked",
        ),

        // DAP has a category for exactly this — something a user should see with
        // the console collapsed — and it is the one notice that must not scroll
        // past, because every other thing bpd says is a positive claim and this
        // one is about an absence
        Told::BlindSpot => Carried::Pushed(
            "an `output` event on the `important` category, which is what DAP has \
             for something the user should see even with the console collapsed",
        ),

        Told::Attached => Carried::Pushed(
            "the `startDebugging` reverse request, carrying a configuration that \
             names the session and how to reach this adapter — and a `console` \
             line saying the child is held. it is the spec's own answer to a \
             second debuggee, rather than debugpy's `debugpyAttach` event, which \
             predates the spec having one",
        ),

        Told::Stopped => Carried::Pushed(
            "a `stopped` event naming the thread and the reason, with \
             `allThreadsStopped` only when the world really was stopped",
        ),

        // the third half is the one a reader would not think to ask for: an exit
        // whose output has **not** all arrived says so, because everything after
        // it is a line the client would otherwise read as having been printed
        // before the program ended
        Told::Exited => Carried::Pushed(
            "an `exited` event carrying the code, and a `terminated` event after \
             it — with an `output` event in front of them when what the program \
             wrote is still being written",
        ),

        Told::Finishing => Carried::Pushed(
            "an `output` event naming the threads still held and why the process \
             cannot exit until they are resumed, and then the `stopped` events \
             for them",
        ),

        // `terminated` and **no** `exited`, which is the protocol saying exactly
        // what is true: DAP's `exited` event carries an `exitCode` as a required
        // field, and there is none to carry
        Told::Ended => Carried::Pushed(
            "a `terminated` event and deliberately no `exited` one, with the \
             reason on the `console` category — DAP's `exited` carries an \
             `exitCode` and bpd did not start that process, so there is no number \
             for it and a zero would be invented",
        ),

        // the one thing the debugger says that DAP has nowhere to put. it is a
        // limit of the protocol's shape rather than a thing nobody got round to:
        // there is no event for "still running", and the request a timeout would
        // answer was answered long before the timeout happened
        Told::StillRunning => Carried::Nowhere {
            why: "DAP answers `continue` **before** the program stops again, so a \
                  deadline that passes has nothing outstanding to be the answer \
                  to — and there is no event for it either, because the client \
                  already knows the program is running: it was told when its \
                  `continue` was answered and it has had no `stopped` since. the \
                  adapter's own wait carries a deadline only so that one \
                  connection cannot block the other sessions of the same \
                  debuggee, and nothing about the program is learned when it \
                  passes",
        },
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
