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
use std::sync::RwLock;

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
#[derive(Clone, Copy)]
struct Step {
    code: usize,
    line: u32,
    thread: u64,
}

#[derive(Default)]
struct State {
    /// whether the program is being recorded at all
    on: bool,
    /// where it has been, oldest first
    went: VecDeque<Step>,
    /// how many steps the window has dropped off the front
    ///
    /// the whole of "say plainly where the window ends". a trail of the last
    /// hundred thousand steps that did not say it had discarded two million is
    /// a trail whose beginning is a fiction
    dropped: u64,
    /// every code object the trail refers to, held so its address stays its own
    ///
    /// a code object that was freed would leave an address the interpreter can
    /// hand to the next one, and the trail would then name the wrong file. this
    /// is one insert per code object rather than per line
    seen: BTreeMap<usize, Py<PyAny>>,
}

static STATE: RwLock<State> = RwLock::new(State {
    on: false,
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
pub(crate) fn record(on: bool) -> (u64, u64) {
    let mut state = write();
    if on && !state.on {
        state.went.clear();
        state.dropped = 0;
        state.seen.clear();
    }
    state.on = on;
    let held = state.went.len() as u64;
    (held, state.dropped)
}

/// remember that a thread reached this line
///
/// the hot path, and the whole reason the window holds addresses rather than
/// filenames. what it does per line is one push and, for a code object it has
/// not seen, one insert
pub(crate) fn went(code: &Bound<'_, PyAny>, line: u32, thread: u64) {
    let address = code.as_ptr() as usize;
    let mut state = write();
    if !state.on {
        return;
    }
    state
        .seen
        .entry(address)
        .or_insert_with(|| code.clone().unbind());

    if state.went.len() == WINDOW {
        state.went.pop_front();
        state.dropped += 1;
    }
    state.went.push_back(Step {
        code: address,
        line,
        thread,
    });
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
            Some(code) => {
                let code = code.bind(python);
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
        });
    }
    Ok(bpd_core::Trail {
        went,
        dropped: state.dropped,
        recording: state.on,
        window: WINDOW as u64,
    })
}
