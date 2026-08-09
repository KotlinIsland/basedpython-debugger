//! turning a python object into something that can be sent over a wire
//!
//! the rule this module exists to enforce: **reading a value does not run the
//! program**. a debugger that evaluates user code to describe a value is a
//! debugger that can hang, or change, the thing it was asked to measure
//!
//! so every structural form here is read through cpython's own concrete
//! interface — the object's storage — and never through the abstract protocol.
//! a `list` subclass that overrides `__getitem__` is read from the list's
//! storage, which is what the object really holds; the type name reported
//! alongside it says what it really is
//!
//! there are exactly two ways program code can run from in here, and both are
//! in the request:
//!
//! - `attributes` reads an object's `__dict__`. for an ordinary object that is
//!   a slot read that reaches no `__getattr__`, no property and no descriptor.
//!   a type is free to make `__dict__` its own code, and then this runs it
//! - `repr` calls `__repr__`, which is arbitrary user code. it is off by
//!   default, because bpd cannot interrupt it once it has started
//!
//! ## what is not claimed
//!
//! only the thread that stopped is held. on a free-threaded build the others
//! are still running, so a container read here is what it held while it was
//! read, and nothing stops another thread from changing it a moment later. the
//! sequence and mapping reads take a snapshot with cpython's own copy so the
//! answer is at least internally consistent, rather than half of one state and
//! half of another

use std::fmt::Write as _;

use bpd_protocol::message::{Content, Detail, Entry, Omitted, Pair, Value};
use pyo3::exceptions::PyAttributeError;
use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyByteArray, PyBytes, PyDict, PyFloat, PyFrozenSet, PyInt, PyList, PySet, PyString,
    PyTuple,
};

use crate::conditions::capture;
use crate::events;

/// what one value costs the budget before any text it carries
///
/// the json envelope around the smallest possible value — the braces, the
/// `kind` and `content` keys and the tag — rounded up. the budget is a bound on
/// what the answer reads, and the encoded answer can exceed it by the envelope
/// of the one value that discovered the budget was gone
const ENVELOPE: usize = 32;

/// reads values under one request's limits
pub(crate) struct Reader<'py> {
    python: Python<'py>,
    detail: Detail,
    spent: usize,
    /// the objects open on the path from the root, and where each was opened
    ///
    /// ancestors only. a value that appears twice in different branches is not
    /// a cycle and is reported twice; a value that appears inside itself is
    /// and is reported once, naming where it came round to
    open: Vec<(usize, String)>,
}

impl<'py> Reader<'py> {
    /// a reader that will answer within `detail`
    pub(crate) const fn new(python: Python<'py>, detail: Detail) -> Self {
        Self {
            python,
            detail,
            spent: 0,
            open: Vec::new(),
        }
    }

    /// read one value, naming it `path` in anything it has to say about itself
    pub(crate) fn read(&mut self, object: &Bound<'py, PyAny>, path: &str) -> PyResult<Value> {
        self.value(object, path, self.detail.depth)
    }

    /// charge the budget, and say whether there is anything left
    fn charge(&mut self, bytes: usize) -> bool {
        self.spent = self.spent.saturating_add(bytes);
        self.spent <= self.detail.budget as usize
    }

    /// whether the byte budget ran out before the answer was finished
    pub(crate) fn exhausted(&self) -> bool {
        self.spent > self.detail.budget as usize
    }

    /// the omission that says the budget is what stopped this
    const fn budget(&self) -> Omitted {
        Omitted::Budget {
            limit: self.detail.budget,
        }
    }

    fn value(&mut self, object: &Bound<'py, PyAny>, path: &str, depth: u32) -> PyResult<Value> {
        let kind = kind_of(object)?;

        if !self.charge(ENVELOPE + kind.len()) {
            return Ok(Value {
                kind,
                content: Content::Unread {
                    omitted: self.budget(),
                },
            });
        }

        let content = self.content(object, path, depth)?;
        Ok(Value { kind, content })
    }

    fn content(&mut self, object: &Bound<'py, PyAny>, path: &str, depth: u32) -> PyResult<Content> {
        if object.is_none() {
            return Ok(Content::None);
        }
        // before `int`, which `bool` is a subclass of
        if let Ok(boolean) = object.cast::<PyBool>() {
            return Ok(Content::Bool {
                value: boolean.is_true(),
            });
        }
        if object.is_instance_of::<PyInt>() {
            return self.integer(object);
        }
        if object.is_instance_of::<PyFloat>() {
            let text = events::float_repr(self.python, object)?;
            self.charge(text.len());
            return Ok(Content::Float { text });
        }
        if let Ok(string) = object.cast::<PyString>() {
            return Ok(self.text(string));
        }
        if let Ok(bytes) = object.cast::<PyBytes>() {
            return Ok(self.bytes(bytes.as_bytes()));
        }
        if let Ok(array) = object.cast::<PyByteArray>() {
            // copied rather than borrowed: a `bytearray` is mutable, and
            // borrowing its buffer over any other call into the interpreter is
            // how a debugger reads freed memory
            return Ok(self.bytes(&array.to_vec()));
        }
        if let Some(items) = self.sequence_items(object)? {
            return self.sequence(object, items, path, depth);
        }
        if let Ok(mapping) = object.cast::<PyDict>() {
            let entries = mapping.items();
            return self.mapping(object, mapping.len(), &entries, path, depth);
        }
        self.object(object, path, depth)
    }

    /// the digits of an integer, whole or not at all
    ///
    /// `int.__repr__` rather than `str(value)`, so an `int` subclass that
    /// overrides `__str__` or `__repr__` cannot change the number that is
    /// reported — the type name says what it is, and the digits say what it
    /// holds
    ///
    /// the text is never cut. half of a number is a different number, and a
    /// debugger that reported one would be reporting a value the program does
    /// not have
    fn integer(&mut self, object: &Bound<'py, PyAny>) -> PyResult<Content> {
        let text = events::int_repr(self.python, object)?;
        let characters = text.chars().count();
        if characters > self.detail.text as usize {
            return Ok(Content::Int {
                text: String::new(),
                omitted: Some(Omitted::Text {
                    characters,
                    limit: self.detail.text,
                }),
            });
        }
        if !self.charge(text.len()) {
            return Ok(Content::Int {
                text: String::new(),
                omitted: Some(self.budget()),
            });
        }
        Ok(Content::Int {
            text,
            omitted: None,
        })
    }

    /// a string, as itself
    fn text(&mut self, string: &Bound<'py, PyString>) -> Content {
        // a python `str` can hold lone surrogates — that is what
        // `surrogateescape` puts there for an undecodable filename — and
        // neither utf-8 nor json can carry one. the replacement is reported
        // rather than performed quietly
        let (text, unencodable) = match string.to_str() {
            Ok(text) => (text.to_string(), false),
            Err(_) => (string.to_string_lossy().into_owned(), true),
        };

        let (text, characters, omitted) = self.cut(&text);
        Content::Str {
            text,
            characters,
            omitted: omitted.or(unencodable.then_some(Omitted::Unencodable)),
        }
    }

    /// bytes, in lowercase hex
    fn bytes(&mut self, all: &[u8]) -> Content {
        let length = all.len();
        let limit = self.detail.text as usize;
        let taken = all.len().min(limit).min(self.remaining() / 2);

        let mut hex = String::with_capacity(taken * 2);
        for byte in &all[..taken] {
            write!(hex, "{byte:02x}").expect("writing to a string cannot fail");
        }
        self.charge(hex.len());

        let omitted = if taken == length {
            None
        } else if length > limit {
            Some(Omitted::Text {
                characters: length,
                limit: self.detail.text,
            })
        } else {
            let budget = self.budget();
            self.exhaust();
            Some(budget)
        };
        Content::Bytes {
            hex,
            length,
            omitted,
        }
    }

    /// the items of a sequence, as a snapshot, or nothing if this is not one
    ///
    /// a list is copied with `PyList_GetSlice` and a set with `list()`, so the
    /// items reported are the ones it held at one moment rather than a walk
    /// that another thread can change halfway through. a tuple is immutable and
    /// needs neither
    ///
    /// a set has to be an **exact** `set` or `frozenset`: cpython has no
    /// concrete accessor for set storage — no `PySet_GetItem` — so the only way
    /// in is iteration, and a subclass can make iteration its own code. a list
    /// or a tuple subclass has one, so it is read like the thing it is
    fn sequence_items(
        &self,
        object: &Bound<'py, PyAny>,
    ) -> PyResult<Option<(usize, Bound<'py, PyList>)>> {
        if let Ok(list) = object.cast::<PyList>() {
            let length = list.len();
            let wanted = length.min(self.detail.children as usize);
            return Ok(Some((length, list.get_slice(0, wanted))));
        }
        if let Ok(tuple) = object.cast::<PyTuple>() {
            let length = tuple.len();
            let wanted = length.min(self.detail.children as usize);
            return Ok(Some((length, tuple.get_slice(0, wanted).to_list())));
        }
        if object.is_exact_instance_of::<PySet>() || object.is_exact_instance_of::<PyFrozenSet>() {
            return Ok(Some((
                object.len()?,
                events::to_list(self.python, object)?.cast_into()?,
            )));
        }
        Ok(None)
    }

    fn sequence(
        &mut self,
        object: &Bound<'py, PyAny>,
        (length, taken): (usize, Bound<'py, PyList>),
        path: &str,
        depth: u32,
    ) -> PyResult<Content> {
        if let Some(cycle) = self.cycle(object) {
            return Ok(Content::Unread { omitted: cycle });
        }
        if depth == 0 {
            return Ok(Content::Sequence {
                items: Vec::new(),
                length,
                omitted: Some(Omitted::Depth {
                    limit: self.detail.depth,
                }),
            });
        }

        self.enter(object, path);
        let mut items = Vec::with_capacity(taken.len());
        let mut omitted = None;
        for index in 0..taken.len() {
            if self.exhausted() {
                omitted = Some(self.budget());
                break;
            }
            let item = taken
                .get_item(index)
                .expect("the snapshot is this reader's own list and the index is inside it");
            items.push(self.value(&item, &format!("{path}[{index}]"), depth - 1)?);
        }
        self.leave();

        Ok(Content::Sequence {
            items,
            length,
            omitted: omitted.or_else(|| self.cut_children(length)),
        })
    }

    fn mapping(
        &mut self,
        object: &Bound<'py, PyAny>,
        length: usize,
        entries: &Bound<'py, PyList>,
        path: &str,
        depth: u32,
    ) -> PyResult<Content> {
        if let Some(cycle) = self.cycle(object) {
            return Ok(Content::Unread { omitted: cycle });
        }
        if depth == 0 {
            return Ok(Content::Mapping {
                entries: Vec::new(),
                length,
                omitted: Some(Omitted::Depth {
                    limit: self.detail.depth,
                }),
            });
        }

        self.enter(object, path);
        let wanted = entries.len().min(self.detail.children as usize);
        let mut pairs = Vec::with_capacity(wanted);
        let mut omitted = None;
        for index in 0..wanted {
            if self.exhausted() {
                omitted = Some(self.budget());
                break;
            }
            let pair = entries
                .get_item(index)
                .expect("the snapshot is the interpreter's own list and the index is inside it");
            let key = pair.get_item(0)?;
            let value = pair.get_item(1)?;
            pairs.push(Pair {
                key: self.value(&key, &format!("{path}[{index}].key"), depth - 1)?,
                value: self.value(&value, &format!("{path}[{index}]"), depth - 1)?,
            });
        }
        self.leave();

        Ok(Content::Mapping {
            entries: pairs,
            length,
            omitted: omitted.or_else(|| self.cut_children(length)),
        })
    }

    /// an object, read from the dictionary it stores its attributes in
    fn object(&mut self, object: &Bound<'py, PyAny>, path: &str, depth: u32) -> PyResult<Content> {
        if self.detail.repr {
            let text = events::repr(self.python, object)?;
            let (text, characters, omitted) = self.cut(&text);
            return Ok(Content::Repr {
                text,
                characters,
                omitted,
            });
        }
        if !self.detail.attributes {
            return Ok(Content::Object {
                attributes: Vec::new(),
                omitted: Some(Omitted::AttributesNotRequested),
            });
        }
        if let Some(cycle) = self.cycle(object) {
            return Ok(Content::Unread { omitted: cycle });
        }
        if depth == 0 {
            return Ok(Content::Object {
                attributes: Vec::new(),
                omitted: Some(Omitted::Depth {
                    limit: self.detail.depth,
                }),
            });
        }

        let stored = match object.getattr("__dict__") {
            Ok(stored) => stored,
            // this is exactly what cpython raises for an object that has no
            // instance dictionary, which is a `__slots__` class or a type
            // implemented in C — not a failure, an absence
            Err(error) if error.is_instance_of::<PyAttributeError>(self.python) => {
                return Ok(Content::Object {
                    attributes: Vec::new(),
                    omitted: Some(Omitted::NoAttributes),
                });
            }
            Err(error) => {
                return Ok(Content::Object {
                    attributes: Vec::new(),
                    omitted: Some(Omitted::AttributesRaised {
                        error: capture(self.python, &error),
                    }),
                });
            }
        };
        let Ok(stored) = stored.cast::<PyDict>() else {
            return Ok(Content::Object {
                attributes: Vec::new(),
                omitted: Some(Omitted::NoAttributes),
            });
        };

        self.enter(object, path);
        let (attributes, omitted) = self.named(&stored.items(), path, depth - 1)?;
        self.leave();
        Ok(Content::Object {
            attributes,
            omitted,
        })
    }

    /// read a namespace mapping as names and values
    ///
    /// shared by an object's `__dict__` and by the namespace of a module or a
    /// class body frame, which is the same thing seen from the other end
    pub(crate) fn named(
        &mut self,
        entries: &Bound<'py, PyList>,
        path: &str,
        depth: u32,
    ) -> PyResult<(Vec<Entry>, Option<Omitted>)> {
        let length = entries.len();
        let wanted = length.min(self.detail.children as usize);

        let mut named = Vec::with_capacity(wanted);
        let mut not_names = 0usize;
        let mut omitted = None;
        for index in 0..wanted {
            if self.exhausted() {
                omitted = Some(self.budget());
                break;
            }
            let pair = entries
                .get_item(index)
                .expect("the snapshot is the interpreter's own list and the index is inside it");
            let key = pair.get_item(0)?;
            let Ok(name) = key.cast::<PyString>() else {
                // a dictionary can be given a key that is not a name by writing
                // into it directly. nothing can reach it by attribute access,
                // so it is not an attribute — and it is counted rather than
                // dropped
                not_names += 1;
                continue;
            };
            let name = name.to_string_lossy().into_owned();
            self.charge(name.len());
            let below = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}.{name}")
            };
            let value = pair.get_item(1)?;
            named.push(Entry {
                value: self.value(&value, &below, depth)?,
                name,
            });
        }

        if omitted.is_none() {
            omitted = self.cut_children(length);
        }
        if omitted.is_none() && not_names > 0 {
            omitted = Some(Omitted::NotNames { count: not_names });
        }
        Ok((named, omitted))
    }

    /// how many bytes of the budget are left
    fn remaining(&self) -> usize {
        (self.detail.budget as usize).saturating_sub(self.spent)
    }

    /// cut a piece of text to the request's limit, charging what is kept
    ///
    /// the character limit and the byte budget are different units and both
    /// have to hold: cutting to the budget in *characters* would spend four
    /// times it on text that is not ascii, and the budget would be a number the
    /// answer exceeded quietly
    fn cut(&mut self, text: &str) -> (String, usize, Option<Omitted>) {
        let characters = text.chars().count();
        let limit = self.detail.text as usize;
        let allowed = self.remaining();

        if characters <= limit && text.len() <= allowed {
            self.charge(text.len());
            return (text.to_string(), characters, None);
        }

        let mut kept = String::new();
        let mut taken = 0usize;
        for character in text.chars() {
            if taken == limit || kept.len() + character.len_utf8() > allowed {
                break;
            }
            kept.push(character);
            taken += 1;
        }
        self.charge(kept.len());

        if taken == limit && characters > limit {
            return (
                kept,
                characters,
                Some(Omitted::Text {
                    characters,
                    limit: self.detail.text,
                }),
            );
        }
        let omitted = self.budget();
        self.exhaust();
        (kept, characters, Some(omitted))
    }

    /// record that the budget is gone
    ///
    /// a value that spends the budget down to exactly zero has still run out,
    /// and everything after it has to see that
    fn exhaust(&mut self) {
        self.spent = (self.detail.budget as usize).saturating_add(1);
    }

    /// the omission for a container the `children` limit cut, if it cut one
    const fn cut_children(&self, length: usize) -> Option<Omitted> {
        if length > self.detail.children as usize {
            Some(Omitted::Children {
                length,
                limit: self.detail.children,
            })
        } else {
            None
        }
    }

    /// whether this object is already open above here, and where
    fn cycle(&self, object: &Bound<'py, PyAny>) -> Option<Omitted> {
        let address = object.as_ptr() as usize;
        self.open
            .iter()
            .find(|(open, _)| *open == address)
            .map(|(_, path)| Omitted::Cycle { path: path.clone() })
    }

    /// the address is only unique while the object is on the path, and it is
    /// held by the caller's own borrow for exactly that long
    fn enter(&mut self, object: &Bound<'py, PyAny>, path: &str) {
        self.open.push((object.as_ptr() as usize, path.to_string()));
    }

    fn leave(&mut self) {
        self.open
            .pop()
            .expect("every `leave` follows the `enter` that pushed for it");
    }
}

/// `type(value)`, qualified by its module unless it is a builtin
fn kind_of(object: &Bound<'_, PyAny>) -> PyResult<String> {
    Ok(object
        .get_type()
        .fully_qualified_name()?
        .to_string_lossy()
        .into_owned())
}
