//! finding the agent build and putting it where an interpreter can import it
//!
//! cargo names the artifact `libbpd_agent.dylib`, `libbpd_agent.so` or
//! `bpd_agent.dll` depending on the platform, and an interpreter only imports a
//! file named after the module. staging is that rename, into a directory that
//! goes on the debuggee's `PYTHONPATH`
//!
//! resolution today is "next to the running executable", which is what a cargo
//! build produces and what an installed layout would also produce. publishing a
//! build per interpreter tag, and choosing between them by `EXT_SUFFIX`, is
//! still ahead — until then an agent built for the wrong interpreter is caught
//! by the agent itself at import, which is the check that actually decides

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// the module name the interpreter imports, and so the file's stem
const MODULE: &str = "bpd_agent";

/// the agent, renamed into a directory an interpreter can import from
#[derive(Debug)]
pub struct Staged {
    /// held so the directory outlives the staging
    _directory: tempfile::TempDir,
    python_path: PathBuf,
}

impl Staged {
    /// the directory to put on the debuggee's `PYTHONPATH`
    pub fn python_path(&self) -> &Path {
        &self.python_path
    }
}

/// copy the built agent into a directory an interpreter can import from
pub fn stage() -> Result<Staged> {
    let built = built_artifact()?;
    let directory = tempfile::tempdir().map_err(|source| Error::StageAgent {
        path: built.clone(),
        source,
    })?;
    let destination = directory
        .path()
        .join(format!("{MODULE}{}", import_suffix()));

    std::fs::copy(&built, &destination).map_err(|source| Error::StageAgent {
        path: built.clone(),
        source,
    })?;

    // canonicalised because the debuggee reports a resolved `sys.path`, and a
    // temporary directory sits under a symlink on macos
    let python_path = directory
        .path()
        .canonicalize()
        .map_err(|source| Error::StageAgent {
            path: destination,
            source,
        })?;

    Ok(Staged {
        _directory: directory,
        python_path,
    })
}

/// what an importable extension module is called on this platform
const fn import_suffix() -> &'static str {
    if cfg!(windows) { ".pyd" } else { ".so" }
}

/// where the agent build lives, relative to whatever is running
///
/// a test binary sits in `<target>/<profile>/deps`, and `bpd` itself in
/// `<target>/<profile>`. both are checked, so the same resolution works from a
/// test and from the installed binary
fn built_artifact() -> Result<PathBuf> {
    let running = std::env::current_exe().map_err(|source| Error::LocateAgent {
        reason: format!("the running executable has no path: {source}"),
    })?;
    let directory = running.parent().ok_or_else(|| Error::LocateAgent {
        reason: format!("`{}` has no parent directory", running.display()),
    })?;

    let name = cargo_artifact_name();
    let candidates = [directory.join(&name), directory.join("..").join(&name)];

    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }

    Err(Error::LocateAgent {
        reason: format!(
            "no `{name}` next to `{}`. build it for a supported interpreter:\n    \
             PYO3_PYTHON=python3.14 cargo build -p bpd_agent",
            running.display()
        ),
    })
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
