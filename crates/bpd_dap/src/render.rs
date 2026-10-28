//! turning a value into the one line DAP shows beside a name
//!
//! this is presentation and nothing else. no summary here decides anything
//! about the program — where a value was cut, and why, comes from the
//! [`Omitted`] the core already attached to it, rendered rather than
//! interpreted. an ellipsis with no explanation is what this project's rules
//! about saying what was left out exist to prevent

use bpd_core::{Content, Omitted, Value};

/// the one line shown beside a variable's name
///
/// a container is shown as **what it holds**, the way the program would print
/// it — `[1.0, 3.14159]`, `{'square': 144.0}` — rather than as its type and a
/// count. measured in the IDE before this: every list and dict read
/// `list[6] — the depth of 0 that was applied ran out here. ask again with a
/// larger 'depth'`, which is bpd's notice where the program's value goes. what
/// the line cannot hold is counted on it — `… 4 more` — and a reason that is
/// not "there was not room" is still said after it
pub fn summary(value: &Value) -> String {
    let (shown, omitted) = match &value.content {
        Content::None => ("None".to_string(), None),
        Content::Bool { value } => ((if *value { "True" } else { "False" }).to_string(), None),
        Content::Int { text, omitted } => (text.clone(), omitted.as_ref()),
        Content::Float { text } => (text.clone(), None),
        Content::Str {
            text,
            characters,
            omitted,
        } => (
            format!("{text:?} ({characters} characters)"),
            omitted.as_ref(),
        ),
        Content::Bytes {
            hex,
            length,
            omitted,
        } => (format!("b'{hex}' ({length} bytes)"), omitted.as_ref()),
        Content::Sequence { omitted, .. } | Content::Mapping { omitted, .. } => {
            let mut room = PREVIEW;
            (preview(value, &mut room), said_beside(omitted.as_ref()))
        }
        Content::Object { omitted, .. } => (value.kind.clone(), omitted.as_ref()),
        Content::Repr {
            text,
            characters,
            omitted,
        } => (
            format!("{text} (__repr__, {characters} characters)"),
            omitted.as_ref(),
        ),
        Content::Unread { omitted } => (String::new(), Some(omitted)),

        // `Content` is `#[non_exhaustive]`: a form this adapter has not been
        // taught is shown as what the core called it rather than as nothing
        _ => (value.kind.clone(), None),
    };

    match omitted {
        Some(omitted) if shown.is_empty() => format!("not read — {omitted}"),
        Some(omitted) => format!("{shown} — {omitted}"),
        None => shown,
    }
}

/// how many characters a container's preview grows to before the rest of it is
/// counted rather than shown
///
/// one line beside a name, so about what an editor's variables view shows
/// before it cuts the line itself — and a cut of the editor's would say nothing
/// about how much was left out
const PREVIEW: usize = 100;

/// the reason a container was cut that its preview does not already say
///
/// a depth or a child limit that ran out is items that were not read, and the
/// preview counts those where they would have been. a budget, a cycle or a type
/// with nothing to read is a different fact, and is said after it
fn said_beside(omitted: Option<&Omitted>) -> Option<&Omitted> {
    omitted.filter(|omitted| !matches!(omitted, Omitted::Depth { .. } | Omitted::Children { .. }))
}

/// a value as the program would print it, in at most about `room` characters
///
/// what does not fit, or was not read, is counted where it would have been:
/// `[1, 2, … 98 more]`. a value nothing was read of is `<list, not read>` —
/// never an empty container, which would be a claim about what it holds
fn preview(value: &Value, room: &mut usize) -> String {
    let text = match &value.content {
        Content::None => "None".to_string(),
        Content::Bool { value } => (if *value { "True" } else { "False" }).to_string(),
        Content::Int {
            text,
            omitted: None,
        }
        | Content::Float { text }
        | Content::Repr { text, .. } => text.clone(),
        Content::Str {
            text,
            characters,
            omitted,
        } => {
            let cut = if omitted.is_some() || text.chars().count() < *characters {
                "…"
            } else {
                ""
            };
            format!("'{}{cut}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
        }
        Content::Bytes {
            hex,
            length,
            omitted,
        } => {
            let cut = if omitted.is_some() || hex.len() / 2 < *length {
                "…"
            } else {
                ""
            };
            format!("bytes({length}: {hex}{cut})")
        }
        Content::Sequence { items, length, .. } => {
            let (open, close) = brackets(&value.kind, length.min(&items.len()).to_owned());
            let mut shown = Vec::new();
            for item in items {
                if *room == 0 {
                    break;
                }
                shown.push(preview(item, room));
            }
            joined(&value.kind, open, close, &shown, *length)
        }
        Content::Mapping {
            entries, length, ..
        } => {
            let mut shown = Vec::new();
            for pair in entries {
                if *room == 0 {
                    break;
                }
                let key = preview(&pair.key, room);
                let item = preview(&pair.value, room);
                shown.push(format!("{key}: {item}"));
            }
            joined(&value.kind, "{", "}", &shown, *length)
        }
        Content::Unread { .. } => format!("<{}, not read>", value.kind),
        // an object, an integer whose digits were not read, and a form this
        // adapter has not been taught — `Content` is `#[non_exhaustive]` — are
        // their type and nothing that would claim to be their value
        _ => format!("<{}>", value.kind),
    };
    *room = room.saturating_sub(text.chars().count() + 2);
    text
}

/// how a sequence of this type is written, and its brackets
fn brackets(kind: &str, items: usize) -> (&'static str, &'static str) {
    match kind {
        // a one-element tuple is `(x,)`, and `(x)` is not a tuple
        "tuple" if items == 1 => ("(", ",)"),
        "tuple" => ("(", ")"),
        "set" | "frozenset" => ("{", "}"),
        _ => ("[", "]"),
    }
}

/// the items shown, and a count of the ones that are not
///
/// the builtins are written as their literals and anything else as its type
/// around one — `deque([1, 2])`, `OrderedDict({'a': 1})` — which is how most of
/// them print themselves. an empty set is `set()`, because `{}` is a dict
fn joined(kind: &str, open: &str, close: &str, shown: &[String], length: usize) -> String {
    let rest = length.saturating_sub(shown.len());
    let mut inside = shown.join(", ");
    if rest > 0 {
        if !inside.is_empty() {
            inside.push_str(", ");
        }
        inside.push_str("… ");
        inside.push_str(&rest.to_string());
        inside.push_str(" more");
    }
    match kind {
        "list" | "tuple" | "dict" => format!("{open}{inside}{close}"),
        "set" if !inside.is_empty() => format!("{open}{inside}{close}"),
        _ if inside.is_empty() => format!("{kind}()"),
        _ => format!("{kind}({open}{inside}{close})"),
    }
}

/// whether opening this value would show anything more
///
/// a value the *depth* cut short is expandable even though nothing of it was
/// read: opening it is what asks for it again, one level deeper. one the
/// **budget** cut short is not, and neither is one cut by `children` or `text`
/// — reading deeper does not buy a bigger budget, and the answer to all three
/// is a larger bound in the launch configuration, which is what the omission
/// itself says
pub fn expandable(value: &Value) -> bool {
    if !crate::handles::children(value).is_empty() {
        return true;
    }
    match &value.content {
        Content::Unread { omitted } => reachable_by_reading_deeper(omitted),
        Content::Sequence { omitted, .. }
        | Content::Mapping { omitted, .. }
        | Content::Object { omitted, .. } => {
            omitted.as_ref().is_some_and(reachable_by_reading_deeper)
        }
        // a number, a string and a repr have nothing inside them, and neither
        // has a form this adapter has not been taught — `Content` is
        // `#[non_exhaustive]` and a claim about an unknown form is a guess
        _ => false,
    }
}

/// whether reading the same value one level deeper would answer this omission
fn reachable_by_reading_deeper(omitted: &Omitted) -> bool {
    match omitted {
        Omitted::Depth { .. } => true,
        // everything else is answered by a bound the launch configuration
        // carries — `budget`, `children`, `text` — or is a statement about the
        // value rather than about how far the read got: a cycle ends there
        // however deep the request is, and a type with no instance dictionary
        // has nothing more to give. `Omitted` is `#[non_exhaustive]`, and a
        // reason bpd has not seen is one it cannot claim a deeper read answers
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(kind: &str, content: Content) -> Value {
        Value {
            kind: kind.to_string(),
            content,
        }
    }

    #[test]
    fn a_value_that_was_cut_says_so_on_the_line_the_client_shows() {
        let cut = value(
            "str",
            Content::Str {
                text: "abc".to_string(),
                characters: 9000,
                omitted: Some(Omitted::Text {
                    characters: 9000,
                    limit: 3,
                }),
            },
        );

        let said = summary(&cut);
        assert!(said.contains("9000"), "said {said}");
        assert!(said.contains("`text`"), "said {said}");
    }

    #[test]
    fn a_value_nothing_was_read_of_is_shown_as_that_rather_than_as_empty() {
        let unread = value(
            "list",
            Content::Unread {
                omitted: Omitted::Depth { limit: 2 },
            },
        );

        let said = summary(&unread);
        assert!(said.starts_with("not read"), "said {said}");
        assert!(said.contains("depth of 2"), "said {said}");
    }

    #[test]
    fn what_the_depth_cut_off_can_be_opened_and_what_the_child_limit_cut_off_cannot() {
        let by_depth = value(
            "list",
            Content::Sequence {
                items: Vec::new(),
                length: 3,
                omitted: Some(Omitted::Depth { limit: 1 }),
            },
        );
        assert!(
            expandable(&by_depth),
            "opening it is what asks for it deeper"
        );

        let by_children = value(
            "list",
            Content::Sequence {
                items: Vec::new(),
                length: 900,
                omitted: Some(Omitted::Children {
                    length: 900,
                    limit: 0,
                }),
            },
        );
        assert!(
            !expandable(&by_children),
            "no depth answers a child limit, and the omission says to raise it"
        );
    }

    fn int(text: &str) -> Value {
        value(
            "int",
            Content::Int {
                text: text.to_string(),
                omitted: None,
            },
        )
    }

    #[test]
    fn a_container_is_shown_as_what_it_holds_with_what_was_not_read_counted() {
        // the children limit cut two of five: they are counted where they
        // would have been, rather than the line ending as though it were whole
        let cut = value(
            "list",
            Content::Sequence {
                items: vec![int("1"), int("2"), int("3")],
                length: 5,
                omitted: Some(Omitted::Children {
                    length: 5,
                    limit: 3,
                }),
            },
        );
        assert_eq!(summary(&cut), "[1, 2, 3, … 2 more]");

        // a nested container the depth did not reach is said to be unread —
        // never shown empty, which would be a claim about what it holds
        let nested = value(
            "list",
            Content::Sequence {
                items: vec![value(
                    "dict",
                    Content::Unread {
                        omitted: Omitted::Depth { limit: 1 },
                    },
                )],
                length: 1,
                omitted: None,
            },
        );
        assert_eq!(summary(&nested), "[<dict, not read>]");

        // and a reason that is not "there was not room" is still said
        let budget = value(
            "list",
            Content::Sequence {
                items: vec![int("1")],
                length: 9,
                omitted: Some(Omitted::Budget { limit: 8 }),
            },
        );
        let said = summary(&budget);
        assert!(said.starts_with("[1, … 8 more] — "), "said {said}");
        assert!(said.contains("budget"), "said {said}");
    }

    #[test]
    fn a_number_is_never_shown_cut_in_half() {
        // half of an integer is a different integer, so the core leaves the
        // digits out entirely and the summary has to be the reason
        let huge = value(
            "int",
            Content::Int {
                text: String::new(),
                omitted: Some(Omitted::Budget { limit: 8 }),
            },
        );
        assert!(summary(&huge).starts_with("not read"));
        assert!(!expandable(&huge), "an integer has nothing inside it");
    }
}
