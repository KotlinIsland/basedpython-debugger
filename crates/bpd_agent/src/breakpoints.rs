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
use std::sync::{Arc, RwLock};

use bpd_core::{Binding, Resolved, Site, SourceBreakpoint, Unbound};
use pyo3::prelude::*;

use crate::conditions::Plan;
use crate::files::{self, FileId};
use crate::{code, events, session, templates};

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
    /// the compiled condition, hit count and log message, or why there are none
    ///
    /// compiled once, when the request arrives, and never on the event path.
    /// the hit counter lives in here, which is why an unchanged request keeps
    /// the same `Plan` across a rebinding
    plan: Result<Arc<Plan>, Unbound>,
}

/// a code object with `LINE` events turned on, and what to do at each line
#[derive(Debug)]
struct Armed {
    /// the strong reference that keeps this code object's address unique
    code: Py<PyAny>,
    /// line -> what the breakpoints bound to it do, smallest id first
    lines: BTreeMap<u32, Vec<Arc<Plan>>>,
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

/// what the breakpoints bound to this line of this code object do, if any
///
/// the whole of the `LINE` event path's decision. it is a lookup on an address
/// and an integer, and it allocates nothing unless the answer is yes
///
/// the plans are handed back rather than read under the lock, because deciding
/// them runs user python: a condition that imports a module re-enters the
/// rebinding path, which takes this lock for writing, and `RwLock` is not
/// reentrant
pub(crate) fn hit(address: usize, line: u32) -> Option<Vec<Arc<Plan>>> {
    read().armed.get(&address)?.lines.get(&line).cloned()
}

/// the breakpoints bound to this line of this code object, by the client's id
///
/// the same lookup [`hit`] makes and none of the deciding: nothing is evaluated
/// and no hit count moves. it exists because a jump's destination line is
/// **not** announced — no `LINE` event is delivered for it — so a breakpoint
/// bound there does not fire for the pass the jump lands in, and the answer to
/// the jump has to say which ones those are
pub(crate) fn bound_at(address: usize, line: u32) -> Vec<u32> {
    read()
        .armed
        .get(&address)
        .and_then(|armed| armed.lines.get(&line))
        .map(|plans| plans.iter().map(|plan| plan.id).collect())
        .unwrap_or_default()
}

/// whether anything is set, which is what decides if `PY_START` stays on
pub(crate) fn any_set() -> bool {
    !read().pending.is_empty()
}

/// what the breakpoint set wants of one code object
///
/// half of what the interpreter is told about it — a step being made in the
/// same code object wants the other half, and `set_local_events` takes one mask
pub(crate) fn local(address: usize) -> events::Local {
    events::Local {
        line: read().armed.contains_key(&address),
        py_return: false,
        py_start: false,
    }
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

    // a breakpoint the client asked for again, unchanged, keeps its hit counter.
    // rebuilding it would reset the count every time any *other* breakpoint in
    // the set moved, and "the third time this line runs" would quietly mean
    // something else
    let previous: BTreeMap<u32, (SourceBreakpoint, Result<Arc<Plan>, Unbound>)> = read()
        .pending
        .iter()
        .map(|pending| {
            (
                pending.request.id,
                (pending.request.clone(), pending.plan.clone()),
            )
        })
        .collect();

    let pending = requested
        .into_iter()
        .map(|request| Pending {
            identity: files::identify(&request.file),
            plan: match previous.get(&request.id) {
                Some((before, plan)) if *before == request => plan.clone(),
                _ => Plan::compile(python, &request).map(Arc::new),
            },
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

    // django may have become importable with this file, and that changes the
    // answer for every template breakpoint in the set at once. it is a
    // `sys.modules` lookup, and it stops being asked the moment it succeeds
    let django_arrived = !templates::available() && templates::resolve_hooks(python)?;

    if !relevant && !django_arrived {
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
    type Snapshot = (
        SourceBreakpoint,
        Result<FileId, String>,
        Result<Arc<Plan>, Unbound>,
    );
    let pending: Vec<Snapshot> = read()
        .pending
        .iter()
        .map(|pending| {
            (
                pending.request.clone(),
                pending.identity.clone(),
                pending.plan.clone(),
            )
        })
        .collect();

    // django's template machinery is only there once the program has imported
    // it, and the answer changes when it does. asking `sys.modules` costs one
    // dictionary lookup per resolution, and a resolution happens when the
    // breakpoint set changes or a new file is loaded — never on an event path
    templates::resolve_hooks(python)?;

    let mut all = Vec::with_capacity(pending.len());
    let mut armed: BTreeMap<usize, Armed> = BTreeMap::new();
    let mut in_templates: BTreeMap<(FileId, u32), Vec<Arc<Plan>>> = BTreeMap::new();

    for (request, identity, plan) in pending {
        let binding = resolve(
            python,
            &request,
            identity.as_ref(),
            &plan,
            &mut armed,
            &mut in_templates,
        )?;
        all.push(Resolved {
            id: request.id,
            binding,
        });
    }

    let previous = {
        let mut state = write();
        std::mem::replace(&mut state.armed, armed)
    };
    // the `Template.__init__` hook has to be armed *before* anything binds to a
    // template, because it is the only thing that can make one bind. so it
    // follows the breakpoints a template parse could still answer, not the ones
    // already bound to a template
    let watching_parses = all.iter().any(|resolution| {
        matches!(
            resolution.binding,
            Binding::BoundInTemplate { .. }
                | Binding::Unbound {
                    reason: Unbound::NotLoaded { .. } | Unbound::NoRenderedNode { .. },
                }
        )
    });
    templates::rearm(in_templates, watching_parses);
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
    plan: &Result<Arc<Plan>, Unbound>,
    armed: &mut BTreeMap<usize, Armed>,
    in_templates: &mut BTreeMap<(FileId, u32), Vec<Arc<Plan>>>,
) -> PyResult<Binding> {
    // before the file, because an expression that does not compile makes the
    // breakpoint impossible wherever the file turns out to be, and that answer
    // does not change when a module is imported later
    let plan = match plan {
        Ok(plan) => plan,
        Err(reason) => {
            return Ok(Binding::Unbound {
                reason: reason.clone(),
            });
        }
    };

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
    // a partial view of a file answers every question about it wrongly and
    // plausibly, so nothing is answered from one
    if !code::whole_file_seen(identity) {
        if !units.is_empty() {
            return Ok(Binding::Unbound {
                reason: Unbound::PartiallyLoaded {
                    file: request.file.clone(),
                },
            });
        }
        // the interpreter has compiled nothing from this file. the other thing
        // it could be is a django template, and that is a binding of its own
        // rather than a guess: a template is only bindable once django has
        // parsed it, and bpd has seen it do so
        return Ok(templates::resolve(request, identity, plan, in_templates));
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
        let plans = entry.lines.entry(line).or_default();
        plans.push(Arc::clone(plan));
        plans.sort_unstable_by_key(|plan| plan.id);
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

    Ok(Binding::Bound {
        line,
        sites,
        evaluation: plan.evaluation(),
    })
}

/// make the interpreter's instrumentation match the new `armed` set
///
/// every code object goes through [`crate::session::refresh_code`], which is
/// the one place that decides a code object's whole local mask. assigning
/// `LINE` here directly would disarm a step being made in the same code
/// object, because `set_local_events` is a whole-set assignment
fn arm(python: Python<'_>, previous: &BTreeMap<usize, Armed>) -> PyResult<()> {
    let stale: Vec<Py<PyAny>> = {
        let state = read();
        previous
            .iter()
            .filter(|(address, _)| !state.armed.contains_key(*address))
            .map(|(_, armed)| armed.code.clone_ref(python))
            .collect()
    };
    let live: Vec<Py<PyAny>> = read()
        .armed
        .values()
        .map(|armed| armed.code.clone_ref(python))
        .collect();

    // the django hooks are code objects of django's rather than of the
    // program's, and they are refreshed alongside so that a template breakpoint
    // arms them and the last one going away disarms them again
    let hooks = templates::hook_codes(python);

    for code in stale.iter().chain(&live).chain(&hooks) {
        session::refresh_code(python, code.bind(python))?;
    }

    // a line that has already run returned `DISABLE` and would never be
    // reported again. a breakpoint that lands on one has to undo that, and PEP
    // 669 has no per-location undo
    events::restart(python)
}
