//! a real django, on disk, for the tests that render real templates
//!
//! django's template internals are not a stable API. `Node.render_annotated`,
//! `Parser.extend_nodelist` and `ExtendsNode.render` are all things `bpd` reads
//! and none of them is documented as something django will keep. so the version
//! is **pinned here and asserted in the debuggee**, the way the cpython
//! characterisation tests name the interpreter they measured: a suite that
//! silently started measuring a different django would be worse than one that
//! fails
//!
//! it is installed rather than vendored, into a tree beside the built agent, by
//! `uv`. the tree is pure python and no interpreter compiled anything into it,
//! so the same one serves every interpreter in the matrix — it is put on the
//! debuggee's `sys.path` by the fixture itself rather than by an environment
//! variable, so nothing about how the program is launched changes
//!
//! there is **no silent skip**. a machine without `uv`, or without the network
//! to fetch django once, fails and says so, for the reason
//! [`crate::Interpreters::require`] does

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// the django every measurement in the suite was taken against
///
/// changing it is changing what the suite proves. the fixtures assert it, so a
/// version bumped here and not re-measured fails rather than drifting
pub const VERSION: &str = "6.1";

/// the directory holding django, put there if it was not there already
///
/// # panics
///
/// if `uv` is not installed, or the install fails. either means the tests below
/// it would be asserting against no django at all
pub fn installed() -> &'static Path {
    static INSTALLED: OnceLock<PathBuf> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let tree = home().join(format!("django-{VERSION}"));
        if tree.join("django").is_dir() {
            return tree;
        }
        install(&tree);
        tree
    })
}

/// where the tree goes: beside the built agent, so `cargo clean` takes it too
fn home() -> PathBuf {
    let running = std::env::current_exe().expect("a running test binary has a path");
    let deps = running
        .parent()
        .expect("a test binary is inside cargo's `deps` directory");
    deps.parent().unwrap_or(deps).join("bpd-test-django")
}

/// fetch django into `tree`, atomically
///
/// installed beside it and renamed into place, because several test binaries
/// run at once and a half-written tree read by another one would fail in a way
/// that has nothing to do with what it was testing
fn install(tree: &Path) {
    let parent = tree.parent().expect("the tree is under a directory");
    std::fs::create_dir_all(parent)
        .unwrap_or_else(|error| panic!("could not create {}: {error}", parent.display()));

    let staging = tempfile::tempdir_in(parent).unwrap_or_else(|error| {
        panic!(
            "could not stage a django install in {}: {error}",
            parent.display()
        )
    });

    let output = Command::new("uv")
        .args(["pip", "install", "--quiet", "--target"])
        .arg(staging.path())
        .arg(format!("django=={VERSION}"))
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "could not run `uv` to install django {VERSION}: {error}. the \
                 django tests need it — install uv, or put a django {VERSION} \
                 tree at {}",
                tree.display()
            )
        });
    assert!(
        output.status.success(),
        "`uv pip install django=={VERSION}` failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // another test binary may have won the race, and its tree is as good as
    // this one — the version is pinned, so they hold the same bytes
    match std::fs::rename(staging.path(), tree) {
        Ok(()) => {}
        Err(_) if tree.join("django").is_dir() => {}
        Err(error) => panic!(
            "could not put the django install at {}: {error}",
            tree.display()
        ),
    }
}

/// the preamble a django fixture opens with
///
/// it puts django on `sys.path` itself rather than being handed a
/// `PYTHONPATH`, so that a fixture launched under `bpd` and one launched
/// directly see exactly the same import state — the thing
/// [`crate::debuggee`] exists to keep true
///
/// `debug` is the template engine's own `'OPTIONS': {'debug': ...}`, which
/// `DEBUG = True` turns on by default and which is the setting people expect to
/// matter here. it is a parameter so a test can prove it does not
pub fn preamble(debug: bool) -> String {
    let django = installed().display().to_string();
    let debug = if debug { "True" } else { "False" };
    format!(
        r#"import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent
MARKS = HERE / "marks"
sys.path.insert(0, {django:?})

import django
from django.conf import settings

assert django.__version__ == {VERSION:?}, (
    f"the suite measured django {VERSION} and imported {{django.__version__}}"
)

settings.configure(
    DEBUG=False,
    TEMPLATES=[
        {{
            "BACKEND": "django.template.backends.django.DjangoTemplates",
            "DIRS": [str(HERE / "templates")],
            "APP_DIRS": False,
            "OPTIONS": {{"debug": {debug}}},
        }}
    ],
)
django.setup()

from django.template.loader import get_template
"#
    )
}
