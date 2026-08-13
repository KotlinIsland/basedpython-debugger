//! the intellij plugin sends exactly the attributes it offers, and no others
//!
//! `editors/intellij/` is a registration and nothing else: a jetbrains IDE
//! resolves a debug session through a run configuration type and a
//! `ProgramRunner`, neither of which can be named from a settings file. what it
//! declares is a run configuration's stored fields, and what it *sends* is the
//! map a `DapLaunchArgumentsProvider` hands the platform — which becomes the
//! `launch` request [`bpd_dap::Configuration`] reads
//!
//! this is the seam where they drift, and it is worse here than in vs code:
//! `Configuration` deliberately does **not** deny unknown fields, because a DAP
//! `launch` carries the client's own keys — `type`, `request`, `name`. so a
//! misspelled attribute is not an error, it is a setting the person fills in,
//! sees saved, and never gets. that is the placeholder ban in a kotlin map, and
//! nothing in either language catches it
//!
//! **the field lists are asked of serde**, the same way `vscode.rs` asks. what
//! is read out of the kotlin is the map literal itself, and this test fails
//! loudly if it cannot find one rather than passing on an empty set
//!
//! what this **cannot** check is that the platform loads the plugin, that a run
//! configuration reaches a session, or that a breakpoint binds. what checks
//! those is `editors/intellij/src/test/`, which downloads a real pycharm and
//! starts a real session through the plugin

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use bpd_dap::Configuration;

mod fields;
use fields::fields_of;

/// the plugin's directory, from this crate
fn plugin() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../editors/intellij")
}

/// a file of the plugin, read whole
fn source(name: &str) -> String {
    let file = plugin().join(name);
    std::fs::read_to_string(&file)
        .unwrap_or_else(|error| panic!("{} is part of the plugin: {error}", file.display()))
}

/// the kotlin the adapter registration and the launch arguments live in
fn adapter() -> String {
    source("src/main/kotlin/com/github/kotlinisland/bpd/BpdAdapter.kt")
}

/// the kotlin the run configuration and its stored fields live in
fn configuration() -> String {
    source("src/main/kotlin/com/github/kotlinisland/bpd/BpdRunConfiguration.kt")
}

/// the keys of the `mapOf(...)` a launch request is built from
///
/// bounded to the block `getLaunchArguments` returns, so a `mapOf` anywhere else
/// in the file cannot be mistaken for it. `"key" to` is the only shape kotlin
/// writes a map entry in, and every entry in that block is one attribute
fn sent() -> BTreeSet<String> {
    let kotlin = adapter();
    let start = kotlin
        .find("fun getLaunchArguments(")
        .expect("the plugin builds its launch request in `getLaunchArguments`");
    let block = &kotlin[start..];
    let opened = block
        .find("mapOf(")
        .expect("`getLaunchArguments` builds the request out of a `mapOf`");
    let closed = block[opened..]
        .find("\n            ),")
        .expect("the `mapOf` this reads is the argument list, closed at its own indent");
    let entries = &block[opened..opened + closed];

    let mut found = BTreeSet::new();
    for line in entries.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix('"') else {
            continue;
        };
        let Some((name, tail)) = rest.split_once('"') else {
            continue;
        };
        assert!(
            tail.trim_start().starts_with("to "),
            "`{name}` is quoted inside the launch arguments and is not a key of them, so \
             this test is reading the wrong block"
        );
        assert!(
            found.insert(name.to_owned()),
            "`{name}` is sent twice, and the second one is the value the adapter reads"
        );
    }
    assert!(
        !found.is_empty(),
        "no launch attribute was read out of the kotlin, so this test is asserting nothing"
    );
    found
}

#[test]
fn the_plugin_sends_only_attributes_the_adapter_reads() {
    let read = fields_of::<Configuration>();
    let sent = sent();
    assert!(
        sent.is_subset(&read),
        "the plugin sends {:?}, which the adapter does not read — and `Configuration` does \
         not deny unknown fields, so nothing else would ever say so",
        &sent - &read,
    );
}

#[test]
fn every_attribute_the_plugin_leaves_out_is_one_it_could_not_honour() {
    // the two lists are not equal and are not meant to be. what is left out is
    // left out on purpose, and each reason is here so that an attribute dropped
    // by accident is not mistaken for one of them
    let excused: BTreeSet<String> = [
        // bpd has no path that launches a program without its agent. the
        // adapter refuses it by name, and the run configuration suppresses the
        // default run action so the IDE never offers it
        String::from("noDebug"),
        // the adapter's defaults apply. offering them would mean two more
        // fields in a dialog for bounds most people never move, and an absent
        // field is honest where a field that did nothing would not be
        String::from("variables"),
        String::from("threadSettleMs"),
    ]
    .into();

    let read = fields_of::<Configuration>();
    assert!(
        read.is_superset(&excused),
        "an attribute excused here is one the adapter still reads. it does not read {:?}, \
         so the excuse is stale",
        &excused - &read,
    );
    assert_eq!(&read - &excused, sent());
}

/// the run configuration field an attribute is stored in
///
/// the same word, except where a jetbrains run configuration has a name of its
/// own for the thing: `parameters` is what one calls a program's arguments, and
/// `interpreter` is the word the dialog uses for what the adapter calls
/// `python`. the two spellings are here rather than in the kotlin so that the
/// adapter's names stay the adapter's
fn stored(attribute: &str) -> String {
    match attribute {
        "args" => String::from("parameters"),
        "python" => String::from("interpreter"),
        other => other.to_owned(),
    }
}

#[test]
fn every_attribute_the_plugin_sends_is_one_a_person_can_set() {
    // a value sent from a field nothing writes is a constant wearing a
    // configuration, and a field the editor cannot reach is a setting nobody
    // can change. so each attribute is followed from the options class through
    // the run configuration to the editor
    let options = configuration();
    for attribute in sent() {
        let stored = stored(&attribute);
        assert!(
            options.contains(&format!("var {stored}:")),
            "`{attribute}` is sent in the launch request and `{stored}` is not a field of \
             the run configuration, so nothing stores it"
        );
        assert!(
            options.contains(&format!("configuration.{stored} = ")),
            "`{stored}` is stored and the settings editor never writes it, so it is a field \
             nobody can change"
        );
        assert!(
            options.contains(&format!("= configuration.{stored}")),
            "`{stored}` is stored and the settings editor never reads it back, so opening \
             the dialog would show a value that is not the one in use"
        );
    }
}

#[test]
fn the_plugin_declares_the_module_the_layer_lives_in() {
    // `com.intellij.platform.dap` is not in every IDE — it is absent from
    // PyCharm Community and IDEA Community at 2026.2.1 — and this dependency is
    // what turns that into a plugin that declines to load rather than one that
    // loads and fails at the first session
    let manifest = source("src/main/resources/META-INF/plugin.xml");
    assert!(
        manifest.contains("<module name=\"intellij.platform.dap\"/>"),
        "the manifest does not depend on the DAP layer, so an IDE without it would install \
         this plugin and fail when somebody starts a session"
    );
    // the breakpoints a session binds are the python plugin's own types, so the
    // plugin cannot load without it either
    assert!(
        manifest.contains("<plugin id=\"com.intellij.modules.python\"/>"),
        "the manifest does not depend on python support, and the breakpoint types the \
         descriptor names come from it"
    );

    let kotlin = adapter();
    assert!(
        kotlin.contains("DebugAdapterId(\"bpd\", \"bpd\")"),
        "the adapter id is what the platform routes a session by, and it is no longer `bpd`"
    );
    // the transport is the socket rather than the pipes, and it is what makes
    // `debugChildren` refusable rather than impossible
    assert!(
        kotlin.contains("\"dap\", \"--listen\", \"0\""),
        "the plugin no longer starts `bpd dap --listen 0`, so the port and token the \
         connection is built on are not there to read"
    );
}
