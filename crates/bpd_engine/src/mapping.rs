//! turning a `.by` breakpoint into a generated one, and the answer back again
//!
//! the interpreter only ever knows about python, so a breakpoint the user set in
//! `app.by` has to reach the agent as a line of the python `by` transpiled that
//! file to. this is the layer that does it, and it is **out of process** on
//! purpose:
//!
//! - a breakpoint is set before the program has run, and in DAP before it has
//!   been launched at all. a translation that had to ask the debuggee would be
//!   an answer that arrives after the question
//! - the map is verified by hashing two files on disk, which is work the
//!   debuggee has no privileged view of and no reason to be given
//! - the agent never learns that any of this happened, so nothing about a `.by`
//!   program's `sys.modules` or `sys.path` differs from a bare run of the same
//!   generated python. what a program can tell about being debugged stays at
//!   nothing, which is what `crates/bpd/tests/launch_parity.rs` is the guard on
//!
//! the invariant the whole module serves: **every answer about a translated
//! breakpoint is mapped back before a client sees it**. the agent answers about
//! a file in a temporary build directory, and a client that was shown that
//! answer raw would be reading a line number of a file it never wrote. so the
//! translation is recorded per breakpoint id in [`Translated`], and [`restore`]
//! is applied to every `Resolved` that leaves — the answer to a set, and the
//! rebindings that arrive unprompted when loading a file changes one

use std::collections::BTreeMap;
use std::path::Path;

use bpd_core::source_map::{Located, SourceMap};
use bpd_core::{Binding, Resolved, SourceBreakpoint, Unbound};

/// the extension a basedpython source file has
///
/// the discriminator is the extension rather than "the map has heard of it",
/// and that is deliberate. a `.by` the map says nothing about is a file the
/// interpreter is never going to run, and letting it through to the agent would
/// earn it "the interpreter has not loaded any code from this file. it will bind
/// if that file is imported later" — a sentence that is true of a `.py` and
/// describes something that can never happen for a `.by`
const SOURCE_EXTENSION: &str = "by";

/// where one breakpoint really went, kept so its answer can be mapped back
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Translated {
    /// the `.by` file the client named, spelled the way the client spelled it
    file: std::path::PathBuf,
    /// the line of it the client asked for
    requested: u32,
    /// the generated location it was sent to the agent as
    generated: Located,
}

/// what a breakpoint set became, and what it takes to read the answer
#[derive(Debug, Default)]
pub(crate) struct Sent {
    /// the set as the agent should receive it
    pub(crate) breakpoints: Vec<SourceBreakpoint>,
    /// the answers that were decided here, because the map refused them
    ///
    /// these never reach the agent. a breakpoint the map cannot place has no
    /// line to arm and asking about one would be asking about a location that
    /// does not exist
    pub(crate) refused: Vec<Resolved>,
    /// where each translated breakpoint went, by its client id
    pub(crate) translated: BTreeMap<u32, Translated>,
    /// the ids of the set, in the order the client asked about them
    ///
    /// a breakpoint the map refused never reaches the agent, so the agent's
    /// answers are a subset in its own order and the refusals have to be put
    /// back among them. a client that reads an answer by position — every DAP
    /// client does, because `setBreakpoints` answers an array — would otherwise
    /// read the answer to one breakpoint as the answer to another
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

/// whether a file is basedpython source, which only a map can place
pub(crate) fn is_source(file: &Path) -> bool {
    file.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(SOURCE_EXTENSION))
}

/// translate a whole breakpoint set for the agent
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
        match map.to_generated(&breakpoint.file, breakpoint.line) {
            Ok(generated) => {
                sent.translated.insert(
                    breakpoint.id,
                    Translated {
                        file: breakpoint.file.clone(),
                        requested: breakpoint.line,
                        generated: generated.clone(),
                    },
                );
                sent.breakpoints.push(SourceBreakpoint {
                    file: generated.file,
                    line: generated.line,
                    ..breakpoint
                });
            }
            Err(reason) => sent
                .refused
                .push(unbound(breakpoint.id, Unbound::Unmappable { reason })),
        }
    }
    sent
}

/// map every answer about a translated breakpoint back into `.by` terms
///
/// answers about breakpoints that were never translated pass through unchanged,
/// which is what makes this safe to apply to every `Resolved` that leaves the
/// engine rather than only to the ones a caller remembered to route through it
pub(crate) fn restore(
    map: Option<&SourceMap>,
    translated: &BTreeMap<u32, Translated>,
    resolved: Vec<Resolved>,
) -> Vec<Resolved> {
    resolved
        .into_iter()
        .map(|answer| {
            let Some(went) = translated.get(&answer.id) else {
                return answer;
            };
            let Some(map) = map else {
                unreachable!(
                    "breakpoint {} was translated, and a translation is only \
                     made when there is a map to make it with",
                    answer.id
                )
            };
            Resolved {
                id: answer.id,
                binding: back(map, went, answer.binding),
                // carried through the translation rather than rebuilt. mapping
                // changes which file and line a binding is reported in and
                // nothing about whether the interpreter is watching it yet
                waiting_for: answer.waiting_for,
            }
        })
        .collect()
}

/// one binding, in the terms the client asked its question in
fn back(map: &SourceMap, went: &Translated, binding: Binding) -> Binding {
    match binding {
        Binding::Bound {
            line,
            sites,
            evaluation,
        } => {
            // the line the agent bound is not always the line it was asked for:
            // a request on a line the interpreter cannot stop on moves to the
            // next one it can. so the answer is mapped rather than the request
            // being assumed to have survived, and a generated line the map
            // marks as having no source is a refusal here — the breakpoint is
            // armed somewhere the user never wrote, and saying which `.by` line
            // that is would mean inventing one
            match map.to_source(&went.generated.file, line) {
                Ok(source) => Binding::BoundInSource {
                    line: source.line,
                    generated: Located {
                        file: went.generated.file.clone(),
                        line,
                    },
                    sites,
                    evaluation,
                },
                Err(reason) => unbound_binding(Unbound::Unmappable { reason }),
            }
        }
        Binding::Unbound { reason } => unbound_binding(Unbound::InGeneratedPython {
            file: went.file.clone(),
            requested: went.requested,
            generated: went.generated.clone(),
            reason: Box::new(reason),
        }),
        // a translated breakpoint names a `.py` in the build directory, and
        // django binds a template. the agent cannot have answered one with the
        // other, and if it did the thing to do is stop rather than describe a
        // template line as a `.by` line
        Binding::BoundInTemplate { line, nodes, .. } => unreachable!(
            "a breakpoint translated to `{}` was answered as django template \
             line {line} compiled to {nodes:?}",
            went.generated.file.display()
        ),
        // and it cannot come back already mapped: the agent has never heard of
        // a source map
        Binding::BoundInSource { generated, .. } => unreachable!(
            "the agent answered about `{}` with a source-mapped binding, and \
             nothing in the debuggee knows what a source map is",
            generated.file.display()
        ),
    }
}

/// a whole answer that is a refusal
fn unbound(id: u32, reason: Unbound) -> Resolved {
    Resolved {
        id,
        binding: unbound_binding(reason),
        // a breakpoint that did not bind is not waiting for one either: there
        // is nothing for the arming to arm
        waiting_for: None,
    }
}

fn unbound_binding(reason: Unbound) -> Binding {
    Binding::Unbound { reason }
}
