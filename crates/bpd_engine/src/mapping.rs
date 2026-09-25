//! which `.by` breakpoints a build can answer, decided before the agent is asked
//!
//! `by run` compiles every module it staged **as the `.by` it was transpiled
//! from**: its runner reads the generated python, compiles it, and names the code
//! object after the `.by` path with every line the `.by` line it came from. so the
//! interpreter of a basedpython build is already running `.by` locations, and a
//! breakpoint on `app.by` line 7 goes to the agent as exactly that — the agent
//! binds it by the file's identity and the code's own line table, the way it binds
//! a line of any python file, and its answer is already in the terms the client
//! asked in
//!
//! what is decided here, out of process, is only whether the build can answer at
//! all. a `.by` in a program with no map is in a program `by run` did not start,
//! and a `.by` the map does not describe is not part of the build that is running.
//! both are refused by name rather than sent to wait for code that will never
//! load

use std::collections::BTreeMap;
use std::path::Path;

use bpd_core::source_map::SourceMap;
use bpd_core::{Binding, Resolved, SourceBreakpoint, Unbound};

/// the extension a basedpython source file has
///
/// the discriminator is the extension rather than "the map has heard of it",
/// and that is deliberate. a `.by` the map says nothing about is a file this
/// build never staged, and letting it through to the agent would earn it "the
/// interpreter has not loaded any code from this file. it will bind if that file
/// is imported later" — a promise nothing in this program can keep
const SOURCE_EXTENSION: &str = "by";

/// what a breakpoint set became
#[derive(Debug, Default)]
pub(crate) struct Sent {
    /// the set as the agent should receive it
    pub(crate) breakpoints: Vec<SourceBreakpoint>,
    /// the answers that were decided here, because the build cannot answer them
    ///
    /// these never reach the agent. a `.by` the build does not hold has no code
    /// to wait for, and asking about one would be asking the agent to wait for
    /// ever
    pub(crate) refused: Vec<Resolved>,
    /// the ids of the set, in the order the client asked about them
    ///
    /// a breakpoint refused here never reaches the agent, so the agent's answers
    /// are a subset in its own order and the refusals have to be put back among
    /// them. a client that reads an answer by position — every DAP client does,
    /// because `setBreakpoints` answers an array — would otherwise read the
    /// answer to one breakpoint as the answer to another
    pub(crate) order: Vec<u32>,
}

/// put a set of answers back into the order they were asked about
pub(crate) fn reorder(order: &[u32], mut answers: Vec<Resolved>) -> Vec<Resolved> {
    let at: BTreeMap<u32, usize> = order
        .iter()
        .enumerate()
        .map(|(index, id)| (*id, index))
        .collect();
    answers.sort_by_key(|answer| {
        at.get(&answer.id).copied().unwrap_or_else(|| {
            unreachable!(
                "breakpoint {} was answered and it was not in the set that was \
                 asked about",
                answer.id
            )
        })
    });
    answers
}

/// whether a file is basedpython source, which only a build can hold
pub(crate) fn is_source(file: &Path) -> bool {
    file.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(SOURCE_EXTENSION))
}

/// sort a whole breakpoint set into what the agent is asked and what is refused
///
/// `map` is `None` when bpd was given no source map. that is not the same as a
/// map that has nothing to say: with no map at all a `.by` breakpoint is refused
/// naming what would produce one, and every other file goes through untouched
pub(crate) fn send(map: Option<&SourceMap>, breakpoints: Vec<SourceBreakpoint>) -> Sent {
    let mut sent = Sent {
        order: breakpoints.iter().map(|breakpoint| breakpoint.id).collect(),
        ..Sent::default()
    };
    for breakpoint in breakpoints {
        if !is_source(&breakpoint.file) {
            sent.breakpoints.push(breakpoint);
            continue;
        }
        let Some(map) = map else {
            sent.refused.push(unbound(
                breakpoint.id,
                Unbound::NoSourceMap {
                    file: breakpoint.file.clone(),
                },
            ));
            continue;
        };
        match map.generated_from(&breakpoint.file) {
            Ok(_) => sent.breakpoints.push(breakpoint),
            Err(reason) => sent
                .refused
                .push(unbound(breakpoint.id, Unbound::Unmappable { reason })),
        }
    }
    sent
}

/// a whole answer that is a refusal
fn unbound(id: u32, reason: Unbound) -> Resolved {
    Resolved {
        id,
        binding: Binding::Unbound { reason },
        // a breakpoint that did not bind is not waiting for one either: there
        // is nothing for the arming to arm
        waiting_for: None,
    }
}
