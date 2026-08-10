//! the cached agent is a file a real interpreter really imports
//!
//! the unit tests next to the cache prove what it does with bytes; this proves
//! the thing those bytes are for. a cache that held a correct copy of the agent
//! which cpython then refused to import would pass every one of them, and it is
//! exactly what a wrong file name, a mode nobody can read, or a directory that
//! is not on `sys.path` would produce
//!
//! it also stages twice into the same cache, because reuse is the whole point:
//! the second launch has to import the entry the first one published rather
//! than a copy of it

use std::path::Path;

use bpd_core::python::Capabilities;

/// the interpreter the built agent matches, or a failure saying how to get one
fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// what the debuggee is asked: the module imports, and says where it came from
const WHERE: &str = "import bpd_agent; print(bpd_agent.__file__)";

fn imported_from(interpreter: &Capabilities, python_path: &Path) -> String {
    let output = std::process::Command::new(&interpreter.executable)
        .env("PYTHONPATH", python_path)
        .args(["-c", WHERE])
        .output()
        .unwrap_or_else(|error| panic!("could not run the interpreter: {error}"));

    assert!(
        output.status.success(),
        "the staged agent did not import: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("cpython prints a path as utf8")
        .trim()
        .to_owned()
}

#[test]
fn a_cached_agent_is_imported_by_the_interpreter_it_was_built_for() {
    let cache = tempfile::tempdir().expect("a temporary directory can be made");

    let first = bpd_engine::agent::stage_into(cache.path())
        .unwrap_or_else(|error| panic!("could not stage the agent: {error}"));
    let again = bpd_engine::agent::stage_into(cache.path())
        .unwrap_or_else(|error| panic!("could not stage the agent again: {error}"));

    assert_eq!(
        first.python_path(),
        again.python_path(),
        "the same build has to be staged to the same place, or nothing is reused"
    );

    let imported = imported_from(interpreter(), again.python_path());
    assert_eq!(
        Path::new(&imported)
            .parent()
            .expect("an imported module has a directory"),
        again.python_path(),
        "the interpreter has to have imported the cache entry, not another copy"
    );
}

#[test]
fn a_cache_that_cannot_be_trusted_is_refused_rather_than_worked_around() {
    let outside = tempfile::tempdir().expect("a temporary directory can be made");
    let cache = outside.path().join("not a directory");
    std::fs::write(&cache, "something else entirely").expect("the file can be written");

    let refused = bpd_engine::agent::stage_into(&cache);

    let Err(error) = refused else {
        panic!("staging into a file has to be refused, not worked around");
    };
    let said = error.to_string();
    assert!(said.contains(&cache.display().to_string()), "{said}");
    assert!(said.contains("not a directory"), "{said}");
}
