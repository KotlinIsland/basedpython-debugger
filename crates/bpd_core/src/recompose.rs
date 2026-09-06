//! why the program's ui recomposed — the trace ring `basedpython_ui.runtime`
//! keeps, as `bpd` reads it
//!
//! the runtime of basedpython-ui keeps a bounded record of why every scope
//! ran, what every state write did and what every frame cost, and announces
//! every record it appends as an audit event. `bpd` reads the ring at a stop and
//! forwards the announcements while a client is watching. nothing here is
//! inferred: every field is a slot of a tuple the runtime wrote, read by the
//! layout the runtime's own documentation fixes under [`TRACE_FORMAT`], and a
//! slot that does not read by that layout refuses the whole answer by name
//!
//! what is **added** on the way through is the mapping. the runtime records
//! the generated python it runs; every location here goes through the same
//! source map a stack frame goes through, so a `.by` build reports its `.by`
//! lines and carries the generated location beside them — see [`Location`]
//!
//! the value slots — `old` and `new` of a state cause, of an args cause and of
//! a derived cause — are whatever the program stored, and they are the one
//! place a reader could run the program to describe it. they are rendered by
//! exact type instead, without calling anything of the object's: an exact
//! builtin scalar as itself, an exact builtin container as its kind and size,
//! anything else as `a <Type>`. that is the trail's rule, and it is the weaker
//! answer that cannot be wrong

use std::ffi::CStr;
use std::fmt;

use crate::frame::Kept;
use crate::source_map::{Located, Mapping, Unmapped};
use crate::stop::Mode;

/// the trace format this `bpd` reads
///
/// the runtime writes its own under the same name, and a reader compares the
/// two before reading anything else. a change to any record layout bumps it, so
/// a mismatch is refused by name rather than read by a layout that no longer
/// holds — see [`crate::Refusal::UiTraceFormat`]
pub const TRACE_FORMAT: u32 = 1;

/// the audit event the runtime raises for every record it appends
///
/// raised with the record tuple as the one argument, on the thread that
/// appended it. the agent's native audit hook recognises it beside the
/// process-making events — see [`crate::audit::watched`]
pub const TRACE_EVENT: &CStr = c"basedpython_ui.trace";

/// how many records one answer carries at most, newest kept
///
/// the runtime's own ring defaults to the same number, so an ordinary answer is
/// whole. a program that raised its own limit is answered with the newest this
/// many and a count of the rest — the count is in [`Recompositions::records`],
/// beside the runtime's own
pub const RECORDS_KEPT: usize = 4096;

/// the trace ring, as it stood when a held thread read it
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Recompositions {
    /// the runtime's `TRACE_FORMAT`, which is [`TRACE_FORMAT`] or the answer
    /// was refused
    pub format: u32,
    /// how many runtimes `live_runtimes` held
    ///
    /// the records of every one of them are here, concatenated in that order,
    /// each carrying the index of its own. a program ordinarily has one
    pub runtimes: u32,
    /// whether any runtime was tracing
    ///
    /// false only when there was no runtime to trace: a program that imported
    /// the runtime module and made nothing yet. a runtime that exists with
    /// tracing off is a refusal rather than an empty answer — see
    /// [`crate::Refusal::UiTracingOff`]
    pub tracing: bool,
    /// the records, oldest first, and how many are not here
    ///
    /// `dropped` is the runtime's own count of what fell off the front of its
    /// ring plus whatever this answer left out to stay under [`RECORDS_KEPT`].
    /// an answer whose `dropped` is above zero does not begin where the trace
    /// did
    pub records: Kept<TraceRecord>,
    /// how the program was moving while this was read
    pub mode: Mode,
}

/// a record the ui runtime wrote while a client watched, as the stream hands
/// it on
///
/// the agent forwards a record from a bounded queue that a thread of its own
/// drains, so the program's ui thread never waits on the connection. when the
/// queue is full the oldest record in it is dropped and counted, and the count
/// rides the next record that gets through as `dropped_before`: a client
/// reading one above zero has a gap in the stream before this record, and one
/// reading zero has none — see [`crate::Reporting::recomposed`]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Recomposed {
    /// the record, read by the layout of its kind
    pub record: TraceRecord,
    /// how many records the agent dropped unsent since the one before this
    ///
    /// zero is the ordinary case and means the stream is whole up to here
    pub dropped_before: u64,
}

/// one record of the trace ring
///
/// the first element of every tuple the runtime writes is its kind, and this is
/// that kind with the rest of the tuple read by the layout of it. deliberately
/// closed: a kind the layout does not have is a format the runtime's own
/// documentation bumps [`TRACE_FORMAT`] for, and it is refused as one
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum TraceRecord {
    /// a scope ran
    Run {
        /// which of `live_runtimes` wrote it
        runtime: u32,
        /// the runtime's frame counter when the run started
        frame: u64,
        /// the scope's id; the root is 0
        scope: u64,
        /// the parent scope's id, or `None` for the root
        parent: Option<u64>,
        /// the composable's `__name__`, or `root` for the root scope
        name: String,
        /// where the composable is defined
        defined: Location,
        /// the call site in the parent, or `None` for the root
        called: Option<Location>,
        /// the enclosing `key(...)` value, when there is one
        key: Option<Key>,
        /// why the run happened at all — see [`Origin`]
        origin: Origin,
        /// why it ran, never empty
        causes: Vec<Cause>,
        /// ids of child scopes this run emitted as references instead of
        /// running
        skipped: Vec<u64>,
        /// children disposed after this run because the run did not reach them
        disposed: Vec<Disposed>,
        /// wall time of the body, in nanoseconds
        elapsed_ns: u64,
    },

    /// a state write happened, or a derived recomputed
    ///
    /// one per public mutator call that changed something, and one per derived
    /// recompute. a write whose cause has `readers` of zero is the record that
    /// says nothing depended on the cell
    Write {
        /// which of `live_runtimes` wrote it
        runtime: u32,
        /// the runtime's frame counter when it happened
        frame: u64,
        /// the state or derived cause, whole
        cause: Cause,
    },

    /// a frame finished, having run at least one scope
    Frame {
        /// which of `live_runtimes` wrote it
        runtime: u32,
        /// the frame that finished
        frame: u64,
        /// how many scopes ran
        runs: u64,
        /// how many scopes were emitted as references instead
        skips: u64,
        /// how long composition took, in nanoseconds
        compose_ns: u64,
        /// how long the commit took, in nanoseconds
        commit_ns: u64,
    },

    /// a scope raised
    Error {
        /// which of `live_runtimes` wrote it
        runtime: u32,
        /// the runtime's frame counter when it happened
        frame: u64,
        /// the scope that raised
        scope: u64,
        /// its composable's `__name__`
        name: String,
        /// `str(exception)`
        error: String,
        /// whether a committed subtree stayed on screen
        kept_previous: bool,
    },

    /// a write during composition was refused
    Refused {
        /// which of `live_runtimes` wrote it
        runtime: u32,
        /// the runtime's frame counter when it happened
        frame: u64,
        /// the scope that was composing
        scope: u64,
        /// its composable's `__name__`
        name: String,
        /// the cell kind and op that were refused
        what: String,
    },
}

impl TraceRecord {
    /// which of `live_runtimes` wrote this record
    pub const fn runtime(&self) -> u32 {
        match self {
            Self::Run { runtime, .. }
            | Self::Write { runtime, .. }
            | Self::Frame { runtime, .. }
            | Self::Error { runtime, .. }
            | Self::Refused { runtime, .. } => *runtime,
        }
    }

    /// what to call this kind of record in a message about it
    ///
    /// the same word the wire carries under `record`, so a refusal naming a
    /// slot names the kind a reader will find it under
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Run { .. } => "run",
            Self::Write { .. } => "write",
            Self::Frame { .. } => "frame",
            Self::Error { .. } => "error",
            Self::Refused { .. } => "refused",
        }
    }
}

impl fmt::Display for TraceRecord {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Run {
                frame,
                scope,
                name,
                defined,
                called,
                key,
                origin,
                causes,
                skipped,
                disposed,
                elapsed_ns,
                ..
            } => {
                write!(out, "frame {frame}: `{name}` (scope {scope}")?;
                if let Some(key) = key {
                    write!(out, ", key {key}")?;
                }
                write!(out, ") ran {origin} because ")?;
                for (index, cause) in causes.iter().enumerate() {
                    if index > 0 {
                        out.write_str("; ")?;
                    }
                    write!(out, "{cause}")?;
                }
                write!(out, ". defined at {defined}")?;
                match called {
                    Some(called) => write!(out, ", called from {called}")?,
                    None => out.write_str(", the root")?,
                }
                write!(
                    out,
                    ". {} child(ren) skipped, {} disposed, {elapsed_ns} ns",
                    skipped.len(),
                    disposed.len()
                )
            }
            Self::Write { frame, cause, .. } => write!(out, "frame {frame}: {cause}"),
            Self::Frame {
                frame,
                runs,
                skips,
                compose_ns,
                commit_ns,
                ..
            } => write!(
                out,
                "frame {frame} finished: {runs} run(s), {skips} skip(s), composed \
                 in {compose_ns} ns and committed in {commit_ns} ns"
            ),
            Self::Error {
                frame,
                scope,
                name,
                error,
                kept_previous,
                ..
            } => write!(
                out,
                "frame {frame}: `{name}` (scope {scope}) raised {error}, and {}",
                if *kept_previous {
                    "its committed subtree stayed on screen"
                } else {
                    "nothing of it stayed on screen"
                }
            ),
            Self::Refused {
                frame,
                scope,
                name,
                what,
                ..
            } => write!(
                out,
                "frame {frame}: `{name}` (scope {scope}) was refused a {what} \
                 while composing"
            ),
        }
    }
}

/// why a scope's run happened at all
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// created this run
    First,
    /// popped from the dirty heap
    #[serde(rename = "self")]
    Itself,
    /// its parent ran it
    Parent,
}

impl fmt::Display for Origin {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::First => "for the first time",
            Self::Itself => "on its own, off the dirty heap",
            Self::Parent => "because its parent ran it",
        })
    }
}

/// a `key(...)` value or the index or key an op touched — an integer or text
///
/// the runtime refuses any other type in `key(...)`, and writes a dict key of
/// any other type as its `repr` before it reaches the `at` slot of a state
/// cause, so neither slot ever holds a program object
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Key {
    /// an integer
    Int(i64),
    /// a string
    Text(String),
}

impl fmt::Display for Key {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(number) => write!(out, "{number}"),
            Self::Text(text) => write!(out, "{text:?}"),
        }
    }
}

/// a child disposed after a run because the run did not reach it
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Disposed {
    /// the child's scope id
    pub scope: u64,
    /// its composable's `__name__`
    pub name: String,
    /// the `key(...)` value it was made under, when there was one
    pub key: Option<Key>,
}

/// a location of the program, as every frame `bpd` reports one
///
/// `file` and `line` are the `.by` location when the build's source map covers
/// the generated file, and the generated location itself otherwise. `generated`
/// is the location the interpreter ran — the file and line the runtime wrote —
/// and is `None` only when `file` already is it, because nothing mapped it. a
/// generated line the map marks as having no `.by` line behind it keeps the
/// generated location in `file` and `line`, carries it in `generated` as well,
/// and says why in `reason`: the same three-way vocabulary a stack frame's
/// [`crate::Mapping`] carries
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Location {
    /// the file to show, which is the `.by` when there is one
    pub file: String,
    /// the line of it, counting from one
    pub line: u32,
    /// where the interpreter really was, when the map put a `.by` line in front
    /// of it or said there was none to put
    pub generated: Option<Located>,
    /// what the map said when it covered the file and had no `.by` line for
    /// that line of it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Unmapped>,
}

impl Location {
    /// a location as the map reported it, in the three-way vocabulary a frame
    /// uses
    ///
    /// `file` and `line` are what the map said to show, and `mapping` is what
    /// it said about them — `None` when nothing in the build generated the
    /// file. it is built here, in the crate that defines [`Mapping`], because
    /// that enum is closed to everyone else and a reader that had to add a
    /// catch-all arm to unpack it would be a reader that silently mis-filed a
    /// mapping added later
    #[must_use]
    pub fn mapped(file: String, line: u32, mapping: Option<Mapping>) -> Self {
        match mapping {
            None => Self {
                file,
                line,
                generated: None,
                reason: None,
            },
            Some(Mapping::FromSource { generated }) => Self {
                file,
                line,
                generated: Some(generated),
                reason: None,
            },
            // the generated location stands, and it is carried under
            // `generated` as well: that field is always where the interpreter
            // was, and here that is the same place `file` names
            Some(Mapping::InGeneratedPython { reason }) => Self {
                generated: Some(Located {
                    file: std::path::PathBuf::from(&file),
                    line,
                }),
                file,
                line,
                reason: Some(reason),
            },
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}:{}", self.file, self.line)?;
        if let Some(generated) = &self.generated
            && (generated.file.display().to_string() != self.file || generated.line != self.line)
        {
            write!(
                out,
                " (generated {}:{})",
                generated.file.display(),
                generated.line
            )?;
        }
        Ok(())
    }
}

/// why a scope ran
///
/// what the runtime recorded, one entry per reason, in the order they happened.
/// a scope popped from the heap carries every state and derived cause recorded
/// for it since its last run, so a handler that wrote three cells is three
/// causes on one run
///
/// a key change has no cause of its own, because whether an old key was really
/// given up is known only when the parent's run ends: the new scope carries
/// [`Self::Created`], and the parent's run record names the old key under
/// [`TraceRecord::Run`]'s `disposed`
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum Cause {
    /// first composition of a new scope
    Created,

    /// a cell the scope read changed
    State {
        /// `id()` of the cell
        cell: u64,
        /// `state`, `list`, `dict` or `ambient`
        kind: String,
        /// the public mutator that changed it: `set`, `append`, `insert`,
        /// `remove`, `pop`, `clear`, `put`, `delete` or `provide`
        op: String,
        /// the index or key the op touched, or `None` for a whole-cell write
        at: Option<Key>,
        /// the value before, rendered without running the program
        old: String,
        /// the value after, rendered the same way
        new: String,
        /// where the cell was created, or `None` for one created outside
        /// composition
        declared: Option<Location>,
        /// the name the cell was bound to at that site, when a store followed
        /// the call
        declared_name: Option<String>,
        /// the frame that called the public mutator
        written: Location,
        /// `threading.get_ident()` of the writer
        thread: u64,
        /// whether the write came from another thread and was applied at the
        /// next frame
        posted: bool,
        /// how many trackers were notified; zero means nothing depended on it
        readers: u64,
    },

    /// a derived the scope read recomputed to a different value
    Derived {
        /// `id()` of the derived
        derived: u64,
        /// where it was created, or `None` for one created outside composition
        declared: Option<Location>,
        /// the name it was bound to, when a store followed the call
        declared_name: Option<String>,
        /// the value before, rendered without running the program
        old: String,
        /// the value after, rendered the same way
        new: String,
        /// whether the value compared unequal and readers were invalidated
        changed: bool,
        /// the state or derived cause that made it recompute
        because: Box<Cause>,
    },

    /// `Runtime.invalidate` was called with no cause
    Invalidated,

    /// the parent ran and this argument differed
    Args {
        /// the parameter's name
        parameter: String,
        /// the value before, rendered without running the program
        old: String,
        /// the value after, rendered the same way
        new: String,
        /// false when the argument's type is unstable and it was never compared
        compared: bool,
    },

    /// the scope takes a content block, so it re-runs whenever its parent runs
    Inline,

    /// the previous run raised
    Recovery {
        /// `str()` of what it raised
        error: String,
    },

    /// the scope had not been committed yet
    Uncommitted,

    /// the scope was already dirty when its parent reached it
    Dirty {
        /// the causes that made it dirty
        causes: Vec<Cause>,
    },
}

impl Cause {
    /// what to call this kind of cause in a message about it
    ///
    /// the same word the wire carries under `cause`
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::State { .. } => "state",
            Self::Derived { .. } => "derived",
            Self::Invalidated => "invalidated",
            Self::Args { .. } => "args",
            Self::Inline => "inline",
            Self::Recovery { .. } => "recovery",
            Self::Uncommitted => "uncommitted",
            Self::Dirty { .. } => "dirty",
        }
    }
}

/// how a cell is named in a sentence: by the name it was bound to, or by what
/// it is when nothing bound it
fn cell_called(declared_name: Option<&str>, what: &str) -> String {
    declared_name.map_or_else(
        || format!("a {what} created outside composition"),
        |name| format!("`{name}`"),
    )
}

impl fmt::Display for Cause {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => out.write_str("it was created"),
            Self::State {
                kind,
                op,
                at,
                old,
                new,
                declared_name,
                written,
                thread,
                posted,
                readers,
                ..
            } => {
                write!(
                    out,
                    "{} ({kind} {op}",
                    cell_called(declared_name.as_deref(), kind)
                )?;
                if let Some(at) = at {
                    write!(out, " at {at}")?;
                }
                write!(
                    out,
                    ") changed from {old} to {new} at {written}, notifying {readers} reader(s)"
                )?;
                if *posted {
                    write!(out, ", posted from thread {thread}")?;
                }
                Ok(())
            }
            Self::Derived {
                declared_name,
                old,
                new,
                changed,
                because,
                ..
            } => write!(
                out,
                "{} recomputed from {old} to {new}{} because {because}",
                cell_called(declared_name.as_deref(), "derived"),
                if *changed {
                    ""
                } else {
                    " (equal, so nothing was invalidated)"
                }
            ),
            Self::Invalidated => out.write_str("it was invalidated by hand"),
            Self::Args {
                parameter,
                old,
                new,
                compared: true,
            } => write!(out, "argument `{parameter}` changed from {old} to {new}"),
            Self::Args {
                parameter,
                old,
                new,
                compared: false,
            } => write!(
                out,
                "argument `{parameter}` went from {old} to {new} and its type is \
                 unstable, so the two were never compared"
            ),
            Self::Inline => {
                out.write_str("it takes a content block, so it runs whenever its parent does")
            }
            Self::Recovery { error } => write!(out, "its previous run raised {error}"),
            Self::Uncommitted => out.write_str("it had not been committed yet"),
            Self::Dirty { causes } => {
                out.write_str("it was already dirty when its parent reached it")?;
                if !causes.is_empty() {
                    out.write_str(": ")?;
                    for (index, cause) in causes.iter().enumerate() {
                        if index > 0 {
                            out.write_str("; ")?;
                        }
                        write!(out, "{cause}")?;
                    }
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(file: &str, line: u32) -> Location {
        Location {
            file: file.to_string(),
            line,
            generated: None,
            reason: None,
        }
    }

    fn state_cause() -> Cause {
        Cause::State {
            cell: 4401,
            kind: "state".to_string(),
            op: "set".to_string(),
            at: None,
            old: "0".to_string(),
            new: "2".to_string(),
            declared: Some(at("/app/counter.by", 12)),
            declared_name: Some("count".to_string()),
            written: at("/app/counter.by", 14),
            thread: 8674,
            posted: false,
            readers: 1,
        }
    }

    #[test]
    fn a_record_is_tagged_by_its_kind_and_a_cause_by_its_kind() {
        // the wire's own words, which is what a reader that goes field by field
        // switches on. a tag spelled differently from `kind()` would be a
        // refusal naming a slot under a word no reader can find
        let record = TraceRecord::Write {
            runtime: 0,
            frame: 3,
            cause: state_cause(),
        };
        let json = serde_json::to_value(&record).expect("serde is derived");
        assert_eq!(json["record"], record.kind());
        assert_eq!(json["cause"]["cause"], state_cause().kind());
        assert_eq!(json["cause"]["declared"]["file"], "/app/counter.by");
        assert_eq!(
            json["cause"]["declared"]["generated"],
            serde_json::Value::Null
        );
        assert!(
            json["cause"]["declared"].get("reason").is_none(),
            "a location nothing mapped carries no reason: {json}"
        );

        let back: TraceRecord = serde_json::from_value(json).expect("it round trips");
        assert_eq!(back, record);
    }

    #[test]
    fn a_key_is_carried_as_the_integer_or_the_text_it_is() {
        // a key change is `created` on the new scope and the old key under
        // the parent's `disposed` — there is no cause of its own for it, so
        // the two places a key appears are the record's own and a disposal's
        let run = TraceRecord::Run {
            runtime: 0,
            frame: 1,
            scope: 5,
            parent: Some(0),
            name: "Row".to_string(),
            defined: at("/app/rows.by", 9),
            called: Some(at("/app/rows.by", 24)),
            key: Some(Key::Text("second".to_string())),
            origin: Origin::First,
            causes: vec![Cause::Created],
            skipped: vec![7, 8],
            disposed: vec![Disposed {
                scope: 9,
                name: "Row".to_string(),
                key: Some(Key::Int(2)),
            }],
            elapsed_ns: 12_345,
        };
        let json = serde_json::to_value(&run).expect("serde is derived");
        assert_eq!(json["key"], "second");
        assert_eq!(json["origin"], "first");
        assert_eq!(json["causes"][0]["cause"], "created");
        assert_eq!(json["disposed"][0]["key"], 2);
        let back: TraceRecord = serde_json::from_value(json).expect("it round trips");
        assert_eq!(back, run);
    }

    #[test]
    fn a_forwarded_record_carries_what_the_stream_dropped_before_it() {
        // the count is a field of the report rather than a separate report,
        // so a reader that has the record has the gap in front of it
        let forwarded = Recomposed {
            record: TraceRecord::Frame {
                runtime: 0,
                frame: 9,
                runs: 1,
                skips: 0,
                compose_ns: 10,
                commit_ns: 20,
            },
            dropped_before: 3,
        };
        let json = serde_json::to_value(&forwarded).expect("serde is derived");
        assert_eq!(json["dropped_before"], 3);
        assert_eq!(json["record"]["record"], "frame");
        let back: Recomposed = serde_json::from_value(json).expect("it round trips");
        assert_eq!(back, forwarded);
    }

    #[test]
    fn every_kind_of_cause_says_why_in_words_a_person_can_act_on() {
        let causes = [
            (Cause::Created, "created"),
            (state_cause(), "`count` (state set) changed from 0 to 2"),
            (
                Cause::Derived {
                    derived: 77,
                    declared: None,
                    declared_name: Some("total".to_string()),
                    old: "1".to_string(),
                    new: "2".to_string(),
                    changed: true,
                    because: Box::new(state_cause()),
                },
                "`total` recomputed from 1 to 2 because `count`",
            ),
            (Cause::Invalidated, "by hand"),
            (
                Cause::Args {
                    parameter: "step".to_string(),
                    old: "1".to_string(),
                    new: "2".to_string(),
                    compared: false,
                },
                "never compared",
            ),
            (Cause::Inline, "content block"),
            (
                Cause::Recovery {
                    error: "boom".to_string(),
                },
                "raised boom",
            ),
            (Cause::Uncommitted, "not been committed"),
            (
                Cause::Dirty {
                    causes: vec![Cause::Created],
                },
                "already dirty when its parent reached it: it was created",
            ),
        ];
        for (cause, wanted) in causes {
            let said = cause.to_string();
            assert!(said.contains(wanted), "expected {wanted:?} in {said:?}");
        }
    }

    #[test]
    fn a_state_cause_names_a_cell_nothing_bound_by_what_it_is() {
        // a cell created outside composition has no name to give, and a
        // sentence with a hole in it is a sentence about nothing
        let Cause::State {
            cell,
            kind,
            op,
            at,
            old,
            new,
            written,
            thread,
            posted,
            readers,
            ..
        } = state_cause()
        else {
            unreachable!("that is a state cause")
        };
        let unnamed = Cause::State {
            cell,
            kind,
            op,
            at,
            old,
            new,
            declared: None,
            declared_name: None,
            written,
            thread,
            posted,
            readers,
        };
        let said = unnamed.to_string();
        assert!(
            said.contains("a state created outside composition"),
            "it said {said}"
        );
    }

    #[test]
    fn a_run_record_reads_as_one_sentence_naming_the_scope_and_every_cause() {
        let run = TraceRecord::Run {
            runtime: 0,
            frame: 3,
            scope: 5,
            parent: Some(0),
            name: "Counter".to_string(),
            defined: Location {
                file: "/app/counter.by".to_string(),
                line: 9,
                generated: Some(Located {
                    file: std::path::PathBuf::from("/tmp/build/counter.py"),
                    line: 41,
                }),
                reason: None,
            },
            called: None,
            key: None,
            origin: Origin::Itself,
            causes: vec![state_cause(), Cause::Inline],
            skipped: Vec::new(),
            disposed: Vec::new(),
            elapsed_ns: 100,
        };
        let said = run.to_string();
        for wanted in [
            "frame 3",
            "`Counter`",
            "scope 5",
            "off the dirty heap",
            "`count` (state set) changed from 0 to 2",
            "content block",
            "/app/counter.by:9 (generated /tmp/build/counter.py:41)",
            "the root",
            "100 ns",
        ] {
            assert!(said.contains(wanted), "expected {wanted:?} in {said:?}");
        }
    }
}
