//! what a `variablesReference` and a `frameId` actually name
//!
//! DAP hands a client an opaque number and expects it back later. bpd's own
//! ids are not opaque — a [`FrameId`] carries the stop it was minted at, so a
//! client holding a stale one finds out rather than being answered about a
//! different program. this table is where the two meet: a reference is an index
//! into it, and the entries of a stop are forgotten when that stop's thread is
//! resumed — so a reference from a stop that has ended is refused by name
//! rather than resolved against whatever is at that index now
//!
//! ## why a nested value is a path and not a value
//!
//! DAP expands an object graph one node at a time, and bpd reads one to a
//! stated depth in a single request. so expanding a node re-reads its scope one
//! level deeper and walks back down to it. the alternative — keeping the graph
//! that was read and serving expansions out of it — cannot go deeper than the
//! depth the first read used, and the depth a client will ask for is not
//! knowable in advance

use bpd_core::{Content, FrameId, Scope, Value};

/// one move from a value to something inside it
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// an attribute of an object, by name
    Attribute(String),
    /// an item of a list, a tuple or a set, by position
    Item(usize),
    /// the key of a mapping's nth entry
    Key(usize),
    /// the value of a mapping's nth entry
    Value(usize),
}

impl std::fmt::Display for Step {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Attribute(name) => write!(formatter, ".{name}"),
            Self::Item(index) => write!(formatter, "[{index}]"),
            Self::Key(index) => write!(formatter, "[{index}].key"),
            Self::Value(index) => write!(formatter, "[{index}].value"),
        }
    }
}

/// what a reference the adapter handed out names
#[derive(Debug, Clone)]
pub enum Handle {
    /// a frame of a held thread's stack
    Frame(FrameId),

    /// a django template frame of a held thread's stack
    ///
    /// a separate handle rather than a flag beside [`Handle::Frame`], because
    /// what a client may do with one is different: it has no python scopes, its
    /// variables are a layered template context, and an expression evaluated in
    /// it is template syntax. DAP has one `frameId` for both, so the handle is
    /// where the difference is kept
    TemplateFrame(FrameId),

    /// one layer of a template frame's django context
    ///
    /// `Context.dicts` is a stack, and each layer is offered as its own DAP
    /// scope. merging them would hide which layer holds a name, which is what
    /// decides the render
    TemplateLayer {
        /// which template frame
        frame: FrameId,
        /// which layer of its context, counting from zero at the outermost
        index: u32,
    },

    /// one scope of one frame
    Scope {
        /// which frame
        frame: FrameId,
        /// which scope of it
        scope: Scope,
    },

    /// something inside one scope of one frame
    Nested {
        /// which frame
        frame: FrameId,
        /// which scope of it
        scope: Scope,
        /// how to get from the scope's names to this value
        path: Vec<Step>,
    },

    /// a value that was read once and has no scope to re-read it from
    ///
    /// what an evaluated expression produces. it cannot be re-read, because
    /// reading it again would mean running the expression again — and an
    /// expression is the program's own code, which a client asked to run once.
    /// so it is opened only as deep as the read that produced it went, and
    /// where that ran out the value says so
    Stored {
        /// the stop it was read at, which is when it stops being about the
        /// program in front of the client
        stop: u64,
        /// what was read
        value: Value,
    },
}

impl Handle {
    /// the stop this reference belongs to
    ///
    /// every reference is about a thread that is held, and stops being about
    /// anything the moment that thread is resumed
    pub const fn stop(&self) -> u64 {
        match self {
            Self::Frame(frame)
            | Self::TemplateFrame(frame)
            | Self::Scope { frame, .. }
            | Self::Nested { frame, .. }
            | Self::TemplateLayer { frame, .. } => frame.stop,
            Self::Stored { stop, .. } => *stop,
        }
    }
}

/// the references handed out, and which stop each belongs to
///
/// a forgotten reference leaves a hole rather than being removed: the reference
/// **is** the index, so closing the gap would hand a client's old number to
/// somebody else's frame — which is the stale handle problem with an extra step
#[derive(Debug, Default)]
pub struct Handles {
    entries: Vec<Option<Handle>>,
}

impl Handles {
    /// forget the references that belong to stops that have ended
    ///
    /// a reference that survived a resume would name a frame that has run on
    /// since. bpd's own [`FrameId`] carries the stop it was minted at and can
    /// say so; DAP's opaque number cannot, so this is where the difference is
    /// kept: the number stops resolving, and the client is told why
    pub fn forget(&mut self, stops: &[u64]) {
        for entry in &mut self.entries {
            if entry
                .as_ref()
                .is_some_and(|held| stops.contains(&held.stop()))
            {
                *entry = None;
            }
        }
    }

    /// forget every reference handed out
    pub fn clear(&mut self) {
        for entry in &mut self.entries {
            *entry = None;
        }
    }

    /// hand out a reference to `handle`
    pub fn add(&mut self, handle: Handle) -> i64 {
        self.entries.push(Some(handle));
        i64::try_from(self.entries.len()).expect("a stop does not hand out 2^63 references")
    }

    /// what a reference names, or `None` when it names nothing
    pub fn get(&self, reference: i64) -> Option<&Handle> {
        let index = usize::try_from(reference).ok()?.checked_sub(1)?;
        self.entries.get(index)?.as_ref()
    }
}

/// one thing inside a value
#[derive(Debug)]
pub struct Child<'v> {
    /// what to call it
    pub name: String,
    /// how to reach it from the value it is in
    pub step: Step,
    /// what it holds
    pub value: &'v Value,
}

/// what is inside a value, in the order the interpreter keeps it
///
/// a mapping is the awkward one, and deliberately so: a key is an object, not a
/// name, so `key: value` is a false statement about every dict that is not
/// keyed by strings. a **whole** string key is a name and is used as one;
/// anything else — a number, a tuple, a string the read had to cut — gets a
/// positional name and its key is listed beside it, so nothing about the entry
/// goes unsaid
pub fn children(value: &Value) -> Vec<Child<'_>> {
    match &value.content {
        Content::Sequence { items, .. } => items
            .iter()
            .enumerate()
            .map(|(index, item)| Child {
                name: format!("[{index}]"),
                step: Step::Item(index),
                value: item,
            })
            .collect(),

        Content::Mapping { entries, .. } => {
            let mut children = Vec::new();
            for (index, entry) in entries.iter().enumerate() {
                match &entry.key.content {
                    Content::Str {
                        text,
                        omitted: None,
                        ..
                    } => children.push(Child {
                        name: text.clone(),
                        step: Step::Value(index),
                        value: &entry.value,
                    }),
                    _ => {
                        children.push(Child {
                            name: format!("[{index}] key"),
                            step: Step::Key(index),
                            value: &entry.key,
                        });
                        children.push(Child {
                            name: format!("[{index}]"),
                            step: Step::Value(index),
                            value: &entry.value,
                        });
                    }
                }
            }
            children
        }

        Content::Object { attributes, .. } => attributes
            .iter()
            .map(|entry| Child {
                name: entry.name.clone(),
                step: Step::Attribute(entry.name.clone()),
                value: &entry.value,
            })
            .collect(),

        // a number, a string, a repr and a value nothing was read of have
        // nothing inside them to name. `Content` is `#[non_exhaustive]`, and a
        // form this adapter has not been taught is the same answer: inventing a
        // name for something it cannot read is the one thing it must not do
        _ => Vec::new(),
    }
}

/// follow `path` into `root`
///
/// the path was recorded against an earlier read and the value is read again to
/// follow it, so a step can fail to land — a list another thread shortened, a
/// dict whose iteration order moved. that is reported rather than absorbed:
/// answering about whatever is at that index now would be answering about a
/// different object
pub fn walk<'v>(root: &'v Value, path: &[Step]) -> Result<&'v Value, Lost> {
    let mut here = root;
    for (taken, step) in path.iter().enumerate() {
        here = match (step, &here.content) {
            (Step::Item(index), Content::Sequence { items, .. }) => items.get(*index),
            (Step::Key(index), Content::Mapping { entries, .. }) => {
                entries.get(*index).map(|entry| &entry.key)
            }
            (Step::Value(index), Content::Mapping { entries, .. }) => {
                entries.get(*index).map(|entry| &entry.value)
            }
            (Step::Attribute(name), Content::Object { attributes, .. }) => attributes
                .iter()
                .find(|entry| entry.name == *name)
                .map(|entry| &entry.value),
            _ => None,
        }
        .ok_or_else(|| Lost {
            path: path.to_vec(),
            reached: taken,
            found: here.kind.clone(),
        })?;
    }
    Ok(here)
}

/// a path that no longer leads anywhere
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lost {
    /// the whole path that was being followed
    pub path: Vec<Step>,
    /// how many steps of it landed
    pub reached: usize,
    /// the type of the value the walk stopped at
    pub found: String,
}

impl std::fmt::Display for Lost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let whole: String = self.path.iter().map(ToString::to_string).collect();
        let reached: String = self
            .path
            .iter()
            .take(self.reached)
            .map(ToString::to_string)
            .collect();
        write!(
            formatter,
            "`{whole}` no longer leads anywhere: `{reached}` is a `{}` with \
             nothing at `{}`. the value was read again to open it and the \
             program has changed it since — every thread but the held one keeps \
             running. expand it again from the scope it is in",
            self.found, self.path[self.reached]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bpd_core::{Entry, Omitted, Pair};

    fn int(text: &str) -> Value {
        Value {
            kind: "int".to_string(),
            content: Content::Int {
                text: text.to_string(),
                omitted: None,
            },
        }
    }

    fn text(value: &str, omitted: Option<Omitted>) -> Value {
        Value {
            kind: "str".to_string(),
            content: Content::Str {
                text: value.to_string(),
                characters: value.chars().count(),
                omitted,
            },
        }
    }

    fn mapping(entries: Vec<Pair>) -> Value {
        Value {
            kind: "dict".to_string(),
            content: Content::Mapping {
                length: entries.len(),
                entries,
                omitted: None,
            },
        }
    }

    #[test]
    fn a_reference_is_never_zero_and_names_what_it_was_given() {
        let mut handles = Handles::default();
        let first = handles.add(Handle::Frame(FrameId { stop: 1, depth: 0 }));

        assert_ne!(first, 0, "zero means `not expandable` in DAP");
        assert!(matches!(handles.get(first), Some(Handle::Frame(_))));
        assert!(handles.get(first + 1).is_none());
        assert!(handles.get(0).is_none());
    }

    #[test]
    fn resuming_one_thread_forgets_its_references_and_leaves_the_others_alone() {
        let mut handles = Handles::default();
        let resumed = handles.add(Handle::Frame(FrameId { stop: 1, depth: 0 }));
        let other = handles.add(Handle::Frame(FrameId { stop: 2, depth: 0 }));

        handles.forget(&[1]);

        assert!(
            handles.get(resumed).is_none(),
            "a reference minted at a stop that has ended has to be refused"
        );
        assert!(
            handles.get(other).is_some(),
            "a stop holds one thread, so resuming it says nothing about another"
        );

        // and the hole stays a hole: the reference is the index, so closing it
        // would hand this client's old number to somebody else's frame
        let minted = handles.add(Handle::Frame(FrameId { stop: 3, depth: 0 }));
        assert_ne!(minted, resumed);
    }

    #[test]
    fn a_whole_string_key_is_a_name_and_anything_else_is_listed_beside_its_value() {
        let dict = mapping(vec![
            Pair {
                key: text("total", None),
                value: int("7"),
            },
            Pair {
                key: int("42"),
                value: int("8"),
            },
        ]);

        let named: Vec<_> = children(&dict)
            .into_iter()
            .map(|child| (child.name, child.step))
            .collect();

        assert_eq!(
            named,
            vec![
                ("total".to_string(), Step::Value(0)),
                ("[1] key".to_string(), Step::Key(1)),
                ("[1]".to_string(), Step::Value(1)),
            ]
        );
    }

    #[test]
    fn a_key_the_read_had_to_cut_is_not_used_as_a_name() {
        // half of a key is a different key, and using it as a name would claim
        // the entry is called something it is not
        let dict = mapping(vec![Pair {
            key: text(
                "abc",
                Some(Omitted::Text {
                    characters: 900,
                    limit: 3,
                }),
            ),
            value: int("1"),
        }]);

        assert_eq!(
            children(&dict)
                .into_iter()
                .map(|child| child.name)
                .collect::<Vec<_>>(),
            vec!["[0] key".to_string(), "[0]".to_string()]
        );
    }

    #[test]
    fn a_path_walks_down_and_says_where_it_stopped_when_it_cannot() {
        let object = Value {
            kind: "Widget".to_string(),
            content: Content::Object {
                attributes: vec![Entry {
                    name: "items".to_string(),
                    value: Value {
                        kind: "list".to_string(),
                        content: Content::Sequence {
                            items: vec![int("1")],
                            length: 1,
                            omitted: None,
                        },
                    },
                }],
                omitted: None,
            },
        };

        let path = vec![Step::Attribute("items".to_string()), Step::Item(0)];
        assert_eq!(
            walk(&object, &path).expect("the path leads somewhere").kind,
            "int"
        );

        let gone = vec![Step::Attribute("items".to_string()), Step::Item(4)];
        let lost = walk(&object, &gone).expect_err("there is no fifth item");
        assert_eq!(lost.reached, 1);
        let said = lost.to_string();
        assert!(said.contains(".items[4]"), "said {said}");
        assert!(said.contains("`list`"), "said {said}");
    }
}
