//! where the program went, over a bounded window
//!
//! stepping backwards, honestly. **measured first**, on a loop of 300,000
//! iterations over three lines:
//!
//! | what is stored per line               | time     | against bare |
//! | ------------------------------------- | -------- | ------------ |
//! | nothing                               | 13.1 ms  | —            |
//! | the location — code object, line      | 75.1 ms  | 6×           |
//! | the location and a copy of the locals | 390.8 ms | 30×          |
//!
//! so the two halves of "step back" have very different prices, and this module
//! is deliberately only the first. **where** the program went fits a fixed ring
//! of small entries; **what a variable was** costs five times that again and is
//! unbounded, because it is a copy of live objects per line — which perturbs the
//! heap it is copying from
//!
//! a trail that says where the program went and refuses to say what anything was
//! is a debugger reporting what it has. one that interpolated the values would be
//! inventing history, which is the thing this whole project is against
//!
//! ## what it costs while it is on
//!
//! everything. recording is the one mode that turns off the property the rest of
//! the design rests on: a location is normally `DISABLE`d the first time it is
//! seen — six callbacks for 900,000 line executions — and a recorder needs every
//! one of them. that is 4× a bare run for the delivery alone
//!
//! so it is a **mode**, off by default, and turning it on is a thing a person
//! asks for knowing what it costs

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::collections::btree_map::Entry;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU8, Ordering};

use bpd_core::Depth;
use pyo3::prelude::*;

/// how many steps the window holds
///
/// fixed rather than configurable, for now, and **not** silently exceeded: what
/// falls out of the window is counted, because a trail that quietly began later
/// than the recording did is one whose start nobody can trust
const WINDOW: usize = 100_000;

/// one place the program was
///
/// the code object's address rather than its file, because resolving a filename
/// per line is the cost this is trying not to pay. the object is held in
/// [`State::seen`] for as long as the trail refers to it, so the address cannot
/// come to name a different one
#[derive(Clone)]
struct Step {
    code: usize,
    line: u32,
    thread: u64,
    /// what the frame held, rendered, when the recording is deep enough
    ///
    /// **an experiment.** text rather than references, so the window stays
    /// bounded in memory and nothing the program has finished with is kept
    /// alive by the recorder
    held: bpd_core::Kept<(String, String)>,
}

#[derive(Default)]
struct State {
    /// whether the program is being recorded at all
    on: bool,
    /// how much of each step is kept
    depth: Depth,
    /// `sys._getframe`, held so the hot path is a call and not a lookup
    ///
    /// a `LINE` event carries no frame, so every depth past [`Depth::Where`]
    /// needs one — and doing the two attribute lookups per line would measure
    /// the lookups rather than the thing under test
    getframe: Option<Py<PyAny>>,
    /// where it has been, oldest first
    went: VecDeque<Step>,
    /// how many steps the window has dropped off the front
    ///
    /// the whole of "say plainly where the window ends". a trail of the last
    /// hundred thousand steps that did not say it had discarded two million is
    /// a trail whose beginning is a fiction
    dropped: u64,
    /// every code object the window still refers to, held so its address stays
    /// its own, with how many of its steps refer to it
    ///
    /// a code object that was freed would leave an address the interpreter can
    /// hand to the next one, and the trail would then name the wrong file. this
    /// is one insert per code object rather than per line
    ///
    /// the count is what keeps this **bounded**. holding a code object for as
    /// long as any step names it is necessary; holding it after the last of
    /// those steps fell out of the window is a leak, and one with no ceiling in
    /// a program that compiles code as it runs — a django template engine, an
    /// ORM, anything built on `exec`. so the last step to go takes it with it,
    /// and the window bounds the objects held as well as the steps
    seen: BTreeMap<usize, Held>,
}

/// a code object the window refers to, and how many of its steps do
struct Held {
    code: Py<PyAny>,
    steps: usize,
}

/// the depth, outside the lock so the shipped path never pays for it
///
/// `Where` is what the trail ships as, and it must cost what it did before this
/// experiment existed: one write lock per line and nothing else. reading the
/// depth out of the lock would have put a second acquisition on every line of
/// every recording — measured, and it is not free
static DEPTH: AtomicU8 = AtomicU8::new(0);

/// [`Depth`] as the atomic carries it
const fn code_of(depth: Depth) -> u8 {
    match depth {
        Depth::Where => 0,
        Depth::Frame => 1,
        Depth::Locals => 2,
        Depth::Values => 3,
    }
}

static STATE: RwLock<State> = RwLock::new(State {
    on: false,
    depth: Depth::Where,
    getframe: None,
    went: VecDeque::new(),
    dropped: 0,
    seen: BTreeMap::new(),
});

fn read() -> std::sync::RwLockReadGuard<'static, State> {
    STATE
        .read()
        .unwrap_or_else(|_| unreachable!("nothing panics holding the trail"))
}

fn write() -> std::sync::RwLockWriteGuard<'static, State> {
    STATE
        .write()
        .unwrap_or_else(|_| unreachable!("nothing panics holding the trail"))
}

/// whether the program is being recorded
///
/// read on the event path, so it is one lock and a bool. it is what keeps a line
/// from being `DISABLE`d and what arms `LINE` for the whole program
pub(crate) fn recording() -> bool {
    read().on
}

/// start or stop recording, and say what the window holds
///
/// stopping **keeps** the trail: a person stops the recording in order to read
/// it, and throwing it away at that moment would be the one thing they were
/// about to do made impossible. starting clears it, because a trail spanning two
/// recordings has a gap in it that nothing marks
pub(crate) fn record(python: Python<'_>, on: bool, depth: Depth) -> PyResult<(u64, u64)> {
    // resolved once, here, rather than twice per line. a `LINE` event carries no
    // frame, so every depth past `Where` needs `sys._getframe` — and looking it
    // up on the hot path would put two attribute lookups into every measurement
    // taken of what is actually being tried
    let getframe = if on && depth != Depth::Where {
        Some(python.import("sys")?.getattr("_getframe")?.unbind())
    } else {
        None
    };

    // the cleared code objects leave the lock still owned and are released
    // below, for the reason `went` gives: a python deallocation that reached
    // back into the recorder would meet a lock this thread already holds
    let (held, dropped, gone) = {
        let mut state = write();
        let gone = if on && !state.on {
            state.went.clear();
            state.dropped = 0;
            std::mem::take(&mut state.seen)
        } else {
            BTreeMap::new()
        };
        state.on = on;
        state.depth = depth;
        state.getframe = getframe;
        DEPTH.store(if on { code_of(depth) } else { 0 }, Ordering::Relaxed);
        (state.went.len() as u64, state.dropped, gone)
    };

    drop(gone);
    Ok((held, dropped))
}

/// remember that a thread reached this line
///
/// the hot path, and the whole reason the window holds addresses rather than
/// filenames. what it does per line is one push and, for a code object it has
/// not seen, one insert
pub(crate) fn went(python: Python<'_>, code: &Bound<'_, PyAny>, line: u32, thread: u64) {
    let address = code.as_ptr() as usize;

    // the shipped depth does none of this and pays for none of it: one relaxed
    // load, and then the same single write lock the trail has always taken
    let held = if DEPTH.load(Ordering::Relaxed) == 0 {
        bpd_core::Kept::whole(Vec::new())
    } else {
        // outside the lock, because it calls into python: `sys._getframe`, then
        // `f_locals`, then a render per name. holding the recorder's lock
        // across any of that would put the whole program behind it
        let (depth, getframe) = {
            let state = read();
            if !state.on {
                return;
            }
            (
                state.depth,
                state.getframe.as_ref().map(|one| one.clone_ref(python)),
            )
        };
        capture(python, depth, getframe.as_ref())
    };

    // whatever the window let go of leaves this scope still owned, and is
    // released below with the lock given up. dropping a `Py` decrements a
    // python refcount, which can deallocate — and a deallocation that reached
    // back into the recorder would meet a lock this thread already holds
    let gone = {
        let mut state = write();
        if !state.on {
            return;
        }

        let gone = if state.went.len() == WINDOW {
            let oldest = state
                .went
                .pop_front()
                .expect("the window is full, so it has a front");
            state.dropped += 1;
            forget(&mut state, oldest.code)
        } else {
            None
        };

        match state.seen.entry(address) {
            Entry::Occupied(mut held) => held.get_mut().steps += 1,
            Entry::Vacant(slot) => {
                slot.insert(Held {
                    code: code.clone().unbind(),
                    steps: 1,
                });
            }
        }
        state.went.push_back(Step {
            code: address,
            line,
            thread,
            held,
        });
        gone
    };

    drop(gone);
}

/// how many names one step keeps, at most
///
/// a frame with more than this many locals gives up the rest. an experiment can
/// afford a crude bound; what it cannot afford is an unbounded one, which is the
/// thing being measured
const NAMES: usize = 16;

/// how much of one rendered value is kept
const TEXT: usize = 64;

/// what the frame held at this line, as far as `depth` asks for
///
/// the four settings are cumulative on purpose: each does everything the one
/// above it does and then one more thing, so the difference between two
/// measurements is that one step and nothing else
fn capture(
    python: Python<'_>,
    depth: Depth,
    getframe: Option<&Py<PyAny>>,
) -> bpd_core::Kept<(String, String)> {
    if depth == Depth::Where {
        return bpd_core::Kept::whole(Vec::new());
    }
    let Some(getframe) = getframe else {
        // asked for a depth and given no way to reach a frame. an experiment
        // reports nothing rather than guessing, the same as the rest of this
        return bpd_core::Kept::whole(Vec::new());
    };

    // `_getframe(0)` from inside a native callback is the **program's** frame:
    // a C function has no python frame of its own, so the innermost one is the
    // one whose line raised this event
    let Ok(frame) = getframe.bind(python).call1((0_u32,)) else {
        return bpd_core::Kept::whole(Vec::new());
    };
    if depth == Depth::Frame {
        return bpd_core::Kept::whole(Vec::new());
    }

    let Ok(locals) = frame.getattr("f_locals") else {
        return bpd_core::Kept::whole(Vec::new());
    };
    if depth == Depth::Locals {
        return bpd_core::Kept::whole(Vec::new());
    }

    let Ok(items) = locals.try_iter() else {
        return bpd_core::Kept::whole(Vec::new());
    };

    // read whole and then bounded, so what the cap left out is counted rather
    // than walked away from. `Kept::of` is what makes that impossible to forget
    // **everything not kept is counted**, and that is the whole point of the
    // type: a name skipped because it could not be read looks exactly like a
    // frame that never bound it. the first cut of this counted only what the cap
    // cut and let the three failure arms drop names silently — which is the
    // defect the type was introduced to make impossible, still in its only
    // caller
    //
    // and only the kept ones are rendered. rendering the whole of `f_locals`
    // and then truncating means a module-level line pays a render per global,
    // every step, to throw all but sixteen away
    let mut held = Vec::new();
    let mut lost = 0_u64;
    for name in items {
        let Ok(name) = name else {
            lost += 1;
            break;
        };
        let Ok(text) = name.extract::<String>() else {
            lost += 1;
            continue;
        };
        let Ok(value) = locals.get_item(&name) else {
            lost += 1;
            continue;
        };
        if held.len() < NAMES {
            held.push((text, rendered(&value)));
        } else {
            lost += 1;
        }
    }
    bpd_core::Kept::counted(held, lost)
}

/// one value as text, **without running any of the program**
///
/// the rule the retainer walk arrived at, for the same reason: `repr` is the
/// object's own code, and so are `__len__`, `__hash__` and `__str__`. running
/// any of them to describe the program is running the program to describe
/// itself, and during a recording it would be doing so on every line
///
/// so the exact builtin types render as themselves and everything else says
/// what it is. that is a weaker answer than a repr, and it is one that cannot
/// be wrong
fn rendered(value: &Bound<'_, PyAny>) -> String {
    use pyo3::types::{PyBool, PyFloat, PyInt, PyString};

    if value.is_none() {
        return "None".to_string();
    }
    // `bool` before `int`, because a bool **is** an int in python and the
    // exact check for one would otherwise never be reached
    if value.is_exact_instance_of::<PyBool>()
        || value.is_exact_instance_of::<PyInt>()
        || value.is_exact_instance_of::<PyFloat>()
    {
        return value.str().map_or_else(
            |_| "a number".to_string(),
            |text| text.extract::<String>().unwrap_or_default(),
        );
    }
    if value.is_exact_instance_of::<PyString>() {
        return value.extract::<String>().map_or_else(
            |_| "a str".to_string(),
            |text| {
                let mut cut: String = text.chars().take(TEXT).collect();
                if cut.len() < text.len() {
                    cut.push('…');
                }
                format!("{cut:?}")
            },
        );
    }
    value
        .get_type()
        .name()
        .and_then(|kind| kind.extract::<String>())
        .map_or_else(|_| "?".to_string(), |kind| format!("a {kind}"))
}

/// one fewer step names this code object, and the last one takes it with it
fn forget(state: &mut State, address: usize) -> Option<Py<PyAny>> {
    let Entry::Occupied(mut held) = state.seen.entry(address) else {
        unreachable!("a step in the window has its code object held")
    };
    held.get_mut().steps -= 1;
    if held.get().steps == 0 {
        Some(held.remove().code)
    } else {
        None
    }
}

/// the window, resolved into places a person can read
///
/// the filenames are looked up **here** rather than while recording, which is
/// the whole shape of this: the cost of naming a place is paid once per trail
/// read instead of once per line executed
pub(crate) fn taken(python: Python<'_>) -> PyResult<bpd_core::Trail> {
    let state = read();
    let mut went = Vec::with_capacity(state.went.len());
    for step in &state.went {
        let (file, function) = match state.seen.get(&step.code) {
            Some(held) => {
                let code = held.code.bind(python);
                (
                    code.getattr("co_filename")?.extract()?,
                    code.getattr("co_qualname")?.extract()?,
                )
            }
            // held for as long as the trail refers to it, so this cannot happen
            // — and saying so is cheaper than a variant nobody can reach
            None => unreachable!("every code object the trail refers to is held"),
        };
        went.push(bpd_core::Visited {
            file,
            line: step.line,
            function,
            thread: step.thread,
            held: step.held.clone(),
        });
    }
    Ok(bpd_core::Trail {
        went,
        dropped: state.dropped,
        recording: state.on,
        window: WINDOW as u64,
    })
}
