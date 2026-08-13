//! reporting a generated python location as the `.by` line it came from
//!
//! basedpython transpiles `.by` to `.py` and the interpreter runs the `.py`, so
//! **every** location this agent can read is a location in a file the user did
//! not write. this is where the substitution happens, and it happens here — in
//! the debuggee, at the moment a location is made — for one reason: a location
//! leaves the debugger through about thirty fields, and mapping them on the way
//! out means finding every one of them. missing one reports two different files
//! for a single location, which is worse than consistently reporting the one
//! the interpreter has
//!
//! it is the same shape [`crate::templates`] uses for django, and for the same
//! reason: what a client sees is decided once, where the fact is read
//!
//! ## what does not happen here
//!
//! **nothing decides that a map is trustworthy.** a [`MappedFile`] arrives over
//! the control connection and it only exists because `bpd` parsed
//! `_by_sourcemap.py` itself and hashed both files it describes against disk
//! before the launch went ahead. that check stays out of process on purpose: a
//! debuggee vouching for the instrument that measures it is not evidence, and a
//! breakpoint is translated before the program has run at all — an answer that
//! had to ask the debuggee would arrive after the question
//!
//! so this module holds tables and applies them. it imports nothing, reads no
//! file to install one, and puts nothing in `sys.modules` — the map is agent
//! memory, exactly as the breakpoint table is, and
//! `crates/bpd/tests/launch_parity.rs` is the guard on that
//!
//! ## the rule, which is the same rule one level up
//!
//! a map either resolves a location or it says why it cannot. there is no
//! identity fallback and no nearest-line search:
//!
//! - a file no entry is about is **not touched**. that is the standard library,
//!   a dependency, and the `_by_runner.py` shim `by run` starts — none of them
//!   is basedpython, and dressing one as `.by` would be inventing a source file
//! - a generated line the map marks `None` has no `.by` line behind it. the
//!   location stays the generated one and carries
//!   [`Mapping::InGeneratedPython`] saying so, because a temporary path in
//!   front of a user with nothing to explain it is its own kind of wrong

use std::collections::BTreeMap;
use std::sync::Mutex;

use bpd_core::source_map::{Located, MappedFile, Mapping, Unmapped};

use crate::files::{self, FileId};

/// one file of the build, with what makes it findable from a `co_filename`
#[derive(Debug)]
struct Entry {
    /// the tables, as `bpd` verified them
    file: MappedFile,
    /// the filesystem's identity for the generated python
    ///
    /// `None` when it could not be taken. `bpd` read that file to hash it a
    /// moment before this arrived, so the ordinary reason is a build directory
    /// deleted underneath a running program — and the entry is then reachable
    /// only by the exact path the map spells, which is the path the interpreter
    /// compiled it under. neither route resembles anything: one is the
    /// filesystem's own answer and the other is string equality
    identity: Option<FileId>,
}

#[derive(Debug, Default)]
struct State {
    /// the build, empty for every program that is not basedpython
    entries: Vec<Entry>,
    /// which entry a `co_filename` is, worked out once per distinct filename
    ///
    /// the event path reaches this for every location it reports, and the
    /// answer for a filename already seen is a lookup. without it every stop in
    /// a program would `stat` a file to find out it is not part of the build
    known: BTreeMap<String, Option<usize>>,
}

/// a `Mutex` and not an `RwLock` because a lookup **writes**: it remembers what
/// a filename resolved to, and an `RwLock` cannot be upgraded from a read guard
/// to a write one without dropping it first — which is a window for a second
/// thread to install something else underneath the index just worked out.
/// nothing here is on a path that has to be cheap: a location is made at a stop
static STATE: Mutex<State> = Mutex::new(State {
    entries: Vec::new(),
    known: BTreeMap::new(),
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE
        .lock()
        .expect("the source map lock is only held for map operations, which do not panic")
}

/// install the build's tables, and say how many files they cover
///
/// replaces whatever was there. a session is sent one map, at launch, before a
/// line of the program has run — and a second one would be a second idea of
/// where every location in the process is
pub(crate) fn install(files: Vec<MappedFile>) -> u32 {
    let entries: Vec<Entry> = files
        .into_iter()
        .map(|file| Entry {
            identity: files::identify(&file.generated).ok(),
            file,
        })
        .collect();

    let installed = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    let mut state = state();
    state.entries = entries;
    // the cache is about the tables that were there. a filename that resolved
    // to entry 3 of the old set is not entry 3 of this one
    state.known.clear();
    installed
}

/// a location as it should be reported, and what the map said about it
///
/// the whole answer in one value, because the three fields are decided together
/// and a caller that took two of them would be the inconsistency this exists to
/// prevent
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reported {
    /// the file to report, which is the `.by` when there is one
    pub(crate) file: String,
    /// the line of it
    pub(crate) line: u32,
    /// what the map said, or `None` when nothing in the build generated `file`
    pub(crate) mapping: Option<Mapping>,
}

/// how a location the interpreter reported should be reported to a client
///
/// the one function every site in this crate that produces a location goes
/// through. a location in a file the build did not generate comes back exactly
/// as it went in
pub(crate) fn locate(file: String, line: u32) -> Reported {
    let mut state = state();
    let Some(entry) = entry_for(&mut state, &file) else {
        return Reported {
            file,
            line,
            mapping: None,
        };
    };

    match entry.file.to_source(line) {
        Ok(source) => Reported {
            file: entry.file.source.display().to_string(),
            line: source,
            mapping: Some(Mapping::FromSource {
                generated: Located {
                    file: entry.file.generated.clone(),
                    line,
                },
            }),
        },
        // the generated location stands, and it says why. reporting a `.by`
        // line for a line the transpiler invented would be the debugger writing
        // one the user never did
        Err(reason) => Reported {
            file,
            line,
            mapping: Some(Mapping::InGeneratedPython { reason }),
        },
    }
}

/// the generated line a `.by` line of this frame's own source is
///
/// the inbound direction, and the reason it exists: once a frame reports
/// `demo.by:11`, a client asking to move that frame to another line is naming a
/// line of `demo.by`. a debugger that reported one file's lines and accepted
/// another's would be two debuggers
///
/// `None` when the frame's file is not generated python of this build, which is
/// every ordinary python frame — the line is already a line of the file the
/// frame reported
pub(crate) fn to_generated(file: &str, line: u32) -> Option<Result<u32, Unmapped>> {
    let mut state = state();
    let entry = entry_for(&mut state, file)?;
    Some(entry.file.to_generated(line))
}

/// the `.by` behind a generated file, and the digest that proves it is still it
///
/// what [`crate::source`] needs and nothing else does: showing a user the
/// source around a `.by` line means reading a file the interpreter never read,
/// so the only thing that can say it is the right file is the digest the
/// transpiler wrote and `bpd` checked
pub(crate) fn source_of(file: &str) -> Option<MappedFile> {
    let mut state = state();
    entry_for(&mut state, file).map(|entry| entry.file.clone())
}

/// which entry of the build a `co_filename` names, if any
///
/// the identity comparison is the filesystem's own — the same one a breakpoint
/// is bound by — because comparing path text is wrong in every direction a
/// symlinked or case-insensitive filesystem can be wrong in. what makes it
/// cheap is that the answer for a filename is worked out once
fn entry_for<'a>(state: &'a mut State, file: &str) -> Option<&'a Entry> {
    if state.entries.is_empty() {
        return None;
    }
    let found = match state.known.get(file) {
        Some(known) => *known,
        None => {
            let found = resolve(state, file);
            state.known.insert(file.to_string(), found);
            found
        }
    };
    found.map(|at| &state.entries[at])
}

/// look a filename up against the build, without the cache
fn resolve(state: &State, file: &str) -> Option<usize> {
    let path = std::path::Path::new(file);
    if let Some(at) = state
        .entries
        .iter()
        .position(|entry| entry.file.generated == path)
    {
        return Some(at);
    }
    // a pseudo-filename — `<string>`, a frozen module — has no identity, and
    // nothing in a build directory can be one
    let identity = files::identify(path).ok()?;
    state
        .entries
        .iter()
        .position(|entry| entry.identity.as_ref() == Some(&identity))
}
