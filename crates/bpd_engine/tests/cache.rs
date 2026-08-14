//! the cache reader is looking at the entries a launch would really use
//!
//! the unit tests beside the module prove what it does with entries of the
//! test's own making. this proves the two claims they cannot: that `current()`
//! names the entry **staging** produces from the agent this build actually
//! made — a reader that agreed with itself about a digest would pass all of
//! them and still point a user at the wrong entry to keep — and that what a
//! **real interpreter** leaves in a child hook entry is what the reader accounts
//! for, since the `__pycache__` in one is cpython's doing and not bpd's

use std::path::Path;

use bpd_engine::cache::Kind;

/// the agent staged into a cache of this test's own, and where it landed
///
/// under the root as it was spelled. `Staged` canonicalises its answer, because
/// a debuggee reports a resolved `sys.path` and on macos a temporary directory
/// is under a `/var` that is a link to `/private/var` — the same entry, named
/// the way the cache was asked about
fn staged_into(cache: &Path) -> std::path::PathBuf {
    let staged = bpd_engine::agent::stage_for_into(cache, bpd_test::agent::matching_interpreter())
        .unwrap_or_else(|error| panic!("could not stage the agent: {error}"));
    let digest = staged
        .python_path()
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .expect("a staged entry is named after its digest");
    cache.join(digest)
}

#[test]
fn the_current_digest_is_the_entry_staging_put_the_built_agent_in() {
    let directory = tempfile::tempdir().expect("a temporary directory can be made");
    let root = directory.path().join("agents");
    let staged = staged_into(&root);

    let cache = bpd_engine::cache::open_at(Kind::Agents, &root)
        .unwrap_or_else(|error| panic!("the cache could not be read: {error}"));
    let current =
        bpd_engine::cache::current().unwrap_or_else(|error| panic!("no built agent: {error}"));

    // one entry per agent this `bpd` carries, and the one that was just staged
    // has to be among them — a reader that agreed with itself about a digest
    // would pass every unit test and still point a user at the wrong entry
    let digests: Vec<&str> = current
        .iter()
        .map(bpd_engine::cache::Current::digest)
        .collect();
    let entry = digests
        .iter()
        .find_map(|digest| cache.entry(digest))
        .unwrap_or_else(|| {
            panic!("the agent for this interpreter was staged and none of {digests:?} is reported")
        });
    assert_eq!(
        entry.path(),
        staged,
        "the current entry has to be the directory staging returned, or \
         `--keep-current` keeps the wrong one"
    );
    let module = staged.join(if cfg!(windows) {
        "bpd_agent.pyd"
    } else {
        "bpd_agent.so"
    });
    assert_eq!(
        entry.size(),
        std::fs::metadata(&module)
            .expect("the staged module is there")
            .len(),
        "an entry's size is the agent it is holding"
    );
}

#[test]
fn clearing_a_cache_removes_the_agent_a_launch_would_have_imported() {
    let directory = tempfile::tempdir().expect("a temporary directory can be made");
    let root = directory.path().join("agents");
    let staged = staged_into(&root);

    let cleared = bpd_engine::cache::open_at(Kind::Agents, &root)
        .unwrap_or_else(|error| panic!("the cache could not be read: {error}"))
        .clear(&[])
        .unwrap_or_else(|error| panic!("clearing failed: {error}"));

    assert!(cleared.succeeded());
    assert_eq!(cleared.removed().len(), 1);
    assert!(!staged.exists(), "the entry a launch would import is gone");

    // and staging it again is the cold load the report warns about, rather than
    // a failure: the cache is a cache
    assert_eq!(
        staged_into(&root),
        staged,
        "the same agent stages back to the same entry"
    );
}

/// what a debugged child really leaves in a child hook entry
///
/// the hook is **imported** out of the entry, so cpython writes the bytecode of
/// it into a `__pycache__` there — bpd never writes one, and a reader that
/// called it a surprise would refuse to clear this cache on every machine that
/// had ever debugged a child. the unit tests write that directory by hand; this
/// is the one that says the shape they write is the shape cpython produces
#[test]
fn an_interpreter_that_imports_the_child_hook_leaves_bytecode_the_entry_accounts_for() {
    let directory = tempfile::tempdir().expect("a temporary directory can be made");
    let root = directory.path().join("children");
    let staged = bpd_engine::agent::stage_child_hook_into(&root)
        .unwrap_or_else(|error| panic!("could not stage the child hook: {error}"));
    let entry = root.join(
        staged
            .python_path()
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .expect("a staged entry is named after its digest"),
    );

    // the entry on `PYTHONPATH` and nothing else, which is what a child of a
    // debuggee with child debugging on has. with none of the `BPD_CHILD_*`
    // variables set the hook reads three of them and returns, so what this runs
    // is the import and not the attach
    let interpreter = &bpd_test::agent::matching_interpreter().executable;
    let ran = std::process::Command::new(interpreter)
        .env("PYTHONPATH", &entry)
        .args([
            "-c",
            "import sys; print(sys.modules['sitecustomize'].__file__)",
        ])
        .output()
        .unwrap_or_else(|error| panic!("could not run `{}`: {error}", interpreter.display()));
    assert!(
        ran.status.success(),
        "the staged hook did not import: {}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(
        Path::new(String::from_utf8_lossy(&ran.stdout).trim())
            .parent()
            .expect("an imported module has a directory"),
        entry,
        "the interpreter has to have imported the entry, or this proves nothing \
         about what it wrote into one"
    );

    let compiled = entry.join("__pycache__");
    assert!(
        compiled.is_dir(),
        "cpython caches the bytecode of a source module it imports beside the \
         source, and that is the whole reason this test exists. if this ever \
         stops being true the allowance in `Kind::compiled` is dead code"
    );

    let cache = bpd_engine::cache::open_at(Kind::Children, &root)
        .unwrap_or_else(|error| panic!("the cache could not be read: {error}"));
    assert!(
        cache.strays().is_empty(),
        "an interpreter compiling the file bpd staged is not something bpd \
         cannot account for: {:?}",
        cache.strays()
    );
    let found = cache
        .entry(&bpd_engine::cache::current_child_hook())
        .unwrap_or_else(|| panic!("the hook this bpd carries was staged into `{entry:?}`"));
    let bytecode: u64 = std::fs::read_dir(&compiled)
        .expect("the bytecode directory can be read")
        .map(|file| {
            file.expect("the directory can be walked")
                .metadata()
                .expect("a file in it has metadata")
                .len()
        })
        .sum();
    assert_eq!(
        found.size(),
        std::fs::metadata(entry.join("sitecustomize.py"))
            .expect("the staged hook is there")
            .len()
            + bytecode,
        "an entry's size is everything in it, and the bytecode is really there"
    );

    let cleared = cache
        .clear(&[])
        .unwrap_or_else(|error| panic!("clearing failed: {error}"));
    assert!(cleared.succeeded());
    assert!(
        !entry.exists(),
        "the entry goes whole, bytecode and all — `remove_dir` on a directory \
         still holding a `.pyc` would have failed here"
    );
}

#[test]
fn a_cache_that_cannot_be_trusted_is_refused_rather_than_read() {
    let outside = tempfile::tempdir().expect("a temporary directory can be made");
    let cache = outside.path().join("not a directory");
    std::fs::write(&cache, "something else entirely").expect("the file can be written");

    let Err(error) = bpd_engine::cache::open_at(Kind::Agents, &cache) else {
        panic!("reading a cache that is a file has to be refused");
    };
    let said = error.to_string();
    assert!(said.contains(&cache.display().to_string()), "{said}");
    assert!(said.contains("not a directory"), "{said}");
}
