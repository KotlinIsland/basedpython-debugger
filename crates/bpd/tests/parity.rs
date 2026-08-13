//! the parity rule, checked from both sides
//!
//! the rule is that no capability exists in one adapter and not the other, and
//! this is the test the whole `bpd_core` refactor existed to make writable. a
//! capability used to be a method on the engine's debuggee, rust cannot
//! enumerate methods, and so the rule was a policy someone had to remember
//!
//! it bites in two places, and both are needed:
//!
//! - **at compile time.** `bpd_dap::reach_of` and `bpd_mcp::reach_of` match
//!   `bpd_core::Request` with no catch-all arm, so a variant added to the core
//!   does not compile in either adapter until someone says how that front end
//!   gets at it. the same is true of `reach_of_facet` for the capabilities
//!   carried inside a request, and of `carriage_of` for what the debugger says
//! - **here.** saying it is reached is not the same as reaching it, so this
//!   compares what the two adapters claim — and a claim of "cannot" has to be
//!   one that was written down and justified, rather than one that appeared
//!
//! the rule is about both directions. what a client **asks for** is a `Request`
//! and a `Facet`; what the debugger **says** is a `Told`, which was held by
//! `bpd_core::Reporting` — a trait with no default bodies, which forces an
//! implementation to exist and is satisfied by an empty one. nothing failed if
//! an adapter took a report and dropped it on the floor, and that is the half
//! this file grew to cover
//!
//! what this file cannot do is watch either claim come true. that is each
//! adapter's own coverage test, which drives a real conversation and reads what
//! the session was asked and what the client was told
//!
//! this test lives in the `bpd` crate because that is the only place both
//! adapters are visible at once. neither may depend on the other, and neither
//! may depend on the engine

use std::collections::BTreeSet;

use bpd_core::parity::{Carried, Facet, Reach, Told};
use bpd_core::surface;

/// every capability one adapter cannot reach, and which adapter cannot
///
/// this list is written by hand and it is meant to be short. an entry is here
/// because someone established that the front end's **protocol** cannot carry
/// the capability at all, wrote the reason into that adapter's `reach_of`, and
/// added the line below — so a gap that appears without all three fails this
/// test rather than joining a list of excuses
///
/// a capability neither adapter can reach is not eligible for this list at any
/// price: that is not a gap in a front end, it is a capability nobody has
const JUSTIFIED: &[(&str, &str)] = &[
    // DAP carries a hit condition as free text whose meaning is a per-client
    // convention. `bpd_core::HitCondition` is deliberately not that string, so
    // there is nothing to map it onto — and MCP, whose tools take JSON Schema
    // input, carries the typed form as itself
    ("DAP", "a breakpoint's hit condition"),
    // an MCP client is an agent and has no terminal for a program to run on:
    // it reads the program's output out of a tool's answer. DAP's
    // `runInTerminal` works by asking a client that **owns** a terminal to make
    // one, and the equivalent here would be this server opening a
    // pseudo-terminal and calling it the agent's — `isatty()` answering `True`
    // about a thing that is not a terminal
    ("MCP", "running the debuggee on a terminal the client owns"),
];

/// everything the debugger says that one front end cannot pass on, and which
///
/// the sibling of [`JUSTIFIED`], kept separately so that a stale line in one
/// cannot excuse a gap in the other. the bar is the same and so is the reason
/// for it: an entry is here because someone established that the front end's
/// **protocol** has nowhere to put the fact, wrote the reason into that
/// adapter's `carriage_of`, and added the line
///
/// a fact neither front end passes on is not eligible at any price. that is not
/// a gap in a front end — it is the debugger establishing something and throwing
/// it away, which is the one thing this project says it will never do
const SILENT: &[(&str, &str)] = &[
    // DAP answers a `continue` before the program stops again, so a deadline
    // that passes has nothing outstanding to be the answer to and there is no
    // event for it. the client already knows the program is running. MCP, whose
    // control tools return the stop they produced, has to carry it — and does
    ("DAP", "a deadline passing with the program still running"),
];

/// what each adapter says about one capability
struct Both {
    capability: &'static str,
    dap: Reach,
    mcp: Reach,
}

/// what each adapter says about one thing the debugger says
struct Said {
    told: Told,
    dap: Carried,
    mcp: Carried,
}

/// everything the debugger says, as both adapters describe passing it on
fn said() -> Vec<Said> {
    Told::ALL
        .map(|told| Said {
            told,
            dap: bpd_dap::carriage_of(told),
            mcp: bpd_mcp::carriage_of(told),
        })
        .into_iter()
        .collect()
}

/// every capability of the core, as both adapters describe it
fn everything() -> Vec<Both> {
    let mut all: Vec<Both> = surface()
        .iter()
        .map(|request| Both {
            capability: request.name(),
            dap: bpd_dap::reach_of(request),
            mcp: bpd_mcp::reach_of(request),
        })
        .collect();
    all.extend(Facet::ALL.map(|facet| Both {
        capability: facet.name(),
        dap: bpd_dap::reach_of_facet(facet),
        mcp: bpd_mcp::reach_of_facet(facet),
    }));
    all
}

#[test]
fn no_capability_of_the_core_is_out_of_reach_of_every_front_end() {
    // the failure this rules out is a capability that was built, is answered by
    // the engine, and that nobody can ask for. it is not a parity problem — it
    // is a feature that does not exist — and no entry in `JUSTIFIED` excuses it
    for both in everything() {
        assert!(
            both.dap.reaches() || both.mcp.reaches(),
            "`{}` cannot be reached from DAP or from MCP, so nothing can ask \
             for it. DAP says {:?} and MCP says {:?}",
            both.capability,
            both.dap,
            both.mcp
        );
    }
}

#[test]
fn the_capabilities_one_adapter_cannot_reach_are_exactly_the_ones_written_down() {
    let mut found: BTreeSet<(&str, &str)> = BTreeSet::new();
    for both in everything() {
        if !both.dap.reaches() {
            found.insert(("DAP", both.capability));
        }
        if !both.mcp.reaches() {
            found.insert(("MCP", both.capability));
        }
    }

    let written: BTreeSet<(&str, &str)> = JUSTIFIED.iter().copied().collect();

    let unexplained: Vec<&(&str, &str)> = found.difference(&written).collect();
    assert!(
        unexplained.is_empty(),
        "a capability is out of one adapter's reach and nobody has said so in \
         `JUSTIFIED`: {unexplained:?}. either give that front end a route to it, \
         or add the line and the reason — a gap that appears quietly is how the \
         two front ends drift apart"
    );

    let stale: Vec<&(&str, &str)> = written.difference(&found).collect();
    assert!(
        stale.is_empty(),
        "`JUSTIFIED` says these are out of reach and both adapters now reach \
         them: {stale:?}. take the line out — an exception nobody needs is an \
         excuse the next one hides behind"
    );
}

#[test]
fn a_capability_a_front_end_cannot_reach_says_what_stands_in_the_way() {
    for both in everything() {
        for (front_end, reach) in [("DAP", &both.dap), ("MCP", &both.mcp)] {
            let Reach::Unreachable { why } = reach else {
                continue;
            };
            assert!(
                why.len() > 40,
                "`{}` is out of {front_end}'s reach and the reason given is \
                 {why:?}. a reason is what separates a limit of the protocol \
                 from a thing nobody got round to",
                both.capability
            );
        }
    }
}

#[test]
fn every_route_a_front_end_claims_names_the_thing_it_goes_through() {
    // a reach is read by a person deciding whether a front end really has a
    // capability. one that named nothing would be a table saying "yes"
    for both in everything() {
        for (front_end, reach) in [("DAP", &both.dap), ("MCP", &both.mcp)] {
            let named = match reach {
                Reach::Direct(how) | Reach::OnItsOwn(how) => *how,
                Reach::Composed { why, .. } => *why,
                Reach::Unreachable { .. } => continue,
            };
            assert!(
                !named.trim().is_empty(),
                "{front_end}'s route to `{}` names nothing",
                both.capability
            );
        }
    }
}

#[test]
fn nothing_the_debugger_says_is_dropped_by_every_front_end() {
    // the failure this rules out is a fact the debugger established, is sure
    // enough of to report, and that nobody is ever told. it is not a parity
    // problem — it is a measurement taken and thrown away — and no entry in
    // `SILENT` excuses it
    for said in said() {
        assert!(
            said.dap.carries() || said.mcp.carries(),
            "`{}` reaches no client of either front end, so the debugger \
             establishes it and throws it away. DAP says {:?} and MCP says {:?}",
            said.told.name(),
            said.dap,
            said.mcp
        );
    }
}

#[test]
fn the_things_one_front_end_cannot_pass_on_are_exactly_the_ones_written_down() {
    let mut found: BTreeSet<(&str, &str)> = BTreeSet::new();
    for said in said() {
        if !said.dap.carries() {
            found.insert(("DAP", said.told.name()));
        }
        if !said.mcp.carries() {
            found.insert(("MCP", said.told.name()));
        }
    }

    let written: BTreeSet<(&str, &str)> = SILENT.iter().copied().collect();

    let unexplained: Vec<&(&str, &str)> = found.difference(&written).collect();
    assert!(
        unexplained.is_empty(),
        "one front end passes none of this on and nobody has said so in \
         `SILENT`: {unexplained:?}. either give that front end somewhere to put \
         it, or add the line and the reason — a fact that quietly stops \
         arriving is worse than one that never did, because a client goes on \
         reading the silence as news"
    );

    let stale: Vec<&(&str, &str)> = written.difference(&found).collect();
    assert!(
        stale.is_empty(),
        "`SILENT` says these reach nobody and both front ends now carry them: \
         {stale:?}. take the line out — an exception nobody needs is an excuse \
         the next one hides behind"
    );
}

#[test]
fn a_front_end_that_cannot_pass_something_on_says_what_stands_in_the_way() {
    for said in said() {
        for (front_end, carried) in [("DAP", &said.dap), ("MCP", &said.mcp)] {
            let Carried::Nowhere { why } = carried else {
                continue;
            };
            assert!(
                why.len() > 40,
                "`{}` reaches no {front_end} client and the reason given is \
                 {why:?}. a reason is what separates a limit of the protocol \
                 from a report somebody quietly stopped writing",
                said.told.name()
            );
        }
    }
}

#[test]
fn every_route_a_front_end_claims_for_something_it_says_names_where_it_turns_up() {
    // the same rule the request half has, and it matters more here. a client
    // cannot ask whether it was told something it was never told, so the only
    // check available is somebody going and looking in the place this names —
    // which is what each adapter's coverage test does
    for said in said() {
        for (front_end, carried) in [("DAP", &said.dap), ("MCP", &said.mcp)] {
            let named = match carried {
                Carried::Pushed(where_it_goes) | Carried::Pulled(where_it_goes) => *where_it_goes,
                Carried::Nowhere { .. } => continue,
            };
            assert!(
                !named.trim().is_empty(),
                "{front_end}'s route for `{}` names nothing",
                said.told.name()
            );
        }
    }
}

#[test]
fn the_front_end_with_no_push_says_so_on_every_route_and_the_other_on_none() {
    // the one structural difference between the two, stated once rather than
    // drifting into prose. MCP writes nothing that is not an answer to a call,
    // so every fact it passes on rides on one; DAP has an event stream and says
    // a thing when it happens, because an event nobody asked for is the only
    // way a client learns that a program it is not asking about has moved
    for said in said() {
        assert!(
            !matches!(said.mcp, Carried::Pushed(_)),
            "MCP claims to push `{}`, and this server writes nothing that is \
             not an answer to something the client asked",
            said.told.name()
        );
        assert!(
            !matches!(said.dap, Carried::Pulled(_)),
            "DAP claims to hold `{}` back for an answer, and a DAP client is \
             not obliged to ask anything again — a fact kept for a request that \
             never comes is a fact nobody is told",
            said.told.name()
        );
    }
}

#[test]
fn a_composition_is_made_of_capabilities_that_front_end_can_itself_reach() {
    let reachable = |front_end: &str, part: &str| {
        surface().iter().any(|request| {
            request.name() == part
                && match front_end {
                    "DAP" => matches!(
                        bpd_dap::reach_of(request),
                        Reach::Direct(_) | Reach::OnItsOwn(_)
                    ),
                    _ => matches!(
                        bpd_mcp::reach_of(request),
                        Reach::Direct(_) | Reach::OnItsOwn(_)
                    ),
                }
        })
    };

    for both in everything() {
        for (front_end, reach) in [("DAP", &both.dap), ("MCP", &both.mcp)] {
            let Reach::Composed { of, .. } = reach else {
                continue;
            };
            assert!(
                !of.is_empty(),
                "{front_end} says `{}` is a composition of nothing",
                both.capability
            );
            for part in *of {
                assert!(
                    reachable(front_end, part),
                    "{front_end} says `{}` is a composition of `{part}`, which \
                     it cannot itself reach",
                    both.capability
                );
            }
        }
    }
}
