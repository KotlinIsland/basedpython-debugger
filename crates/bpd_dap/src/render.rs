//! turning a value into the one line DAP shows beside a name
//!
//! this is presentation and nothing else. no summary here decides anything
//! about the program — where a value was cut, and why, comes from the
//! [`Omitted`] the core already attached to it, rendered rather than
//! interpreted. an ellipsis with no explanation is what this project's rules
//! about saying what was left out exist to prevent

use bpd_core::{Content, Omitted, Value};

/// the one line shown beside a variable's name
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
        Content::Sequence {
            length, omitted, ..
        } => (format!("{}[{length}]", value.kind), omitted.as_ref()),
        Content::Mapping {
            length, omitted, ..
        } => (format!("{}{{{length}}}", value.kind), omitted.as_ref()),
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
