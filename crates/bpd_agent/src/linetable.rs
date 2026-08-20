//! moving a frame to an offset, by making that offset a line `f_lineno` can name
//!
//! `frame.f_lineno = line` is the only supported way to move a running frame, and
//! it will only land on a **mark**: `marklines` walks `co_linetable` and marks
//! each range start whose line differs from the last, and `frame_lineno_set_impl`
//! chooses among the marks carrying the line that was asked for. everything else
//! it does — `mark_stacks`, the stack-compatibility check, the unwind that
//! decrefs what it pops and hands an `Except` entry back to `tstate->exc_info`,
//! binding unbound locals to `None` — is general over offsets and is exactly what
//! this needs
//!
//! so the offset is made a mark. the code object's linetable is replaced for the
//! length of one assignment by one that carries a line number the code object
//! does not otherwise have, starting at the offset wanted and running to the end,
//! and then it is put back
//!
//! ```text
//! the real table, byte for byte, up to the offset │ sentinel │ sentinel │ …
//! ```
//!
//! the prefix is **byte-identical**, which is the whole reason the shape is a
//! prefix and a tail rather than a re-encode: every offset the frame can actually
//! be at keeps the line it really has, and only the epilogue it is about to be
//! moved into reads differently
//!
//! ## what can be observed while it is swapped
//!
//! `PyCode_Addr2Line` reads the line data monitoring cached, not the linetable,
//! for any code object instrumented for `LINE` — and a frame bpd can restart is
//! in one. so `frame.f_lineno` and every traceback still answer the real line
//! throughout. what does read differently is `co_lines()`, `co_positions()` and
//! `dis` over that one code object's tail. measured on 3.13, 3.14 and 3.14t
//!
//! the swapped-in bytes are **never freed**. a thread that read the pointer out
//! of the field before it was put back may still be reading the bytes behind it,
//! and on a free-threaded build nothing serialises the two. they are tens of
//! bytes and there is one per forced exit
//!
//! ## the one `unsafe` in the workspace
//!
//! `unsafe_code` is denied for the whole tree and allowed here, for four writes
//! and two reads of a single word. there is no safe way to reach the field:
//! `pyo3-ffi` does not declare `PyCodeObject`, hand-writing the layout is the
//! per-version table this project refuses everywhere else, and doing the same
//! poke through `ctypes` would only move it out of the lint's sight — it is the
//! same write, made harder to review
//!
//! what makes it reviewable is that the slot is not believed, it is **checked**:
//! against the address of this code object's own `co_linetable`, immediately
//! before the write, every time. a slot that does not check is a refusal and not
//! a write. and the only value ever written into it is one of two `bytes`
//! objects that are both alive across the write
#![allow(
    unsafe_code,
    reason = "the `co_linetable` slot is a raw word, and it is verified against \
              this code object before every write — see the module note"
)]

use std::sync::{Mutex, OnceLock};

use bpd_core::Address;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

/// a location entry's kind, of the sixteen `_PyCodeLocationInfoKind` has
///
/// only the two this writes are named. the rest are parsed by length alone,
/// which is all the prefix needs
const NO_COLUMNS: u8 = 13;
/// the kind carrying no location at all, whose entries have no payload
const NO_LOCATION: u8 = 15;

/// the most code units one location entry can cover
const PER_ENTRY: u32 = 8;

/// how far into a code object the `co_linetable` slot is looked for
///
/// `PyCodeObject`'s fixed part is nowhere near this long on any build — the slot
/// is word 17 on 3.13 and 3.14 and word 19 on 3.14t — and the scan is bounded
/// again by the object's own size, so this is a ceiling rather than a guess
const SLOTS: usize = 96;

/// why a frame could not be moved to an offset
#[derive(Debug)]
pub(crate) enum Unmarked {
    /// the mechanism could not be established, and nothing was written
    Unusable(Address),
    /// cpython refused the assignment, and nothing moved
    ///
    /// a refused `f_lineno` moves nothing and binds nothing — measured on 3.13,
    /// 3.14 and 3.14t — so the thread is still held exactly where it was
    Refused { error: PyErr },
}

/// move `frame` to `offset` of its own code object
///
/// the caller has already established that `offset` is at abstract stack depth
/// zero and runs nothing of the program before it returns. this only gets it
/// there, and says so if it could not
pub(crate) fn move_to(
    frame: &Bound<'_, PyAny>,
    code: &Bound<'_, PyAny>,
    offset: u32,
) -> PyResult<Result<(), Unmarked>> {
    let python = frame.py();
    let real: Bound<'_, PyBytes> = code.getattr("co_linetable")?.cast_into()?;
    let (Some(slot), Some(sentinel)) = (slot(python, code, &real)?, sentinel(code)?) else {
        return Ok(Err(Unmarked::Unusable(Address::Field)));
    };
    let doctored = PyBytes::new(
        python,
        &tail_marked(code, real.as_bytes(), offset, sentinel)?,
    );
    // held for the process, for the reason in the module note
    kept().push(doctored.clone().unbind());

    let outcome = {
        let _swapped = Swapped::over(code, slot, &real, &doctored);
        // **cpython reads the table back before the frame is touched.** the
        // encoder is bpd's, and a table that does not parse into exactly one
        // mark at exactly this offset would send the frame somewhere nobody
        // read. asking `co_lines()` is the interpreter parsing what was written,
        // with the frame still where it was and everything still undoable
        match marks_once(code, offset, sentinel) {
            Ok(true) => frame
                .setattr("f_lineno", sentinel)
                .map_err(|error| Unmarked::Refused { error }),
            Ok(false) => Err(Unmarked::Unusable(Address::Mark)),
            Err(error) => Err(Unmarked::Refused { error }),
        }
    };
    if let Err(why) = outcome {
        return Ok(Err(why));
    }

    let landed: u32 = frame.getattr("f_lasti")?.extract()?;
    assert_eq!(
        landed, offset,
        "the sentinel is the only line in the table bpd wrote, and cpython read \
         it back as one mark at this offset before the frame was moved, so the \
         move has exactly one destination to choose",
    );
    Ok(Ok(()))
}

/// a code object's linetable, replaced until this is dropped
///
/// a guard rather than a pair of writes, so that the table goes back on **every**
/// way out — an early return, a `PyErr`, or a panic unwinding out of the
/// assertion below. a code object left carrying a table bpd wrote is a debugger
/// that changed the program and did not change it back, which is worse than any
/// answer it could have been trying to give
struct Swapped<'py> {
    /// the word of the code object that holds the field
    word: *mut usize,
    /// the table that was there, held so that nothing can free it meanwhile
    real: Bound<'py, PyBytes>,
}

impl<'py> Swapped<'py> {
    /// put `doctored` in place of `real`
    fn over(
        code: &Bound<'py, PyAny>,
        slot: usize,
        real: &Bound<'py, PyBytes>,
        doctored: &Bound<'py, PyBytes>,
    ) -> Self {
        // SAFETY: `slot` was verified against this code object's own
        // `co_linetable` immediately before this, so the word at it is that
        // field. both objects are alive across the swap — `real` is held by this
        // guard and `doctored` is held for the process — so neither write
        // changes what anything owns
        let word = unsafe { code.as_ptr().cast::<usize>().add(slot) };
        unsafe { word.write(doctored.as_ptr() as usize) };
        Self {
            word,
            real: real.clone(),
        }
    }
}

impl Drop for Swapped<'_> {
    fn drop(&mut self) {
        // SAFETY: the same word `over` wrote, and the same object it read there
        unsafe { self.word.write(self.real.as_ptr() as usize) };
    }
}

/// whether the table now in place marks `offset`, and nothing else, with
/// `sentinel`
///
/// `co_lines()` coalesces adjacent ranges carrying the same line, which is the
/// same collapsing `marklines` does when it marks only on a change — so an
/// answer of exactly one range, starting here, carrying the sentinel, is an
/// answer that `marklines` will produce exactly one candidate
fn marks_once(code: &Bound<'_, PyAny>, offset: u32, sentinel: u32) -> PyResult<bool> {
    let mut carrying = Vec::new();
    for entry in code.call_method0("co_lines")?.try_iter()? {
        let (start, _end, line): (u32, u32, Option<u32>) = entry?.extract()?;
        if line == Some(sentinel) {
            carrying.push(start);
        }
    }
    Ok(carrying == [offset])
}

/// every doctored table this process has swapped in, kept alive
fn kept() -> std::sync::MutexGuard<'static, Vec<Py<PyBytes>>> {
    static KEPT: OnceLock<Mutex<Vec<Py<PyBytes>>>> = OnceLock::new();
    KEPT.get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("the kept list is only ever pushed to")
}

/// the word of `PyCodeObject` that holds `co_linetable`, verified against this
/// code object
///
/// calibrated once, against a code object bpd compiles itself so that the match
/// is unambiguous — swept over both stdlibs the slot is the same for every code
/// object, and unique for all but one of 31927 on 3.14, whose linetable is the
/// same bytes object as another field of its own. that one is why the
/// calibration uses a probe and the **use** is a check rather than another scan
fn slot(
    python: Python<'_>,
    code: &Bound<'_, PyAny>,
    real: &Bound<'_, PyBytes>,
) -> PyResult<Option<usize>> {
    static SLOT: OnceLock<Option<usize>> = OnceLock::new();
    let found = match SLOT.get() {
        Some(found) => *found,
        None => {
            let calibrated = calibrate(python)?;
            *SLOT.get_or_init(|| calibrated)
        }
    };
    let Some(slot) = found else {
        return Ok(None);
    };
    if slot >= words_of(code)? {
        return Ok(None);
    }
    // SAFETY: `slot` is inside the object's own reported size, so the read is of
    // this object's memory. it is a read, and a mismatch returns rather than
    // writing
    let held = unsafe { code.as_ptr().cast::<usize>().add(slot).read() };
    Ok((held == real.as_ptr() as usize).then_some(slot))
}

/// find the slot against a code object compiled here
fn calibrate(python: Python<'_>) -> PyResult<Option<usize>> {
    let builtins = python.import("builtins")?;
    let probe = builtins.call_method1(
        "compile",
        ("nought = 0\none = 1\ntwo = 2\n", "<bpd probe>", "exec"),
    )?;
    let table: Bound<'_, PyBytes> = probe.getattr("co_linetable")?.cast_into()?;
    let wanted = table.as_ptr() as usize;
    let limit = words_of(&probe)?.min(SLOTS);
    let words = probe.as_ptr().cast::<usize>();
    let mut found = None;
    for index in 0..limit {
        // SAFETY: bounded by the object's own reported size, and a read
        if unsafe { words.add(index).read() } == wanted {
            if found.is_some() {
                // two fields of the probe hold the same object, so neither one
                // is evidence about the other. nothing is written on this path
                return Ok(None);
            }
            found = Some(index);
        }
    }
    Ok(found)
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

/// a line number the code object does not have, and can be moved to
///
/// one past its largest, so `first_line_not_before` — which snaps a requested
/// line up to the first mark at or after it — lands on the sentinel and on
/// nothing else
fn sentinel(code: &Bound<'_, PyAny>) -> PyResult<Option<u32>> {
    let mut largest = 0u32;
    for entry in code.call_method0("co_lines")?.try_iter()? {
        let (_start, _end, line): (u32, u32, Option<u32>) = entry?.extract()?;
        largest = largest.max(line.unwrap_or(0));
    }
    let first: u32 = code.getattr("co_firstlineno")?.extract()?;
    Ok(largest.max(first).checked_add(1))
}

/// one location entry, as far as walking the table needs to understand it
struct Entry {
    /// where the entry starts in the table
    at: usize,
    /// how many bytes it occupies, its first byte included
    bytes: usize,
    /// how many code units it covers
    units: u32,
    /// what it does to the running start line, or `None` when it carries no
    /// location at all and leaves it alone
    delta: Option<i32>,
}

/// a linetable that reads like `table` below `offset` and marks `offset` with
/// `sentinel`
fn tail_marked(
    code: &Bound<'_, PyAny>,
    table: &[u8],
    offset: u32,
    sentinel: u32,
) -> PyResult<Vec<u8>> {
    let total = u32::try_from(code.getattr("co_code")?.len()?)
        .expect("a code object is not four billion bytes of bytecode")
        / 2;
    let target = offset / 2;
    assert!(
        target > 0 && target < total,
        "an exit offset is inside the code object and never its first unit"
    );

    let mut out: Vec<u8> = Vec::with_capacity(table.len() + 16);
    let mut running = i64::from(code.getattr("co_firstlineno")?.extract::<u32>()?);
    let mut unit = 0u32;
    for entry in entries(table) {
        if unit + entry.units <= target {
            out.extend_from_slice(&table[entry.at..entry.at + entry.bytes]);
            if let Some(delta) = entry.delta {
                running += i64::from(delta);
            }
            unit += entry.units;
            continue;
        }
        if unit < target {
            // the entry straddles the target, so its first half is re-emitted
            // carrying the same line and the rest of it is dropped
            let head = target - unit;
            match entry.delta {
                None => out.push(0x80 | (NO_LOCATION << 3) | u8::try_from(head - 1).unwrap_or(7)),
                Some(delta) => {
                    out.extend_from_slice(&emit(delta, head));
                    running += i64::from(delta);
                }
            }
        }
        break;
    }

    let delta = i64::from(sentinel) - running;
    let delta = i32::try_from(delta).expect("a source file is not two billion lines long");
    out.extend_from_slice(&emit(delta, total - target));
    Ok(out)
}

/// the entries of a location table, in order
///
/// an entry is a byte with the top bit set followed by every byte without it
fn entries(table: &[u8]) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < table.len() {
        let first = table[at];
        assert!(
            first & 0x80 != 0,
            "a location entry starts with the top bit set"
        );
        let kind = (first >> 3) & 15;
        let units = u32::from(first & 7) + 1;
        let mut end = at + 1;
        while end < table.len() && table[end] & 0x80 == 0 {
            end += 1;
        }
        out.push(Entry {
            at,
            bytes: end - at,
            units,
            delta: start_line_delta(kind, &table[at + 1..end]),
        });
        at = end;
    }
    out
}

/// what an entry of this kind does to the running start line
///
/// the kinds are `_PyCodeLocationInfoKind`: 0–9 carry column information and no
/// line change, 10–12 carry a delta of `kind - 10`, 13 and 14 both open with a
/// signed varint, and 15 carries no location and leaves the running line alone
fn start_line_delta(kind: u8, payload: &[u8]) -> Option<i32> {
    match kind {
        NO_LOCATION => None,
        0..=9 => Some(0),
        10..=12 => Some(i32::from(kind) - 10),
        _ => Some(signed(payload)),
    }
}

/// the signed varint a payload opens with
fn signed(payload: &[u8]) -> i32 {
    let mut value: i64 = 0;
    let mut shift = 0;
    for byte in payload {
        value |= i64::from(byte & 0x3F) << shift;
        shift += 6;
        if byte & 0x40 == 0 {
            break;
        }
        if shift >= 60 {
            break;
        }
    }
    let magnitude = i32::try_from(value >> 1).unwrap_or(i32::MAX);
    if value & 1 == 1 {
        -magnitude
    } else {
        magnitude
    }
}

/// entries carrying `delta` over `units` code units, in the no-column form
///
/// an entry covers at most eight units, so a long tail takes several — and only
/// the first carries the delta, which is what keeps `marklines` to one mark
fn emit(delta: i32, units: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut left = units;
    let mut first = true;
    while left > 0 {
        let take = left.min(PER_ENTRY);
        out.push(
            0x80 | (NO_COLUMNS << 3)
                | u8::try_from(take - 1).expect("an entry covers at most eight units"),
        );
        out.extend_from_slice(&varint(signed_to_unsigned(if first { delta } else { 0 })));
        left -= take;
        first = false;
    }
    out
}

/// the unsigned form a signed varint is written as
///
/// the magnitude doubled, with the sign in the low bit. `i32` widened first,
/// because `-i32::MIN` does not fit in an `i32`
fn signed_to_unsigned(value: i32) -> u64 {
    let wide = i64::from(value);
    (wide.unsigned_abs() << 1) | u64::from(wide < 0)
}

/// an unsigned varint, six bits to a byte, bit six set on every byte but the last
fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while value >= 64 {
        out.push(u8::try_from(value & 0x3F).expect("six bits fit in a byte") | 0x40);
        value >>= 6;
    }
    out.push(u8::try_from(value).expect("six bits fit in a byte"));
    out
}
