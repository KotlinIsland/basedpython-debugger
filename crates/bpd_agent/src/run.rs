//! entering the user's program the way the interpreter would have
//!
//! the agent is reached through `python -c`, which is not the launch form the
//! user asked for. everything here exists to undo that: the `-c` marker in
//! `sys.argv`, the empty `sys.path[0]`, and a `__main__` module that belongs to
//! the bootstrap rather than to the program
//!
//! none of it is guessed. `crates/bpd_test/tests/launch_forms.rs` records what a
//! bare interpreter produces, and `crates/bpd/tests/launch_parity.rs` compares
//! this against it

use std::path::Path;

use pyo3::exceptions::PySystemExit;
use pyo3::prelude::*;
use pyo3::types::{PyList, PyModule};

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
pub(crate) fn script(python: Python<'_>, as_given: &str, absolute: &Path) -> PyResult<()> {
    let displayed = absolute.display().to_string();

    let sys = PyModule::import(python, "sys")?;
    repair_argv(&sys, as_given)?;
    repair_path(&sys, absolute)?;

    let source = match std::fs::read(absolute) {
        Ok(source) => source,
        Err(error) => return Err(unopenable(python, &sys, &displayed, &error)),
    };

    let builtins = PyModule::import(python, "builtins")?;
    let code = builtins
        .getattr("compile")?
        .call1((source.as_slice(), &displayed, "exec"))?;

    let main = install_main(python, &sys, &builtins, &displayed)?;
    builtins
        .getattr("exec")?
        .call1((code, main.getattr("__dict__")?))?;
    Ok(())
}

/// report an unreadable script exactly as the interpreter would, and exit the
/// way it would
///
/// the wording is not reconstructed: the prefix is `sys.executable` and the
/// description comes from `os.strerror`, which is the same source cpython uses.
/// rust's own `io::Error` says "entity not found" where cpython says "No such
/// file or directory", and a debugger that reworded this would send people
/// looking for a different problem
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
        let executable = sys.getattr("executable")?;
        sys.getattr("stderr")?.call_method1(
            "write",
            (format!(
                "{executable}: can't open file '{displayed}': [Errno {errno}] {strerror}\n"
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
fn repair_argv(sys: &Bound<'_, PyModule>, target: &str) -> PyResult<()> {
    let argv = sys.getattr("argv")?;
    let list = argv.cast::<PyList>()?;
    list.set_item(0, target)?;
    Ok(())
}

/// `sys.path[0]` as the program expects it: the directory the script lives in
///
/// under `-c` this slot is the empty string, meaning the working directory. a
/// script that imports a sibling module would find the wrong one, or nothing
fn repair_path(sys: &Bound<'_, PyModule>, target: &Path) -> PyResult<()> {
    let directory = target
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .display()
        .to_string();

    let path = sys.getattr("path")?;
    let list = path.cast::<PyList>()?;
    list.set_item(0, directory)?;
    Ok(())
}

/// a `__main__` that belongs to the program rather than to the bootstrap
///
/// the module the bootstrap ran in holds `bpd_agent` and nothing else the
/// program should see, so the program gets a fresh one. `__spec__` and
/// `__package__` stay `None`, which is what a script gets and what `-m` does not
fn install_main<'py>(
    python: Python<'py>,
    sys: &Bound<'py, PyModule>,
    builtins: &Bound<'py, PyModule>,
    target: &str,
) -> PyResult<Bound<'py, PyAny>> {
    let main = PyModule::import(python, "types")?
        .getattr("ModuleType")?
        .call1(("__main__",))?;
    main.setattr("__file__", target)?;
    main.setattr("__builtins__", builtins)?;

    let loader = PyModule::import(python, "importlib.machinery")?
        .getattr("SourceFileLoader")?
        .call1(("__main__", target))?;
    main.setattr("__loader__", loader)?;

    sys.getattr("modules")?.set_item("__main__", &main)?;
    Ok(main)
}

/// report an exception the program did not catch, exactly as the interpreter
/// would, and turn it into the exit it would have caused
///
/// the traceback is captured before it unwinds through the bootstrap, so it
/// holds the program's frames and nothing of `bpd`'s
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
