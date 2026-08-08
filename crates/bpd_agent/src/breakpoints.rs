//! turning a source location into a code object and an offset
//!
//! the rule this module exists to enforce is the one in the architecture doc:
//! `bpd` never reports a breakpoint as set unless there is a code object and an
//! offset behind it. everything else here follows from that — a location that
//! cannot be resolved is reported unbound with the reason, a location that is
//! not executable moves and says where it moved to, and a location in a module
//! that has not been imported is reported unbound now and reported again, on
//! its own, the moment the import makes it bindable

use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;

use bpd_protocol::message::{Binding, Resolved, Site, SourceBreakpoint, Unbound};
use pyo3::prelude::*;

use crate::files::{self, FileId};
use crate::{code, events};

/// one requested breakpoint, with the file it was resolved against
///
/// the identity is worked out when the client sets the breakpoint and kept,
/// rather than recomputed: rebinding is attempted every time a new file is
/// loaded, and a `stat` per breakpoint per import would put the filesystem on
/// a path that runs while the program does
#[derive(Debug)]
struct Pending {
    request: SourceBreakpoint,
    identity: Result<FileId, String>,
}

/// a code object with `LINE` events turned on, and what to do at each line
#[derive(Debug)]
struct Armed {
    /// the strong reference that keeps this code object's address unique
    code: Py<PyAny>,
    /// line -> the breakpoints bound to it, smallest id first
    lines: BTreeMap<u32, Vec<u32>>,
}

#[derive(Debug)]
struct State {
    pending: Vec<Pending>,
    /// what the client was last told about each breakpoint
    reported: BTreeMap<u32, Binding>,
    /// by code object address, which is sound because `Armed` holds the object
    armed: BTreeMap<usize, Armed>,
}

impl State {
    const fn new() -> Self {
        Self {
            pending: Vec::new(),
            reported: BTreeMap::new(),
            armed: BTreeMap::new(),
        }
    }
}

static STATE: RwLock<State> = RwLock::new(State::new());

fn read() -> std::sync::RwLockReadGuard<'static, State> {
    STATE
        .read()
        .expect("the breakpoint lock is only held for map operations, which do not panic")
}

fn write() -> std::sync::RwLockWriteGuard<'static, State> {
    STATE
        .write()
        .expect("the breakpoint lock is only held for map operations, which do not panic")
}

/// the breakpoints bound to this line of this code object, if any
///
/// the whole of the `LINE` event path. it is a lookup on an address and an
/// integer, and it allocates nothing unless the answer is yes
pub(crate) fn hit(address: usize, line: u32) -> Option<Vec<u32>> {
    read().armed.get(&address)?.lines.get(&line).cloned()
}

/// whether anything is set, which is what decides if `PY_START` stays on
pub(crate) fn any_set() -> bool {
    !read().pending.is_empty()
}

/// replace the whole breakpoint set and say how every one of them resolved
pub(crate) fn apply(
    python: Python<'_>,
    requested: Vec<SourceBreakpoint>,
) -> PyResult<Vec<Resolved>> {
    debug_assert!(
        requested
            .iter()
            .map(|request| request.id)
            .collect::<BTreeSet<_>>()
            .len()
            == requested.len(),
        "the engine refuses a request with a repeated breakpoint id, so an id \
         names one breakpoint here"
    );

    let pending = requested
        .into_iter()
        .map(|request| Pending {
            identity: files::identify(&request.file),
            request,
        })
        .collect();
    write().pending = pending;

    let (all, _changed) = resolve_all(python)?;
    Ok(all)
}

/// re-resolve everything and report only what a newly loaded file changed
pub(crate) fn rebind(python: Python<'_>, loaded: &FileId) -> PyResult<Vec<Resolved>> {
    let relevant = read()
        .pending
        .iter()
        .any(|pending| pending.identity.as_ref() == Ok(loaded));
    if !relevant {
        return Ok(Vec::new());
    }

    let (_all, changed) = resolve_all(python)?;
    Ok(changed)
}

/// work out every binding, arm the interpreter for it, and record what was said
///
/// returns every resolution and, separately, the ones whose answer is not what
/// the client was last told
fn resolve_all(python: Python<'_>) -> PyResult<(Vec<Resolved>, Vec<Resolved>)> {
    // the resolution reads code objects and line tables, so the lock is not held
    // across it: a rebind runs inside a `PY_START` callback, and a lock held
    // over a call into the interpreter is a lock another thread can be waiting
    // for while it holds the one thing this one needs
    let pending: Vec<(SourceBreakpoint, Result<FileId, String>)> = read()
        .pending
        .iter()
        .map(|pending| (pending.request.clone(), pending.identity.clone()))
        .collect();

    let mut all = Vec::with_capacity(pending.len());
    let mut armed: BTreeMap<usize, Armed> = BTreeMap::new();

    for (request, identity) in pending {
        let binding = resolve(python, &request, identity.as_ref(), &mut armed)?;
        all.push(Resolved {
            id: request.id,
            binding,
        });
    }

    let previous = {
        let mut state = write();
        std::mem::replace(&mut state.armed, armed)
    };
    arm(python, &previous)?;

    let mut state = write();
    let changed = all
        .iter()
        .filter(|resolution| state.reported.get(&resolution.id) != Some(&resolution.binding))
        .cloned()
        .collect();
    state.reported = all
        .iter()
        .map(|resolution| (resolution.id, resolution.binding.clone()))
        .collect();
    drop(state);

    Ok((all, changed))
}

/// bind one breakpoint, adding it to `armed` for every code object that holds it
fn resolve(
    python: Python<'_>,
    request: &SourceBreakpoint,
    identity: Result<&FileId, &String>,
    armed: &mut BTreeMap<usize, Armed>,
) -> PyResult<Binding> {
    let identity = match identity {
        Ok(identity) => identity,
        Err(reason) => {
            let loaded_under_that_name = request.file.to_str().is_some_and(code::loaded_under);
            return Ok(Binding::Unbound {
                reason: Unbound::Unresolvable {
                    file: request.file.clone(),
                    reason: reason.clone(),
                    loaded_under_that_name,
                },
            });
        }
    };

    let units = code::units_for(python, identity)?;
    if units.is_empty() {
        return Ok(Binding::Unbound {
            reason: Unbound::NotLoaded {
                file: request.file.clone(),
            },
        });
    }

    let executable: BTreeSet<u32> = units
        .iter()
        .flat_map(|unit| unit.lines.keys())
        .copied()
        .collect();
    let Some(&line) = executable.range(request.line..).next() else {
        return Ok(Binding::Unbound {
            reason: Unbound::NoExecutableLine {
                file: request.file.clone(),
                requested: request.line,
                last_executable: executable.last().copied(),
            },
        });
    };

    let mut sites = Vec::new();
    for unit in units {
        let Some(&offset) = unit.lines.get(&line) else {
            continue;
        };
        sites.push(Site {
            qualname: unit.qualname.clone(),
            first_line: unit.first_line,
            offset,
        });

        let entry = armed
            .entry(unit.code.as_ptr() as usize)
            .or_insert_with(|| Armed {
                code: unit.code.clone_ref(python),
                lines: BTreeMap::new(),
            });
        let ids = entry.lines.entry(line).or_default();
        ids.push(request.id);
        ids.sort_unstable();
    }

    assert!(
        !sites.is_empty(),
        "line {line} was taken from the union of the line tables of the code \
         objects in `{}`, so at least one of them holds it",
        request.file.display()
    );
    sites.sort_by(|left, right| {
        (left.first_line, &left.qualname, left.offset).cmp(&(
            right.first_line,
            &right.qualname,
            right.offset,
        ))
    });

    Ok(Binding::Bound { line, sites })
}

/// make the interpreter's instrumentation match the new `armed` set
///
/// clearing first and then setting, rather than diffing, because
/// `set_local_events` is a whole-set assignment per code object and a code
/// object that keeps a breakpoint simply gets the same assignment twice
fn arm(python: Python<'_>, previous: &BTreeMap<usize, Armed>) -> PyResult<()> {
    let state = read();
    for (address, stale) in previous {
        if !state.armed.contains_key(address) {
            events::watch_lines(python, stale.code.bind(python), false)?;
        }
    }
    for live in state.armed.values() {
        events::watch_lines(python, live.code.bind(python), true)?;
    }
    drop(state);

    // a line that has already run returned `DISABLE` and would never be
    // reported again. a breakpoint that lands on one has to undo that, and PEP
    // 669 has no per-location undo
    events::restart(python)
}
