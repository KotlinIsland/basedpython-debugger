//! every code object the program has, and the line table of each
//!
//! PEP 669 has no "code object created" event, so there is no notification to
//! subscribe to. the only way to see every code object — including ones `exec`
//! builds long after the program started — is a global `PY_START` that
//! registers each one the first time it is called and then returns `DISABLE`
//!
//! that alone is not enough to bind a breakpoint, because a code object that
//! has never been called has never fired anything. what closes the gap is that
//! a module's code object holds its functions, its classes, its lambdas and its
//! generator expressions in `co_consts`, recursively. registering the module
//! gives the whole tree, and binding walks it

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::RwLock;

use pyo3::prelude::*;

use crate::files::{self, FileId};

/// what the program has compiled so far
#[derive(Debug)]
struct Registry {
    /// the code objects seen at `PY_START`, by their own `co_filename`
    ///
    /// only files that identify on disk are kept. code from a zip archive, a
    /// frozen module or a string handed to `exec` can never be the target of a
    /// breakpoint — bpd binds to files it can identify — so retaining it would
    /// be a leak that grows with every `exec` a program performs
    roots: BTreeMap<String, Vec<Py<PyAny>>>,

    /// what each distinct `co_filename` resolves to, worked out once
    resolved: BTreeMap<String, Result<FileId, String>>,

    /// the filenames that turned out to be the same file
    by_id: BTreeMap<FileId, BTreeSet<String>>,

    /// the addresses of the code objects in `roots`
    ///
    /// `restart_events` re-enables `PY_START` everywhere, so a code object is
    /// offered again after every breakpoint change. an address is a sound key
    /// here only because `roots` holds a strong reference to everything in this
    /// set, which is what stops the allocator handing the same address to a
    /// different code object
    retained: BTreeSet<usize>,
}

impl Registry {
    const fn new() -> Self {
        Self {
            roots: BTreeMap::new(),
            resolved: BTreeMap::new(),
            by_id: BTreeMap::new(),
            retained: BTreeSet::new(),
        }
    }
}

static REGISTRY: RwLock<Registry> = RwLock::new(Registry::new());

fn read() -> std::sync::RwLockReadGuard<'static, Registry> {
    REGISTRY
        .read()
        .expect("the code registry lock is only held for map operations, which do not panic")
}

fn write() -> std::sync::RwLockWriteGuard<'static, Registry> {
    REGISTRY
        .write()
        .expect("the code registry lock is only held for map operations, which do not panic")
}

/// record a code object the interpreter is about to run
///
/// returns the file identity when this is the **first** sighting of a filename
/// that names a real file, which is the only moment a breakpoint's answer can
/// change from "that module is not loaded" to a binding
pub(crate) fn register(code: &Bound<'_, PyAny>) -> PyResult<Option<FileId>> {
    let attribute = code.getattr("co_filename")?;
    // borrowed, not copied: this runs once per code object in the program, and
    // the answer for a filename that is already known is a lookup and nothing
    // else
    let filename: Cow<'_, str> = attribute.extract()?;
    let address = code.as_ptr() as usize;

    let mut registry = write();
    match registry.resolved.get(&*filename).map(Result::is_ok) {
        Some(false) => return Ok(None),
        Some(true) => {
            if registry.retained.insert(address) {
                registry
                    .roots
                    .get_mut(&*filename)
                    .expect("a filename that resolved was given a roots entry when it resolved")
                    .push(code.clone().unbind());
            }
            return Ok(None);
        }
        None => {}
    }

    let identity = files::identify(Path::new(&*filename));
    let filename = filename.into_owned();
    let newly = match &identity {
        Ok(id) => {
            registry
                .roots
                .insert(filename.clone(), vec![code.clone().unbind()]);
            registry.retained.insert(address);
            registry
                .by_id
                .entry(id.clone())
                .or_default()
                .insert(filename.clone());
            Some(id.clone())
        }
        Err(_) => None,
    };
    registry.resolved.insert(filename, identity);
    Ok(newly)
}

/// whether the interpreter has compiled anything under exactly this filename
///
/// the difference between "you typed a path that does not exist" and "that code
/// is real but does not come from the filesystem"
pub(crate) fn loaded_under(filename: &str) -> bool {
    read().resolved.contains_key(filename)
}

/// one code object, with everything binding needs to know about it
#[derive(Debug)]
pub(crate) struct Unit {
    /// the code object itself, which is what `set_local_events` is given
    pub(crate) code: Py<PyAny>,
    /// `co_qualname`
    pub(crate) qualname: String,
    /// `co_firstlineno`
    pub(crate) first_line: u32,
    /// every executable line, and the first instrumentable offset for each
    pub(crate) lines: BTreeMap<u32, u32>,
}

/// every code object compiled from the file with this identity
///
/// the walk starts at the roots registered for that file and descends through
/// `co_consts`. a code object is kept only if its **own** `co_filename` is that
/// same file — normally every descendant's is, but `CodeType.replace` can give
/// a code object a filename its children do not share, and binding a child to a
/// file it does not belong to would be exactly the quiet wrongness this project
/// refuses
pub(crate) fn units_for(python: Python<'_>, wanted: &FileId) -> PyResult<Vec<Unit>> {
    // the roots are copied out and the lock released before any python is
    // touched, so the registry is never held across a call into the interpreter
    let roots: Vec<Py<PyAny>> = {
        let registry = read();
        let Some(filenames) = registry.by_id.get(wanted) else {
            return Ok(Vec::new());
        };
        filenames
            .iter()
            .filter_map(|filename| registry.roots.get(filename))
            .flatten()
            .map(|code| code.clone_ref(python))
            .collect()
    };

    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    for root in &roots {
        let root = root.bind(python).clone();
        let code_type = root.get_type().into_any();
        descend(&root, &code_type, &mut seen, &mut found)?;
    }

    let mut units = Vec::with_capacity(found.len());
    for (filename, unit) in found {
        if identity_of(&filename).as_ref() == Ok(wanted) {
            units.push(unit);
        }
    }
    Ok(units)
}

/// the identity of a `co_filename`, from the cache when it is already known
fn identity_of(filename: &str) -> Result<FileId, String> {
    if let Some(known) = read().resolved.get(filename) {
        return known.clone();
    }

    let identity = files::identify(Path::new(filename));
    let mut registry = write();
    if let Ok(id) = &identity {
        registry
            .by_id
            .entry(id.clone())
            .or_default()
            .insert(filename.to_string());
    }
    registry
        .resolved
        .insert(filename.to_string(), identity.clone());
    identity
}

/// collect a code object and everything nested inside it
fn descend(
    code: &Bound<'_, PyAny>,
    code_type: &Bound<'_, PyAny>,
    seen: &mut BTreeSet<usize>,
    found: &mut Vec<(String, Unit)>,
) -> PyResult<()> {
    if !seen.insert(code.as_ptr() as usize) {
        return Ok(());
    }
    found.push(inspect(code)?);

    for item in code.getattr("co_consts")?.try_iter()? {
        let item = item?;
        if item.is_instance(code_type)? {
            descend(&item, code_type, seen, found)?;
        }
    }
    Ok(())
}

/// read one code object's line table
fn inspect(code: &Bound<'_, PyAny>) -> PyResult<(String, Unit)> {
    let filename: String = code.getattr("co_filename")?.extract()?;
    let qualname: String = code.getattr("co_qualname")?.extract()?;
    let first_line: u32 = code.getattr("co_firstlineno")?.extract()?;

    let mut lines: BTreeMap<u32, u32> = BTreeMap::new();
    for entry in code.call_method0("co_lines")?.try_iter()? {
        let (start, _end, line): (u32, u32, Option<u32>) = entry?.extract()?;

        // an entry with no line is bytecode the compiler could not attribute to
        // one. line 0 is cpython's way of saying the same thing for a module's
        // leading `RESUME` — source lines are 1-based, so 0 is not one of them
        let Some(line) = line.filter(|line| *line > 0) else {
            continue;
        };

        // one line covers several offsets, and the breakpoint goes on the
        // first: a stop at any of the others would land mid-statement
        lines
            .entry(line)
            .and_modify(|first| *first = (*first).min(start))
            .or_insert(start);
    }

    Ok((
        filename,
        Unit {
            code: code.clone().unbind(),
            qualname,
            first_line,
            lines,
        },
    ))
}
