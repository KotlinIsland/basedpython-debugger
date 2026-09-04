//! entering the user's program the way the interpreter would have
//!
//! the agent is reached through `python -c`, which is one of the three launch
//! forms and is not usually the one the user asked for. everything here exists
//! to undo the difference: the `-c` marker in `sys.argv`, the empty
//! `sys.path[0]`, and a `__main__` module that belongs to the bootstrap rather
//! than to the program
//!
//! none of it is guessed. `crates/bpd_test/tests/launch_forms.rs` records what a
//! bare interpreter produces for each form, and
//! `crates/bpd/tests/launch_parity.rs` compares this against it

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use bpd_protocol::env::Form;
use pyo3::exceptions::PySystemExit;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyModule};

use crate::frames;

/// enter the program the way the interpreter would have
///
/// the one place a launch form decides anything. what `target` holds is what
/// the form says it holds: a path as the user typed it, a module name, or the
/// source of a command
pub(crate) fn enter(python: Python<'_>, form: Form, target: &str) -> PyResult<()> {
    forget_the_bootstraps_source(python)?;

    match form {
        Form::Script => {
            // absolutised once, here, so the path the compiled code object
            // carries is one spelling and one only. the spelling the user typed
            // is kept as well, because `sys.argv[0]` must not be absolutised
            let absolute = std::path::absolute(target).map_err(|error| {
                PySystemExit::new_err(format!("bpd: could not resolve `{target}`: {error}"))
            })?;
            script(python, target, &absolute)
        }
        Form::Module => module(python, target),
        Form::Command => command(python, target),
    }
}

/// how the program's own code object is recognised, so the entry stop lands on
/// it and on nothing else
///
/// two shapes, because the forms genuinely differ in who compiles the program.
/// bpd compiles it itself for a script and for `-c`, so the code object **is**
/// the program and identity is the exact answer. `-m` is resolved and compiled
/// inside `runpy`, which bpd deliberately does not repeat — what identifies it
/// there is the file `__main__` names, which the module namespace carries
/// before its body runs
enum Entry {
    /// the code object bpd compiled, matched by identity
    Compiled(Py<PyAny>),
    /// the `__main__` namespace runpy is about to fill, matched on `__file__`
    MainModule(Py<PyDict>),
}

/// what the entry stop is waiting for, or nothing before a form has decided
static ENTRY: Mutex<Option<Entry>> = Mutex::new(None);

/// whether the entry stop has already happened
static STOPPED_AT_ENTRY: AtomicBool = AtomicBool::new(false);

fn entry() -> std::sync::MutexGuard<'static, Option<Entry>> {
    ENTRY
        .lock()
        .expect("nothing panics holding the entry gate: it is written once before the program runs and read after")
}

/// whether the entry stop has already happened
pub(crate) fn has_stopped_at_entry() -> bool {
    STOPPED_AT_ENTRY.load(Ordering::Relaxed)
}

/// record that the entry stop has happened, so it happens once
pub(crate) fn mark_stopped_at_entry() {
    STOPPED_AT_ENTRY.store(true, Ordering::Relaxed);
}

/// whether this code object is the program's own body
///
/// asked once per code object, and only until the entry stop has happened —
/// after that [`has_stopped_at_entry`] answers first and nothing here runs again
pub(crate) fn is_the_program(python: Python<'_>, code: &Bound<'_, PyAny>) -> PyResult<bool> {
    match entry().as_ref() {
        // the form has not entered the program yet, so nothing running now can
        // be it
        None => Ok(false),
        Some(Entry::Compiled(program)) => Ok(program.is(code)),
        Some(Entry::MainModule(namespace)) => {
            let Some(file) = namespace.bind(python).get_item("__file__")? else {
                // runpy has not reached `_run_code` yet: everything running is
                // the import machinery on the way to the program
                return Ok(false);
            };
            code.getattr("co_filename")?.eq(file)
        }
    }
}

fn watch_for(gate: Entry) {
    let mut entry = entry();
    assert!(
        entry.is_none(),
        "one launch enters one program, and a second entry gate was armed"
    );
    *entry = Some(gate);
}

/// run a script as `__main__`, as `python <script>` would have
///
/// both spellings of the path are needed, and using either one for both is a
/// parity bug. cpython leaves `sys.argv[0]` exactly as it was typed, and
/// absolutises `__file__` and the code object's filename — so a program that
/// prints its own invocation and a traceback that names a file disagree about
/// the path on purpose
///
/// absolute and not canonical: cpython does not resolve symlinks here, and a
/// debugger that reported the resolved path would disagree with the program's
/// own `__file__`
fn script(python: Python<'_>, as_given: &str, absolute: &Path) -> PyResult<()> {
    let displayed = absolute.display().to_string();

    let sys = PyModule::import(python, "sys")?;
    repair_argv(&sys, as_given)?;
    repair_path(
        &sys,
        &absolute
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .display()
            .to_string(),
    )?;

    let source = match std::fs::read(absolute) {
        Ok(source) => source,
        Err(error) => return Err(unopenable(python, &sys, &displayed, &error)),
    };

    let builtins = PyModule::import(python, "builtins")?;
    let code = builtins
        .getattr("compile")?
        .call1((source.as_slice(), &displayed, "exec"))?;

    let main = install_main(&sys)?;
    // what `_PyRun_SimpleFileObject` puts on `__main__` for a file, on top of
    // what every main module gets: the loader that read it and the path it came
    // from
    let from_source = source_file_loader(python)?;
    main.setattr("__loader__", from_source.call1(("__main__", &displayed))?)?;
    main.setattr("__file__", &displayed)?;
    if namespaces_carry_cached(&sys, &from_source)? {
        // set beside `__file__` in the same branch of the same function, and
        // **present and null** rather than a path — a script is never loaded
        // from a `.pyc`
        main.setattr("__cached__", python.None())?;
    }

    watch_for(Entry::Compiled(code.clone().unbind()));
    builtins
        .getattr("exec")?
        .call1((code, main.getattr("__dict__")?))?;
    Ok(())
}

/// run a module as `__main__`, as `python -m <module>` would have
///
/// this hands the whole of it to `runpy._run_module_as_main`, which is private
/// and is also **exactly what cpython calls**: `pymain_run_module` imports
/// `runpy` and calls that function by name. the two alternatives were both
/// rejected for the same measured reason — a bare `-m` traceback holds runpy's
/// own frames:
///
/// ```text
/// Traceback (most recent call last):
///   File "<frozen runpy>", line 203, in _run_module_as_main
///   File "<frozen runpy>", line 88, in _run_code
///   File "/tmp/boom.py", line 1, in <module>
/// ```
///
/// so resolving the module here with `runpy._get_module_details` and running
/// the code object directly would produce a traceback with **fewer** frames
/// than a bare run, and resolving it once for the origin and then calling
/// `_run_module_as_main` anyway would run a failing package's `__init__` twice
/// and report the failure from the wrong depth. calling the one function is the
/// only arrangement that reproduces a bare `-m` exactly
///
/// it is also why the entry stop cannot be told which file to expect: nothing
/// out here knows it, and asking would be the resolution that must not happen
/// twice. [`Entry::MainModule`] is the answer
fn module(python: Python<'_>, module: &str) -> PyResult<()> {
    let sys = PyModule::import(python, "sys")?;

    // `-m` prepends the **working directory** where a script prepends its own
    // directory, and cpython takes it from `getcwd` — so it is absolute, and
    // asking `os` for it is asking the same source
    let working_directory: String = PyModule::import(python, "os")?
        .getattr("getcwd")?
        .call0()?
        .extract()?;
    repair_path(&sys, &working_directory)?;

    // `sys.argv[0]` is deliberately left alone. `_run_module_as_main` rewrites
    // it to the **resolved file** of the module it ran — not the module name —
    // and doing it here as well would be writing a value bpd would have to
    // resolve for itself
    let main = install_main(&sys)?;
    watch_for(Entry::MainModule(
        main.getattr("__dict__")?.cast_into::<PyDict>()?.unbind(),
    ));

    // `alter_argv` is passed rather than left to its default, because it is the
    // flag that separates `-m` from running a directory or a zip file, and
    // cpython passes it explicitly for the same reason
    PyModule::import(python, "runpy")?
        .getattr("_run_module_as_main")?
        .call1((module, true))?;
    Ok(())
}

/// run source as `__main__`, as `python -c <source>` would have
///
/// the least work of the three, because the bootstrap already entered the
/// interpreter through this very form. `sys.argv[0]` is already `-c` and
/// `sys.path[0]` is already the empty string — which means "the working
/// directory, resolved at import time" and is **not** the same as the working
/// directory spelled out. writing one in place of the other changes what a
/// relative import finds after the program calls `os.chdir`
fn command(python: Python<'_>, source: &str) -> PyResult<()> {
    let sys = PyModule::import(python, "sys")?;
    let builtins = PyModule::import(python, "builtins")?;

    // `<string>` is what cpython compiles a command under, and it is what a
    // `SyntaxError` and every traceback frame of the program will name
    let code = builtins
        .getattr("compile")?
        .call1((source, "<string>", "exec"))?;

    // the source goes where cpython puts a command's source, so a traceback
    // through the program prints the program's own line. `run_mod` in
    // `pythonrun.c` calls exactly this function for exactly this reason, and it
    // takes the code object rather than a filename because it registers every
    // nested code object as well
    PyModule::import(python, "linecache")?
        .getattr("_register_code")?
        .call1((&code, source, "<string>"))?;

    let main = install_main(&sys)?;
    watch_for(Entry::Compiled(code.clone().unbind()));
    builtins
        .getattr("exec")?
        .call1((code, main.getattr("__dict__")?))?;
    Ok(())
}

/// take the bootstrap's own source out of the traceback machinery
///
/// cpython registers the source of a `-c` command with `linecache` so that a
/// traceback can print the line it came from, keyed on
/// `(co_filename, co_qualname, co_firstlineno)`. the bootstrap is a `-c`
/// command, so `("<string>", "<module>", 1)` is registered as
/// `import bpd_agent; bpd_agent.main()` before the agent has run at all
///
/// **that is a wrong line, not a missing one.** any code the program compiles
/// under `<string>` — which is what `compile` and `exec` default to — takes
/// that key, and a traceback through it would print bpd's bootstrap as the
/// program's source with a caret under it. a bare script or `-m` run has no
/// entry there and prints no source line
///
/// it is removed rather than overwritten because for two of the three forms
/// there is nothing to put in its place. the command form registers its own
/// source afterwards, exactly as the interpreter does
fn forget_the_bootstraps_source(python: Python<'_>) -> PyResult<()> {
    let linecache = PyModule::import(python, "linecache")?;
    let bootstrap = frames::bootstrap_code(python)?;
    let key = linecache.getattr("_make_key")?.call1((bootstrap,))?;
    linecache
        .getattr("_interactive_cache")?
        .call_method1("pop", (key, python.None()))?;
    Ok(())
}

/// report an unreadable script exactly as the interpreter would, and exit the
/// way it would
///
/// the wording is not reconstructed: the description comes from `os.strerror`,
/// which is the same source cpython uses. rust's own `io::Error` says "entity
/// not found" where cpython says "No such file or directory", and a debugger
/// that reworded this would send people looking for a different problem
///
/// the prefix is `sys.orig_argv[0]` — the name the interpreter was **invoked
/// by** — because that is what cpython puts there, and it is not
/// `sys.executable`. cpython normalises the second and leaves the first alone,
/// so an interpreter named `/…/bin/../bin/python3.13` refuses a missing script
/// under that name and reports `/…/bin/python3.13` as its executable. the two
/// are far apart on a macos framework build, which is where this was found: the
/// invocation is `…/Resources/Python.app/Contents/MacOS/Python` and
/// `sys.executable` is `…/Versions/3.13/bin/python3.13`
///
/// `an_interpreter_names_itself_the_way_it_was_invoked_and_not_the_way_it_resolves`
/// is the test, and it reaches the same difference without a framework build
///
/// the exit code is 2, which is what cpython uses for a script it cannot open —
/// not the 1 that an uncaught exception produces
fn unopenable(
    python: Python<'_>,
    sys: &Bound<'_, PyModule>,
    displayed: &str,
    error: &std::io::Error,
) -> PyErr {
    let errno = error.raw_os_error().unwrap_or(0);

    let described = || -> PyResult<()> {
        let strerror = PyModule::import(python, "os")?
            .getattr("strerror")?
            .call1((errno,))?;
        // an interpreter always has an argv, and a debuggee bpd launched always
        // has `[<the interpreter>, "-c", <the bootstrap>]`. if that is somehow
        // empty this raises rather than inventing a name for the program
        let invoked = sys.getattr("orig_argv")?.get_item(0)?;
        sys.getattr("stderr")?.call_method1(
            "write",
            (format!(
                "{invoked}: can't open file '{displayed}': [Errno {errno}] {strerror}\n"
            ),),
        )?;
        Ok(())
    };

    if let Err(failed) = described() {
        return failed;
    }
    PySystemExit::new_err(2)
}

/// `sys.argv` as the program expects it: its own path as typed, then its own
/// arguments
///
/// under `-c` the interpreter leaves `-c` in slot zero and the program's
/// arguments after it. the path is **not** absolutised: cpython does not, and a
/// program that reports its own invocation would show a different command line
/// under the debugger than without it
///
/// only the script form needs this. `-c` wants the `-c` that is already there,
/// and `-m` has its slot zero written by runpy
fn repair_argv(sys: &Bound<'_, PyModule>, target: &str) -> PyResult<()> {
    let argv = sys.getattr("argv")?;
    let list = argv.cast::<PyList>()?;
    list.set_item(0, target)?;
    Ok(())
}

/// put back the `sys.path` entry the interpreter would have prepended
///
/// under `-c` this slot is the empty string, which is right for a command and
/// wrong for the other two: a script wants its own directory and a module wants
/// the working directory. a program that got the wrong one imports a different
/// module, or none
///
/// **`PYTHONSAFEPATH` and `-P` turn the whole prepending off**, and then there
/// is no slot of the interpreter's to replace — slot zero is the first real
/// entry, which on a stock build is the stdlib zip. writing over it would give
/// the program a search path a bare run never has, in the direction that finds
/// *more* than it should, and would take a stdlib entry out on the way. measured
/// rather than assumed: with `PYTHONSAFEPATH=1` all three forms leave
/// `sys.path[0]` as that zip
fn repair_path(sys: &Bound<'_, PyModule>, entry: &str) -> PyResult<()> {
    if sys.getattr("flags")?.getattr("safe_path")?.is_truthy()? {
        return Ok(());
    }

    let path = sys.getattr("path")?;
    let list = path.cast::<PyList>()?;
    list.set_item(0, entry)?;
    Ok(())
}

/// the loader class cpython puts on a script's `__main__`
///
/// reached the way `set_main_loader` in `pythonrun.c` reaches it — through the
/// frozen `importlib._bootstrap_external` the interpreter is already running on
/// — rather than through `importlib.machinery`, which is the same class object
/// behind four `sys.modules` entries a bare script run does not have. two of
/// those four are aliases for modules the interpreter has already loaded, so
/// importing them buys the debuggee a fingerprint and nothing else
fn source_file_loader(python: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    PyModule::import(python, "_frozen_importlib")?
        .getattr("_bootstrap_external")?
        .getattr("SourceFileLoader")
}

/// whether this interpreter still puts `__cached__` in a module's namespace
///
/// **cpython 3.15 removed it.** up to 3.14 `_PyRun_SimpleFileObject` set it
/// beside `__file__` for a script and `runpy._run_code` set it for `-m`; 3.15
/// does neither, finishing the deprecation of `module.__cached__`. the module
/// form needs no help — runpy answers for itself either way — but the script
/// form writes that name by hand, and writing it on 3.15 is a name a bare run
/// does not have
///
/// asked of the **running interpreter** rather than of its version number,
/// because a version table is a second place for this to be wrong and this
/// project has no version branches anywhere else. what is read is a module this
/// interpreter has already loaded through the very loader the script form is
/// about to install on `__main__`, which is the same removal seen from the same
/// process
///
/// the loader is the test and a `.py` filename is not: `_frozen_importlib_
/// external` carries a `__file__` ending in `.py` and has never had a
/// `__cached__`, so a filename would answer this wrongly on every version
fn namespaces_carry_cached(
    sys: &Bound<'_, PyModule>,
    from_source: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let modules = sys.getattr("modules")?;
    let modules = modules.cast_into::<PyDict>()?;

    for (_, module) in &modules {
        let Ok(loader) = module.getattr("__loader__") else {
            continue;
        };
        if loader.is_instance(from_source)? {
            return module.hasattr("__cached__");
        }
    }

    unreachable!(
        "the agent imports `linecache` before it enters the program, and no \
         interpreter freezes it — cpython has already loaded it from source to \
         hold the `-c` bootstrap's own line"
    )
}

/// a `__main__` that belongs to the program rather than to the bootstrap
///
/// the module the bootstrap ran in holds `bpd_agent` and nothing else the
/// program should see, so the program gets a fresh one
///
/// what it starts as is **the interpreter's own** `__main__` namespace, copied:
/// every dunder in it and nothing else. that is exactly the shape a bare run
/// starts from, because a bare run starts from the same module — cpython's
/// `add_main_module` builds it once, and then the form adds to it. copying it
/// rather than listing what it holds is the difference between a rule and a
/// list that goes stale: **cpython 3.13 puts an `__annotations__` there and
/// 3.14 does not**, and a launcher that wrote the fields out by hand would be
/// right on one of them
///
/// every form then overwrites what it owns: the script form directly, and `-m`
/// through runpy's `_run_code`. `__spec__` and `__package__` stay as the
/// interpreter left them, which is `None` — what a script and a command get,
/// and what `-m` replaces
fn install_main<'py>(sys: &Bound<'py, PyModule>) -> PyResult<Bound<'py, PyAny>> {
    let modules = sys.getattr("modules")?;
    let interpreters = modules.get_item("__main__")?.getattr("__dict__")?;
    let interpreters = interpreters.cast_into::<PyDict>()?;

    // `types.ModuleType` without `types`: `Lib/types.py` defines that name as
    // `type(sys)` and nothing else, so this is the same class one import
    // earlier — and `types` is two `sys.modules` entries a bare script or `-c`
    // run does not have
    let main = sys.get_type().call1(("__main__",))?;
    let namespace = main.getattr("__dict__")?.cast_into::<PyDict>()?;

    for (name, value) in &interpreters {
        let spelled: std::borrow::Cow<'_, str> = name.extract()?;
        // the bootstrap bound exactly one name of its own — the agent — and it
        // is not a dunder. every other name in here is the interpreter's
        if spelled.starts_with("__") && spelled.ends_with("__") {
            namespace.set_item(&name, &value)?;
        }
    }

    modules.set_item("__main__", &main)?;
    Ok(main)
}

/// report an exception the program did not catch, exactly as the interpreter
/// would, and turn it into the exit it would have caused
///
/// the traceback is captured before it unwinds through the bootstrap, so it
/// holds the program's frames and nothing of `bpd`'s. under `-m` it holds
/// runpy's two frames as well, because a bare `-m` traceback holds them too —
/// `runpy` is called from C there and from rust here, and neither pushes a
/// frame of its own
///
/// the printing is cpython's own, not a reconstruction. taking the exception
/// apart and handing the pieces to `sys.excepthook` loses information a
/// `SyntaxError` carries — the line, the source text and the caret — and
/// produces a report subtly unlike the one the program would have got. it also
/// respects a hook the program installed, because that is what the interpreter
/// does
pub(crate) fn report_uncaught(python: Python<'_>, error: PyErr) -> PyErr {
    if error.is_instance_of::<PySystemExit>(python) {
        return error;
    }

    error.print(python);
    PySystemExit::new_err(1)
}
