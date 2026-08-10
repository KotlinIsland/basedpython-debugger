//! the shipped skill names nothing that has gone away
//!
//! `skills/bpd/SKILL.md` is for clients that have skills, which is a **client**
//! feature and no part of MCP. that makes it the surface with nothing else
//! checking it: no schema validates it, no host parses it, and a tool it names
//! after that tool is renamed reads exactly as well as one that is true
//!
//! so it is checked here, the same way `crates/bpd_mcp/tests/teaching.rs`
//! checks a resource: every backticked name in it that looks like one of bpd's
//! own has to still be one. the file is markdown rather than a rust `const`
//! because a client reads it off disk, so the check is over the text on disk

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// the shipped skill directory, from this crate
fn skill() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/bpd/SKILL.md")
}

fn source() -> String {
    let file = skill();
    std::fs::read_to_string(&file)
        .unwrap_or_else(|error| panic!("{} is the shipped skill: {error}", file.display()))
}

/// every backticked run in the text
fn quoted(text: &str) -> Vec<&str> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|quoted| !quoted.is_empty())
        .collect()
}

#[test]
fn the_skill_declares_the_frontmatter_a_client_matches_on() {
    // a skill is loaded because its description matched what the user asked
    // for. one without a description is a file a client will never reach for,
    // and one without a name has nothing to invoke
    let source = source();
    let (frontmatter, body) = source
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .expect("a skill opens with yaml frontmatter between `---` lines");

    assert!(
        frontmatter.contains("name: bpd"),
        "the skill has to be named, and its frontmatter is:\n{frontmatter}"
    );
    let description = frontmatter
        .lines()
        .find_map(|line| line.strip_prefix("description: "))
        .expect("a skill says what it is for");
    assert!(
        description.len() > 80,
        "the description is the whole of what a client matches on, and it is {} \
         characters",
        description.len()
    );
    assert!(
        body.contains("bpd mcp"),
        "the skill has to say how the server is started"
    );
}

#[test]
fn every_tool_the_skill_names_is_still_a_tool() {
    let source = source();
    let offered: BTreeSet<&str> = bpd_mcp::tools().into_iter().map(|tool| tool.name).collect();

    // the direction that bites: a name in the text that is one of bpd's own and
    // has stopped existing. anything else backticked is python, json or a field
    // of an answer, and this cannot tell those from an invented word — which is
    // why the ones that matter are asserted to be present below
    let named: BTreeSet<&str> = quoted(&source)
        .into_iter()
        .filter(|quoted| offered.contains(quoted))
        .collect();

    for wanted in [
        "launch",
        "set_breakpoints",
        "continue_",
        "state",
        "stack",
        "variables",
        "evaluate",
        "step_over",
        "step_in",
        "step_out",
        "pause",
        "run_script",
        "stop_the_world",
    ] {
        assert!(
            offered.contains(wanted),
            "the skill walks a session through `{wanted}`, which this server no \
             longer offers. what is offered: {offered:?}"
        );
        assert!(
            named.contains(wanted),
            "the skill no longer names `{wanted}`, so this check has stopped \
             covering the session it describes"
        );
    }
}

#[test]
fn every_resource_and_prompt_the_skill_points_at_exists() {
    let source = source();
    let quoted = quoted(&source);

    for resource in bpd_mcp::resources() {
        assert!(
            quoted.contains(&resource.uri),
            "the skill points a host at the resources and does not name `{}`",
            resource.uri
        );
    }
    for prompt in bpd_mcp::prompts() {
        assert!(
            quoted.contains(&prompt.name),
            "the skill names the investigations and does not name `{}`",
            prompt.name
        );
    }

    // and nothing it points at has gone: a uri or a prompt name in the text
    // that this server does not offer is a slash command a user will not find
    let uris: BTreeSet<&str> = bpd_mcp::resources()
        .iter()
        .map(|resource| resource.uri)
        .collect();
    for named in &quoted {
        assert!(
            !named.starts_with("bpd://") || uris.contains(named),
            "the skill names the resource `{named}`, which this server does not \
             offer. what it offers: {uris:?}"
        );
    }
}
