//! why the program's ui recomposed: reading basedpython-ui's trace ring, and
//! forwarding what the runtime announces
//!
//! the runtime of basedpython-ui keeps a bounded ring of records — why every
//! scope ran, what every state write did, what every frame cost — under a
//! documented layout, and raises `basedpython_ui.trace` for every record it
//! appends. this module is the reader of both
//!
//! ## storage only, and no import
//!
//! the module is found in `sys.modules` and never imported: importing a package
//! the program never asked for is the debugger changing the program, and doing
//! it from inside an audit hook is the trap that corrupted line numbers once
//! already. `sys.modules` itself is read through `PySys_GetObject`, which is a
//! dictionary lookup on the `sys` module and runs no import machinery — a
//! `python.import("sys")` would go through `builtins.__import__`, which a
//! program can replace, and run it once per record on the ui thread. every
//! object on the way to the ring is read through its instance dictionary —
//! [`crate::storage::instance_dict`] — and never through an attribute, because
//! an attribute read is the class's `__getattribute__`. the runtime's own
//! documentation promises ordinary instance dictionaries and exact lists for
//! exactly this reason
//!
//! ## every slot is checked, and nothing is guessed
//!
//! a record is a tuple of exact builtins, and the layout says which builtin is
//! in which slot. a slot holding anything else refuses the whole answer, naming
//! the record kind, the slot and what was there — the layout is fixed per
//! `TRACE_FORMAT`, so a slot that does not read by it is a record this reader
//! does not understand, and a value read by the wrong layout would mean
//! something else
//!
//! the value slots — `old` and `new` of a state cause, of an args cause and of
//! a derived cause — are the exception, because they hold whatever the
//! program stored. they are rendered by exact type and never by calling
//! anything of the object's: `repr` is the program's code, and so are
//! `__len__` and `__str__`. a text slot holding a lone surrogate — a `str`
//! with no UTF-8 spelling — is spelled the way `repr` spells one, `\udcff`,
//! which is what a value slot's `repr` already gives: the character is named
//! rather than the record refused with a sentence about its type that is not
//! true
//!
//! ## the stream
//!
//! while a client is watching, the audit hook reads the announced record off
//! the event's argument tuple, on the thread that appended it, with the GIL
//! held, by the same layout and under the same bounds as a read at a stop, and
//! hands it to [`crate::stream`] — a bounded queue that a thread of the
//! agent's own writes, so the ui thread never waits on the connection. which
//! runtime wrote it is found by identity: the announced tuple is the one the
//! runtime just appended, so it is the **last** entry of that runtime's ring,
//! and nothing but the last entry is compared. a tuple that is not the last
//! entry of any live runtime's ring, or does not read by the layout, is not
//! forwarded: the stream carries the runtime's records and a program raising
//! the event by hand — with a stale tuple out of the ring, or one in no ring —
//! is not the runtime. the ring read at a stop is where a record that does not
//! read is refused by name
//!
//! a watch is accepted before the program has imported the runtime. watching
//! is an interest in records to come, and only the read at a stop needs a ring
//! to exist

use bpd_core::recompose::{
    Cause, Disposed, Key, Location, Origin, RECORDS_KEPT, Recompositions, TRACE_FORMAT, TraceRecord,
};
use bpd_core::{Kept, Refusal};
use bpd_protocol::message::FromAgent;
use pyo3::exceptions::PyOSError;
use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyBytes, PyDict, PyFloat, PyFrozenSet, PyInt, PyList, PyModule, PySet, PyString,
    PyTuple,
};
use pyo3::{ffi, intern};

use crate::{sources, storage, stream, world};

/// the module the ring lives in
const MODULE: &str = "basedpython_ui.runtime";

/// how much of a rendered value is kept
const TEXT: usize = 64;

/// what stands between a request and its answer
///
/// a refusal is an answer — the request was understood and the program is not
/// in a state it can be answered about — and a python error is the agent's
/// own, handed back as `CouldNotAnswer` by the stop loop
enum Fail {
    Refused(Refusal),
    Python(PyErr),
}

impl From<PyErr> for Fail {
    fn from(error: PyErr) -> Self {
        Self::Python(error)
    }
}

impl From<Refusal> for Fail {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// a record that does not read by the layout
fn unreadable(what: impl Into<String>) -> Fail {
    Fail::Refused(Refusal::UiTraceUnreadable { what: what.into() })
}

/// the answer, or the refusal, to the engine's request for the ring
pub(crate) fn read(python: Python<'_>) -> PyResult<FromAgent> {
    match ring(python) {
        Ok(recompositions) => Ok(FromAgent::Recompositions { recompositions }),
        Err(Fail::Refused(reason)) => Ok(FromAgent::Refused { reason }),
        Err(Fail::Python(error)) => Err(error),
    }
}

/// start or stop forwarding announced records, and say what is set
///
/// not refused for a program that has not imported the runtime yet: watching
/// is an interest in records to come, and a program that imports the runtime
/// later is watched from then on. what comes back is read off the flag
/// rather than echoed
///
/// # errors
///
/// when the stream's writer could not be put on the process — a descriptor or
/// a thread the operating system refused — raised as an `OSError` naming it,
/// which the stop loop hands back as the agent's own failure
pub(crate) fn watch(on: bool) -> PyResult<FromAgent> {
    match stream::watch(on) {
        Ok(on) => Ok(FromAgent::Watching { on }),
        Err(error) => Err(PyOSError::new_err(format!(
            "the thread that writes the trace stream could not be started: {error}"
        ))),
    }
}

/// the runtime announced a record — forward it, if a client is watching
///
/// called from the audit hook, which already compared the event's name. the
/// arguments are the event's tuple, whose one element is the record. nothing
/// here writes to the connection: the rendered record goes to the stream's
/// queue, and a thread of the agent's own takes it from there
pub(crate) fn announced(python: Python<'_>, arguments: Option<&Bound<'_, PyAny>>) {
    if !stream::watching() {
        return;
    }
    let Some(arguments) = arguments else {
        return;
    };
    let Ok(record) = arguments.get_item(0) else {
        return;
    };
    let Ok(Some(runtime)) = runtime_of(python, &record) else {
        return;
    };
    if let Ok(record) = convert(&record, runtime) {
        stream::queue(record);
    }
}

/// `sys.modules`, read without the import machinery
///
/// `PySys_GetObject` is a lookup in the `sys` module's own dictionary, which
/// is what makes it fit for an audit hook that runs once per record on the ui
/// thread: `python.import("sys")` is `PyImport_Import`, which calls
/// `builtins.__import__` — a function the program can replace, and one it
/// would then see the debugger calling for every scope run
fn modules(python: Python<'_>) -> Result<Bound<'_, PyDict>, Fail> {
    // SAFETY: `PySys_GetObject` takes a nul-terminated name and returns a
    // borrowed reference to that attribute of the `sys` module, or null when
    // there is none, without setting an exception. it is called with the GIL
    // held, and the reference it returns is owned by the `sys` module's
    // dictionary, which outlives this call — `from_borrowed_ptr_or_opt` takes
    // a reference of its own while that borrow is live
    #[expect(
        unsafe_code,
        reason = "reading `sys.modules` without `builtins.__import__` is a raw \
                  `PySys_GetObject` call — see above"
    )]
    let found = unsafe {
        let pointer = ffi::PySys_GetObject(c"modules".as_ptr());
        Bound::from_borrowed_ptr_or_opt(python, pointer)
    };
    let Some(found) = found else {
        return Err(unreadable(
            "`sys` has no `modules`, and this reader looks the runtime module up in it",
        ));
    };
    if !found.is_exact_instance_of::<PyDict>() {
        return Err(unreadable(format!(
            "`sys.modules` is a {} and this reader looks the runtime module up \
             in a dict",
            type_name(&found)
        )));
    }
    Ok(found
        .cast_into::<PyDict>()
        .expect("an exact dict casts to one"))
}

/// the module, when the program has imported it
///
/// off `sys.modules` and never by importing. an entry under the module's name
/// that is not a module — a lazy proxy, a test double — is not the runtime the
/// layout describes and not an absence either, so it is refused naming what
/// was there rather than as a program that never imported the runtime
fn runtime_module(python: Python<'_>) -> Result<Option<Bound<'_, PyModule>>, Fail> {
    let Some(entry) = modules(python)?.get_item(intern!(python, MODULE))? else {
        return Ok(None);
    };
    if !entry.is_instance_of::<PyModule>() {
        return Err(unreadable(format!(
            "`sys.modules[{MODULE:?}]` is a {}, which is not a module, and the \
             layout puts the runtime module there",
            type_name(&entry)
        )));
    }
    Ok(Some(
        entry
            .cast_into::<PyModule>()
            .expect("an instance of module casts to one"),
    ))
}

/// the module's globals, which is where the format and the runtimes are
fn globals(python: Python<'_>) -> Result<Bound<'_, PyDict>, Fail> {
    let Some(module) = runtime_module(python)? else {
        return Err(Refusal::NoUiRuntime.into());
    };
    Ok(module.dict())
}

/// every live runtime, in the order the module keeps them
fn live_runtimes<'py>(globals: &Bound<'py, PyDict>) -> Result<Bound<'py, PyList>, Fail> {
    let Some(runtimes) = globals.get_item("live_runtimes")? else {
        return Err(unreadable(format!(
            "`live_runtimes` is missing from {MODULE}, and the layout puts the \
             runtimes there"
        )));
    };
    if !runtimes.is_exact_instance_of::<PyList>() {
        return Err(unreadable(format!(
            "`live_runtimes` of {MODULE} is a {} and the layout says list",
            type_name(&runtimes)
        )));
    }
    Ok(runtimes
        .cast_into::<PyList>()
        .expect("an exact list casts to one"))
}

/// one runtime's ring, or `None` when its tracing is off
fn ring_of<'py>(
    runtime: &Bound<'py, PyAny>,
    index: usize,
) -> Result<Option<(Bound<'py, PyList>, u64)>, Fail> {
    let Some(fields) = storage::instance_dict(runtime)? else {
        return Err(unreadable(format!(
            "runtime {index} keeps no instance dictionary, and the layout reads \
             its `trace` out of one"
        )));
    };
    let Some(trace) = fields.get_item("trace")? else {
        return Err(unreadable(format!(
            "runtime {index} has no `trace`, and the layout says a Trace or None"
        )));
    };
    if trace.is_none() {
        return Ok(None);
    }
    let Some(held) = storage::instance_dict(&trace)? else {
        return Err(unreadable(format!(
            "the trace of runtime {index} keeps no instance dictionary, and the \
             layout reads its ring out of one"
        )));
    };
    let records = named(&held, "records", index)?;
    if !records.is_exact_instance_of::<PyList>() {
        return Err(unreadable(format!(
            "`records` of runtime {index}'s trace is a {} and the layout says list",
            type_name(&records)
        )));
    }
    let dropped = named(&held, "dropped", index)?;
    let dropped = exact_uint(&dropped).ok_or_else(|| {
        unreadable(format!(
            "`dropped` of runtime {index}'s trace is a {} and the layout says a \
             non-negative int",
            type_name(&dropped)
        ))
    })?;
    // read for its type alone: a ring whose limit is not an int is a ring of
    // some other layout, and the count of what fell off it is not to be
    // believed either
    let limit = named(&held, "limit", index)?;
    if exact_uint(&limit).is_none() {
        return Err(unreadable(format!(
            "`limit` of runtime {index}'s trace is a {} and the layout says a \
             non-negative int",
            type_name(&limit)
        )));
    }
    Ok(Some((
        records
            .cast_into::<PyList>()
            .expect("an exact list casts to one"),
        dropped,
    )))
}

/// one field of a trace's instance dictionary
fn named<'py>(
    held: &Bound<'py, PyDict>,
    name: &str,
    index: usize,
) -> Result<Bound<'py, PyAny>, Fail> {
    held.get_item(name)?.ok_or_else(|| {
        unreadable(format!(
            "the trace of runtime {index} has no `{name}`, and the layout puts \
             one there"
        ))
    })
}

/// the whole ring, as it stands
fn ring(python: Python<'_>) -> Result<Recompositions, Fail> {
    let globals = globals(python)?;

    match globals.get_item("TRACE_FORMAT")? {
        None => {
            return Err(Refusal::UiTraceFormat {
                found: None,
                wanted: TRACE_FORMAT,
            }
            .into());
        }
        Some(found) => {
            if !found.is_exact_instance_of::<PyInt>() {
                return Err(unreadable(format!(
                    "`TRACE_FORMAT` of {MODULE} is a {} and the layout says int",
                    type_name(&found)
                )));
            }
            let Ok(found) = found.extract::<i64>() else {
                return Err(unreadable(format!(
                    "`TRACE_FORMAT` of {MODULE} does not fit an integer this reader \
                     compares"
                )));
            };
            if found != i64::from(TRACE_FORMAT) {
                return Err(Refusal::UiTraceFormat {
                    found: Some(found),
                    wanted: TRACE_FORMAT,
                }
                .into());
            }
        }
    }

    let runtimes = live_runtimes(&globals)?;
    let mut records = Vec::new();
    let mut dropped = 0_u64;
    let mut tracing = false;
    for (index, runtime) in runtimes.iter().enumerate() {
        let Some((ring, fell_off)) = ring_of(&runtime, index)? else {
            continue;
        };
        tracing = true;
        let runtime = u32::try_from(index).map_err(|_| {
            unreadable(format!(
                "runtime {index} is beyond the {} runtimes an answer can number",
                u32::MAX
            ))
        })?;
        for record in ring.iter() {
            records.push(convert(&record, runtime)?);
        }
        dropped += fell_off;
    }

    // a runtime that exists with tracing off is a refusal rather than an empty
    // answer. no runtime at all is not: the program imported the module and
    // made nothing yet, and there is nothing to be off
    if !runtimes.is_empty() && !tracing {
        return Err(Refusal::UiTracingOff.into());
    }

    // newest kept, oldest counted. the runtime's own ring defaults to the same
    // bound, so this ordinarily leaves out nothing
    if records.len() > RECORDS_KEPT {
        let cut = records.len() - RECORDS_KEPT;
        records.drain(..cut);
        dropped += u64::try_from(cut).expect("a count of records fits");
    }

    Ok(Recompositions {
        format: TRACE_FORMAT,
        runtimes: u32::try_from(runtimes.len())
            .expect("fewer than u32::MAX runtimes were numbered above"),
        tracing,
        records: Kept::counted(records, dropped),
        mode: world::mode(),
    })
}

/// which live runtime's ring an announced record is the last entry of
///
/// `None` when it is none of theirs — a record the runtime announced before
/// appending, an entry of a ring that is not its last, or an event the program
/// raised by hand. **only the last entry is compared**: the runtime appends
/// before it announces, in `Trace.append` and in the hot append it inlines,
/// so the record it just announced is the last one it holds. a tuple found
/// anywhere else in a ring is a record the runtime announced once already,
/// and forwarding it again would show a client the ui going back in time. the
/// identity comparison is a pointer compare, and it reaches nothing of the
/// program's
fn runtime_of(python: Python<'_>, record: &Bound<'_, PyAny>) -> Result<Option<u32>, Fail> {
    let runtimes = live_runtimes(&globals(python)?)?;
    for (index, runtime) in runtimes.iter().enumerate() {
        let Some((ring, _)) = ring_of(&runtime, index)? else {
            continue;
        };
        let length = ring.len();
        if length == 0 {
            continue;
        }
        if ring.get_item(length - 1)?.is(record) {
            return Ok(u32::try_from(index).ok());
        }
    }
    Ok(None)
}

/// the slots of one tuple, read by name so a refusal can say which
struct Slots<'a, 'py> {
    tuple: &'a Bound<'py, PyTuple>,
    /// what the tuple is, as a refusal names it: a run record, a state cause
    what: String,
}

impl<'py> Slots<'_, 'py> {
    fn of<'a>(
        value: &'a Bound<'py, PyAny>,
        what: String,
        slots: usize,
    ) -> Result<Slots<'a, 'py>, Fail> {
        let Ok(tuple) = value.cast::<PyTuple>() else {
            return Err(unreadable(format!(
                "{what} is a {} and the layout says tuple",
                type_name(value)
            )));
        };
        if !value.is_exact_instance_of::<PyTuple>() {
            return Err(unreadable(format!(
                "{what} is a {}, a subclass of tuple, and the layout says an exact one",
                type_name(value)
            )));
        }
        if tuple.len() != slots {
            return Err(unreadable(format!(
                "{what} has {} slots and the layout has {slots}",
                tuple.len()
            )));
        }
        Ok(Slots { tuple, what })
    }

    fn slot(&self, at: usize) -> Result<Bound<'py, PyAny>, Fail> {
        Ok(self.tuple.get_item(at)?)
    }

    fn wrong(&self, at: usize, name: &str, found: &Bound<'py, PyAny>, wanted: &str) -> Fail {
        unreadable(format!(
            "slot {at} (`{name}`) of {} is a {} and the layout says {wanted}",
            self.what,
            type_name(found)
        ))
    }

    fn uint(&self, at: usize, name: &str) -> Result<u64, Fail> {
        let value = self.slot(at)?;
        exact_uint(&value).ok_or_else(|| self.wrong(at, name, &value, "a non-negative int"))
    }

    fn line(&self, at: usize, name: &str) -> Result<u32, Fail> {
        let value = self.slot(at)?;
        exact_uint(&value)
            .and_then(|line| u32::try_from(line).ok())
            .ok_or_else(|| self.wrong(at, name, &value, "a line number"))
    }

    fn uint_or_none(&self, at: usize, name: &str) -> Result<Option<u64>, Fail> {
        let value = self.slot(at)?;
        if value.is_none() {
            return Ok(None);
        }
        exact_uint(&value)
            .map(Some)
            .ok_or_else(|| self.wrong(at, name, &value, "a non-negative int or None"))
    }

    fn line_or_none(&self, at: usize, name: &str) -> Result<Option<u32>, Fail> {
        let value = self.slot(at)?;
        if value.is_none() {
            return Ok(None);
        }
        exact_uint(&value)
            .and_then(|line| u32::try_from(line).ok())
            .map(Some)
            .ok_or_else(|| self.wrong(at, name, &value, "a line number or None"))
    }

    fn text(&self, at: usize, name: &str) -> Result<String, Fail> {
        let value = self.slot(at)?;
        exact_text(&value).ok_or_else(|| self.wrong(at, name, &value, "str"))
    }

    fn text_or_none(&self, at: usize, name: &str) -> Result<Option<String>, Fail> {
        let value = self.slot(at)?;
        if value.is_none() {
            return Ok(None);
        }
        exact_text(&value)
            .map(Some)
            .ok_or_else(|| self.wrong(at, name, &value, "str or None"))
    }

    fn boolean(&self, at: usize, name: &str) -> Result<bool, Fail> {
        let value = self.slot(at)?;
        if !value.is_exact_instance_of::<PyBool>() {
            return Err(self.wrong(at, name, &value, "bool"));
        }
        Ok(value.is_truthy()?)
    }

    /// an int, a str or None
    fn key(&self, at: usize, name: &str) -> Result<Option<Key>, Fail> {
        let value = self.slot(at)?;
        if value.is_none() {
            return Ok(None);
        }
        if value.is_exact_instance_of::<PyInt>() {
            let Ok(number) = value.extract::<i64>() else {
                return Err(self.wrong(
                    at,
                    name,
                    &value,
                    "an int that fits 64 bits, a str or None",
                ));
            };
            return Ok(Some(Key::Int(number)));
        }
        exact_text(&value)
            .map(|text| Some(Key::Text(text)))
            .ok_or_else(|| self.wrong(at, name, &value, "an int, a str or None"))
    }

    /// the value slot of a state cause, rendered without running the program
    fn rendered(&self, at: usize) -> Result<String, Fail> {
        Ok(rendered(&self.slot(at)?))
    }

    fn tuple(&self, at: usize, name: &str) -> Result<Bound<'py, PyTuple>, Fail> {
        let value = self.slot(at)?;
        if !value.is_exact_instance_of::<PyTuple>() {
            return Err(self.wrong(at, name, &value, "tuple"));
        }
        Ok(value
            .cast_into::<PyTuple>()
            .expect("an exact tuple casts to one"))
    }

    /// a tuple of ints
    fn uints(&self, at: usize, name: &str) -> Result<Vec<u64>, Fail> {
        let items = self.tuple(at, name)?;
        let mut out = Vec::with_capacity(items.len());
        for item in items.iter() {
            let Some(number) = exact_uint(&item) else {
                return Err(unreadable(format!(
                    "slot {at} (`{name}`) of {} holds a {} and the layout says a tuple of int",
                    self.what,
                    type_name(&item)
                )));
            };
            out.push(number);
        }
        Ok(out)
    }

    /// a location the runtime wrote as a file and a line, through the map
    fn location(&self, file: usize, line: usize, name: &str) -> Result<Location, Fail> {
        let file = self.text(file, &format!("{name}_file"))?;
        let line = self.line(line, &format!("{name}_line"))?;
        Ok(located(file, line))
    }

    /// the same, where both halves may be None together
    fn location_or_none(
        &self,
        file: usize,
        line: usize,
        name: &str,
    ) -> Result<Option<Location>, Fail> {
        let file_name = format!("{name}_file");
        let line_name = format!("{name}_line");
        match (
            self.text_or_none(file, &file_name)?,
            self.line_or_none(line, &line_name)?,
        ) {
            (Some(found_file), Some(found_line)) => Ok(Some(located(found_file, found_line))),
            (None, None) => Ok(None),
            (found_file, found_line) => Err(unreadable(format!(
                "slots {file} and {line} (`{file_name}`, `{line_name}`) of {} are {} and \
                 {}, and the layout has them None together or neither",
                self.what,
                found_file.map_or("None", |_| "a str"),
                found_line.map_or("None", |_| "an int")
            ))),
        }
    }
}

/// a location as every frame reports one: through the build's map
fn located(file: String, line: u32) -> Location {
    let reported = sources::locate(file, line);
    Location::mapped(reported.file, reported.line, reported.mapping)
}

/// one record tuple, by the layout of its kind
fn convert(value: &Bound<'_, PyAny>, runtime: u32) -> Result<TraceRecord, Fail> {
    let Ok(tuple) = value.cast::<PyTuple>() else {
        return Err(unreadable(format!(
            "a record is a {} and the layout says tuple",
            type_name(value)
        )));
    };
    if tuple.is_empty() {
        return Err(unreadable(
            "a record is an empty tuple, and the layout puts its kind first",
        ));
    }
    let kind = tuple.get_item(0)?;
    let Some(kind) = exact_uint(&kind) else {
        return Err(unreadable(format!(
            "slot 0 (the kind) of a record is a {} and the layout says int",
            type_name(&kind)
        )));
    };
    match kind {
        1 => run(value, runtime),
        2 => {
            let slots = Slots::of(value, "a `write` record".to_string(), 3)?;
            Ok(TraceRecord::Write {
                runtime,
                frame: slots.uint(1, "frame")?,
                cause: cause(&slots.slot(2)?, "slot 2 (`cause`) of a `write` record")?,
            })
        }
        3 => {
            let slots = Slots::of(value, "a `frame` record".to_string(), 6)?;
            Ok(TraceRecord::Frame {
                runtime,
                frame: slots.uint(1, "frame")?,
                runs: slots.uint(2, "runs")?,
                skips: slots.uint(3, "skips")?,
                compose_ns: slots.uint(4, "compose_ns")?,
                commit_ns: slots.uint(5, "commit_ns")?,
            })
        }
        4 => {
            let slots = Slots::of(value, "an `error` record".to_string(), 6)?;
            Ok(TraceRecord::Error {
                runtime,
                frame: slots.uint(1, "frame")?,
                scope: slots.uint(2, "scope_id")?,
                name: slots.text(3, "name")?,
                error: slots.text(4, "error")?,
                kept_previous: slots.boolean(5, "kept_previous")?,
            })
        }
        5 => {
            let slots = Slots::of(value, "a `refused` record".to_string(), 5)?;
            Ok(TraceRecord::Refused {
                runtime,
                frame: slots.uint(1, "frame")?,
                scope: slots.uint(2, "scope_id")?,
                name: slots.text(3, "name")?,
                what: slots.text(4, "what")?,
            })
        }
        other => Err(unreadable(format!(
            "a record's kind is {other}, and the layout has kinds 1 to 5"
        ))),
    }
}

/// a run record
fn run(value: &Bound<'_, PyAny>, runtime: u32) -> Result<TraceRecord, Fail> {
    let slots = Slots::of(value, "a `run` record".to_string(), 15)?;
    let origin = slots.text(10, "origin")?;
    let origin = match origin.as_str() {
        "first" => Origin::First,
        "self" => Origin::Itself,
        "parent" => Origin::Parent,
        other => {
            return Err(unreadable(format!(
                "slot 10 (`origin`) of a `run` record is {other:?} and the layout \
                 says \"first\", \"self\" or \"parent\""
            )));
        }
    };
    let causes = slots.tuple(11, "causes")?;
    if causes.is_empty() {
        return Err(unreadable(
            "slot 11 (`causes`) of a `run` record is empty, and the layout says never empty",
        ));
    }
    let causes = causes
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            cause(
                &entry,
                &format!("entry {index} of slot 11 (`causes`) of a `run` record"),
            )
        })
        .collect::<Result<Vec<_>, Fail>>()?;
    let disposed = slots
        .tuple(13, "disposed")?
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let entry = Slots::of(
                &entry,
                format!("entry {index} of slot 13 (`disposed`) of a `run` record"),
                3,
            )?;
            Ok(Disposed {
                scope: entry.uint(0, "id")?,
                name: entry.text(1, "name")?,
                key: entry.key(2, "key")?,
            })
        })
        .collect::<Result<Vec<_>, Fail>>()?;
    Ok(TraceRecord::Run {
        runtime,
        frame: slots.uint(1, "frame")?,
        scope: slots.uint(2, "scope_id")?,
        parent: slots.uint_or_none(3, "parent_id")?,
        name: slots.text(4, "name")?,
        defined: slots.location(5, 6, "def")?,
        called: slots.location_or_none(7, 8, "call")?,
        key: slots.key(9, "key")?,
        origin,
        causes,
        skipped: slots.uints(12, "skipped")?,
        disposed,
        elapsed_ns: slots.uint(14, "elapsed_ns")?,
    })
}

/// one cause tuple, by the layout of its kind
///
/// `where` is what the refusal names when the tuple itself is wrong — the slot
/// of the record it sits in
fn cause(value: &Bound<'_, PyAny>, place: &str) -> Result<Cause, Fail> {
    let Ok(tuple) = value.cast::<PyTuple>() else {
        return Err(unreadable(format!(
            "{place} is a {} and the layout says a cause tuple",
            type_name(value)
        )));
    };
    if tuple.is_empty() {
        return Err(unreadable(format!(
            "{place} is an empty tuple, and the layout puts a cause's kind first"
        )));
    }
    let kind = tuple.get_item(0)?;
    let Some(kind) = exact_text(&kind) else {
        return Err(unreadable(format!(
            "slot 0 (the kind) of the cause at {place} is a {} and the layout says str",
            type_name(&kind)
        )));
    };
    let what = format!("a `{kind}` cause");
    match kind.as_str() {
        "created" => {
            Slots::of(value, what, 1)?;
            Ok(Cause::Created)
        }
        "state" => state_cause(&Slots::of(value, what, 15)?),
        "derived" => derived_cause(&Slots::of(value, what, 9)?),
        "invalidated" => {
            Slots::of(value, what, 1)?;
            Ok(Cause::Invalidated)
        }
        "args" => {
            let slots = Slots::of(value, what, 5)?;
            Ok(Cause::Args {
                parameter: slots.text(1, "parameter")?,
                old: slots.rendered(2)?,
                new: slots.rendered(3)?,
                compared: slots.boolean(4, "compared")?,
            })
        }
        "inline" => {
            Slots::of(value, what, 1)?;
            Ok(Cause::Inline)
        }
        "recovery" => {
            let slots = Slots::of(value, what, 2)?;
            Ok(Cause::Recovery {
                error: slots.text(1, "error")?,
            })
        }
        "uncommitted" => {
            Slots::of(value, what, 1)?;
            Ok(Cause::Uncommitted)
        }
        "dirty" => {
            let slots = Slots::of(value, what, 2)?;
            let inner = slots.tuple(1, "inner_causes")?;
            Ok(Cause::Dirty {
                causes: inner
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        cause(
                            &entry,
                            &format!("entry {index} of slot 1 (`inner_causes`) of a `dirty` cause"),
                        )
                    })
                    .collect::<Result<Vec<_>, Fail>>()?,
            })
        }
        other => Err(unreadable(format!(
            "the cause at {place} is {other:?}, which is not a kind the layout has"
        ))),
    }
}

/// a state cause, whose two value slots are rendered rather than read
fn state_cause(slots: &Slots<'_, '_>) -> Result<Cause, Fail> {
    Ok(Cause::State {
        cell: slots.uint(1, "cell_id")?,
        kind: slots.text(2, "cell_kind")?,
        op: slots.text(3, "op")?,
        at: slots.key(4, "at")?,
        old: slots.rendered(5)?,
        new: slots.rendered(6)?,
        declared: slots.location_or_none(7, 8, "decl")?,
        declared_name: slots.text_or_none(9, "decl_name")?,
        written: slots.location(10, 11, "write")?,
        thread: slots.uint(12, "thread")?,
        posted: slots.boolean(13, "posted")?,
        readers: slots.uint(14, "readers")?,
    })
}

/// a derived cause, which carries the cause that made it recompute
fn derived_cause(slots: &Slots<'_, '_>) -> Result<Cause, Fail> {
    Ok(Cause::Derived {
        derived: slots.uint(1, "derived_id")?,
        declared: slots.location_or_none(2, 3, "decl")?,
        declared_name: slots.text_or_none(4, "decl_name")?,
        old: slots.rendered(5)?,
        new: slots.rendered(6)?,
        changed: slots.boolean(7, "changed")?,
        because: Box::new(cause(
            &slots.slot(8)?,
            "slot 8 (`because`) of a `derived` cause",
        )?),
    })
}

/// an exact `int` that is not negative, as a number
///
/// `bool` is a subclass of `int` and is not one of these: the exact check is
/// what keeps a `True` in an int slot from reading as 1
fn exact_uint(value: &Bound<'_, PyAny>) -> Option<u64> {
    if !value.is_exact_instance_of::<PyInt>() {
        return None;
    }
    value.extract::<u64>().ok()
}

/// an exact `str`, as text
///
/// a `str` holding a lone surrogate — `str(OSError)` over a `surrogateescape`d
/// file name is one — has no UTF-8 spelling. it is spelled the way `repr`
/// spells it, `\udcff`, through `str.encode` with `backslashreplace`: the
/// exact type is what makes that cpython's own method rather than anything
/// of the program's, as it is for the `repr` a value slot goes through. that
/// names the character rather than losing it, and never produces the sentence
/// "is a str and the layout says str" for a slot that is one
fn exact_text(value: &Bound<'_, PyAny>) -> Option<String> {
    if !value.is_exact_instance_of::<PyString>() {
        return None;
    }
    let text = value.cast::<PyString>().ok()?;
    Some(match text.to_cow() {
        Ok(text) => text.into_owned(),
        Err(_) => escaped(text).unwrap_or_else(|| text.to_string_lossy().into_owned()),
    })
}

/// a `str` no UTF-8 can spell, with each such character written as `repr`
/// writes it
///
/// `None` only if `str.encode` itself failed, which `backslashreplace` does
/// not do for any `str`; the caller falls back to replacing the character so
/// that a `str` is never refused as not being one
fn escaped(text: &Bound<'_, PyString>) -> Option<String> {
    let python = text.py();
    let encoded = text
        .call_method1(
            intern!(python, "encode"),
            (
                intern!(python, "utf-8"),
                intern!(python, "backslashreplace"),
            ),
        )
        .ok()?;
    let bytes = encoded.cast::<PyBytes>().ok()?;
    String::from_utf8(bytes.as_bytes().to_vec()).ok()
}

/// a type's name, for a refusal that says what was there instead
fn type_name(value: &Bound<'_, PyAny>) -> String {
    value
        .get_type()
        .qualname()
        .and_then(|name| name.extract::<String>())
        .unwrap_or_else(|_| "?".to_string())
}

/// a stored value as text, **without running any of the program**
///
/// the trail's rule. an exact builtin scalar renders as itself through the
/// type's own slot — the exact check is what makes that cpython's code rather
/// than a subclass's — an exact builtin container by its kind and size, read
/// through the concrete accessor, and anything else says what it is. that is a
/// weaker answer than a `repr` and it is one that cannot be wrong
fn rendered(value: &Bound<'_, PyAny>) -> String {
    if value.is_none() {
        return "None".to_string();
    }
    if value.is_exact_instance_of::<PyBool>()
        || value.is_exact_instance_of::<PyInt>()
        || value.is_exact_instance_of::<PyFloat>()
        || value.is_exact_instance_of::<PyString>()
    {
        return value.repr().map_or_else(
            |_| format!("a {}", type_name(value)),
            |text| cut(&text.to_string_lossy()),
        );
    }
    if let Ok(items) = value.cast::<PyList>()
        && value.is_exact_instance_of::<PyList>()
    {
        return format!("list[{}]", items.len());
    }
    if let Ok(items) = value.cast::<PyTuple>()
        && value.is_exact_instance_of::<PyTuple>()
    {
        return format!("tuple[{}]", items.len());
    }
    if let Ok(items) = value.cast::<PyDict>()
        && value.is_exact_instance_of::<PyDict>()
    {
        return format!("dict{{{}}}", items.len());
    }
    if let Ok(items) = value.cast::<PySet>()
        && value.is_exact_instance_of::<PySet>()
    {
        return format!("set{{{}}}", items.len());
    }
    if let Ok(items) = value.cast::<PyFrozenSet>()
        && value.is_exact_instance_of::<PyFrozenSet>()
    {
        return format!("frozenset{{{}}}", items.len());
    }
    format!("a {}", type_name(value))
}

/// the first [`TEXT`] characters, with the cut marked
fn cut(text: &str) -> String {
    let mut kept: String = text.chars().take(TEXT).collect();
    if kept.len() < text.len() {
        kept.push('…');
    }
    kept
}
