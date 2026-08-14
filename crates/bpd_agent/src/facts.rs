//! proving things about a value, and judging how long the proof lasts
//!
//! [`crate::values`] answers "what is this now". this answers "what is true of
//! this, and what would have to happen for it to stop being true" — which is
//! the form a client reasoning about code that has not run yet needs, and which
//! only something holding the object can produce
//!
//! ## the two judgements
//!
//! **what was read** comes out of the object's storage or its type's slots, and
//! never out of running the program. the rule is stricter than the one
//! [`crate::values`] holds: a fact is carried *forward* over code that has not
//! run, so a reading taken by calling `__bool__` would be a claim about the
//! future built on one call to arbitrary code. there is no `repr` escape hatch
//! here and there is not going to be one
//!
//! **how long it lasts** is read off the type. cpython answers three questions
//! that decide it, all of them slot reads:
//!
//! - `Py_TPFLAGS_HEAPTYPE` — a heap type is one a `class` statement made, and
//!   `__class__` can be assigned on one. cpython refuses the assignment for a
//!   static type, so `type(x) is int` is permanent and `type(x) is User` is not
//! - `tp_dictoffset` — non-zero means instances keep a dictionary, so any
//!   attribute of one can be assigned, deleted or added
//! - the type itself, against the table of builtins whose storage *is* their
//!   value. a `tuple`'s length cannot change and a `list`'s can, and that is
//!   the difference between a permanent length fact and a provisional one
//!
//! ## why an exact type and not a subclass
//!
//! a length or a truthiness is only read when `type(value)` is **exactly** one
//! of the builtins in [`Builtin`]. a `list` subclass may well not override
//! `__len__` — but deciding that means asking whether a name is defined
//! anywhere in the MRO and whether what is found is cpython's own slot, and a
//! wrong answer there is the debugger calling into the program while claiming
//! it did not
//!
//! so a subclass produces the facts that do not depend on the question — its
//! class, and the classes above it — and produces no length. an absent fact is
//! not a claim, and the answer is shorter rather than wrong

use std::fmt::Write as _;

use bpd_core::fact::{Class, Fact, Limit, Mutation, Observed, Silence, Stability};
use bpd_core::frame::Scope;
use pyo3::PyTypeInfo;
use pyo3::exceptions::PyAttributeError;
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyByteArray, PyBytes, PyDict, PyFloat, PyFrozenSet, PyInt, PyList, PySet, PyString,
    PyTuple, PyType,
};

use crate::events;

/// the builtin types whose storage is their value
///
/// membership is decided by **identity with the type object**, not by
/// `isinstance`, for the reason in the module docs. the split is what each one
/// promises about a reading of it:
///
/// - a value type's own value cannot change, so everything read off it is
///   permanent
/// - a frozen container's length cannot change, so its length and its
///   truthiness are permanent even though what it holds may not be
/// - a mutable container's length can change with the next statement
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Builtin {
    /// `None`, `bool`, `int`, `float`, `str`, `bytes`
    Value,
    /// `tuple`, `frozenset`
    Frozen,
    /// `list`, `dict`, `set`, `bytearray`
    Mutable,
}

impl Builtin {
    /// how long a reading of the object's own contents lasts
    const fn stability(self) -> Stability {
        match self {
            Self::Value | Self::Frozen => Stability::Permanent,
            Self::Mutable => Stability::Until {
                mutation: Mutation::Contents,
            },
        }
    }
}

/// proves what it can about values, under one request's limits
pub(crate) struct Prover<'py> {
    python: Python<'py>,
    limit: Limit,
}

impl<'py> Prover<'py> {
    /// a prover that will answer within `limit`
    pub(crate) const fn new(python: Python<'py>, limit: Limit) -> Self {
        Self { python, limit }
    }

    /// everything provable about what `name` holds
    ///
    /// several facts, or none. a name that produces none is the caller's to
    /// report as silent — this does not decide that, because a dotted path can
    /// fail at a segment and only the caller knows which
    pub(crate) fn about(
        &self,
        value: &Bound<'py, PyAny>,
        name: &str,
        scope: Scope,
    ) -> PyResult<Vec<Fact>> {
        let mut facts = Vec::new();
        let mut add = |observed, stability| {
            facts.push(Fact {
                name: name.to_string(),
                scope,
                observed,
                stability,
            });
        };

        let class = Self::class_of(value)?;
        add(
            Observed::IsExactly {
                class: class.clone(),
            },
            class_stability(value),
        );

        let Some(builtin) = builtin_of(value) else {
            // not a builtin whose storage is its value. the one thing left that
            // can be read without running anything is enum membership, which
            // lives in the member's own instance dictionary
            if let Some(member) = self.enum_member(value)? {
                add(
                    Observed::IsEnumMember { class, member },
                    Stability::Until {
                        mutation: Mutation::Attributes,
                    },
                );
            }
            return Ok(facts);
        };

        let permanent = builtin.stability();
        if let Some(observed) = self.value_of(value)? {
            add(observed, permanent.clone());
        }
        if let Some(length) = length_of(value) {
            add(Observed::HasLength { length }, permanent.clone());
            add(Observed::IsTruthy { truthy: length > 0 }, permanent);
        } else if let Some(truthy) = self.truthiness_of(value)? {
            add(Observed::IsTruthy { truthy }, permanent);
        }

        Ok(facts)
    }

    /// `type(value)`, named so that something reading source can resolve it
    ///
    /// `PyType_GetQualName` and `PyType_GetModuleName` on every interpreter bpd
    /// supports, which read the type's own storage. going through `getattr`
    /// would reach a metaclass `__getattribute__`, which is the program
    fn class_of(value: &Bound<'py, PyAny>) -> PyResult<Class> {
        Self::named(&value.get_type())
    }

    /// one class, named
    fn named(class: &Bound<'py, PyType>) -> PyResult<Class> {
        Ok(Class {
            module: class.module()?.to_cow()?.into_owned(),
            qualname: class.qualname()?.to_cow()?.into_owned(),
        })
    }

    /// the value itself, for the types whose storage is their value
    ///
    /// `None` when there is a value and it is longer than the request allowed.
    /// a cut value is worse than an absent one: a fact is compared against
    /// source, and `x == "abc…"` is a different claim from `x == "abcd"` with
    /// no way to mark it as approximate
    fn value_of(&self, value: &Bound<'py, PyAny>) -> PyResult<Option<Observed>> {
        if value.is_none() {
            return Ok(Some(Observed::IsNone));
        }
        // before `int`, which `bool` is a subclass of
        if let Ok(boolean) = value.cast::<PyBool>() {
            return Ok(Some(Observed::IsBool {
                value: boolean.is_true(),
            }));
        }
        if value.is_instance_of::<PyInt>() {
            let text = events::int_repr(self.python, value)?;
            let within = self.within(text.chars().count());
            return Ok(within.then_some(Observed::IsInt { text }));
        }
        if value.is_instance_of::<PyFloat>() {
            return Ok(Some(Observed::IsFloat {
                text: events::float_repr(self.python, value)?,
            }));
        }
        if let Ok(string) = value.cast::<PyString>() {
            let text = string.to_cow()?.into_owned();
            let within = self.within(text.chars().count());
            return Ok(within.then_some(Observed::IsStr { text }));
        }
        if let Ok(bytes) = value.cast::<PyBytes>() {
            let raw = bytes.as_bytes();
            if !self.within(raw.len()) {
                return Ok(None);
            }
            let mut hex = String::with_capacity(raw.len() * 2);
            for byte in raw {
                write!(hex, "{byte:02x}").expect("writing to a string cannot fail");
            }
            return Ok(Some(Observed::IsBytes { hex }));
        }
        Ok(None)
    }

    /// whether a reading that long is one the request asked to see
    const fn within(&self, characters: usize) -> bool {
        characters <= self.limit.text as usize
    }

    /// `bool(value)` for the value types that have no length
    ///
    /// the ones with a length are answered from it instead, because that is one
    /// reading rather than two that have to agree
    fn truthiness_of(&self, value: &Bound<'py, PyAny>) -> PyResult<Option<bool>> {
        if value.is_none() {
            return Ok(Some(false));
        }
        if let Ok(boolean) = value.cast::<PyBool>() {
            return Ok(Some(boolean.is_true()));
        }
        if value.is_instance_of::<PyInt>() {
            // against zero rather than through `__bool__`, and by comparing the
            // digits cpython itself wrote — an `int` has no width, so there is
            // no rust integer this always fits in
            let text = events::int_repr(self.python, value)?;
            return Ok(Some(text != "0"));
        }
        if value.is_instance_of::<PyFloat>() {
            let float: f64 = value.extract()?;
            return Ok(Some(float != 0.0));
        }
        Ok(None)
    }

    /// which member of which enum, for a value that is one
    ///
    /// `_name_` out of the member's own instance dictionary rather than `.name`,
    /// which is a `DynamicClassAttribute` and therefore a descriptor call
    ///
    /// a program that never imported `enum` has no enum members in it, which is
    /// why this asks `sys.modules` rather than importing anything: importing a
    /// module into the debuggee to answer a question about it would be the
    /// debugger changing the program
    fn enum_member(&self, value: &Bound<'py, PyAny>) -> PyResult<Option<String>> {
        let Some(base) = self.enum_base()? else {
            return Ok(None);
        };
        // the `isinstance` builtin would go through `EnumMeta.__instancecheck__`,
        // which is python
        if !is_subtype(&value.get_type(), &base) {
            return Ok(None);
        }

        let Ok(stored) = value.getattr("__dict__") else {
            return Ok(None);
        };
        let Ok(stored) = stored.cast::<PyDict>() else {
            return Ok(None);
        };
        let Some(name) = stored.get_item("_name_")? else {
            return Ok(None);
        };
        let Ok(name) = name.cast::<PyString>() else {
            return Ok(None);
        };
        Ok(Some(name.to_cow()?.into_owned()))
    }

    /// `enum.Enum`, when the program has imported `enum` at all
    fn enum_base(&self) -> PyResult<Option<Bound<'py, PyType>>> {
        let modules = self.python.import("sys")?.getattr("modules")?;
        let Ok(Some(module)) = modules.get_item("enum").map(Some) else {
            return Ok(None);
        };
        let Ok(base) = module.getattr("Enum") else {
            return Ok(None);
        };
        Ok(base.cast_into::<PyType>().ok())
    }

    /// follow a dotted path from the object a name is bound to
    ///
    /// every segment is read out of the object's **own** storage. a segment the
    /// type has a data descriptor for is refused rather than read, because a
    /// data descriptor wins over the instance dictionary and reading it means
    /// calling its `__get__` — the program
    pub(crate) fn follow(
        &self,
        root: &Bound<'py, PyAny>,
        segments: &[&str],
    ) -> PyResult<Result<Bound<'py, PyAny>, Silence>> {
        let mut value = root.clone();
        for segment in segments {
            if let Some(owner) = self.shadowed_by_descriptor(&value, segment)? {
                return Ok(Err(Silence::WouldRun {
                    member: (*segment).to_string(),
                    owner,
                }));
            }
            let Some(next) = self.stored(&value, segment)? else {
                return Ok(Err(Silence::Missing {
                    segment: (*segment).to_string(),
                }));
            };
            value = next;
        }
        Ok(Ok(value))
    }

    /// what the object's own dictionary holds for a name
    fn stored(
        &self,
        object: &Bound<'py, PyAny>,
        name: &str,
    ) -> PyResult<Option<Bound<'py, PyAny>>> {
        let stored = match object.getattr("__dict__") {
            Ok(stored) => stored,
            // exactly what cpython raises for an object with no instance
            // dictionary — a `__slots__` class, or a type implemented in C
            Err(error) if error.is_instance_of::<PyAttributeError>(self.python) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let Ok(stored) = stored.cast::<PyDict>() else {
            return Ok(None);
        };
        stored.get_item(name)
    }

    /// the class defining a data descriptor of this name, when one does
    ///
    /// a data descriptor is one whose own type defines `__set__` or
    /// `__delete__`, and cpython gives it priority over the instance
    /// dictionary. a `property` is the one everybody meets
    ///
    /// the MRO is walked through `tp_mro` and each class's own dictionary
    /// through `PyType_GetDict`, so neither the metaclass nor any
    /// `__getattribute__` is reached
    fn shadowed_by_descriptor(
        &self,
        object: &Bound<'py, PyAny>,
        name: &str,
    ) -> PyResult<Option<Class>> {
        for class in self.mro(&object.get_type()) {
            let Some(found) = self.own_dict(&class).get_item(name)? else {
                continue;
            };
            // the first definition in the MRO is the one cpython would use, so
            // the walk ends here whichever kind it turns out to be. a
            // non-descriptor, or a descriptor with only `__get__`, loses to the
            // instance dictionary and so is not in the way
            return Ok(if self.is_data_descriptor(&found)? {
                Some(Self::named(&class)?)
            } else {
                None
            });
        }
        Ok(None)
    }

    /// whether this object's own type defines `__set__` or `__delete__`
    ///
    /// which is cpython's definition of a data descriptor, and therefore of the
    /// thing that takes priority over an instance dictionary
    fn is_data_descriptor(&self, found: &Bound<'py, PyAny>) -> PyResult<bool> {
        for class in self.mro(&found.get_type()) {
            let dict = self.own_dict(&class);
            if dict.contains("__set__")? || dict.contains("__delete__")? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// a type's own `__mro__`, read off the type's slot
    ///
    /// a type that has not been readied yet has no `tp_mro`, and the only entry
    /// that can be claimed for it is the type itself
    fn mro(&self, class: &Bound<'py, PyType>) -> Vec<Bound<'py, PyType>> {
        mro_slot(self.python, class).map_or_else(
            || vec![class.clone()],
            |mro| {
                mro.iter()
                    .filter_map(|entry| entry.cast_into::<PyType>().ok())
                    .collect()
            },
        )
    }

    /// a type's own dictionary, read off the type rather than through it
    fn own_dict(&self, class: &Bound<'py, PyType>) -> Bound<'py, PyDict> {
        type_dict(self.python, class).unwrap_or_else(|| PyDict::new(self.python))
    }
}

/// how long "the type is exactly this" stays true
///
/// `__class__` is assignable only between heap types with compatible layout, so
/// an instance of a static type — every builtin — can never change class, and an
/// instance of a `class` statement's type can
fn class_stability(value: &Bound<'_, PyAny>) -> Stability {
    if is_heap_type(&value.get_type()) {
        Stability::Until {
            mutation: Mutation::Class,
        }
    } else {
        Stability::Permanent
    }
}

/// which builtin a value is exactly an instance of, if any
///
/// identity with the type object rather than `isinstance`. a subclass is not
/// one of these, for the reason in the module docs
fn builtin_of(value: &Bound<'_, PyAny>) -> Option<Builtin> {
    if value.is_none() {
        return Some(Builtin::Value);
    }
    let class = value.get_type();
    let python = value.py();
    let exactly = |target: &Bound<'_, PyType>| class.is(target);

    if exactly(&PyBool::type_object(python))
        || exactly(&PyInt::type_object(python))
        || exactly(&PyFloat::type_object(python))
        || exactly(&PyString::type_object(python))
        || exactly(&PyBytes::type_object(python))
    {
        return Some(Builtin::Value);
    }
    if exactly(&PyTuple::type_object(python)) || exactly(&PyFrozenSet::type_object(python)) {
        return Some(Builtin::Frozen);
    }
    if exactly(&PyList::type_object(python))
        || exactly(&PyDict::type_object(python))
        || exactly(&PySet::type_object(python))
        || exactly(&PyByteArray::type_object(python))
    {
        return Some(Builtin::Mutable);
    }
    None
}

/// `len(value)` read from the object's own storage
///
/// only for a value [`builtin_of`] recognised, so the length is cpython's own
/// count rather than anything a `__len__` decided
fn length_of(value: &Bound<'_, PyAny>) -> Option<usize> {
    if let Ok(string) = value.cast::<PyString>() {
        return string.len().ok();
    }
    if let Ok(bytes) = value.cast::<PyBytes>() {
        return Some(bytes.as_bytes().len());
    }
    if let Ok(array) = value.cast::<PyByteArray>() {
        return Some(array.len());
    }
    if let Ok(list) = value.cast::<PyList>() {
        return Some(list.len());
    }
    if let Ok(tuple) = value.cast::<PyTuple>() {
        return Some(tuple.len());
    }
    if let Ok(mapping) = value.cast::<PyDict>() {
        return Some(mapping.len());
    }
    if let Ok(set) = value.cast::<PySet>() {
        return Some(set.len());
    }
    if let Ok(frozen) = value.cast::<PyFrozenSet>() {
        return Some(frozen.len());
    }
    None
}

/// whether a type is one a `class` statement made
///
/// `__class__` is assignable only between heap types, so this is the whole of
/// the difference between an object whose type can change under an analysis and
/// one whose cannot
///
/// SAFETY: `as_type_ptr` on a live `Bound<PyType>` is a valid `PyTypeObject`
/// for as long as the binding is held, and `PyType_HasFeature` only reads
/// `tp_flags` off it. no reference count changes and nothing is stored
#[expect(
    unsafe_code,
    reason = "cpython exposes a type's flags nowhere else, and reading \
              `__flags__` through the attribute would reach a metaclass — see \
              above"
)]
fn is_heap_type(class: &Bound<'_, PyType>) -> bool {
    unsafe { ffi::PyType_HasFeature(class.as_type_ptr(), ffi::Py_TPFLAGS_HEAPTYPE) != 0 }
}

/// whether one type is below another in the MRO
///
/// the `isinstance` builtin asks the metaclass, and `EnumMeta` answers it with
/// python. this walks `tp_mro` in C and runs nothing
///
/// SAFETY: both pointers come from live `Bound<PyType>` bindings and
/// `PyType_IsSubtype` only walks the MRO tuples they already own
#[expect(
    unsafe_code,
    reason = "the safe spelling of this question calls `__instancecheck__`, \
              which is the program — see above"
)]
fn is_subtype(class: &Bound<'_, PyType>, base: &Bound<'_, PyType>) -> bool {
    unsafe { ffi::PyType_IsSubtype(class.as_type_ptr(), base.as_type_ptr()) == 1 }
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
fn mro_slot<'py>(python: Python<'py>, class: &Bound<'py, PyType>) -> Option<Bound<'py, PyTuple>> {
    let mro = unsafe { (*class.as_type_ptr()).tp_mro };
    if mro.is_null() {
        return None;
    }
    unsafe { Bound::from_borrowed_ptr(python, mro) }
        .cast_into::<PyTuple>()
        .ok()
}

/// a type's own `__dict__`, without going through the type
///
/// SAFETY: `PyType_GetDict` returns a new reference or null, and
/// `from_owned_ptr` takes exactly that ownership. it is only called on the
/// non-null branch
#[expect(
    unsafe_code,
    reason = "reading `__dict__` off the type as an attribute reaches the \
              metaclass, and this is the C accessor that does not — see above"
)]
fn type_dict<'py>(python: Python<'py>, class: &Bound<'py, PyType>) -> Option<Bound<'py, PyDict>> {
    let dict = unsafe { ffi::PyType_GetDict(class.as_type_ptr()) };
    if dict.is_null() {
        return None;
    }
    unsafe { Bound::from_owned_ptr(python, dict) }
        .cast_into::<PyDict>()
        .ok()
}
