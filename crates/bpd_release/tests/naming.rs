//! the two names and the two versions this project has, and that they agree
//!
//! a release is published under whatever the wheel says, and the wheel says
//! whatever it was told. the things that could disagree are `pyproject.toml`,
//! the workspace's cargo version, and the default this crate ships — and every
//! way they can disagree produces a package rather than an error:
//!
//! - a name that is not the project pypi holds is either an upload refused at
//!   the last step of a release, or a second project nobody is looking at
//! - a version that is not the crates' is a wheel whose `bpd --version`
//!   disagrees with the wheel it came out of, which is the one number anybody
//!   reports a bug against
//!
//! so they are checked here, where the check costs nothing, rather than by
//! whoever notices

use bpd_release::wheel::DISTRIBUTION;

/// the value of one `key = "..."` under `[project]` in `pyproject.toml`
///
/// a scan rather than a toml parse, because the alternative is a dependency
/// carried by the whole workspace to read two strings out of a file that is
/// eleven lines long. it fails loudly when the shape it assumes is gone
fn pyproject(key: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pyproject.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|why| panic!("`{}` is the workspace's pyproject: {why}", path.display()));

    let mut project = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            project = line == "[project]";
            continue;
        }
        if !project {
            continue;
        }
        if let Some(value) = line.strip_prefix(key).and_then(|rest| {
            rest.trim_start()
                .strip_prefix('=')
                .map(|value| value.trim().trim_matches('"'))
        }) {
            return value.to_string();
        }
    }
    panic!(
        "`{}` has no `{key}` under `[project]`, and this test reads one",
        path.display()
    )
}

#[test]
fn the_wheel_is_built_under_the_name_pyproject_declares() {
    assert_eq!(
        pyproject("name"),
        DISTRIBUTION,
        "`pyproject.toml` names the project pypi holds, and `bpd-release` \
         defaults to the name it publishes under. a release built while those \
         two disagree goes somewhere nobody is looking"
    );
}

#[test]
fn the_python_version_is_the_crates_version() {
    // the two spellings of one number. cargo takes a semver prerelease with a
    // dash — `0.0.1-a1` — and pep 440 takes the same thing without one. so the
    // rule is that the python version is the crates' version with the dash
    // dropped, and nothing else
    let crates = env!("CARGO_PKG_VERSION");
    let python = pyproject("version");
    assert_eq!(
        python,
        crates.replace('-', ""),
        "`pyproject.toml` says `{python}` and the crates say `{crates}`. they \
         are one version written two ways, and a release built from the two of \
         them ships a wheel whose `bpd --version` is not its own"
    );
    assert!(
        !python.contains('-'),
        "and a wheel filename joins its fields with `-`, so `{python}` could \
         not be carried in one at all"
    );
}
