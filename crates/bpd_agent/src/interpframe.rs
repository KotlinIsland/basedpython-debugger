//! the frame's own data, reached by a layout that is measured rather than declared
//!
//! cpython will move a running frame — `frame.f_lineno = line` pops the operand
//! stack, closes what it pops, runs the block cleanup that a jump implies and
//! sets the instruction pointer. what it will not do is **unbind a local**. a
//! jump binds every unbound local of the frame to `None`, and a frame restarted
//! at the top of its own body therefore starts with names bound that a frame
//! the interpreter had just built would not have
//!
//! that is the whole of the difference, and it is not a cosmetic one:
//!
//! ```py
//! def f(arg):
//!     if arg == "never":
//!         cond = 1
//!     print(cond)          # a fresh call raises UnboundLocalError
//! ```
//!
//! a frame reset by a jump alone prints `None` there. so the reset is a jump
//! **and** this: every slot of `localsplus` that a fresh call would not have
//! bound is put back to empty, after the jump, because the jump is what bound
//! them
//!
//! ## why the layout is measured
//!
//! `_PyInterpreterFrame` is internal and it moves. `localsplus` is word 9 on
//! 3.13, which has an `int stacktop`, and word 10 on 3.14 and 3.15, which have a
//! `_PyStackRef *stackpointer` instead. `f_frame` is word 3 of `PyFrameObject`
//! under the gil and word 5 without it, because a free-threaded `PyObject_HEAD`
//! is four words rather than two. that is a per-version, per-build table, and
//! this project does not hand-maintain one — a table is right until an
//! interpreter it was not written for loads it, and then it is silently wrong
//! about somebody's memory
//!
//! so nothing here is declared. every field is **found by matching a value the
//! debugger already knows** — `f_back` against the frame above it, `f_executable`
//! against the code object, `localsplus` against a probe whose locals are one
//! owned reference, one immortal, and one that is never bound at all. a build
//! that does not match is a build this refuses on, and refusing is a restart
//! that does not happen rather than a write into a field that is not there
//!
//! ## what an empty slot is, and which slots own what they hold
//!
//! both measured, for the same reason. `PyStackRef_NULL` is `0` on 3.13, whose
//! slots are plain `PyObject *`, and `1` on 3.14 and 3.15, whose `Py_TAG_REFCNT`
//! is set on a reference the slot does not own
//!
//! the ownership rule needs no table and is the one thing here that is argued
//! rather than measured: a pointer to a `PyObject` is at least eight-aligned, so
//! its low two bits are clear, and **every** tagged form cpython puts in a slot
//! sets one of them — `Py_TAG_REFCNT` and `Py_TAG_DEFERRED` are both `1`,
//! `Py_INT_TAG` is `3`, `Py_TAG_INVALID` is `2`. so a slot holding `bits` owns a
//! reference exactly when `bits & 3 == 0`, and a slot that does not own one is
//! left alone. an immortal, a deferred reference and a tagged int are all things
//! `PyStackRef_CLOSE` does nothing to, and so is this
//!
//! ## the second `unsafe` in the workspace
//!
//! the first is [`crate::linetable`], and this is held to the same standard: the
//! layout is not believed, it is **checked** against the code object the frame
//! says it is running, immediately before any write, every time. a frame whose
//! data does not hold its own code object is a refusal and not a write
#![allow(
    unsafe_code,
    reason = "`_PyInterpreterFrame` is internal, and the layout reaching it is \
              measured against known values and re-checked before every write — \
              see the module note"
)]

use std::sync::OnceLock;

use pyo3::prelude::*;
use pyo3::types::PyDict;

/// how far into either object a field is looked for
///
/// `PyFrameObject` is ten words and `localsplus` is never past word twelve on
/// any build this supports, so both scans are bounded well above where the
/// answer is and bounded again by the object's own reported size
const SLOTS: usize = 16;

/// the low bits every tagged form of a slot sets, and a real pointer clears
const TAG_BITS: usize = 3;

/// why a frame's own data could not be reached
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unreachable {
    /// the layout did not match on this build, so nothing here is known
    Uncalibrated,
    /// the frame's data does not hold the code object the frame says it runs
    ///
    /// the check that stands between a measured layout and a write into
    /// somebody else's memory. it has never fired
    NotItsOwnCode,
}

/// where the fields this needs sit, in whole words
#[derive(Debug, Clone, Copy)]
pub(crate) struct Layout {
    /// the word of `PyFrameObject` holding `f_frame`
    f_frame: usize,
    /// the word of `_PyInterpreterFrame` holding `f_executable`
    executable: usize,
    /// the word `localsplus` begins at
    localsplus: usize,
    /// what a slot holding nothing holds
    null_bits: usize,
}

/// the layout of this interpreter's frames, calibrated once
///
/// `None` once a calibration has been attempted and did not match, so a build
/// this does not understand costs one probe rather than one per request
fn layout(python: Python<'_>) -> PyResult<Option<Layout>> {
    static LAYOUT: OnceLock<Option<Layout>> = OnceLock::new();
    if let Some(found) = LAYOUT.get() {
        return Ok(*found);
    }
    let calibrated = calibrate(python)?;
    Ok(*LAYOUT.get_or_init(|| calibrated))
}

/// unbind every local of `frame` that `keep` does not name
///
/// `keep` is one flag per `localsplus` slot, in the interpreter's own order —
/// arguments first, then the rest of the locals, then cells and free variables.
/// the slots it does not name are put back to empty and what they held is
/// released
///
/// **called after the jump, never before.** the jump is what binds the unbound
/// locals to `None`, so a pass made first would leave exactly the slots this
/// exists to clear
pub(crate) fn unbind_locals(
    frame: &Bound<'_, PyAny>,
    keep: &[bool],
) -> PyResult<Result<(), Unreachable>> {
    let python = frame.py();
    let Some(layout) = layout(python)? else {
        return Ok(Err(Unreachable::Uncalibrated));
    };
    let code = frame.getattr("f_code")?;
    let Some(data) = data_of(&layout, frame, &code)? else {
        return Ok(Err(Unreachable::NotItsOwnCode));
    };
    assert_eq!(
        keep.len(),
        nlocalsplus(&code)?,
        "the caller decides slot by slot, so it has decided about every slot",
    );

    for (index, keeping) in keep.iter().enumerate() {
        if *keeping {
            continue;
        }
        // SAFETY: `data` was checked to hold this frame's own code object at
        // `executable`, so it is this frame's data, and `index` is inside the
        // `nlocalsplus` slots every frame of this code object has
        let word = unsafe { data.add(layout.localsplus + index) };
        let bits = unsafe { word.read() };
        if bits == layout.null_bits {
            continue;
        }
        // emptied **before** the reference is released, so that nothing which
        // runs during the release can see the slot still naming what it held
        unsafe { word.write(layout.null_bits) };
        if bits & TAG_BITS == 0 {
            // an owned reference, and the frame is the owner — see the module
            // note on why a tagged slot is left alone
            unsafe { pyo3::ffi::Py_DecRef(bits as *mut pyo3::ffi::PyObject) };
        }
    }
    Ok(Ok(()))
}

/// how many slots `localsplus` has for this code object
pub(crate) fn nlocalsplus(code: &Bound<'_, PyAny>) -> PyResult<usize> {
    let mut total = 0;
    for name in ["co_varnames", "co_cellvars", "co_freevars"] {
        total += code.getattr(name)?.len()?;
    }
    Ok(total)
}

/// this frame's `_PyInterpreterFrame`, once it has proved it is that
///
/// the proof is the code object: a frame's data holds the thing the frame says
/// it is executing, and one that does not is not this frame's data — which is
/// the difference between a measured layout and a write into somebody else's
/// memory
fn data_of(
    layout: &Layout,
    frame: &Bound<'_, PyAny>,
    code: &Bound<'_, PyAny>,
) -> PyResult<Option<*mut usize>> {
    if layout.f_frame >= words_of(frame)? {
        return Ok(None);
    }
    // SAFETY: inside the frame object's own reported size, and a read
    let data = unsafe { frame.as_ptr().cast::<usize>().add(layout.f_frame).read() };
    if data == 0 || data % size_of::<usize>() != 0 {
        return Ok(None);
    }
    let data = data as *mut usize;
    // SAFETY: `data` is word-aligned and non-null, and the calibration
    // established that a frame's data holds its code object at this word. it is
    // a read, and a mismatch returns rather than writing
    let held = unsafe { data.add(layout.executable).read() };
    Ok((held & !TAG_BITS == code.as_ptr() as usize).then_some(data))
}

/// how many whole words the interpreter says this object occupies
fn words_of(object: &Bound<'_, PyAny>) -> PyResult<usize> {
    let size: usize = object
        .py()
        .import("sys")?
        .call_method1("getsizeof", (object,))?
        .extract()?;
    Ok(size / size_of::<usize>())
}

/// the probe whose frame the layout is matched against
///
/// a generator, deliberately. a suspended generator keeps its locals, and its
/// frame's data lives **inside the generator object** — so every read the
/// calibration makes is bounded by an object bpd is holding and has asked the
/// size of, rather than a dereference of whatever a word happens to contain
///
/// its three locals are the whole signature: one owned reference, one immortal,
/// and one the interpreter never binds at all. between them they pin the word
/// `localsplus` starts at and say what an empty slot holds on this build
const PROBE: &str = "import sys


def probe():
    held = ['bpd frame probe']
    immortal = None
    if held is None:
        never = 1
    yield sys._getframe()
";

/// find the layout by matching values bpd already knows
///
/// nothing here is declared. `f_executable` is matched against the probe's own
/// code object, `localsplus` against the probe's three locals, and the word
/// holding `f_frame` against the generator the data is inside
///
/// **one match or none.** two candidates is not a tie to be broken by
/// preferring the lower — it is this not knowing which word is the field, and
/// the next thing it would do with that is write
fn calibrate(python: Python<'_>) -> PyResult<Option<Layout>> {
    let builtins = python.import("builtins")?;
    let namespace = PyDict::new(python);
    let compiled = builtins.call_method1("compile", (PROBE, "<bpd frame probe>", "exec"))?;
    builtins.call_method1("exec", (compiled, &namespace))?;
    let Some(probe) = namespace.get_item("probe")? else {
        unreachable!("the probe source defines `probe`, and `exec` has just run it")
    };
    let generator = probe.call0()?;
    let frame = builtins.call_method1("next", (&generator,))?;
    let code = frame.getattr("f_code")?;
    let held = frame.getattr("f_locals")?.get_item("held")?;
    let none = python.None();

    let word = size_of::<usize>();
    let low = generator.as_ptr() as usize;
    let high = low + words_of(&generator)? * word;
    let slots = nlocalsplus(&code)?;
    assert_eq!(
        slots, 3,
        "the probe binds `held` and `immortal`, never binds `never`, and has no \
         cells or free variables",
    );

    let mut found: Vec<Layout> = Vec::new();
    for f_frame in 0..words_of(&frame)?.min(SLOTS) {
        // SAFETY: inside the frame object's own reported size, and a read
        let data = unsafe { frame.as_ptr().cast::<usize>().add(f_frame).read() };
        if data < low || data >= high || data % word != 0 {
            continue;
        }
        // every read below this line is inside the generator, which is held for
        // the whole of the scan, and bounded by what is left of it
        let left = (high - data) / word;
        let data = data as *const usize;
        for executable in 0..left.min(SLOTS) {
            // SAFETY: bounded by `left`, which is what remains of the generator
            let held_there = unsafe { data.add(executable).read() };
            if held_there & !TAG_BITS != code.as_ptr() as usize {
                continue;
            }
            for localsplus in 0..SLOTS {
                if localsplus + slots > left {
                    break;
                }
                // SAFETY: the three reads are bounded by the check above
                let read = |at: usize| unsafe { data.add(localsplus + at).read() };
                let null_bits = read(2);
                let matches = read(0) == held.as_ptr() as usize
                    && read(1) & !TAG_BITS == none.as_ptr() as usize
                    && null_bits <= 1;
                if matches {
                    found.push(Layout {
                        f_frame,
                        executable,
                        localsplus,
                        null_bits,
                    });
                }
            }
        }
    }
    Ok(match found.len() {
        1 => found.pop(),
        _ => None,
    })
}

/// whether this frame's data can be reached at all, before anything is written
///
/// the calibration and the code-object check, made **before** the jump that a
/// reset begins with. after the jump the frame has already moved, and a layout
/// that turned out not to be usable would leave a frame reset half way — which
/// is the outcome the whole of this exists to prevent
pub(crate) fn reachable(frame: &Bound<'_, PyAny>) -> PyResult<Result<(), Unreachable>> {
    let Some(layout) = layout(frame.py())? else {
        return Ok(Err(Unreachable::Uncalibrated));
    };
    let code = frame.getattr("f_code")?;
    match data_of(&layout, frame, &code)? {
        Some(_) => Ok(Ok(())),
        None => Ok(Err(Unreachable::NotItsOwnCode)),
    }
}
