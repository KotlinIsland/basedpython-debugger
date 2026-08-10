//! answering a whole state query, and comparing two of the answers
//!
//! the query is **composed of the requests the tree walk is made of** — a stack
//! walk, a scope read, an evaluation, a source read — so a client that asks the
//! declarative way and one that walks the tree cannot be told different things
//! about a value. what the query removes is the round trips, not the machinery
//!
//! ## the budget is one budget
//!
//! `detail.budget` bounds the **whole** query rather than each read inside it. a
//! query of twenty parts under eight kilobytes spends eight kilobytes, not a
//! hundred and sixty, and the parts that did not fit are named in `left_out`
//! rather than being quietly absent
//!
//! it is checked before each part and charged after, so one part can carry the
//! total past it — by at most that part, whose own bound is the same `budget`.
//! that is the same rule the debug script's byte budget follows, and `bytes` on
//! the answer says what was really spent either way
//!
//! ## the order parts are read in
//!
//! the stack first, because it is what the rest is addressed by. then the
//! expressions, because they are what the client asked for by name. then, frame
//! by frame, the source and the scopes — the scopes last because they are the
//! open ended part, and a scope of a module namespace can spend a whole budget
//! on its own

use bpd_core::{
    Answer, Detail, Difference, FrameState, NotRead, Omitted, QueryPart, Reporting, ScopeState,
    Snapshot, SnapshotId, Source, State, StateQuery, Wanted,
};
use bpd_protocol::message::{FromAgent, FromEngine};
use sha2::{Digest as _, Sha256};

use super::{Debuggee, unexpected};
use crate::Result;

/// how many hex characters of the digest name a state
///
/// sixteen bytes of sha-256. it is content addressing rather than a checksum:
/// two states that differ anywhere have different ids, and reading the same
/// state twice gives the same id back
const DIGEST: usize = 32;

impl Debuggee {
    /// describe one stop's state, and keep the answer under an id
    pub(super) fn describe(
        &mut self,
        stop: u64,
        query: &StateQuery,
        reporting: &mut dyn Reporting,
    ) -> Result<Snapshot> {
        let state = self.read_state(stop, query, reporting)?;
        Ok(self.keep(state))
    }

    /// what changed between two states this session read
    pub(super) fn compare(&self, before: &SnapshotId, after: &SnapshotId) -> Result<Difference> {
        let before = self.snapshot(before)?;
        let after = self.snapshot(after)?;
        let held: Vec<u64> = self.held.iter().map(|stop| stop.stop).collect();
        Ok(bpd_core::difference(before, after, &held))
    }

    /// the state kept under one id, or the refusal that names what is kept
    fn snapshot(&self, id: &SnapshotId) -> Result<&Snapshot> {
        self.snapshots
            .iter()
            .find(|snapshot| &snapshot.id == id)
            .ok_or_else(|| {
                bpd_core::Error::NoSuchSnapshot {
                    id: id.clone(),
                    held: self
                        .snapshots
                        .iter()
                        .map(|snapshot| snapshot.id.to_string())
                        .collect(),
                }
                .into()
            })
    }

    /// keep a state under the digest of itself
    ///
    /// nothing evicts one. an id that resolved yesterday and not today is
    /// exactly the stale handle this is not, and what one costs is bounded by
    /// the byte budget the query it came from carried
    fn keep(&mut self, state: State) -> Snapshot {
        let id = SnapshotId {
            stop: state.stop,
            digest: digest_of(&state),
        };
        let snapshot = Snapshot { id, state };
        // the same state read twice is the same id, so it is already kept. two
        // entries would be one value under one name, counted twice
        if !self.snapshots.iter().any(|kept| kept.id == snapshot.id) {
            self.snapshots.push(snapshot.clone());
        }
        snapshot
    }

    fn read_state(
        &mut self,
        stop: u64,
        query: &StateQuery,
        reporting: &mut dyn Reporting,
    ) -> Result<State> {
        // always walked, even for no frames at all: it is what says how deep the
        // stack is and what mode the answer was read in, and neither of those
        // can be assumed
        let walked = self.walk_stack(stop, Some(query.frames), reporting)?;
        let held = self
            .held
            .iter()
            .find(|held| held.stop == stop)
            .expect("the stack walk answered, so this stop is held")
            .clone();

        let mut reading = Reading {
            budget: query.detail.budget,
            spent: 0,
            left_out: Vec::new(),
        };
        reading.charge(&walked.frames);

        let mut values = Vec::with_capacity(query.expressions.len());
        for wanted in &query.expressions {
            let part = QueryPart::Expression {
                frame: wanted.frame,
                expression: wanted.expression.clone(),
            };
            if reading.gone(part) {
                continue;
            }
            let answer = self.answer(stop, wanted, query.detail, reporting)?;
            reading.charge(&answer);
            values.push(answer);
        }

        let mut frames = Vec::with_capacity(walked.frames.len());
        for frame in walked.frames {
            let depth = frame.id.depth;
            let source = match query.source {
                Some(around) if !reading.gone(QueryPart::Source { frame: depth }) => {
                    let read = self.read_source(frame.id, around, reporting)?;
                    reading.charge(&read);
                    Some(read)
                }
                _ => None,
            };

            let mut scopes = Vec::with_capacity(query.scopes.len());
            for scope in &query.scopes {
                let part = QueryPart::Scope {
                    frame: depth,
                    scope: *scope,
                };
                if reading.gone(part) {
                    continue;
                }
                let read = self.read_scope(frame.id, *scope, query.detail, reporting)?;
                let read = ScopeState {
                    scope: *scope,
                    entries: read.entries,
                    unbound: read.unbound,
                    unreadable: read.unreadable,
                    omitted: read.omitted,
                };
                reading.charge(&read);
                scopes.push(read);
            }

            frames.push(FrameState {
                frame,
                source,
                scopes,
            });
        }

        Ok(State {
            stop,
            thread: held.thread,
            reason: held.reason,
            frames,
            depth: walked.depth,
            values,
            left_out: reading.left_out,
            mode: walked.mode,
            bytes: reading.spent,
        })
    }

    /// evaluate one of a query's expressions
    fn answer(
        &mut self,
        stop: u64,
        wanted: &Wanted,
        detail: Detail,
        reporting: &mut dyn Reporting,
    ) -> Result<Answer> {
        let frame = bpd_core::FrameId {
            stop,
            depth: wanted.frame,
        };
        let result = self.evaluate_in(frame, &wanted.expression, detail, reporting)?;
        Ok(Answer {
            expression: wanted.expression.clone(),
            frame: wanted.frame,
            result,
        })
    }

    /// the source around one frame's line, read and checked in the debuggee
    pub(super) fn read_source(
        &mut self,
        frame: bpd_core::FrameId,
        around: u32,
        reporting: &mut dyn Reporting,
    ) -> Result<Source> {
        const EXPECTED: &str = "the source around a frame's line";

        let request = FromEngine::Source { frame, around };
        match self.ask(&request, EXPECTED, reporting)? {
            FromAgent::Source { source } => Ok(source),
            other => Err(unexpected(&other, EXPECTED)),
        }
    }
}

/// one query, part way through its budget
struct Reading {
    budget: u32,
    spent: u64,
    left_out: Vec<NotRead>,
}

impl Reading {
    /// whether the budget is gone, writing down what this part cost nobody
    ///
    /// checked **before** a part is read rather than after, so a part that is
    /// not here was not read at all — which is what `left_out` claims about it
    fn gone(&mut self, part: QueryPart) -> bool {
        if self.spent < u64::from(self.budget) {
            return false;
        }
        self.left_out.push(NotRead {
            part,
            why: Omitted::Budget { limit: self.budget },
        });
        true
    }

    /// charge what one part of the answer cost
    ///
    /// json, because that is what both front ends render and what spends the
    /// context window the budget exists to protect. it is the same unit the
    /// debug script's byte budget uses, for the same reason
    fn charge<T: serde::Serialize>(&mut self, part: &T) {
        let size = serde_json::to_vec(part)
            .expect("a query's parts are built from types whose serde is derived and cannot fail")
            .len();
        self.spent += size as u64;
    }
}

/// the digest a state is named by
fn digest_of(state: &State) -> String {
    let encoded = serde_json::to_vec(state)
        .expect("a state is built from types whose serde is derived and cannot fail");
    let digest = Sha256::digest(&encoded);
    let mut hex = String::with_capacity(DIGEST);
    for byte in digest.iter().take(DIGEST / 2) {
        use std::fmt::Write as _;
        write!(hex, "{byte:02x}").expect("writing to a string cannot fail");
    }
    hex
}
