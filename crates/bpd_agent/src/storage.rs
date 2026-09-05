//! reading an object's storage without going through the object
//!
//! `obj.__dict__` is an attribute read, and an attribute read is
//! `type(obj).__getattribute__` — the program's, whenever a class overrides
//! it. `cls.__mro__` and `cls.__dict__` are the same read one level up, through
//! the metaclass. every reader in the agent that promises to run nothing of the
//! program has to reach the same storage some other way, and this is the one
//! place that knows how: the instance dictionary through
//! `PyObject_GenericGetDict`, which is cpython's own accessor for the slot and
//! consults no class; a type's dictionary through `PyType_GetDict`; a type's
//! MRO off `tp_mro`
//!
//! what these read is what the object **holds**, which is not always what
//! `obj.__dict__` would have answered. a class that makes `__dict__` a property
//! answers the property; this answers the slot. that is the point rather than a
//! discrepancy: the debugger reports storage, and the program's code is what
//! the program runs

use std::ptr;

use pyo3::exceptions::PyAttributeError;
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple, PyType};

/// an object's instance dictionary, read off the object
///
/// `None` is an object that keeps no instance dictionary — a `__slots__`
/// class, a type implemented in C, a builtin value — which cpython reports as
/// an `AttributeError` and this reports as an absence. an object whose slot is
/// still empty is given its dictionary here, which is what the first
/// `obj.__dict__` of the program would have done too
///
/// SAFETY: `as_ptr` on a live `Bound` is a valid object for as long as the
/// binding is held. `PyObject_GenericGetDict` returns a new reference or null
/// with an exception set, and `from_owned_ptr` takes exactly that ownership on
/// the non-null branch
#[expect(
    unsafe_code,
    reason = "reading `__dict__` as an attribute runs `__getattribute__`, and \
              this is the C accessor for the slot that does not — see above"
)]
pub(crate) fn instance_dict<'py>(
    object: &Bound<'py, PyAny>,
) -> PyResult<Option<Bound<'py, PyDict>>> {
    let python = object.py();
    let dict = unsafe { ffi::PyObject_GenericGetDict(object.as_ptr(), ptr::null_mut()) };
    if dict.is_null() {
        let error = PyErr::take(python)
            .expect("PyObject_GenericGetDict returns null only with an exception set");
        if error.is_instance_of::<PyAttributeError>(python) {
            return Ok(None);
        }
        return Err(error);
    }
    let dict = unsafe { Bound::from_owned_ptr(python, dict) };
    Ok(Some(dict.cast_into::<PyDict>().expect(
        "cpython refuses to store anything but a dict in an object's `__dict__` slot",
    )))
}

/// a type's own dictionary, read off the type rather than through it
///
/// SAFETY: `PyType_GetDict` returns a new reference or null, and
/// `from_owned_ptr` takes exactly that ownership. it is only called on the
/// non-null branch
#[expect(
    unsafe_code,
    reason = "reading `__dict__` off a type as an attribute reaches the \
              metaclass, and this is the C accessor that does not — see above"
)]
pub(crate) fn type_dict<'py>(class: &Bound<'py, PyType>) -> Option<Bound<'py, PyDict>> {
    let python = class.py();
    let dict = unsafe { ffi::PyType_GetDict(class.as_type_ptr()) };
    if dict.is_null() {
        return None;
    }
    unsafe { Bound::from_owned_ptr(python, dict) }
        .cast_into::<PyDict>()
        .ok()
}

/// a type's `tp_mro`, when it has been readied
///
/// SAFETY: `as_type_ptr` on a live `Bound<PyType>` is a valid `PyTypeObject`.
/// `tp_mro` is a borrowed reference the type owns, and `from_borrowed_ptr`
/// takes its own — so the tuple outlives the binding returned here regardless
/// of what happens to the type
#[expect(
    unsafe_code,
    reason = "`__mro__` read as an attribute goes through the metaclass, which \
              is what this is avoiding — see above"
)]
fn mro_slot<'py>(class: &Bound<'py, PyType>) -> Option<Bound<'py, PyTuple>> {
    let python = class.py();
    let mro = unsafe { (*class.as_type_ptr()).tp_mro };
    if mro.is_null() {
        return None;
    }
    unsafe { Bound::from_borrowed_ptr(python, mro) }
        .cast_into::<PyTuple>()
        .ok()
}

/// a type's MRO, read off the type's slot
///
/// a type that has not been readied yet has no `tp_mro`, and the only entry
/// that can be claimed for it is the type itself
pub(crate) fn mro<'py>(class: &Bound<'py, PyType>) -> Vec<Bound<'py, PyType>> {
    mro_slot(class).map_or_else(
        || vec![class.clone()],
        |mro| {
            mro.iter()
                .filter_map(|entry| entry.cast_into::<PyType>().ok())
                .collect()
        },
    )
}

/// whether a value is a generator, coroutine or async generator that has
/// started and not finished
///
/// the three are decided by **exact type**, and can be: none of them can be
/// subclassed. that is what makes the attribute read after it safe — the
/// suspended flag is a slot cpython defines on those three types, and no
/// `__getattribute__` of the program is in the way of an object whose type is
/// exactly one of them. `inspect.getgeneratorstate` answers the same question
/// and reads the attribute on whatever it is handed, which for anything else
/// is the program's own `__getattr__`
pub(crate) fn suspended(value: &Bound<'_, PyAny>) -> PyResult<bool> {
    let kind = value.get_type().as_type_ptr();
    let flag = if ptr::eq(kind, &raw const ffi::PyGen_Type) {
        "gi_suspended"
    } else if ptr::eq(kind, &raw const ffi::PyCoro_Type) {
        "cr_suspended"
    } else if ptr::eq(kind, &raw const ffi::PyAsyncGen_Type) {
        "ag_suspended"
    } else {
        return Ok(false);
    };
    value.getattr(flag)?.extract()
}
