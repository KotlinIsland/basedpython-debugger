//! what the debugger can prove about a name, and how long it stays proved
//!
//! a value read is a statement about a moment. a **fact** is a statement about
//! a moment *and* about how far past that moment it can be carried — which is
//! the only form an analysis of code that has not run yet can use
//!
//! the two are different questions and this module keeps them apart:
//!
//! - [`Observed`] is what was read. it is never the result of running the
//!   program's own code, which is why there are so few forms of it: everything
//!   here comes out of an object's storage or out of its type's slots
//! - [`Stability`] is what the runtime object says about whether the reading
//!   can go stale. an `int` cannot change; a `list`'s length can; a heap type's
//!   `__class__` is assignable and a C type's is not. that judgement is only
//!   available to something holding the object, which is why it is made here
//!   rather than by whatever consumes the facts
//!
//! what this module deliberately does **not** judge is whether the *name* still
//! holds the object. a name is rebound by code, and code is what the consumer
//! is reading. a debugger that guessed at it would be answering a question it
//! cannot see the input to
//!
//! ## nothing here runs the program
//!
//! the invariant is the same one [`crate::value`] holds and it is stronger
//! here, because a fact is carried *forward* over code that has not run. so a
//! reading that would need `__bool__`, `__len__`, `__eq__`, a property or a
//! `__getattr__` is not taken and not guessed at: the name is reported in
//! [`Facts::silent`] with [`Silence::WouldRun`] naming the thing that would
//! have run
//!
//! the cost of that rule is a shorter answer. the cost of breaking it is a
//! debugger whose act of measuring changes the program, which is not a trade
//! this project makes

use crate::frame::Scope;
use crate::stop::Mode;

/// everything the debugger can say about the names it was asked about
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Facts {
    /// what was proved, in the order the names were asked about
    ///
    /// one name can produce several: an empty `list` is its class, its length
    /// and its falsiness, and each of the three goes stale differently
    pub proved: Vec<Fact>,

    /// the names nothing could be said about, and why for each
    ///
    /// a name that was asked about and is missing from both lists would be an
    /// answer the caller cannot tell from a name that is bound to something
    /// uninteresting. there is no such thing here: every name asked about
    /// appears in exactly one of the two
    pub silent: Vec<Silent>,

    /// how the program was moving while this was read
    ///
    /// it qualifies the **reading** and not the judgement. whether a `list`'s
    /// length can change is a property of `list` and is true in either mode;
    /// whether it was three when it was read is a sample like any other, and on
    /// a non-stop read another thread could have appended to it since
    pub mode: Mode,
}

/// one thing that is true of one name
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Fact {
    /// the name, as the request spelled it
    ///
    /// a dotted path is spelled back exactly as it was asked for, so a caller
    /// that asked about `self.limit` does not have to reassemble it
    pub name: String,

    /// where the first segment of the name was found
    ///
    /// python decides this at compile time and a debugger that rounded it off
    /// would be reporting a global as a local — see [`Scope`]
    pub scope: Scope,

    /// what was read
    pub observed: Observed,

    /// how long what was read stays true
    pub stability: Stability,
}

/// a name the debugger looked at and could prove nothing about
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Silent {
    /// the name, as the request spelled it
    pub name: String,

    /// why there is nothing to say about it
    pub why: Silence,
}

/// why a name produced no facts
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "silence", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Silence {
    /// no scope the frame can see has this name bound
    ///
    /// a local that has not been assigned yet has this, and so does a plain
    /// misspelling. they are the same situation from here: the frame does not
    /// have it
    Unbound,

    /// a segment of a dotted path is not in the object's own storage
    ///
    /// the object has no instance dictionary, or it has one and this name is
    /// not in it. reaching it would mean `__getattr__`, which is the program
    Missing {
        /// the segment that was not there
        segment: String,
    },

    /// reading it would run the program's own code
    ///
    /// the type puts something of its own in the way — a property, a
    /// `__slots__` descriptor with a getter, a `__getattr__`, an overridden
    /// dunder. what it names is the thing that would have run
    WouldRun {
        /// the attribute of the *type* that would have been called
        member: String,
        /// the type that defines it, so the reader can go and look
        owner: Class,
    },

    /// the path has more segments than the request allowed
    ///
    /// a refusal rather than a truncation: reading four segments of a five
    /// segment path and answering about the fourth would be a fact about a
    /// different thing than the one asked about
    TooDeep {
        /// how many segments it has
        segments: usize,
        /// what the request allowed
        limit: u32,
    },
}

impl std::fmt::Display for Silence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unbound => formatter.write_str(
                "no scope this frame can see has it bound — a local that has not \
                 been assigned yet reads the same way",
            ),
            Self::Missing { segment } => write!(
                formatter,
                "`{segment}` is not in the object's own storage, and reaching it \
                 would mean calling `__getattr__`"
            ),
            Self::WouldRun { member, owner } => write!(
                formatter,
                "reading it would call `{owner}.{member}`, which is the program's \
                 own code — bpd does not run it to answer a question about it"
            ),
            Self::TooDeep { segments, limit } => write!(
                formatter,
                "the path has {segments} segments, of which the request allowed \
                 {limit}. ask again with a larger `depth`"
            ),
        }
    }
}

/// what was read off a value
///
/// every form is either the value itself, for the few types whose value *is*
/// their storage, or a property of the object that its type stores rather than
/// computes. there is nothing here that a `__dunder__` could have decided
///
/// closed on purpose. a consumer that swept an unknown form into a catch-all
/// would be treating a reading it does not understand as one it does
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "observed", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Observed {
    /// the value is `None`
    ///
    /// the singleton, compared by identity. a type that made itself compare
    /// equal to `None` is not `None` and does not get this
    IsNone,

    /// the value is exactly this `bool`
    IsBool {
        /// which one
        value: bool,
    },

    /// the value is exactly this integer, in decimal
    ///
    /// text rather than a number for the reason [`crate::Content::Int`] is: a
    /// python `int` has no width, and a json number that quietly became a float
    /// would be a different value. it is never cut — half of a number is a
    /// different number — so an integer longer than the request allowed
    /// produces no fact rather than a shortened one
    IsInt {
        /// the digits, with a leading `-` when negative
        text: String,
    },

    /// the value is exactly this float, as `float.__repr__` writes it
    ///
    /// python's own text, so `inf`, `nan` and `-0.0` survive
    IsFloat {
        /// the repr
        text: String,
    },

    /// the value is exactly this string
    ///
    /// whole, or not at all, for the reason an integer is
    IsStr {
        /// the characters
        text: String,
    },

    /// the value is exactly these bytes, in lowercase hex
    IsBytes {
        /// the bytes, in hex
        hex: String,
    },

    /// `type(value)` is exactly this class
    ///
    /// exactly, not "an instance of" — the type object itself, not a base of
    /// it. a consumer that wants the looser statement can derive it; one that
    /// was given the looser statement cannot derive this
    IsExactly {
        /// the class
        class: Class,
    },

    /// the value is this member of this enum
    ///
    /// read from the member's own storage rather than through `Enum.name`,
    /// which is a descriptor. the class is the enum class, so a consumer can
    /// resolve `class.member` in the source it is analysing
    IsEnumMember {
        /// the enum class
        class: Class,
        /// `_name_`, which is what `.name` reads
        member: String,
    },

    /// `len(value)` is this
    ///
    /// read from the type's `sq_length` or `mp_length` slot only when that slot
    /// is cpython's own. a `__len__` written in python is the program, and gets
    /// [`Silence::WouldRun`] instead
    HasLength {
        /// how many
        length: usize,
    },

    /// `bool(value)` is this
    ///
    /// decided without calling anything: from the value itself for the types
    /// whose truthiness is their value, and from the length for the containers
    /// whose length was readable. a type with its own `__bool__` produces no
    /// truthiness fact
    IsTruthy {
        /// whether it is truthy
        truthy: bool,
    },
}

/// a class, named so that something reading source can resolve it
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Class {
    /// the module the class was defined in — `__module__`
    ///
    /// `builtins` for a builtin, spelled out rather than left empty, because a
    /// consumer resolving names needs one rule and not two
    pub module: String,

    /// the class's name inside that module — `__qualname__`
    ///
    /// qualified, so a class nested in another class is distinguishable from a
    /// module level one of the same name
    pub qualname: String,
}

impl std::fmt::Display for Class {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.module == "builtins" {
            formatter.write_str(&self.qualname)
        } else {
            write!(formatter, "{}.{}", self.module, self.qualname)
        }
    }
}

/// how long an observation stays true
///
/// this is the question a client analysing later code is really asking, and it
/// is answerable only from the runtime object — which is what makes it the
/// debugger's to answer. the same reading of the same shape of code is
/// permanent for one object and not for another: `x == 3` where `x` is an `int`
/// cannot stop being true, and where `x` is an `int` subclass with a `__dict__`
/// and an assignable `__class__`, it can
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "stability", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Stability {
    /// nothing the program can do makes it false
    ///
    /// short of rebinding the name, which is not this judgement's to make — see
    /// the module docs
    Permanent,

    /// it is true now and something could make it false, named
    Until {
        /// what would have to happen
        mutation: Mutation,
    },
}

/// what could make an observation false
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Mutation {
    /// the object's contents can change
    ///
    /// a `list`, a `dict`, a `set`, a `bytearray`. what it *is* does not change
    /// and what it holds does, so a class fact about one is permanent and a
    /// length fact about the same object is not
    Contents,

    /// the object's attributes can change
    ///
    /// it keeps an instance dictionary, so any attribute of it can be assigned,
    /// deleted, or added. what a dotted path read out of one is worth exactly
    /// as long as nothing assigns to it
    Attributes,

    /// the object's type can change
    ///
    /// `__class__` is assignable on a heap type — an ordinary `class` statement
    /// produces one. cpython refuses it for a static type, which is why an
    /// `int` or a `list` is not this and a `class User:` is
    Class,
}

impl std::fmt::Display for Mutation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Contents => "the object's contents are mutable",
            Self::Attributes => {
                "the object keeps an instance dictionary, so its attributes \
                                 can be assigned"
            }
            Self::Class => "the object's type is a heap type, so `__class__` can be assigned",
        })
    }
}

impl std::fmt::Display for Stability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Permanent => formatter.write_str(
                "nothing the program can do makes it false, short of rebinding \
                 the name",
            ),
            Self::Until { mutation } => write!(formatter, "true until {mutation}"),
        }
    }
}

impl Stability {
    /// whether the observation can be carried over code that has not run
    ///
    /// the whole point of the distinction, in one predicate, so that every
    /// consumer asks it the same way rather than each matching the enum and
    /// deciding for itself what [`Mutation::Attributes`] means
    pub const fn is_permanent(&self) -> bool {
        matches!(self, Self::Permanent)
    }
}

/// how much of a fact reading is allowed to cost
///
/// deserialised with `deny_unknown_fields` for the reason [`crate::Detail`] is:
/// a misspelled bound is an instruction the client gave and bpd ignored
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limit {
    /// how many characters of a string, or digits of an integer, a fact may
    /// carry
    ///
    /// a fact is a value the consumer will compare against source, so a cut one
    /// is worse than none: `x == "abc…"` is a different claim from `x == "abcd"`
    /// and there is no way to mark it as approximate in a claim. a value over
    /// this produces no fact, and the name is reported as silent
    #[serde(default = "Limit::text")]
    pub text: u32,

    /// how many segments deep a dotted path may go
    ///
    /// each segment is another instance dictionary read, and each one is
    /// another thing that could be a descriptor. it is bounded because a client
    /// composing paths from source can compose arbitrarily long ones
    #[serde(default = "Limit::depth")]
    pub depth: u32,
}

impl Limit {
    /// the default character bound
    ///
    /// generous next to [`crate::Detail::text`], because a fact carries one
    /// value rather than a graph of them, and a string literal in source that
    /// somebody is comparing against is usually short
    const fn text() -> u32 {
        1024
    }

    /// the default path depth
    ///
    /// `self.config.timeout` is three segments and is about as deep as source
    /// that a person is reading goes
    const fn depth() -> u32 {
        4
    }
}

impl Default for Limit {
    fn default() -> Self {
        Self {
            text: Self::text(),
            depth: Self::depth(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_builtin_class_is_named_without_its_module_and_anything_else_with_it() {
        let builtin = Class {
            module: "builtins".to_string(),
            qualname: "list".to_string(),
        };
        assert_eq!(builtin.to_string(), "list");

        let nested = Class {
            module: "myapp.models".to_string(),
            qualname: "User.Role".to_string(),
        };
        assert_eq!(nested.to_string(), "myapp.models.User.Role");
    }

    #[test]
    fn every_silence_says_what_stopped_it_and_names_the_thing_that_would_have_run() {
        let cases = [
            (Silence::Unbound, vec!["not been assigned"]),
            (
                Silence::Missing {
                    segment: "limit".to_string(),
                },
                vec!["`limit`", "__getattr__"],
            ),
            (
                Silence::WouldRun {
                    member: "__len__".to_string(),
                    owner: Class {
                        module: "myapp".to_string(),
                        qualname: "Basket".to_string(),
                    },
                },
                vec!["myapp.Basket.__len__", "does not run it"],
            ),
        ];

        for (silence, expected) in cases {
            let said = silence.to_string();
            for wanted in expected {
                assert!(said.contains(wanted), "expected {wanted:?} in {said:?}");
            }
        }
    }

    #[test]
    fn only_a_permanent_observation_may_be_carried_forward() {
        assert!(Stability::Permanent.is_permanent());
        for mutation in [Mutation::Contents, Mutation::Attributes, Mutation::Class] {
            assert!(!Stability::Until { mutation }.is_permanent());
            assert!(
                !mutation.to_string().is_empty(),
                "a mutation a client is shown has to say what it is"
            );
        }
    }

    #[test]
    fn a_bound_that_is_misspelled_is_refused_rather_than_taken_as_the_default() {
        let good: Limit = serde_json::from_str(r#"{"text": 16}"#).expect("`text` is a bound");
        assert_eq!(good.text, 16);
        assert_eq!(good.depth, Limit::depth(), "an absent bound is the default");

        serde_json::from_str::<Limit>(r#"{"txt": 16}"#)
            .expect_err("a misspelled bound is an instruction bpd would otherwise ignore");
    }
}
