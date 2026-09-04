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

    let mut holders: Vec<Bound<'_, PyAny>> = Vec::new();
    for retainer in referrers.try_iter()? {
        let retainer = retainer?;
        // the list `get_referrers` just built holds every retainer, and holds
        // the target through none of them. it is bpd's own and would be an
        // answer about the question rather than about the program
        if retainer.is(&referrers) {
            continue;
        }

        // an instance's attribute table is the **instance**, in the terms the
        // program is written in
        let retainer = match retainer.cast::<PyDict>() {
            Ok(table) => owner_of(python, table)?.unwrap_or_else(|| retainer.clone()),
            Err(_) => retainer,
        };

        // the same holder can be reached twice — as itself and through the
        // table it keeps its attributes in — and naming it twice would read as
        // two holders
        if holders.iter().any(|seen| seen.is(&retainer)) {
            continue;
        }
        holders.push(retainer);
    }

    let mut found = Vec::new();
    for retainer in holders {
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

/// the object whose attribute table this dict is, when it is one
///
/// **a version difference the answer must not carry.** on 3.13 a subclass of a
/// builtin keeps its attributes in a separate `dict`, and that dict is what
/// `gc.get_referrers` answers with; from 3.14 the instance itself is the
/// referrer. measured on both, with the same program: 3.13 named two retainers
/// `a dict holding 1` and 3.14 named the two objects that own them
///
/// a dict is not what the program holds the object in — an object is — and
/// "a dict holding 1" names nothing anybody can go and look at. so the table is
/// resolved to its owner on every release, and the version the interpreter
/// happens to be stops being visible in the answer
///
/// the match is by **identity** against the candidate's own table, read through
/// `object.__getattribute__` rather than `getattr`: that is object's own
/// implementation, so a class that overrides `__getattribute__` cannot decide
/// what the debugger reports here. a class that defines `__dict__` as a
/// property of its own still runs, and a failure is no owner rather than a
/// failed walk
fn owner_of<'py>(
    python: Python<'py>,
    table: &Bound<'py, PyDict>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let generic = python
        .import("builtins")?
        .getattr("object")?
        .getattr("__getattribute__")?;

    let referrers = python
        .import("gc")?
        .getattr("get_referrers")?
        .call1((table,))?;

    for candidate in referrers.try_iter()? {
        let candidate = candidate?;
        if candidate.is(&referrers) || candidate.is(table) {
            continue;
        }
        if let Ok(theirs) = generic.call1((&candidate, "__dict__"))
            && theirs.is(table)
        {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
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
    }

    // a sequence, where the position is the answer
    //
    // **subclasses included, and still without running their code.** pyo3's list
    // and tuple iterators read `PyList_GET_ITEM` and `PyTuple_GET_ITEM` — the
    // concrete storage — so a `class Registry(list)` is answered from the same
    // array cpython itself indexes, and its `__iter__` is never consulted. an
    // earlier cut of this used an **exact** type check to avoid running that
    // `__iter__`, which was the right worry applied with the wrong instrument:
    // it made a subclass holding the target report `None` — "a container whose
    // shape this cannot read" — when the true answer was there for free
    if let Ok(items) = retainer.cast::<PyList>() {
        for (at, item) in items.iter().enumerate() {
            if item.is(target) {
                return Ok(Some(format!("index {at}")));
            }
        }
    }
    if let Ok(items) = retainer.cast::<PyTuple>() {
        for (at, item) in items.iter().enumerate() {
            if item.is(target) {
                return Ok(Some(format!("index {at}")));
            }
        }
    }

    // a collection with no order, which is a different answer rather than the
    // same one. a set's iteration order is its hash table's and moves when the
    // table is resized, so the position it is reached at is not a place the
    // object is — read as a sequence's index it says the program holds it
    // somewhere it does not, and it is not stable enough to be true twice
    //
    // a set has **no** storage iterator in pyo3: its `iter` is
    // `PyObject_GetIter`, so the exact type check is what keeps a subclass's
    // `__iter__` from running, and it is load bearing here in a way it is not
    // above. `try_iter` rather than `iter` because the latter's item unwrap
    // panics if the set is mutated mid-iteration, which a free-threaded build
    // makes reachable — and this **fails the query**, naming the interpreter's
    // own `RuntimeError`, rather than panicking or quietly answering `None`
    if retainer.is_exact_instance_of::<PySet>() || retainer.is_exact_instance_of::<PyFrozenSet>() {
        for item in retainer.try_iter()? {
            if item?.is(target) {
                return Ok(Some("an element of it".to_string()));
            }
        }
    }

    // and an ordinary object, through its own `__dict__` — reached whether or not
    // one of the branches above matched, because a `class Registry(list)` can
    // hold the target on an attribute rather than in its list storage. those
    // branches used to `return Ok(None)` when the storage did not have it, which
    // said "shape unreadable" about a shape that had been read
    //
    // **this one does reach the program**, and saying so is the point:
    // `getattr` is `type(obj).__getattribute__`, which a class may override. it
    // is kept because the alternative is reporting `None` for the commonest
    // retainer there is — an instance holding the target on an attribute — and
    // a failure here is caught and becomes that same `None` rather than an
    // error. what it cannot see is a `__slots__` class, which has no `__dict__`,
    // and a class object, whose `__dict__` is a `mappingproxy` rather than a
    // dict: both fall through to "shape unreadable", which is true of them
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
