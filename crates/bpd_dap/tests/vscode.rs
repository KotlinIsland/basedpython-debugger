//! the vs code extension contributes exactly what the adapter reads
//!
//! `editors/vscode/` is a registration and nothing else: vs code resolves a
//! launch configuration's `type` through an extension, so without one no vs
//! code user can name `bpd` at all. what it declares is a json schema for a
//! `launch.json` entry, and what the adapter reads is
//! [`bpd_dap::Configuration`] — two descriptions of one thing, in two languages,
//! in two files
//!
//! this is the seam where they drift. an attribute in the schema that the
//! adapter does not read is a setting a user can write, see completed, and
//! never get — the placeholder ban in a `package.json`. an attribute the
//! adapter reads and the schema omits is a capability nobody can find
//!
//! **the field lists are asked of serde rather than written down here.** a
//! `Deserialize` implementation hands its field list to the deserializer, so a
//! struct that gains a field gains an entry in this test without anyone
//! remembering to add one — which is the whole point of it
//!
//! what this **cannot** check is that the extension loads, that vs code accepts
//! the manifest, or that the javascript runs, because none of those is
//! reachable from a rust test runner. `docs/development/vscode.md` says which
//! of those was verified by hand and which was not, and the answer today is
//! none of them

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use bpd_core::Detail;
use bpd_dap::Configuration;

/// the extension's directory, from this crate
fn extension() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../editors/vscode")
}

/// a file of the extension, read whole
fn source(name: &str) -> String {
    let file = extension().join(name);
    std::fs::read_to_string(&file)
        .unwrap_or_else(|error| panic!("{} is part of the extension: {error}", file.display()))
}

/// the extension's manifest
fn manifest() -> serde_json::Value {
    serde_json::from_str(&source("package.json")).expect("the manifest is json")
}

/// the one `debuggers` entry, which is the whole of the registration
fn debugger() -> serde_json::Value {
    let manifest = manifest();
    let debuggers = manifest["contributes"]["debuggers"]
        .as_array()
        .expect("the extension contributes a debugger")
        .clone();
    assert_eq!(
        debuggers.len(),
        1,
        "one adapter, so one entry. a second would be a second type nothing implements"
    );
    debuggers
        .into_iter()
        .next()
        .expect("the length was just asserted")
}

/// the schema for a `launch` configuration
fn launch() -> serde_json::Value {
    debugger()["configurationAttributes"]["launch"].clone()
}

/// the properties of a json schema object
fn properties(schema: &serde_json::Value) -> &serde_json::Map<String, serde_json::Value> {
    schema["properties"]
        .as_object()
        .expect("a schema object declares properties")
}

/// the names of those properties
fn declared(schema: &serde_json::Value) -> BTreeSet<String> {
    properties(schema).keys().cloned().collect()
}

/// a `u32` attribute: the bounds are the rust type's, the default the adapter's
fn whole(schema: &serde_json::Value, default: u32) {
    assert_eq!(schema["type"], "integer");
    assert_eq!(schema["minimum"], 0);
    assert_eq!(schema["maximum"], serde_json::json!(u32::MAX));
    assert_eq!(schema["default"], serde_json::json!(default));
}

/// a `bool` attribute
fn flag(schema: &serde_json::Value, default: bool) {
    assert_eq!(schema["type"], "boolean");
    assert_eq!(schema["default"], serde_json::json!(default));
}

#[test]
fn the_extension_contributes_exactly_the_attributes_the_adapter_reads() {
    // `noDebug` is the one field the adapter reads that is deliberately not
    // contributed, and the reason is the same one that keeps a capability out
    // of `initialize`: bpd has no path that launches a program without its
    // agent, so the setting cannot be honoured. vs code sends it itself for
    // "run without debugging" — it is not something a `launch.json` is meant to
    // hold — and the adapter refuses it by name with what to do instead
    let justified: BTreeSet<String> = [String::from("noDebug")].into();

    let read = fields_of::<Configuration>();
    assert!(
        read.is_superset(&justified),
        "`noDebug` is excused here because the adapter reads it and refuses it. \
         it no longer reads it, so the excuse is stale"
    );

    assert_eq!(&read - &justified, declared(&launch()));
}

#[test]
fn every_contributed_attribute_declares_the_type_and_the_default_the_adapter_uses() {
    // the adapter's defaults are whatever a configuration naming only the
    // program comes out as. a declared default that disagreed would be a value
    // vs code writes into a `launch.json` and bpd never uses
    let adapter: Configuration = serde_json::from_str(r#"{"program": "a.py"}"#)
        .expect("a program is the whole of a valid configuration");

    let launch = launch();
    assert_eq!(
        launch["required"],
        serde_json::json!(["program"]),
        "everything but the program parsed above without being given, so \
         everything but the program is optional"
    );

    for (name, schema) in properties(&launch) {
        match name.as_str() {
            "program" => {
                assert_eq!(schema["type"], "string");
                assert!(
                    schema["default"].is_null(),
                    "`program` is required, so a default is a value nothing ever uses"
                );
            }
            "args" => {
                assert_eq!(schema["type"], "array");
                assert_eq!(schema["items"]["type"], "string");
                assert_eq!(schema["default"], serde_json::json!(adapter.args));
            }
            "python" => {
                assert_eq!(schema["type"], "string");
                assert_eq!(schema["default"], serde_json::json!(adapter.python));
            }
            "stopOnEntry" => flag(schema, adapter.stop_on_entry),
            "stopTheWorld" => flag(schema, adapter.stop_the_world),
            "threadSettleMs" => whole(schema, adapter.thread_settle_ms),
            "variables" => {
                assert_eq!(schema["type"], "object");
                assert!(
                    schema["default"].is_null(),
                    "each bound carries its own default, so a default for the whole \
                     object is a second place for them to disagree"
                );
            }
            // no catch-all that passes: an attribute vs code offers and the
            // adapter does not read is the thing this test exists to stop
            other => panic!(
                "the extension contributes `{other}`, which `bpd_dap::Configuration` \
                 does not read. a user would write it, see it completed, and never get it"
            ),
        }
    }
}

#[test]
fn the_bounds_on_a_value_are_contributed_exactly_as_the_core_reads_them() {
    let variables = launch()["properties"]["variables"].clone();

    // `Detail` is deserialised with `deny_unknown_fields`, because a client's
    // spelling of one of these is the only way to raise a bound the answer told
    // it to raise. the schema says the same thing, so vs code says it first
    assert_eq!(variables["additionalProperties"], false);
    assert_eq!(declared(&variables), fields_of::<Detail>());

    let adapter = Detail::default();
    for (name, schema) in properties(&variables) {
        match name.as_str() {
            "depth" => whole(schema, adapter.depth),
            "children" => whole(schema, adapter.children),
            "text" => whole(schema, adapter.text),
            "budget" => whole(schema, adapter.budget),
            "attributes" => flag(schema, adapter.attributes),
            "repr" => flag(schema, adapter.repr),
            other => panic!("the extension contributes a bound `{other}` that `Detail` has not"),
        }
    }
}

#[test]
fn the_extension_offers_no_request_the_adapter_does_not_implement() {
    // attaching is PEP 768, needs cpython 3.14, and is not built —
    // `bpd_dap` refuses an `attach` request by name. a schema that completed
    // `"request": "attach"` would be the same placeholder as an advertised
    // capability with nothing behind it: the user gets the affordance, and the
    // refusal at the moment they need it
    let debugger = debugger();
    let requests: Vec<&String> = debugger["configurationAttributes"]
        .as_object()
        .expect("the debugger declares the requests it takes")
        .keys()
        .collect();
    assert_eq!(requests, ["launch"]);
}

#[test]
fn every_snippet_writes_a_configuration_the_adapter_would_accept() {
    let debugger = debugger();
    let contributed = declared(&launch());
    // vs code's own keys. every configuration carries them and no adapter reads
    // them out of the launch request
    let editors: BTreeSet<String> = ["name", "type", "request"]
        .into_iter()
        .map(String::from)
        .collect();

    let mut bodies: Vec<serde_json::Value> = debugger["configurationSnippets"]
        .as_array()
        .expect("the extension contributes snippets, which is what completion offers")
        .iter()
        .map(|snippet| snippet["body"].clone())
        .collect();
    bodies.extend(
        debugger["initialConfigurations"]
            .as_array()
            .expect("the extension contributes what a new launch.json starts as")
            .iter()
            .cloned(),
    );
    assert!(!bodies.is_empty(), "nothing was checked");

    for body in bodies {
        assert_eq!(body["type"], debugger["type"]);
        assert_eq!(body["request"], "launch");
        for key in body
            .as_object()
            .expect("a configuration is an object")
            .keys()
        {
            assert!(
                contributed.contains(key) || editors.contains(key),
                "a snippet writes `{key}` into a launch.json, and it is neither \
                 vs code's own nor an attribute bpd reads"
            );
        }
    }
}

#[test]
fn breakpoints_are_contributed_for_every_language_the_debugger_claims() {
    // without this a user cannot set a breakpoint in the editor at all, and a
    // debugger nobody can put a breakpoint in is a registration that does not
    // work
    let manifest = manifest();
    let allowed: BTreeSet<&str> = manifest["contributes"]["breakpoints"]
        .as_array()
        .expect("the extension says where a breakpoint may be set")
        .iter()
        .map(|entry| {
            entry["language"]
                .as_str()
                .expect("a breakpoint contribution names a language")
        })
        .collect();

    for language in debugger()["languages"]
        .as_array()
        .expect("the debugger names the languages it debugs")
    {
        let language = language.as_str().expect("a language is a name");
        assert!(
            allowed.contains(language),
            "the debugger claims `{language}` and no breakpoint may be set in it"
        );
    }
}

#[test]
fn the_manifest_and_the_javascript_agree_on_what_they_register() {
    // a text check, and it is worth saying what it can and cannot do: it proves
    // the two files spell the same type and the same setting, which is the
    // drift that silently produces an extension that loads and never activates.
    // it proves nothing about what the javascript then does, which is not
    // something this test runner can run
    let manifest = manifest();
    let source = source("extension.js");

    let entry = manifest["main"]
        .as_str()
        .expect("the extension has an entry");
    assert_eq!(entry, "./extension.js", "the entry point read above");

    let kind = debugger()["type"]
        .as_str()
        .expect("the debugger has a type")
        .to_owned();
    assert!(
        manifest["activationEvents"]
            .as_array()
            .expect("the extension says when it is needed")
            .contains(&serde_json::json!(format!("onDebugResolve:{kind}"))),
        "nothing activates the extension when a `{kind}` session starts, so the \
         factory that names the executable is never registered"
    );
    assert!(
        source.contains(&format!("registerDebugAdapterDescriptorFactory(\"{kind}\"")),
        "the manifest contributes `{kind}` and the extension registers a factory \
         for something else"
    );

    // the adapter is the `bpd` binary itself, run as `bpd dap`
    assert!(
        source.contains("[\"dap\"]"),
        "the adapter is not started as `bpd dap`"
    );

    let settings: Vec<&String> = manifest["contributes"]["configuration"]["properties"]
        .as_object()
        .expect("the extension declares its settings")
        .keys()
        .collect();
    assert_eq!(
        settings,
        ["bpd.executable"],
        "a setting declared and never read is a config option that is parsed and ignored"
    );
    assert!(source.contains("\"bpd.executable\""));
    assert!(source.contains("getConfiguration(\"bpd\""));
    assert!(source.contains(".get(\"executable\")"));
}

/// the field names serde will read for a struct, asked of serde
///
/// a derived `Deserialize` hands its field list — already renamed, so already
/// camel case — to the deserializer it is given. capturing it there is the
/// difference between a test that checks two lists agree and a test that
/// re-states one of them
fn fields_of<'de, T: serde::Deserialize<'de> + std::fmt::Debug>() -> BTreeSet<String> {
    let mut found = Vec::new();
    let error = T::deserialize(Fields { found: &mut found })
        .expect_err("this deserializer answers a struct with an error, always");
    assert_eq!(
        error.to_string(),
        CAPTURED,
        "the field list was never asked for, so `{}` is not a struct serde reads by name",
        std::any::type_name::<T>()
    );
    assert!(
        !found.is_empty(),
        "a struct with no fields tells this test nothing"
    );
    found.into_iter().collect()
}

/// what the capturing deserializer says once it has the field list
const CAPTURED: &str = "the field list is the whole of what this wanted";

/// a deserializer that answers nothing and records the field list it is offered
struct Fields<'a> {
    found: &'a mut Vec<String>,
}

impl<'de> serde::Deserializer<'de> for Fields<'_> {
    type Error = serde::de::value::Error;

    fn deserialize_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.found
            .extend(fields.iter().map(|field| (*field).to_owned()));
        Err(serde::de::Error::custom(CAPTURED))
    }

    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        Err(serde::de::Error::custom(
            "this deserializer answers a struct with its field list and nothing else",
        ))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map enum identifier ignored_any
    }
}
