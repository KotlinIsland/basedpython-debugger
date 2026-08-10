//! nothing a resource or a prompt names has gone away
//!
//! this is the seam prose always loses at. a page that names `run_to` after
//! `run_to` stopped being a tool reads exactly as well as one that is true, and
//! an agent will act on it — which is worse than having no page at all
//!
//! so every tool a page or an investigation names is **declared** beside it, and
//! this checks the declaration in both directions: a declared name has to be a
//! tool this server offers, and a tool named in the text has to be declared. a
//! rename therefore fails here rather than being noticed by a reader
//!
//! what it cannot catch is a name that never existed anywhere — that is a typo
//! rather than a drift, and nothing in a rust test can tell one invented word
//! from another. the same limit is written down beside the parity test's hand
//! kept list of exceptions, for the same reason

use std::collections::BTreeSet;

use bpd_mcp::prompts::prompts;
use bpd_mcp::resources::resources;
use bpd_mcp::tools::tools;

/// the tools this server offers
fn offered() -> BTreeSet<&'static str> {
    tools().into_iter().map(|tool| tool.name).collect()
}

/// check one page's declared references against its text, both ways
fn mentions_hold(what: &str, text: &str, mentions: &[&str]) {
    let offered = offered();
    let declared: BTreeSet<&str> = mentions.iter().copied().collect();
    assert_eq!(
        declared.len(),
        mentions.len(),
        "{what} declares a tool twice"
    );

    for name in &declared {
        assert!(
            offered.contains(name),
            "{what} names `{name}`, which is not a tool this server offers. \
             what is offered: {offered:?}"
        );
        assert!(
            text.contains(&format!("`{name}`")),
            "{what} declares `{name}` and never says it, so the declaration is \
             not evidence of anything"
        );
    }

    for name in &offered {
        assert!(
            !text.contains(&format!("`{name}`")) || declared.contains(name),
            "{what} says `{name}` and does not declare it, so a rename would \
             leave it saying a tool that is gone"
        );
    }
}

#[test]
fn every_tool_a_resource_names_is_still_a_tool() {
    let all = resources();
    assert!(!all.is_empty(), "a capability is declared for these");

    let uris: BTreeSet<&str> = all.iter().map(|resource| resource.uri).collect();
    assert_eq!(uris.len(), all.len(), "two resources share a uri");

    for resource in &all {
        mentions_hold(
            &format!("the resource `{}`", resource.uri),
            resource.text,
            resource.mentions,
        );
        assert!(
            resource.text.len() > 500,
            "`{}` is the deeper model and is {} characters",
            resource.uri,
            resource.text.len()
        );
    }
}

#[test]
fn every_tool_an_investigation_names_is_still_a_tool() {
    let all = prompts();
    assert!(!all.is_empty(), "a capability is declared for these");

    let names: BTreeSet<&str> = all.iter().map(|prompt| prompt.name).collect();
    assert_eq!(names.len(), all.len(), "two prompts share a name");

    for prompt in &all {
        mentions_hold(
            &format!("the prompt `{}`", prompt.name),
            prompt.body,
            prompt.mentions,
        );
    }
}

#[test]
fn every_argument_a_prompt_declares_really_reaches_the_investigation() {
    // an argument accepted and never substituted is the placeholder ban applied
    // to a workflow: the user is asked for a file, and the investigation that
    // comes back does not say which file
    for prompt in prompts() {
        let names: BTreeSet<&str> = prompt
            .arguments
            .iter()
            .map(|argument| argument.name)
            .collect();
        assert_eq!(
            names.len(),
            prompt.arguments.len(),
            "`{}` declares an argument twice",
            prompt.name
        );

        for argument in prompt.arguments {
            assert!(
                prompt.body.contains(&format!("{{{}}}", argument.name)),
                "`{}` takes `{}` and its investigation never uses it",
                prompt.name,
                argument.name
            );
            assert_eq!(
                argument.required,
                argument.fallback.is_none(),
                "`{}`'s `{}` is optional with nothing to stand in for it, or \
                 required with something that would never be used",
                prompt.name,
                argument.name
            );
        }
    }
}

#[test]
fn an_investigation_is_filled_in_completely_or_refused() {
    for prompt in prompts() {
        // nothing given at all: either every argument is optional and the whole
        // investigation comes back, or it is refused naming the one it needs
        let nothing = std::collections::BTreeMap::new();
        let required: Vec<&str> = prompt
            .arguments
            .iter()
            .filter(|argument| argument.required)
            .map(|argument| argument.name)
            .collect();

        match prompt.filled(&nothing) {
            Ok(_) => assert!(
                required.is_empty(),
                "`{}` requires {required:?} and answered without them",
                prompt.name
            ),
            Err(reason) => {
                assert!(
                    !required.is_empty(),
                    "`{}` requires nothing and refused with {reason}",
                    prompt.name
                );
                assert!(
                    reason.contains(required[0]),
                    "`{}` refused without naming the argument it needs: {reason}",
                    prompt.name
                );
            }
        }

        // and with everything given, no placeholder survives into the text an
        // agent reads
        let given: std::collections::BTreeMap<String, String> = prompt
            .arguments
            .iter()
            .map(|argument| (argument.name.to_string(), format!("<<{}>>", argument.name)))
            .collect();
        let filled = prompt.filled(&given).unwrap_or_else(|reason| {
            panic!(
                "`{}` was given everything and refused: {reason}",
                prompt.name
            )
        });
        let text = filled["messages"][0]["content"]["text"]
            .as_str()
            .expect("an investigation is one text message")
            .to_string();

        for argument in prompt.arguments {
            assert!(
                text.contains(&format!("<<{}>>", argument.name)),
                "`{}` did not substitute `{}`",
                prompt.name,
                argument.name
            );
            assert!(
                !text.contains(&format!("{{{}}}", argument.name)),
                "`{}` left a `{{{}}}` unfilled, which an agent would read as \
                 something it was supposed to write",
                prompt.name,
                argument.name
            );
        }
    }
}
