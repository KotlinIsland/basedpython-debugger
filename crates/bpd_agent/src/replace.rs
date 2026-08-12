//! replacing the code the process is running with the code that is on disk
//!
//! the inverse of [`crate::source`]. that module compiles the file and requires
//! the frame's own code object to be in what comes out, so a mismatch means
//! "bpd will not show you this source". here a mismatch is the **reason** to
//! offer a replacement, and the same comparison decides whether one can be made
//!
//! compiling runs none of the program. it is the compiler, on bytes, and a
//! module that would raise on import raises nothing here
//!
//! ## what is compared, and what is deliberately not
//!
//! the file is compiled and the tree that comes out is walked against the tree
//! the process is running, code object by code object, from the module's own
//! code object down through `co_consts`
//!
//! a **body** — the module's code object, or a class's — has to be identical in
//! everything it does: its bytecode, the names it reads and writes, the literal
//! values it holds, and the sequence of things it defines. what is **not**
//! compared is its line table, because that moves whenever a function body above
//! it gains or loses a line and it says nothing about what the body does
//!
//! a **function** may differ freely in its body and may not differ in its
//! parameters. that is the whole of what a replacement is allowed to change
//!
//! ## why the top level is never re-run
//!
//! a module body that is different code could only be applied by executing it
//! again, and that is running the program a second time rather than reloading
//! it: every import, call and registration in it happens again, and every name
//! it binds becomes a new object that anything already holding the old one never
//! sees. the roadmap's "a module with import side effects" is an instance of
//! that rule rather than a case of its own
//!
//! ## the frames
//!
//! **measured on 3.13, 3.14, 3.15 and 3.14t**: assigning `function.__code__`
//! under a frame that is running that code is accepted, the frame runs the old
//! code to completion, and the next call gets the new one. nothing aborts. this
//! is *not* the `f_lineno` trap next door, which really does kill the
//! interpreter, and no message here may say otherwise
//!
//! it is refused because of what it would leave behind: until that frame returns
//! the process is running two versions of one function, and a stack whose frames
//! behave two different ways is evidence about neither. so no frame of the
//! process may be running any code object that is about to change — on a thread,
//! or suspended inside a generator, a coroutine or an async generator

use std::collections::BTreeMap;
use std::path::Path;

use bpd_core::{Divergence, LiveFrame, Rebound, Replaced, Replacement, Suspendable, Unreplaceable};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyTuple};

use crate::conditions::capture;
use crate::files::{self, FileId};
use crate::{breakpoints, code, events, stops, world};

/// `CO_OPTIMIZED` — the code object keeps its locals in compiler-assigned slots
///
/// what separates a function from a body. a module's code object and a class
/// body's both read through a namespace mapping and have it clear, which is the
/// only distinction cpython offers and the one this walk needs: the two are held
/// to different standards
const CO_OPTIMIZED: u32 = 0x1;
const CO_VARARGS: u32 = 0x4;
const CO_VARKEYWORDS: u32 = 0x8;

/// replace the code of one file with what is on disk, or refuse whole
pub(crate) fn replace(python: Python<'_>, file: &Path) -> PyResult<Replaced> {
    let refused = |because: Vec<Unreplaceable>| Replaced {
        file: file.to_path_buf(),
        outcome: Replacement::Refused { because },
        mode: world::mode(),
    };

    let identity = match files::identify(file) {
        Ok(identity) => identity,
        Err(reason) => {
            return Ok(refused(vec![Unreplaceable::NotAFile {
                file: file.to_path_buf(),
                reason,
            }]));
        }
    };

    let roots = match module_roots(python, &identity, file)? {
        Ok(roots) => roots,
        Err(reason) => return Ok(refused(vec![reason])),
    };

    // the filename the interpreter compiled this file under, reused verbatim so
    // that a traceback out of the new code names what the old one named. it is
    // also the key the code registry is rebuilt against
    let filename: String = roots.getattr("co_filename")?.extract()?;
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ok(refused(vec![Unreplaceable::NotAFile {
                file: file.to_path_buf(),
                reason: error.to_string(),
            }]));
        }
    };
    let fresh = match compile(python, &bytes, &filename) {
        Ok(fresh) => fresh,
        Err(error) => {
            return Ok(refused(vec![Unreplaceable::DoesNotCompile {
                file: file.to_path_buf(),
                error: capture(python, &error),
            }]));
        }
    };

    let mut plan = Plan::new(file);
    plan.compare(&roots, &fresh, Kind::Module)?;

    // the heap is walked once, for every code object of the file rather than
    // only the ones that changed: which of them a function object holds is what
    // the walk answers, and it is cheaper to ask about all of them at once
    let live = Live::of(python, &plan.wanted())?;
    plan.check(python, &live)?;

    if !plan.refusals.is_empty() {
        return Ok(refused(plan.refusals));
    }

    let mut changed = Vec::with_capacity(plan.changed.len());
    for (old, new) in &plan.changed {
        let holders = live.functions.get(&(old.as_ptr() as usize));
        for holder in holders.into_iter().flatten() {
            // every way this can fail was checked before anything was written:
            // cpython's only condition on the assignment is that the code's free
            // variable count matches the function's cells, which `Plan::check`
            // refuses on. a partial application is the one outcome this whole
            // feature exists to prevent
            holder
                .bind(python)
                .setattr("__code__", new)
                .expect("a replacement writes only assignments it proved cpython accepts");
        }
        changed.push(Rebound {
            function: new.getattr("co_qualname")?.extract()?,
            was_at: old.getattr("co_firstlineno")?.extract()?,
            now_at: new.getattr("co_firstlineno")?.extract()?,
            objects: u32::try_from(holders.map_or(0, Vec::len))
                .expect("a process does not hold four billion copies of one function"),
        });
    }

    // binding walks down from the file's registered root, and every live
    // function of it now runs the new tree — so the old root describes code
    // nothing will execute, and a breakpoint bound through it would be armed
    // where no thread will ever arrive
    code::adopt(python, &identity, &fresh)?;
    let rebound = breakpoints::reresolve(python)?;

    Ok(Replaced {
        file: file.to_path_buf(),
        outcome: Replacement::Applied {
            changed,
            unchanged: plan.unchanged,
            rebound,
        },
        mode: world::mode(),
    })
}

/// the module-level code object registered for this file
///
/// the root binding already walks from, and the only thing that reaches the
/// whole tree. a file with several is one the interpreter compiled more than
/// once, and which live function belongs to which copy is not answerable from
/// the file
fn module_roots<'py>(
    python: Python<'py>,
    identity: &FileId,
    file: &Path,
) -> PyResult<Result<Bound<'py, PyAny>, Unreplaceable>> {
    let units = code::units_for(python, identity)?;
    if units.is_empty() {
        return Ok(Err(Unreplaceable::NotLoaded {
            file: file.to_path_buf(),
        }));
    }
    if !code::whole_file_seen(identity) {
        return Ok(Err(Unreplaceable::PartiallyLoaded {
            file: file.to_path_buf(),
        }));
    }

    let mut modules: Vec<Bound<'py, PyAny>> = units
        .into_iter()
        .filter(|unit| unit.qualname == "<module>")
        .map(|unit| unit.code.into_bound(python))
        .collect();

    match modules.len() {
        0 => unreachable!(
            "`whole_file_seen` is set exactly when a module-level code object of \
             the file was registered as a root, and `units_for` walks the roots"
        ),
        1 => Ok(Ok(modules.remove(0))),
        copies => Ok(Err(Unreplaceable::CompiledMoreThanOnce {
            file: file.to_path_buf(),
            copies: u32::try_from(copies).expect("a file is not compiled four billion times"),
        })),
    }
}

/// compile the file's bytes the way the import machinery does
///
/// **bytes**, not text: a source file declares its own encoding under PEP 263
/// and cpython is the thing that reads that declaration. `dont_inherit` is what
/// `importlib._bootstrap_external.source_to_code` passes, so a `__future__`
/// statement in the file decides the flags and nothing else does. the same call
/// [`crate::source`] makes, for the same reason
fn compile<'py>(python: Python<'py>, bytes: &[u8], filename: &str) -> PyResult<Bound<'py, PyAny>> {
    let arguments = PyTuple::new(
        python,
        [
            PyBytes::new(python, bytes).into_any(),
            filename.into_pyobject(python)?.into_any(),
            "exec".into_pyobject(python)?.into_any(),
        ],
    )?;
    let keywords = PyDict::new(python);
    keywords.set_item("dont_inherit", true)?;
    python
        .import("builtins")?
        .getattr("compile")?
        .call(arguments, Some(&keywords))
}

/// what a code object is, which decides what it is allowed to differ in
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// the file's own code object
    Module,
    /// a class body — the code that built a class whose instances exist
    Class,
    /// a function, a lambda, a generator expression, an annotation scope
    Function,
}

impl Kind {
    /// what a nested code object is, from the one flag that separates them
    fn of(flags: u32) -> Self {
        if flags & CO_OPTIMIZED == 0 {
            Self::Class
        } else {
            Self::Function
        }
    }
}

/// everything about one code object the comparison reads
///
/// read once. every field is an attribute lookup into the interpreter, and the
/// walk asks about each code object several times
struct Facts {
    qualname: String,
    first_line: u32,
    flags: u32,
    argcount: u32,
    posonly: u32,
    kwonly: u32,
    varnames: Vec<String>,
    names: Vec<String>,
    freevars: Vec<String>,
    cellvars: Vec<String>,
    linetable: Vec<u8>,
    /// `co_qualname` of each nested code object, in `co_consts` order
    nested: Vec<String>,
    /// the instruction stream, resolved and marshalled — see [`instructions`]
    instructions: Vec<u8>,
    /// every constant that is **not** a code object, marshalled
    ///
    /// only [`Facts::identical`] reads it. a constant nothing loads has no
    /// effect on what a body does — which is why it is not a difference a
    /// refusal is made of — but a function's docstring is exactly that, and a
    /// docstring that changed is a code object that has to be replaced
    ///
    /// `marshal` is what a `.pyc` compares with, and it is the only encoding
    /// that keeps a constant's type: `1 == 1.0` and `1 == True` in python, so a
    /// literal changed from one to the other would compare equal
    constants: Vec<u8>,
}

impl Facts {
    fn of(python: Python<'_>, code: &Bound<'_, PyAny>) -> PyResult<Self> {
        let kind = code.get_type();
        let mut nested = Vec::new();
        let plain = PyList::empty(python);
        for constant in code.getattr("co_consts")?.try_iter()? {
            let constant = constant?;
            if constant.is_instance(&kind)? {
                nested.push(constant.getattr("co_qualname")?.extract()?);
            } else {
                plain.append(&constant)?;
            }
        }

        Ok(Self {
            qualname: code.getattr("co_qualname")?.extract()?,
            first_line: code.getattr("co_firstlineno")?.extract()?,
            flags: code.getattr("co_flags")?.extract()?,
            argcount: code.getattr("co_argcount")?.extract()?,
            posonly: code.getattr("co_posonlyargcount")?.extract()?,
            kwonly: code.getattr("co_kwonlyargcount")?.extract()?,
            varnames: code.getattr("co_varnames")?.extract()?,
            names: code.getattr("co_names")?.extract()?,
            freevars: code.getattr("co_freevars")?.extract()?,
            cellvars: code.getattr("co_cellvars")?.extract()?,
            linetable: code.getattr("co_linetable")?.extract()?,
            nested,
            instructions: instructions(python, code)?,
            constants: marshal(python, &plain)?,
        })
    }

    /// the parameters, as they would be written
    ///
    /// `co_varnames` begins with the parameters in the order the compiler lays
    /// them out — positional-only, then positional-or-keyword, then
    /// keyword-only, then `*args`, then `**kwargs`
    fn signature(&self) -> String {
        let named = |from: u32, count: u32| -> Vec<String> {
            (from..from + count)
                .filter_map(|at| self.varnames.get(at as usize).cloned())
                .collect()
        };

        let mut written = named(0, self.posonly);
        if self.posonly > 0 {
            written.push("/".to_string());
        }
        written.extend(named(self.posonly, self.argcount - self.posonly));

        // the compiler lays `*args` out *after* the keyword-only parameters and
        // the source writes it before them, so the two are assembled rather than
        // copied straight out
        let varargs = self.flags & CO_VARARGS != 0;
        if varargs {
            written.extend(
                named(self.argcount + self.kwonly, 1)
                    .into_iter()
                    .map(|name| format!("*{name}")),
            );
        } else if self.kwonly > 0 {
            written.push("*".to_string());
        }
        written.extend(named(self.argcount, self.kwonly));

        if self.flags & CO_VARKEYWORDS != 0 {
            let at = self.argcount + self.kwonly + u32::from(varargs);
            written.extend(named(at, 1).into_iter().map(|name| format!("**{name}")));
        }
        format!("({})", written.join(", "))
    }

    /// whether two code objects take the same arguments
    ///
    /// the free variables are part of it because a function object's closure
    /// cells are positional: a body that closes over different names is a body
    /// the live cells do not fit
    fn same_signature(&self, other: &Self) -> bool {
        self.argcount == other.argcount
            && self.posonly == other.posonly
            && self.kwonly == other.kwonly
            && self.flags & (CO_VARARGS | CO_VARKEYWORDS)
                == other.flags & (CO_VARARGS | CO_VARKEYWORDS)
            && self.parameters() == other.parameters()
            && self.freevars == other.freevars
    }

    /// the names of the parameters, in the compiler's order
    fn parameters(&self) -> &[String] {
        let count = (self.argcount
            + self.kwonly
            + u32::from(self.flags & CO_VARARGS != 0)
            + u32::from(self.flags & CO_VARKEYWORDS != 0)) as usize;
        &self.varnames[..count.min(self.varnames.len())]
    }

    /// everything a body is required to have in common with its replacement
    ///
    /// the line table is left out on purpose: it moves whenever a function body
    /// above this one gains or loses a line, and it says nothing about what the
    /// body does. everything that decides what it does is in here
    fn divergences(&self, other: &Self) -> Vec<Divergence> {
        let mut differences = Vec::new();

        if self.nested != other.nested {
            let (added, removed) = (
                missing(&other.nested, &self.nested),
                missing(&self.nested, &other.nested),
            );
            // both empty is a reordering: the same things, defined in another
            // order, which is a different body and would be reported as no
            // difference at all if it were left here
            if added.is_empty() && removed.is_empty() {
                differences.push(Divergence::Instructions);
            } else {
                differences.push(Divergence::Defines { added, removed });
            }
        }
        if self.names != other.names {
            let (added, removed) = (
                missing(&other.names, &self.names),
                missing(&self.names, &other.names),
            );
            if added.is_empty() && removed.is_empty() {
                differences.push(Divergence::Instructions);
            } else {
                differences.push(Divergence::Names { added, removed });
            }
        }
        if self.instructions != other.instructions
            || self.flags != other.flags
            || self.varnames != other.varnames
            || self.freevars != other.freevars
            || self.cellvars != other.cellvars
            || self.argcount != other.argcount
            || self.posonly != other.posonly
            || self.kwonly != other.kwonly
        {
            differences.push(Divergence::Instructions);
        }

        differences.dedup();
        differences
    }

    /// whether this is, in every respect a debugger reports, the same code
    ///
    /// the line table is in here, unlike [`Self::divergences`]. a function that
    /// moved down the file runs the same statements and reports different line
    /// numbers, and reporting the old ones is the exact lie this milestone
    /// exists to end
    fn identical(&self, other: &Self) -> bool {
        self.divergences(other).is_empty()
            && self.constants == other.constants
            && self.linetable == other.linetable
            && self.first_line == other.first_line
            && self.qualname == other.qualname
    }
}

/// one code object's instructions, resolved, as bytes that can be compared
///
/// **not `co_code`.** the raw bytecode carries indices into `co_names` and
/// `co_consts` rather than the names and values themselves, so comparing it
/// needs those two compared beside it — and a constant nothing loads would then
/// count as a difference in what a body *does*. `dis` resolves each operand to
/// what it means, which is the thing being compared
///
/// a nested code object becomes its `co_qualname`: what a body does is define
/// it, and what is inside it is compared on its own pass
///
/// ## the one thing that is masked, and why
///
/// **cpython stores the class's own source line in every class body**, as
/// `__firstlineno__`, since 3.13. so a class that merely moved down the file has
/// a different body — measured on 3.13, 3.14, 3.15 and 3.14t, where it is a
/// `LOAD_CONST` of the line on 3.13 and a `LOAD_SMALL_INT` of it on the rest.
/// that is a **line number**, in the category already excluded for a body along
/// with the line table, and left in it would refuse every edit above a class as
/// a changed class layout
///
/// so the one instruction that feeds `STORE_NAME __firstlineno__` is replaced
/// rather than dropped: both loads are two bytes wide on every interpreter here,
/// so replacing keeps every later jump's target where it was and masks nothing
/// else. `a_class_body_carries_its_own_source_line_and_it_is_the_only_thing_masked`
/// fails if cpython stops doing this or starts doing it differently
fn instructions(python: Python<'_>, code: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    let kind = code.get_type();
    let stream = PyList::empty(python);
    for instruction in python
        .import("dis")?
        .getattr("get_instructions")?
        .call1((code,))?
        .try_iter()?
    {
        let instruction = instruction?;
        let name: String = instruction.getattr("opname")?.extract()?;
        let operand = instruction.getattr("argval")?;

        if name == "STORE_NAME"
            && operand
                .extract::<String>()
                .is_ok_and(|stored| stored == "__firstlineno__")
            && !stream.is_empty()
        {
            let last = stream.len() - 1;
            stream.set_item(last, ("<the class's own source line>", python.None()))?;
        }

        let key = if operand.is_instance(&kind)? {
            operand.getattr("co_qualname")?
        } else {
            operand
        };
        stream.append((name, key))?;
    }

    marshal(python, &stream)
}

/// the exact bytes of a list of constants, type and all
///
/// what a `.pyc` is compared with. `==` is not that comparison: `1 == 1.0` and
/// `1 == True` in python, so a literal changed from one to the other would come
/// back equal and a body that really moved would be called unchanged
fn marshal(python: Python<'_>, value: &Bound<'_, PyList>) -> PyResult<Vec<u8>> {
    python
        .import("marshal")?
        .getattr("dumps")?
        .call1((value,))?
        .extract()
}

/// the entries of `wanted` that `held` does not have
fn missing(wanted: &[String], held: &[String]) -> Vec<String> {
    let mut found: Vec<String> = wanted
        .iter()
        .filter(|name| !held.contains(name))
        .cloned()
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// the comparison, and what it decided
struct Plan<'py> {
    file: std::path::PathBuf,
    /// the pairs whose code differs, old first
    changed: Vec<(Bound<'py, PyAny>, Bound<'py, PyAny>)>,
    /// `co_qualname` of everything the file has that did not move at all
    unchanged: Vec<String>,
    /// every old code object the walk reached, for the heap scan
    reached: Vec<Bound<'py, PyAny>>,
    /// old code objects the file on disk has no counterpart for
    orphans: Vec<Bound<'py, PyAny>>,
    refusals: Vec<Unreplaceable>,
}

impl<'py> Plan<'py> {
    fn new(file: &Path) -> Self {
        Self {
            file: file.to_path_buf(),
            changed: Vec::new(),
            unchanged: Vec::new(),
            reached: Vec::new(),
            orphans: Vec::new(),
            refusals: Vec::new(),
        }
    }

    /// the addresses the heap scan has to look for
    fn wanted(&self) -> Vec<usize> {
        self.reached
            .iter()
            .map(|code| code.as_ptr() as usize)
            .collect()
    }

    /// walk one old code object against its replacement
    fn compare(
        &mut self,
        old: &Bound<'py, PyAny>,
        new: &Bound<'py, PyAny>,
        kind: Kind,
    ) -> PyResult<()> {
        let python = old.py();
        self.reached.push(old.clone());

        let before = Facts::of(python, old)?;
        let after = Facts::of(python, new)?;

        let lines_up = match kind {
            Kind::Module | Kind::Class => {
                let differences = before.divergences(&after);
                if differences.is_empty() {
                    true
                } else {
                    self.refusals.push(match kind {
                        Kind::Module => Unreplaceable::TopLevelChanged {
                            file: self.file.clone(),
                            differences,
                        },
                        // a body that is not the module's is a class body: the
                        // only two code objects cpython leaves unoptimized
                        _ => Unreplaceable::ClassLayoutChanged {
                            class: before.qualname.clone(),
                            differences,
                        },
                    });
                    // the trees below two bodies that build different things do
                    // not line up, and pairing them would be inventing a
                    // correspondence
                    return Ok(());
                }
            }
            Kind::Function => {
                if !before.same_signature(&after) {
                    self.refusals.push(Unreplaceable::SignatureChanged {
                        function: before.qualname.clone(),
                        was: before.signature(),
                        now: after.signature(),
                    });
                    return Ok(());
                }
                // a function's body is what a replacement is allowed to change,
                // so its nested code objects can only be paired by name
                before.nested == after.nested && before.instructions == after.instructions
            }
        };

        if before.identical(&after) {
            self.unchanged.push(before.qualname.clone());
        } else {
            self.changed.push((old.clone(), new.clone()));
        }

        let mine = nested(old)?;
        let theirs = nested(new)?;
        if lines_up {
            debug_assert_eq!(
                mine.len(),
                theirs.len(),
                "the two code objects define the same sequence of things, which \
                 is what put them in `co_consts` at the same positions"
            );
            for (old, new) in mine.iter().zip(&theirs) {
                let flags: u32 = old.getattr("co_flags")?.extract()?;
                self.compare(old, new, Kind::of(flags))?;
            }
            return Ok(());
        }

        self.pair_by_name(&before.qualname, &mine, &theirs)
    }

    /// pair the nested code objects of a changed function by `co_qualname`
    ///
    /// positions moved with the edit, so there is nothing else to pair on. a
    /// name that appears twice on either side pairs with nothing, because
    /// picking either would be a coin toss over which body a live closure runs
    fn pair_by_name(
        &mut self,
        parent: &str,
        mine: &[Bound<'py, PyAny>],
        theirs: &[Bound<'py, PyAny>],
    ) -> PyResult<()> {
        let mut by_name: BTreeMap<String, Vec<Bound<'py, PyAny>>> = BTreeMap::new();
        for new in theirs {
            by_name
                .entry(new.getattr("co_qualname")?.extract()?)
                .or_default()
                .push(new.clone());
        }

        let mut seen: BTreeMap<String, u32> = BTreeMap::new();
        for old in mine {
            let name: String = old.getattr("co_qualname")?.extract()?;
            *seen.entry(name.clone()).or_default() += 1;
        }

        for old in mine {
            let name: String = old.getattr("co_qualname")?.extract()?;
            let ours = seen.get(&name).copied().unwrap_or_default();
            let matched = by_name.get(&name);
            if ours > 1 || matched.is_some_and(|found| found.len() > 1) {
                self.refusals.push(Unreplaceable::Ambiguous {
                    function: parent.to_string(),
                    nested: name,
                });
                continue;
            }

            match matched.and_then(|found| found.first()) {
                Some(new) => {
                    let flags: u32 = old.getattr("co_flags")?.extract()?;
                    self.compare(old, new, Kind::of(flags))?;
                }
                // nothing on disk corresponds to it. whether that matters is
                // whether anything in the process still runs it, which the heap
                // scan answers — recorded here so that it is asked
                None => {
                    self.reached.push(old.clone());
                    self.orphans.push(old.clone());
                }
            }
        }
        Ok(())
    }

    /// everything the heap says about the code that is about to change
    fn check(&mut self, python: Python<'_>, live: &Live) -> PyResult<()> {
        let mut found = Vec::new();

        // a nested function the file no longer defines, that objects in the
        // process still run. replacing everything around it would leave them
        // running code that is in no version of the file
        for old in &self.orphans {
            let held = live
                .functions
                .get(&(old.as_ptr() as usize))
                .map_or(0, Vec::len);
            if held > 0 {
                found.push(Unreplaceable::Orphaned {
                    function: old.getattr("co_qualname")?.extract()?,
                    objects: u32::try_from(held)
                        .expect("a process does not hold four billion copies of one function"),
                });
            }
        }

        for (old, new) in &self.changed {
            let address = old.as_ptr() as usize;
            let function: String = old.getattr("co_qualname")?.extract()?;

            for frame in live.frames.get(&address).into_iter().flatten() {
                found.push(Unreplaceable::Running {
                    function: function.clone(),
                    frame: frame.clone(),
                });
            }

            // cpython's own condition on the assignment, checked here so that it
            // can never be found half way through applying one
            let wanted = new.getattr("co_freevars")?.len()?;
            for holder in live.functions.get(&address).into_iter().flatten() {
                let closure = holder.bind(python).getattr("__closure__")?;
                let cells = if closure.is_none() { 0 } else { closure.len()? };
                if cells != wanted {
                    found.push(Unreplaceable::ClosureChanged {
                        function: function.clone(),
                        cells: u32::try_from(cells).expect("a closure is not four billion cells"),
                        wanted: u32::try_from(wanted)
                            .expect("a code object has not four billion free variables"),
                    });
                }
            }
        }

        self.refusals.extend(found);
        Ok(())
    }
}

/// the code objects nested directly inside one, in `co_consts` order
fn nested<'py>(code: &Bound<'py, PyAny>) -> PyResult<Vec<Bound<'py, PyAny>>> {
    let kind = code.get_type();
    let mut found = Vec::new();
    for constant in code.getattr("co_consts")?.try_iter()? {
        let constant = constant?;
        if constant.is_instance(&kind)? {
            found.push(constant);
        }
    }
    Ok(found)
}

/// what the process holds that runs the code being replaced
///
/// one walk of the heap. `gc.get_objects` is the only thing that finds a
/// function object no namespace still points at — a decorator's captured
/// original, a closure a factory handed out — and finding those is the whole
/// difference between replacing a module and replacing the names in its
/// dictionary
///
/// **frames are not looked for in the heap.** a frame becomes a heap object when
/// something keeps it, which a caught exception's traceback does, and such a
/// frame has already returned and will never run again. the two kinds that will
/// run are asked for directly: a thread's own chain, and the frame a generator,
/// a coroutine or an async generator is suspended in
struct Live {
    /// function objects, by the address of the code they run
    functions: BTreeMap<usize, Vec<Py<PyAny>>>,
    /// frames that will run that code, by the same address
    frames: BTreeMap<usize, Vec<LiveFrame>>,
}

impl Live {
    fn of(python: Python<'_>, wanted: &[usize]) -> PyResult<Self> {
        let mut live = Self {
            functions: BTreeMap::new(),
            frames: BTreeMap::new(),
        };

        let types = python.import("types")?;
        let function = types.getattr("FunctionType")?;
        let suspendable = [
            (
                types.getattr("GeneratorType")?,
                "gi_frame",
                Suspendable::Generator,
            ),
            (
                types.getattr("CoroutineType")?,
                "cr_frame",
                Suspendable::Coroutine,
            ),
            (
                types.getattr("AsyncGeneratorType")?,
                "ag_frame",
                Suspendable::AsyncGenerator,
            ),
        ];

        for object in python
            .import("gc")?
            .call_method0("get_objects")?
            .try_iter()?
        {
            let object = object?;
            if object.is_instance(&function)? {
                let code = object.getattr("__code__")?;
                let address = code.as_ptr() as usize;
                if wanted.contains(&address) {
                    live.functions
                        .entry(address)
                        .or_default()
                        .push(object.unbind());
                }
                continue;
            }

            for (kind, attribute, name) in &suspendable {
                if !object.is_instance(kind)? {
                    continue;
                }
                let frame = object.getattr(*attribute)?;
                if frame.is_none() {
                    // it ran to its end and its frame is gone. nothing will
                    // execute the code again through this object
                    break;
                }
                let address = frame.getattr("f_code")?.as_ptr() as usize;
                if wanted.contains(&address) {
                    let offset: i64 = frame.getattr("f_lasti")?.extract()?;
                    live.frames
                        .entry(address)
                        .or_default()
                        .push(LiveFrame::Suspended {
                            kind: *name,
                            line: frame.getattr("f_lineno")?.extract()?,
                            started: offset >= 0,
                        });
                }
                break;
            }
        }

        live.walk_threads(python, wanted)?;
        Ok(live)
    }

    /// every frame on every thread's own chain
    ///
    /// a sample of the threads `bpd` is not holding, which is what
    /// [`bpd_core::Replaced::mode`] qualifies. it is the conservative direction:
    /// a sighting refuses, and stopping the world first is what turns the
    /// absence of one into a reading of every thread
    fn walk_threads(&mut self, python: Python<'_>, wanted: &[usize]) -> PyResult<()> {
        let frames = events::current_frames(python)?;
        for (thread, innermost) in frames.cast::<PyDict>()? {
            let thread: u64 = thread.extract()?;
            let held = stops::held_for(thread);
            let mut frame = Some(innermost);

            while let Some(current) = frame {
                let address = current.getattr("f_code")?.as_ptr() as usize;
                if wanted.contains(&address) {
                    self.frames
                        .entry(address)
                        .or_default()
                        .push(LiveFrame::Thread {
                            thread,
                            line: current.getattr("f_lineno")?.extract()?,
                            held,
                        });
                }
                let back = current.getattr("f_back")?;
                frame = (!back.is_none()).then_some(back);
            }
        }
        Ok(())
    }
}
