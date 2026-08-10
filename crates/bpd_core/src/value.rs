//! values, and how much of one an answer is allowed to carry
//!
//! every bound a request sets is a bound the answer is held to, and every bound
//! that bites is named in the answer. there is no setting here that makes a
//! value quietly incomplete

use crate::exception::PythonError;

/// how much of a value to read, and what the debugger may run to read it
///
/// every field is a bound the answer is held to, and every bound that bites is
/// named in the answer. there is no setting here that makes a value quietly
/// incomplete
/// deserialised with `deny_unknown_fields` because a client's spelling of one
/// of these is the *only* way to raise a bound the answer told it to raise. a
/// misspelled `dept` that quietly took the default would be an instruction the
/// client followed and bpd ignored, and the next answer would be cut in the same
/// place with the same advice
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Detail {
    /// how many levels of container or object to open
    ///
    /// zero reports a value's type and size and opens nothing
    #[serde(default = "Detail::depth")]
    pub depth: u32,

    /// how many children of one container to read
    #[serde(default = "Detail::children")]
    pub children: u32,

    /// how many characters of one string, or bytes of one `bytes`, to read
    #[serde(default = "Detail::text")]
    pub text: u32,

    /// the byte budget for the whole answer
    ///
    /// spent on the text a value carries, its type name, and a fixed cost per
    /// value for the envelope around it. when it runs out the answer says so at
    /// the point it ran out, rather than being quietly shorter than it looks
    #[serde(default = "Detail::budget")]
    pub budget: u32,

    /// read an object's instance dictionary
    ///
    /// on by default, because it is **storage**: for an ordinary object it is a
    /// slot read that runs nothing, and it never reaches `__getattr__`, a
    /// property or any other descriptor. a type is free to make `__dict__` its
    /// own code, and then this runs that code — which is why it can be turned
    /// off for a program full of proxies or mocks
    #[serde(default = "Detail::attributes")]
    pub attributes: bool,

    /// call `__repr__` on a value that has no structural representation
    ///
    /// off by default, because it is **behaviour**: `__repr__` is arbitrary user
    /// code that can hang, mutate the program, or reach the network. bpd cannot
    /// interrupt it once it has started, so it is never called unless the
    /// request asked for it
    #[serde(default)]
    pub repr: bool,
}

impl Detail {
    /// the default depth
    const fn depth() -> u32 {
        3
    }
    /// the default number of children per container
    const fn children() -> u32 {
        100
    }
    /// the default number of characters of one string
    const fn text() -> u32 {
        1024
    }
    /// the default byte budget for one answer
    ///
    /// this is a starting point rather than a settled answer: the budget is
    /// spending an agent's context window, and what it is worth cannot be known
    /// until there is an agent surface to measure it against
    const fn budget() -> u32 {
        8192
    }
    /// whether an object's instance dictionary is read by default
    const fn attributes() -> bool {
        true
    }
}

impl Default for Detail {
    fn default() -> Self {
        Self {
            depth: Self::depth(),
            children: Self::children(),
            text: Self::text(),
            budget: Self::budget(),
            attributes: Self::attributes(),
            repr: false,
        }
    }
}

/// a value as the debugger read it
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Value {
    /// `type(value)`, qualified by its module unless it is a builtin
    ///
    /// always present, and always the value's real type: a `defaultdict` reads
    /// as a mapping and says it is a `collections.defaultdict`
    pub kind: String,
    /// what it is
    pub content: Content,
}

/// what a value turned out to be
///
/// the structural forms are read through cpython's concrete C interface — the
/// object's own storage — so an overridden `__getitem__` or `__iter__` cannot
/// change what is reported, and reading one runs no python
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "content", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Content {
    /// `None`
    None,

    /// a `bool`
    Bool {
        /// which one
        value: bool,
    },

    /// an integer, in decimal
    ///
    /// text rather than a number because a python `int` has no width, and a
    /// json number that silently became a float would be a different value.
    /// text is **never** cut: half of a number is a different number, so an
    /// integer too long for the budget is left out entirely and says so
    Int {
        /// the digits, or empty when `omitted` says why they are not here
        text: String,
        /// why the digits are not here
        omitted: Option<Omitted>,
    },

    /// a float, as `float.__repr__` writes it
    ///
    /// python's own text, so `inf`, `nan` and `-0.0` survive — a json number
    /// cannot carry the first two at all
    Float {
        /// the repr
        text: String,
    },

    /// a string, as itself rather than as a repr
    Str {
        /// the characters, cut to the request's limit
        text: String,
        /// how many characters the whole string has
        characters: usize,
        /// why they are not all here
        omitted: Option<Omitted>,
    },

    /// `bytes` or a `bytearray`, in lowercase hex
    Bytes {
        /// the bytes, in hex, cut to the request's limit
        hex: String,
        /// how many bytes the whole value has
        length: usize,
        /// why they are not all here
        omitted: Option<Omitted>,
    },

    /// a list, a tuple or a set
    Sequence {
        /// the items, in order for a list or a tuple and in iteration order for
        /// a set
        items: Vec<Value>,
        /// how many items the whole value has
        length: usize,
        /// why they are not all here
        omitted: Option<Omitted>,
    },

    /// a mapping, as pairs rather than as names
    ///
    /// a key can be any object, so it is a value in its own right. a mapping
    /// reported as `name: value` would be a lie about every dict that is not
    /// keyed by strings
    Mapping {
        /// the entries, in iteration order
        entries: Vec<Pair>,
        /// how many entries the whole mapping has
        length: usize,
        /// why they are not all here
        omitted: Option<Omitted>,
    },

    /// an object, read from its instance dictionary
    Object {
        /// the attributes it stores
        attributes: Vec<Entry>,
        /// why they are not all here, or why there are none
        omitted: Option<Omitted>,
    },

    /// what `__repr__` said, because the request asked for it
    ///
    /// labelled, so nothing can mistake user code's opinion of a value for the
    /// value
    Repr {
        /// the text, cut to the request's limit
        text: String,
        /// how many characters it produced
        characters: usize,
        /// why they are not all here
        omitted: Option<Omitted>,
    },

    /// nothing was read, and this is why
    ///
    /// a cycle, or a budget that ran out before this value was reached
    Unread {
        /// what stopped it
        omitted: Omitted,
    },
}

/// one named thing: a variable, or an attribute
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// the name
    pub name: String,
    /// what it holds
    pub value: Value,
}

/// one entry of a mapping
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Pair {
    /// the key, which is a value like any other
    pub key: Value,
    /// what it maps to
    pub value: Value,
}

/// what is not in an answer, and why
///
/// every one of these is a statement that something exists and is not here. an
/// answer that was cut and did not say so is worse for an agent than for a
/// person, who would at least see the ellipsis
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "omitted", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Omitted {
    /// there is more text than the request asked to see
    Text {
        /// how long the whole thing is
        characters: usize,
        /// what the request allowed
        limit: u32,
    },

    /// there are more children than the request asked to see
    Children {
        /// how many there are
        length: usize,
        /// what the request allowed
        limit: u32,
    },

    /// the depth ran out here
    Depth {
        /// the depth that was applied
        ///
        /// the request's `depth`, unless [`Omitted::Shallower`] says the budget
        /// could not fit it
        limit: u32,
    },

    /// the request's depth did not fit the budget, so less of it was read
    ///
    /// a set of variables is read at the deepest whole level the budget allows,
    /// rather than at the level asked for until it runs out. spending the whole
    /// budget on whichever variable came first is honest and useless: every
    /// module namespace begins with `__builtins__`, and an answer that opened
    /// that and nothing else would be a true statement about the wrong thing
    Shallower {
        /// the depth the request asked for
        asked: u32,
        /// the depth that fitted
        used: u32,
    },

    /// the answer's byte budget ran out here
    Budget {
        /// what the request allowed
        limit: u32,
    },

    /// this object is already open further up the same answer
    ///
    /// a structure that points back at itself terminates here and says where it
    /// came round to, rather than stopping silently — which would look exactly
    /// like a structure that ended
    Cycle {
        /// where in this answer it was already opened
        path: String,
    },

    /// the type keeps no instance dictionary
    ///
    /// a `__slots__` class, or a type implemented in C. what it holds is only
    /// reachable by running its own code
    NoAttributes,

    /// the request did not ask for an object's attributes
    AttributesNotRequested,

    /// reading the instance dictionary raised
    ///
    /// which means the type made `__dict__` its own code, and that code failed
    AttributesRaised {
        /// what it raised
        error: PythonError,
    },

    /// the namespace is not a dictionary
    ///
    /// a class body whose metaclass prepared its own mapping — what `enum` does
    /// — has one. reading it means calling that mapping's own code, which is
    /// the program, so it is named instead of run
    NotADictionary,

    /// the string holds code points that cannot be encoded as utf-8
    ///
    /// lone surrogates, which is what `surrogateescape` produces for a
    /// filename the filesystem encoding could not decode. json cannot carry
    /// them and neither can rust, so they are replaced rather than dropped
    Unencodable,

    /// entries of an object's dictionary whose keys are not names
    ///
    /// reachable by writing into `__dict__` directly. they are not attributes,
    /// nothing can read them by name, and they are not silently dropped either
    NotNames {
        /// how many there are
        count: usize,
    },
}

impl std::fmt::Display for Omitted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text { characters, limit } => write!(
                formatter,
                "{characters} characters, of which the request allowed {limit}. \
                 ask again with a larger `text`"
            ),
            Self::Children { length, limit } => write!(
                formatter,
                "{length} children, of which the request allowed {limit}. ask \
                 again with a larger `children`"
            ),
            Self::Depth { limit } => write!(
                formatter,
                "the depth of {limit} that was applied ran out here. ask again \
                 with a larger `depth`"
            ),
            Self::Shallower { asked, used } => write!(
                formatter,
                "the request asked for a depth of {asked} and the byte budget \
                 fitted {used}, so every value here was read to {used}. ask \
                 again with a larger `budget`, or for one value rather than a \
                 whole scope"
            ),
            Self::Budget { limit } => write!(
                formatter,
                "the request's byte budget of {limit} ran out here. ask again \
                 with a larger `budget`, or for less of the graph"
            ),
            Self::Cycle { path } => write!(
                formatter,
                "this is the same object as `{path}`, which is already open \
                 above it. the structure points back at itself"
            ),
            Self::NoAttributes => formatter.write_str(
                "the type keeps no instance dictionary — it uses `__slots__`, or \
                 it is implemented in C — so what it holds cannot be read \
                 without running its own code. ask again with `repr`",
            ),
            Self::AttributesNotRequested => formatter.write_str(
                "the request asked for no attributes, so the object was not \
                 opened. ask again with `attributes`",
            ),
            Self::AttributesRaised { error } => write!(
                formatter,
                "reading the instance dictionary raised {error}, so the type \
                 made `__dict__` its own code and that code failed"
            ),
            Self::NotADictionary => formatter.write_str(
                "the namespace is not a `dict` — a class body whose metaclass \
                 prepared its own mapping has one — so reading it would mean \
                 running that mapping's own code",
            ),
            Self::Unencodable => formatter.write_str(
                "the string holds code points that cannot be encoded as utf-8 — \
                 lone surrogates, which is what `surrogateescape` produces for \
                 an undecodable filename — and they are replaced here with \
                 U+FFFD",
            ),
            Self::NotNames { count } => write!(
                formatter,
                "{count} entries of the instance dictionary have keys that are \
                 not names, so they are not attributes and nothing can read \
                 them by name"
            ),
        }
    }
}

/// what an expression did
///
/// an expression that raised has an answer, and the answer is the exception.
/// reporting `None` for it would be the debugger inventing a value the program
/// never produced
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "evaluated", rename_all = "snake_case")]
pub enum Evaluated {
    /// it produced a value
    Value {
        /// what it produced
        value: Value,
    },
    /// it raised
    Raised {
        /// what it raised
        error: PythonError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_omission_says_what_is_missing_and_how_to_ask_for_it() {
        let cases = [
            (
                Omitted::Text {
                    characters: 4000,
                    limit: 100,
                },
                ["4000", "`text`"],
            ),
            (
                Omitted::Children {
                    length: 900,
                    limit: 10,
                },
                ["900", "`children`"],
            ),
            (Omitted::Depth { limit: 2 }, ["depth of 2", "`depth`"]),
            (
                Omitted::Shallower { asked: 3, used: 1 },
                ["asked for a depth of 3", "fitted 1"],
            ),
            (Omitted::Budget { limit: 64 }, ["budget of 64", "`budget`"]),
            (
                Omitted::Cycle {
                    path: "node.next".to_string(),
                },
                ["node.next", "points back at itself"],
            ),
            (Omitted::NoAttributes, ["__slots__", "`repr`"]),
            (
                Omitted::AttributesNotRequested,
                ["no attributes", "`attributes`"],
            ),
            (Omitted::NotNames { count: 2 }, ["2 entries", "not names"]),
            (Omitted::Unencodable, ["surrogate", "U+FFFD"]),
            (Omitted::NotADictionary, ["metaclass", "not a `dict`"]),
        ];

        for (omission, expected) in cases {
            let said = omission.to_string();
            for wanted in expected {
                assert!(said.contains(wanted), "expected {wanted:?} in {said:?}");
            }
        }
    }
}
