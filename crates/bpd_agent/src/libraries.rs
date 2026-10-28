//! which code is the program's own, and which is a library's
//!
//! what `Request::JustMyCode` turns on. with it on, an exception raised in a
//! library is reported where it first arrives in the program's own code rather
//! than where the library raised it, and a step in goes through a library's code
//! the way it goes through code at no line
//!
//! ## what a library is
//!
//! **the interpreter's own answer, never a list of names.** a code object is a
//! library's when its `co_filename` is
//!
//! - not a file at all: a frozen module (`<frozen importlib._bootstrap>`), or
//!   source handed to `exec` (`<string>`, which is where `dataclasses` and
//!   `namedtuple` compile the methods they write), or
//! - a file in the standard library — the directory `os` was loaded from, which
//!   every interpreter has imported before a program runs — or in a
//!   site-packages directory `site.getsitepackages()` and
//!   `site.getusersitepackages()` say this interpreter has
//!
//! both are read **in the debuggee**, when the setting is turned on, and nothing
//! is imported to read them: `os` and `site` are already in `sys.modules`. an
//! interpreter started with `-S` has no `site`, and then no site-packages
//! directory is on its path to be counted either. it is read here rather than
//! by the engine's probe because a child that was `exec`'d can be another
//! interpreter, and the probe would be about the one bpd started
//!
//! the directories are handed back in the answer, so what was counted is
//! something a client can see rather than something it has to trust

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

/// whether only the program's own code is stopped and stepped in
static ON: AtomicBool = AtomicBool::new(false);

/// the library directories, and what each filename was found to be
#[derive(Debug, Default)]
struct State {
    /// every directory whose files are a library's, each ending in a separator
    /// so that `/usr/lib/python3.14` does not claim `/usr/lib/python3.14x`
    roots: Vec<String>,
    /// what a `co_filename` was found to be, worked out once per filename
    known: BTreeMap<String, bool>,
}

static STATE: Mutex<State> = Mutex::new(State {
    roots: Vec::new(),
    known: BTreeMap::new(),
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE
        .lock()
        .expect("nothing panics holding the library table: every path through it is a lookup")
}

/// turn it on or off, and say what is counted as a library's
pub(crate) fn set(python: Python<'_>, on: bool) -> PyResult<bpd_core::JustMyCode> {
    let roots = if on { roots(python)? } else { Vec::new() };
    {
        let mut state = state();
        state.roots.clone_from(&roots);
        state.known.clear();
    }
    ON.store(on, Ordering::Relaxed);
    Ok(bpd_core::JustMyCode {
        on,
        libraries: roots
            .iter()
            .map(|root| root.trim_end_matches(std::path::MAIN_SEPARATOR).to_string())
            .collect(),
    })
}

/// whether this code object is skipped: just-my-code is on and it is a
/// library's
pub(crate) fn skips(code: &Bound<'_, PyAny>) -> PyResult<bool> {
    if !ON.load(Ordering::Relaxed) {
        return Ok(false);
    }
    let file: String = code.getattr("co_filename")?.extract()?;
    let mut state = state();
    if let Some(known) = state.known.get(&file) {
        return Ok(*known);
    }
    let library = !std::path::Path::new(&file).is_absolute()
        || state
            .roots
            .iter()
            .any(|root| file.starts_with(root.as_str()));
    state.known.insert(file, library);
    Ok(library)
}

/// the directories whose files are a library's, read off the running
/// interpreter without importing anything
fn roots(python: Python<'_>) -> PyResult<Vec<String>> {
    let modules = PyModule::import(python, "sys")?.getattr("modules")?;
    let modules = modules.cast_into::<PyDict>()?;

    let mut directories = Vec::new();
    let Some(os) = modules.get_item("os")? else {
        unreachable!(
            "`os` is imported by the interpreter's own startup, before the agent \
             or any program runs"
        )
    };
    let os_file: String = os.getattr("__file__")?.extract()?;
    if let Some(stdlib) = std::path::Path::new(&os_file).parent() {
        directories.push(stdlib.display().to_string());
    }

    // absent under `-S`, and then no site-packages directory is on the path
    // for anything to have been imported from
    if let Some(site) = modules.get_item("site")? {
        let packages = site.call_method0("getsitepackages")?;
        for directory in packages.cast_into::<PyList>()?.iter() {
            directories.push(directory.extract()?);
        }
        directories.push(site.call_method0("getusersitepackages")?.extract()?);
    }

    let mut roots: Vec<String> = directories
        .into_iter()
        .map(|directory| {
            let mut root = directory
                .trim_end_matches(std::path::MAIN_SEPARATOR)
                .to_string();
            root.push(std::path::MAIN_SEPARATOR);
            root
        })
        .collect();
    roots.sort();
    roots.dedup();
    Ok(roots)
}
