//! assembling a release, and every way one is refused
//!
//! what is under test is a build step, so nothing here needs an interpreter —
//! the files are stand-ins. what it must not be is a test of the happy path
//! only: a packaging tool that does its best with bad input produces a
//! directory somebody ships, and every refusal below is one that would
//! otherwise have become a release that fails on a machine nobody can reach

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bpd_core::python::InterpreterTag;
use bpd_release::{Refused, agents, assemble, verify};

/// a tag, by the parser the layout itself uses
fn tag(text: &str) -> InterpreterTag {
    InterpreterTag::parse(text).unwrap_or_else(|| panic!("`{text}` is a tag"))
}

/// a file with something in it, so that digests differ
fn file(at: &Path, name: &str, contents: &str) -> PathBuf {
    let path = at.join(name);
    std::fs::write(&path, contents).expect("the temporary directory is writable");
    path
}

/// a binary and two agents, which is what a release carries
///
/// none of the three inputs is named what the layout calls it. that is the
/// whole point of the fixture: a copy that keeps the input's name is a release
/// bpd cannot launch, and it is the mistake this crate was written around
fn inputs(at: &Path) -> (PathBuf, BTreeMap<InterpreterTag, PathBuf>) {
    let binary = file(at, "the-built-binary", "the debugger");
    let agents = BTreeMap::from([
        (tag("3.13"), file(at, "agent-3.13.so", "for 3.13")),
        (tag("3.14t"), file(at, "agent-3.14t.so", "for 3.14t")),
    ]);
    (binary, agents)
}

#[test]
fn a_layout_carries_the_binary_and_an_agent_per_tag_and_verifies_against_its_own_manifest() {
    let held = tempfile::tempdir().expect("a temporary directory");
    let (binary, agents) = inputs(held.path());
    let out = held.path().join("layout");

    let manifest = assemble(&binary, &agents, &out).expect("the release was assembled");
    assert_eq!(manifest.tags, vec![tag("3.13"), tag("3.14t")]);

    // the paths are the ones `bpd_engine::agent` looks in, taken from the same
    // function the scan uses. writing them out as text here is what let the
    // first version of this test pass while the layout it built could not
    // launch: the inputs are named `agent-3.13.so`, the scan joins
    // `libbpd_agent.so`, and a copy that kept the input's name is a release
    // that assembles, verifies, and carries an agent bpd never finds
    assert!(
        out.join(bpd_release::binary_name()).is_file(),
        "the binary is not in the layout under the name this platform runs it \
         by — on windows that name has an extension, and a `Scripts/bpd` with \
         no `.exe` installs from a wheel and cannot be executed"
    );
    for named in [tag("3.13"), tag("3.14t")] {
        let at = out.join(bpd_release::agent_at(named));
        assert!(
            at.is_file(),
            "the agent for {named} is not where bpd looks for one: {}",
            at.display()
        );
        assert_ne!(
            at.file_name(),
            Some(std::ffi::OsStr::new("agent-3.13.so")),
            "the agent kept the name it was built under rather than the one \
             the scan joins"
        );
    }

    let read = verify(&out).expect("the layout is what its manifest says");
    assert_eq!(read, manifest, "verifying read back something else");

    // and it is reproducible: the same inputs make the same manifest, byte for
    // byte. a release nobody can rebuild is one whose contents are an assertion
    let again = held.path().join("again");
    let second = assemble(&binary, &agents, &again).expect("the release was assembled twice");
    assert_eq!(
        second.to_string(),
        manifest.to_string(),
        "assembling the same inputs twice produced two different manifests"
    );
}

#[test]
fn a_layout_that_changed_after_it_was_assembled_says_so_rather_than_verifying() {
    // the whole reason the manifest exists. a digest that is written and never
    // checked says nothing at all, and this is the check
    let held = tempfile::tempdir().expect("a temporary directory");
    let (binary, agents) = inputs(held.path());
    let out = held.path().join("layout");
    assemble(&binary, &agents, &out).expect("the release was assembled");

    std::fs::write(
        out.join(bpd_release::agent_at(tag("3.13"))),
        "something else",
    )
    .expect("the layout is writable");

    match verify(&out) {
        Err(Refused::Changed { file, .. }) => {
            assert_eq!(PathBuf::from(file), bpd_release::agent_at(tag("3.13")));
        }
        other => panic!("a changed agent verified: {other:?}"),
    }
}

#[test]
fn a_layout_missing_a_file_its_manifest_names_says_which() {
    let held = tempfile::tempdir().expect("a temporary directory");
    let (binary, agents) = inputs(held.path());
    let out = held.path().join("layout");
    assemble(&binary, &agents, &out).expect("the release was assembled");

    std::fs::remove_file(out.join(bpd_release::agent_at(tag("3.14t")))).expect("it is removable");

    match verify(&out) {
        Err(Refused::Missing { file }) => {
            assert_eq!(PathBuf::from(file), bpd_release::agent_at(tag("3.14t")));
        }
        other => panic!("a layout missing an agent verified: {other:?}"),
    }
}

#[test]
fn a_release_with_no_agents_is_refused_because_it_would_debug_nothing() {
    let held = tempfile::tempdir().expect("a temporary directory");
    let binary = file(held.path(), "bpd", "the debugger");

    let refused = assemble(&binary, &BTreeMap::new(), &held.path().join("layout"))
        .expect_err("a release with no agents is not a release");
    assert!(matches!(refused, Refused::NoAgents), "{refused:?}");
    assert!(
        !held.path().join("layout").exists(),
        "a refusal left a directory behind, and a directory that exists is one \
         somebody can ship"
    );
}

#[test]
fn an_agent_or_a_binary_that_is_not_there_is_named_rather_than_skipped() {
    let held = tempfile::tempdir().expect("a temporary directory");
    let (binary, agents) = inputs(held.path());

    let missing = held.path().join("not-here");
    let refused = assemble(&missing, &agents, &held.path().join("a"))
        .expect_err("there is no binary to ship");
    assert!(matches!(refused, Refused::NoBinary { .. }), "{refused:?}");

    let mut gone = agents;
    gone.insert(tag("3.15"), held.path().join("no-agent-here.so"));
    let refused =
        assemble(&binary, &gone, &held.path().join("b")).expect_err("an agent is not there");
    match refused {
        Refused::NoAgent { tag: named, .. } => assert_eq!(named, tag("3.15")),
        other => panic!("the refusal did not name the tag: {other:?}"),
    }
    // the message has to say how to build the one that is missing, or the
    // person reading it in a CI log has to come and find this file
    let said = assemble(&binary, &gone, &held.path().join("c"))
        .expect_err("an agent is not there")
        .to_string();
    assert!(
        said.contains("PYO3_PYTHON=python3.15 cargo build -p bpd_agent --release"),
        "the refusal has to say how to make the missing agent, and said {said}"
    );
}

#[test]
fn a_directory_that_already_holds_something_is_never_assembled_over() {
    // the case that would otherwise ship a leftover. a layout assembled over
    // the top of an older one carries an agent for a tag this build never made,
    // and every check here would pass on it
    let held = tempfile::tempdir().expect("a temporary directory");
    let (binary, agents) = inputs(held.path());
    let out = held.path().join("layout");
    std::fs::create_dir_all(out.join("agents/3.9")).expect("the directory is creatable");
    std::fs::write(out.join("agents/3.9/stale.so"), "from an older release")
        .expect("it is writable");

    let refused = assemble(&binary, &agents, &out).expect_err("the directory is not empty");
    assert!(matches!(refused, Refused::NotEmpty { .. }), "{refused:?}");
    assert!(
        out.join("agents/3.9/stale.so").is_file(),
        "the refusal deleted something, and this tool builds releases rather \
         than removing files somebody else put there"
    );
}

#[test]
fn a_tag_that_is_not_one_is_refused_by_the_parser_that_reads_one_back() {
    // parsed rather than pattern-matched, so a spelling this accepts is one an
    // interpreter really reports and a spelling it rejects is one no launch
    // would ever match. a rule written here could come to disagree with that one
    for given in ["python3.13=/a.so", "3.x=/a.so", "=/a.so", "3.13"] {
        let refused =
            agents(&[given.to_string()]).expect_err(&format!("`{given}` is not a tag and a path"));
        assert!(matches!(refused, Refused::NotATag { .. }), "{refused:?}");
    }

    let read = agents(&["3.14t=/a.so".to_string()]).expect("`3.14t` is a tag");
    assert_eq!(read[&tag("3.14t")], PathBuf::from("/a.so"));
}

#[test]
fn the_same_tag_given_twice_is_refused_rather_than_resolved_by_argument_order() {
    let refused = agents(&["3.13=/first.so".to_string(), "3.13=/second.so".to_string()])
        .expect_err("the same tag twice is not a release anybody can reproduce");
    match refused {
        Refused::TagTwice { tag: named, .. } => assert_eq!(named, tag("3.13")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_manifest_that_is_not_one_is_refused_rather_than_read_as_far_as_it_parses() {
    let held = tempfile::tempdir().expect("a temporary directory");
    let out = held.path().join("layout");
    std::fs::create_dir_all(&out).expect("the directory is creatable");

    for (contents, why) in [
        ("", "an empty manifest checks nothing"),
        (
            "not a line of a manifest\n",
            "there is no digest and no path",
        ),
        ("md5:abc  bpd\n", "the digest is not a sha-256"),
    ] {
        std::fs::write(out.join("MANIFEST"), contents).expect("it is writable");
        let refused = verify(&out).expect_err(why);
        assert!(
            matches!(refused, Refused::NotAManifest { .. }),
            "{why}: {refused:?}"
        );
    }
}
