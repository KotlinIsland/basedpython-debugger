//! what is holding an object, and how
//!
//! "why is this still alive". the collector already knows — it has to, to decide
//! what to free — and `gc.get_referrers` is the way in
//!
//! ## the three things this was expected to need, and did not
//!
//! all three were measured before any of it was built:
//!
//! - **it does not need `unsafe`.** the assumption was a native walk over the
//!   referent graph, which means `tp_traverse`, which `unsafe_code = "deny"`
//!   forbids workspace-wide. `gc.get_referrers` is C-implemented and reaches the
//!   same graph from safe code
//! - **it is not slow.** 0.7 ms on a 55,000-object heap, 2.5 ms on 205,000 and
//!   9.8 ms on 805,000 — linear, and an interactive answer at every size
//! - **it does not perturb the heap.** the object count was identical before and
//!   after, once the answer was dropped. the version that keeps an index in
//!   python does not have that property: it grew a 55,000-object heap to 167,000
//!
//! ## and the one thing it does need, which is the opposite of what was expected
//!
//! the concern was that bpd's own frames would show up as retainers and have to
//! be filtered out. measured on 3.13, 3.14 and 3.15: **a frame does not appear
//! as a retainer of its own local**, even materialised through `sys._getframe`
//! with its `f_locals` read — which is the state every frame bpd holds is in.
//! PEP 667 made `f_locals` a snapshot rather than a live dict
//!
//! the real hole is the inverse. bpd's rust-side references are `Py` handles —
//! refcounts, not tracked python objects — so a referent walk **cannot see
//! them**. the debugger is invisible in its own answer rather than noisy in it,
//! and that is in [`bpd_core::Coverage`] on every answer rather than in a page
//! somebody has to find

use bpd_core::{Coverage, Retainer, Retainers};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyFrozenSet, PyList, PySet, PyTuple};

/// what an untracked object costs the answer
const UNTRACKED: &str = "objects the collector does not track never appear here. an int, a str, \
                         a float — anything without gc support — is invisible to the referent \
                         graph, so a container holding one is found and the thing itself is not";

/// what a holder outside python costs the answer
const NOT_PYTHON: &str = "a reference held by C or rust is a refcount rather than something the \
                          collector walks, so it cannot be found. bpd's own are among them — the \
                          agent holds handles to code objects and to recorded task stacks — which \
                          means this answer cannot see the debugger asking it";

/// what a retainer is, in a phrase, **without running the program's code**
///
/// deliberately not `repr()`. a repr calls the object's own `__repr__`, which is
/// the program's code run to answer a question about the program — the same rule
/// that keeps `dir` out of [`through`]. a container says how much it holds,
/// which is what tells two dicts apart, and everything else says what it is
fn described(object: &Bound<'_, PyAny>) -> PyResult<String> {
    let kind: String = object.get_type().name()?.extract()?;

    // `len` reaches `__len__`, which is the program's code, so it is asked only
    // of the builtin containers — whose length is C, and which are the ones a
    // count tells apart. the type is matched **exactly**: a dict subclass may
    // override `__len__`, and that is the program's code again
    let counted = object.is_exact_instance_of::<PyDict>()
        || object.is_exact_instance_of::<PyList>()
        || object.is_exact_instance_of::<PyTuple>()
        || object.is_exact_instance_of::<PySet>()
        || object.is_exact_instance_of::<PyFrozenSet>();
    if !counted {
        return Ok(format!("a {kind}"));
    }

    Ok(match object.len() {
        Ok(held) => format!("a {kind} holding {held}"),
        // no length is not a failure. a plain object has none, and asking is
        // how that is found out
        Err(_) => format!("a {kind}"),
    })
}

/// what is holding `target`, and how each one holds it
///
/// # errors
///
/// when the interpreter refuses a call this makes — `gc` is imported by the
/// agent rather than by the program, and importing it is safe because it is a
/// builtin module that is always already there
pub(crate) fn holding(
    python: Python<'_>,
    target: &Bound<'_, PyAny>,
    mode: bpd_core::Mode,
) -> PyResult<Retainers> {
    let gc = python.import("gc")?;
    let referrers = gc.getattr("get_referrers")?.call1((target,))?;

    let mut found = Vec::new();
    for retainer in referrers.try_iter()? {
        let retainer = retainer?;
        // the list `get_referrers` just built holds every retainer, and holds
        // the target through none of them. it is bpd's own and would be an
        // answer about the question rather than about the program
        if retainer.is(&referrers) {
            continue;
        }
        found.push(Retainer {
            kind: retainer.get_type().name()?.extract()?,
            described: described(&retainer)?,
            through: through(&retainer, target)?,
        });
    }

    Ok(Retainers {
        of: described(target)?,
        found,
        coverage: Coverage {
            untracked: UNTRACKED.to_string(),
            not_python: NOT_PYTHON.to_string(),
            mode,
        },
    })
}

/// where inside a retainer the target sits, when that is knowable
///
/// **`None` is not "nowhere".** it is a container whose shape this cannot read,
/// and answering `None` rather than guessing is the difference between a
/// debugger that does not know and one that invents. a C type with its own
/// traversal reaches its referents by a route no python-level inspection has
fn through(retainer: &Bound<'_, PyAny>, target: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    // a mapping first, because a dict is both the commonest retainer and the
    // one where *which* entry matters most — a module's globals, an instance's
    // attributes and a cache are all dicts
    //
    // the pairs are read straight off the table rather than looked up per key.
    // `get_item` hashes what it is given, which is `__hash__` and `__eq__` —
    // the program's code — and a key whose hash raises would fail a walk that
    // had nothing to do with it
    if let Ok(mapping) = retainer.cast::<PyDict>() {
        for (key, value) in mapping.iter() {
            if value.is(target) {
                return Ok(Some(format!("the value under {}", short(&key)?)));
            }
            if key.is(target) {
                return Ok(Some("a key of it".to_string()));
            }
        }
        return Ok(None);
    }

    // a sequence, where the position is the answer
    //
    // read through the concrete storage and matched by **exact** type, for the
    // reason `described` is: `try_iter` is `PyObject_GetIter`, which is
    // `type(obj).__iter__` — the program's own code — and `is_instance_of`
    // takes subclasses, so a `class Registry(list)` with an iterator of its own
    // would be asked to run it to answer a question about the program. what it
    // yielded would then decide the index reported
    if let Ok(items) = retainer.cast_exact::<PyList>() {
        for (at, item) in items.iter().enumerate() {
            if item.is(target) {
                return Ok(Some(format!("index {at}")));
            }
        }
        return Ok(None);
    }
    if let Ok(items) = retainer.cast_exact::<PyTuple>() {
        for (at, item) in items.iter().enumerate() {
            if item.is(target) {
                return Ok(Some(format!("index {at}")));
            }
        }
        return Ok(None);
    }

    // a collection with no order, which is a different answer rather than the
    // same one. a set's iteration order is its hash table's and moves when the
    // table is resized, so the position it is reached at is not a place the
    // object is — read as a sequence's index it says the program holds it
    // somewhere it does not, and it is not stable enough to be true twice
    if let Ok(items) = retainer.cast_exact::<PySet>() {
        for item in items.iter() {
            if item.is(target) {
                return Ok(Some("an element of it".to_string()));
            }
        }
        return Ok(None);
    }
    if let Ok(items) = retainer.cast_exact::<PyFrozenSet>() {
        for item in items.iter() {
            if item.is(target) {
                return Ok(Some("an element of it".to_string()));
            }
        }
        return Ok(None);
    }

    // and an ordinary object, through its own `__dict__`. asked for rather than
    // walked with `dir`, because `dir` calls `__getattr__` and that runs the
    // program's own code to answer a question about it
    if let Ok(attributes) = retainer.getattr("__dict__")
        && let Ok(attributes) = attributes.cast::<PyDict>()
    {
        for (name, value) in attributes.iter() {
            if value.is(target) {
                return Ok(Some(format!("attribute {}", short(&name)?)));
            }
        }
    }
    Ok(None)
}

/// a key or an attribute name, as text
///
/// a `str` key is the overwhelmingly common case and is used as it is. anything
/// else says what kind it was rather than being rendered, for the reason
/// [`described`] does not use `repr`
fn short(value: &Bound<'_, PyAny>) -> PyResult<String> {
    value.extract::<String>().map_or_else(
        |_| {
            value
                .get_type()
                .name()
                .and_then(|kind| kind.extract::<String>())
                .map(|kind| format!("a {kind} key"))
        },
        |text| Ok(format!("`{text}`")),
    )
}
