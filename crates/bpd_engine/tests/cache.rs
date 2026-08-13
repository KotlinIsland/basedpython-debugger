//! the cache reader is looking at the entry a launch would really use
//!
//! the unit tests beside the module prove what it does with entries of the
//! test's own making. this proves the one claim they cannot: that `current()`
//! names the entry **staging** produces from the agent this build actually
//! made. a reader that agreed with itself about a digest would pass all of them
//! and still point a user at the wrong entry to keep

use std::path::Path;

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

    let cache = bpd_engine::cache::open_at(&root)
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

    let cleared = bpd_engine::cache::open_at(&root)
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

#[test]
fn a_cache_that_cannot_be_trusted_is_refused_rather_than_read() {
    let outside = tempfile::tempdir().expect("a temporary directory can be made");
    let cache = outside.path().join("not a directory");
    std::fs::write(&cache, "something else entirely").expect("the file can be written");

    let Err(error) = bpd_engine::cache::open_at(&cache) else {
        panic!("reading a cache that is a file has to be refused");
    };
    let said = error.to_string();
    assert!(said.contains(&cache.display().to_string()), "{said}");
    assert!(said.contains("not a directory"), "{said}");
}
