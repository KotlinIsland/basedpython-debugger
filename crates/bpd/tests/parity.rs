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
//!   carried inside a request
//! - **here.** saying it is reached is not the same as reaching it, so this
//!   compares what the two adapters claim — and a claim of "cannot" has to be
//!   one that was written down and justified, rather than one that appeared
//!
//! this test lives in the `bpd` crate because that is the only place both
//! adapters are visible at once. neither may depend on the other, and neither
//! may depend on the engine

use std::collections::BTreeSet;

use bpd_core::parity::{Facet, Reach};
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
];

/// what each adapter says about one capability
struct Both {
    capability: &'static str,
    dap: Reach,
    mcp: Reach,
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
