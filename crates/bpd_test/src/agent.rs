//! staging the built agent so an interpreter can import it
//!
//! the agent is a `cdylib`. cargo names it `libbpd_agent.dylib`,
//! `libbpd_agent.so` or `bpd_agent.dll` depending on the platform, and an
//! interpreter will only import a file named after the module. staging is the
//! rename, into a directory that can go on `PYTHONPATH`
//!
//! this is test support today and engine work tomorrow. when the engine gains
//! the ability to launch a debuggee it takes this over, resolving a published
//! agent by the interpreter's `EXT_SUFFIX` rather than picking up whatever the
//! last `cargo build` produced

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use bpd_core::python::Capabilities;

use crate::debuggee::Run;

/// the module name the interpreter imports, and so the file's stem
const MODULE: &str = "bpd_agent";

/// the agent, renamed into a directory an interpreter can import from
#[derive(Debug)]
pub struct Staged {
    /// held so the directory outlives the staging
    _directory: tempfile::TempDir,
    canonical: PathBuf,
}

impl Staged {
    /// the directory to put on `PYTHONPATH`
    pub fn python_path(&self) -> &Path {
        &self.canonical
    }

    /// run a snippet with the agent importable
    ///
    /// deliberately not `-I`, which implies `-E` and would discard the
    /// `PYTHONPATH` that makes the agent importable in the first place
    ///
    /// # panics
    ///
    /// if the interpreter cannot be spawned, or writes non-utf8
    pub fn run(&self, interpreter: &Capabilities, code: &str) -> Run {
        let output = std::process::Command::new(&interpreter.executable)
            .env("PYTHONPATH", self.python_path())
            .args(["-c", code])
            .output()
            .unwrap_or_else(|error| {
                panic!(
                    "could not run `{}`: {error}",
                    interpreter.executable.display()
                )
            });

        Run {
            exit_code: output.status.code(),
            success: output.status.success(),
            stdout: String::from_utf8(output.stdout).expect("the snippet writes utf8"),
            stderr: String::from_utf8(output.stderr).expect("cpython writes utf8 to stderr"),
        }
    }
}

/// the agent this test run built, staged once per process
pub fn staged() -> &'static Staged {
    static STAGED: OnceLock<Staged> = OnceLock::new();
    STAGED.get_or_init(stage)
}

fn stage() -> Staged {
    let built = built_artifact();
    let directory = tempfile::tempdir().expect("a temporary directory is available");
    let destination = directory
        .path()
        .join(format!("{MODULE}{}", import_suffix()));

    std::fs::copy(&built, &destination).unwrap_or_else(|error| {
        panic!(
            "could not stage {} as {}: {error}",
            built.display(),
            destination.display()
        )
    });

    let canonical = directory
        .path()
        .canonicalize()
        .expect("the directory was just created");

    Staged {
        _directory: directory,
        canonical,
    }
}

/// what an importable extension module is called on this platform
///
/// the version-tagged `EXT_SUFFIX` is not needed: cpython also accepts the bare
/// suffix, and the agent refuses at import when the interpreter is not the one
/// it was built for, which is the check that actually matters
const fn import_suffix() -> &'static str {
    if cfg!(windows) { ".pyd" } else { ".so" }
}

/// where cargo left the built `cdylib`
///
/// derived from the running test binary, which lives in `<target>/<profile>/deps`,
/// so it follows `CARGO_TARGET_DIR` and the profile without being told
fn built_artifact() -> PathBuf {
    let test_binary = std::env::current_exe().expect("a running test has a path");
    let profile_directory = test_binary
        .parent()
        .and_then(Path::parent)
        .expect("a test binary lives in <target>/<profile>/deps");

    let candidate = profile_directory.join(cargo_artifact_name());
    assert!(
        candidate.is_file(),
        "the agent has not been built: {} does not exist\n\
         build it for a supported interpreter:\n    \
         PYO3_PYTHON=python3.14 cargo build -p bpd_agent",
        candidate.display()
    );
    candidate
}

fn cargo_artifact_name() -> String {
    if cfg!(windows) {
        format!("{MODULE}.dll")
    } else if cfg!(target_vendor = "apple") {
        format!("lib{MODULE}.dylib")
    } else {
        format!("lib{MODULE}.so")
    }
}
