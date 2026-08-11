//! noticing that the debuggee started a child process
//!
//! `bpd` debugs one process. a program that starts another one has moved the
//! work somewhere the debugger is not, and django's `runserver` is the case
//! that makes it matter: the reloader starts a child and then does nothing but
//! wait on its exit code, so the process holding the agent never renders a
//! template. this module is what stands between that and somebody staring at a
//! breakpoint that never fires
//!
//! it **reports**. the child is not blocked, not rewritten and not debugged,
//! and it runs exactly as it would have without any of this
//!
//! ## why an audit hook, and why a native one
//!
//! `sys.addaudithook` and `PySys_AddAuditHook` are the interfaces cpython
//! documents for seeing a process being created. the alternative is what pydevd
//! does — monkeypatching `os.exec*` and `subprocess` so it can see and rewrite
//! the child's command line — and
//! [python support](../../../docs/development/python-support.md) rules that out
//! here
//!
//! the hook is added through the **C** interface rather than through
//! `sys.addaudithook`, because a hook is called for every audit event the
//! process raises — `open`, `import`, `exec`, `compile`, `marshal.loads` — and
//! a python callable there would be a python frame per file the program opens.
//! that is the "no python-level callbacks on hot events" rule, and an audit hook
//! is hotter than it looks: importing `multiprocessing` alone raises over two
//! hundred of them
//!
//! ## which events, measured rather than assumed
//!
//! against 3.13, 3.14 and 3.15, watching everything:
//!
//! | what the program does             | what it raises                          |
//! | --------------------------------- | --------------------------------------- |
//! | `subprocess.run([...])`           | `subprocess.Popen`, **and** `_posixsubprocess.fork_exec` |
//! | `multiprocessing`, `spawn`        | `_posixsubprocess.fork_exec` **only**   |
//! | `multiprocessing`, `fork`         | `os.fork`                               |
//! | `os.execv`                        | `os.exec`                               |
//! | `os.posix_spawn`                  | `os.posix_spawn`                        |
//! | `os.spawnv`                       | `os.fork`, then `os.exec` in the child  |
//!
//! two things follow, and neither is obvious:
//!
//! - **`subprocess.Popen` is not watched.** it fires for the same child as
//!   `_posixsubprocess.fork_exec` does, so watching both reports every ordinary
//!   subprocess twice
//! - **`_posixsubprocess.fork_exec` cannot be left out.** `multiprocessing` with
//!   the `spawn` start method goes through `multiprocessing.util.spawnv_passfds`
//!   and raises *nothing else at all* — no `subprocess.Popen`, no `os.*`. a
//!   watch list of the `subprocess` and `os` events would miss every one of them
//!
//! what is left is the set below: the events that actually make a process. each
//! child raises exactly one of them
//!
//! ## why only the process that attached reports
//!
//! a fork inherits the hook, and it inherits the control connection's file
//! descriptor. two processes writing length-prefixed frames into one socket
//! desynchronise it, and the engine reports that as a message it does not
//! understand — the debugger blaming its own protocol for the program having
//! forked
//!
//! so the pid is recorded at attach and compared here. a forked child stays
//! silent, and the fork that made it is reported by the parent. that is a
//! stated limit rather than a silence: an `os.exec` inside a forked child is
//! not reported, and `scratch.subprocess.md` is where closing it is designed

use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use bpd_core::{Spawn, Verdict};
use bpd_protocol::message::FromAgent;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};

use crate::attach;

/// the audit events that make a process on this platform, and nothing else
///
/// the list is per platform because the events are: nothing on windows raises
/// `_posixsubprocess.fork_exec`, and nothing on posix raises
/// `_winapi.CreateProcess`. watching both everywhere would be watching for
/// something that cannot happen, and — worse — on windows `subprocess.Popen`
/// and `_winapi.CreateProcess` fire for the same child, so a single list that
/// covered both platforms would report every windows subprocess twice
///
/// `subprocess.Popen` is deliberately absent from both, because on either
/// platform the event beneath it fires for the same child. `os.system` is
/// absent for a different reason: it hands a whole command line to a shell, and
/// what a shell does with one is not knowable from the vector
#[cfg(not(windows))]
const WATCHED: [&CStr; 4] = [
    c"_posixsubprocess.fork_exec",
    c"os.posix_spawn",
    c"os.exec",
    c"os.fork",
];

/// the same on windows, where a process is made by `CreateProcess`
///
/// there is no `os.fork` here and there cannot be one
#[cfg(windows)]
const WATCHED: [&CStr; 2] = [c"_winapi.CreateProcess", c"os.exec"];

/// the process the agent attached to
///
/// `0` before `install`, which no real process id is on any platform bpd runs on
static ATTACHED: AtomicU32 = AtomicU32::new(0);

/// the debuggee's own interpreter, canonicalised, as the strongest evidence a
/// child is python
///
/// read once at attach rather than per event. it is `sys.executable` as the
/// running interpreter reports it, resolved through links, so a child named by
/// a different path to the same file is still recognised
static INTERPRETER: OnceLock<Option<String>> = OnceLock::new();

/// cpython's C audit hook signature
///
/// a non-zero return makes the audited operation **fail**, which is the only
/// influence a hook has over what the program does — there is no way to rewrite
/// the arguments, and this one always returns success
type AuditHook = unsafe extern "C" fn(
    event: *const c_char,
    args: *mut pyo3::ffi::PyObject,
    user: *mut c_void,
) -> c_int;

// pyo3-ffi does not declare this one. it is exported by every cpython build and
// resolves the way every other interpreter symbol an extension module uses does
#[expect(
    unsafe_code,
    reason = "the only interface cpython offers for a native audit hook is a C \
              one, and a python callable in its place would be a python frame \
              per audit event — which is every file the program opens"
)]
unsafe extern "C" {
    fn PySys_AddAuditHook(hook: AuditHook, user: *mut c_void) -> c_int;
}

/// start watching for child processes
///
/// called once, from the agent's entry point, after the control connection is
/// up — the hook reports through it and there would be nowhere to send to
/// before that
pub(crate) fn install(python: Python<'_>) -> PyResult<()> {
    let executable: String = PyModule::import(python, "sys")?
        .getattr("executable")?
        .extract()?;
    INTERPRETER
        .set(resolve(&executable))
        .unwrap_or_else(|_| unreachable!("the agent installs the audit hook once"));
    ATTACHED.store(std::process::id(), Ordering::Relaxed);

    // SAFETY: called with the GIL held on an initialised interpreter, which is
    // what this expects. `saw` is a `'static` function and the user pointer is
    // never dereferenced
    #[expect(unsafe_code, reason = "see the extern block above")]
    let added = unsafe { PySys_AddAuditHook(saw, std::ptr::null_mut()) };
    if added != 0 {
        return Err(pyo3::exceptions::PyRuntimeError::new_err(
            "cpython refused the audit hook bpd watches child processes with. \
             without it bpd cannot tell that the program started another \
             process, and a session pointed at a supervisor would look like a \
             breakpoint that never binds",
        ));
    }
    Ok(())
}

/// a path with its links resolved, or `None` when there is nothing behind it
fn resolve(path: &str) -> Option<String> {
    std::fs::canonicalize(path)
        .ok()
        .map(|resolved| resolved.display().to_string())
}

/// the interpreter calls this for **every** audit event the process raises
///
/// so the first thing it does is a comparison against a five-element list of
/// static strings, and the overwhelmingly common answer is that this event is
/// not one of them
///
/// a panic here would cross an `extern "C"` boundary, which rust turns into an
/// abort rather than an unwind. that is the right outcome for a broken
/// invariant inside the debugger and the wrong one for anything the program
/// did, so nothing below panics on a shape the program can choose
#[expect(unsafe_code, reason = "see the extern block above")]
unsafe extern "C" fn saw(
    event: *const c_char,
    args: *mut pyo3::ffi::PyObject,
    _: *mut c_void,
) -> c_int {
    // SAFETY: cpython passes a nul-terminated static string for every event
    let name = unsafe { CStr::from_ptr(event) };
    if !WATCHED.contains(&name) {
        return 0;
    }

    // a forked child inherits this hook and the control connection's fd. it
    // does not report — see the module documentation
    if std::process::id() != ATTACHED.load(Ordering::Relaxed) {
        return 0;
    }

    // SAFETY: an audit hook is called with the GIL held, and `args` is a
    // borrowed reference to the event's argument tuple that outlives this call
    Python::attach(|python| {
        let arguments = unsafe { Bound::from_borrowed_ptr_or_opt(python, args) };
        if let Some(child) = describe(&name.to_string_lossy(), arguments.as_ref()) {
            attach::send(&FromAgent::Spawned { child });
        }
    });
    0
}

/// what to say about one audit event, or nothing when the child is not python
///
/// each event puts the program and the argument vector in its own shape, and
/// they are read here rather than assumed to be one shape:
///
/// - `os.fork` carries nothing, because the child is this process
/// - `os.exec` and `os.posix_spawn` carry `(path, argv, env)`
/// - `_posixsubprocess.fork_exec` carries `(candidates, argv, env)`, where the
///   candidates are every path the `PATH` search will try
/// - `_winapi.CreateProcess` carries `(application_name, command_line, cwd)`,
///   where the application name is usually absent and the command line is one
///   string
fn describe(event: &str, arguments: Option<&Bound<'_, PyAny>>) -> Option<Spawn> {
    if event == "os.fork" {
        return Some(Spawn {
            event: event.to_string(),
            executable: None,
            arguments: Vec::new(),
            verdict: Verdict::ThisProcess,
        });
    }

    let arguments = arguments?;
    let first = words(arguments.get_item(0).ok().as_ref());
    let second = words(arguments.get_item(1).ok().as_ref());

    let (candidates, vector) = if event == "_winapi.CreateProcess" {
        // one command line rather than a vector. the program is the application
        // name when the caller gave one, and otherwise the command line's own
        // first word — read by the rule `CommandLineToArgvW` applies to the
        // zeroth argument, which is not the rule it applies to the rest
        let line = second.first().cloned().unwrap_or_default();
        let program = first.first().cloned().or_else(|| program_of(&line));
        (program.into_iter().collect(), second)
    } else {
        (first, second)
    };

    Some(Spawn {
        event: event.to_string(),
        executable: candidates.first().cloned(),
        verdict: verdict(&candidates, &vector)?,
        arguments: vector,
    })
}

/// the program a windows command line names, by the rule that applies to it
///
/// `CommandLineToArgvW` treats the zeroth argument differently from every other
/// one: a line that opens with a quote runs to the next quote, and one that does
/// not runs to the first whitespace. no escape processing happens in either
/// case, which is why this is the rule and not an approximation of it
fn program_of(line: &str) -> Option<String> {
    let program = match line.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        None => line.split_whitespace().next()?,
    };
    (!program.is_empty()).then(|| program.to_string())
}

/// every string in an audit argument, whether it is one or a sequence of them
///
/// cpython hands these over as `str` or as `bytes` depending on how the program
/// spelled them, and `_posixsubprocess.fork_exec` mixes the two in one list.
/// anything that is neither is left out rather than rendered through `repr`,
/// which would put python syntax into a report about a command line
fn words(argument: Option<&Bound<'_, PyAny>>) -> Vec<String> {
    let Some(argument) = argument else {
        return Vec::new();
    };
    if let Some(text) = text(argument) {
        return vec![text];
    }
    argument
        .try_iter()
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|item| text(&item))
        .collect()
}

/// one `str` or `bytes` as text, or nothing for anything else
///
/// a path that is not utf-8 is a path all the same, so bytes are decoded
/// lossily rather than dropped — a report naming a child with one character
/// replaced is worth more than no report
fn text(value: &Bound<'_, PyAny>) -> Option<String> {
    if let Ok(string) = value.cast::<PyString>() {
        return string.extract().ok();
    }
    let bytes = value.cast::<PyBytes>().ok()?;
    Some(String::from_utf8_lossy(bytes.as_bytes()).into_owned())
}

/// what can be told about a child from the program it will run and its vector
///
/// the rule, in order, and there is no fourth case:
///
/// 1. one of the candidate programs **is** the interpreter this process is
///    running, compared as a resolved path. that is as certain as an argument
///    vector gets
/// 2. a candidate program's *file name* is a python interpreter's name. a name
///    is evidence and not proof — a file called `python` can be a wrapper —
///    so it is reported as the name it was read from
/// 3. an argument after the first is a python interpreter's name. this is
///    `env python3 …` and `uv run python …`, where the child's program is a
///    launcher and what it will run is the launcher's business
/// 4. none of those, and nothing is said at all — a child running `ls` is not
///    something anybody needs told about
///
/// what case three deliberately does **not** do is look inside an argument. a
/// command handed to a shell — `sh -c "python app.py"` — is one string, and
/// splitting it to hunt for a word would report a python child for
/// `git commit -m "port to python 3"`
fn verdict(candidates: &[String], vector: &[String]) -> Option<Verdict> {
    let interpreter = INTERPRETER
        .get()
        .unwrap_or_else(|| unreachable!("the hook is not installed before `install` ran"));

    if let Some(interpreter) = interpreter
        && candidates
            .iter()
            .any(|candidate| resolve(candidate).as_ref() == Some(interpreter))
    {
        return Some(Verdict::ThisInterpreter);
    }

    if let Some(named) = candidates.iter().find_map(|candidate| named(candidate)) {
        return Some(Verdict::AnotherInterpreter { named });
    }

    vector
        .iter()
        .skip(1)
        .find_map(|argument| named(argument))
        .map(|named| Verdict::Perhaps { named })
}

/// the file name of `path`, when it is a python interpreter's name
///
/// the rule is `python`, followed by nothing or by a version — so `python`,
/// `python3`, `python3.14`, `python3.14t`, `pythonw` and any of them with a
/// windows `.exe` on the end. `python-config` and `pythonpath.txt` are not
/// interpreters and do not match
fn named(path: &str) -> Option<String> {
    let name = Path::new(path).file_name()?.to_str()?;
    let stem = name.strip_suffix(".exe").unwrap_or(name);
    let rest = stem.strip_prefix("python")?;

    rest.chars()
        .all(|character| character.is_ascii_digit() || character == '.')
        .then(|| name.to_string())
        .or_else(|| {
            // `pythonw` and `python3.14t` are the two suffixed spellings cpython
            // itself ships, and neither is a different program
            let trimmed = rest.strip_suffix('t').or_else(|| rest.strip_suffix('w'))?;
            trimmed
                .chars()
                .all(|character| character.is_ascii_digit() || character == '.')
                .then(|| name.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interpreters_name_is_recognised_and_a_program_that_merely_starts_with_one_is_not() {
        for interpreter in [
            "python",
            "python3",
            "python3.14",
            "/usr/bin/python3.13",
            "python3.14t",
            "pythonw",
            "python.exe",
        ] {
            assert!(
                named(interpreter).is_some(),
                "`{interpreter}` is a python interpreter's name"
            );
        }

        // a directory is stripped by `Path::file_name`, which means the
        // platform's own separator and only that one. a windows path is
        // therefore only a path on windows, and asserting on one anywhere else
        // would be asserting that `std` does something it does not
        #[cfg(windows)]
        assert!(named("C:\\Python314\\python.exe").is_some());

        for other in [
            "python-config",
            "pythonic",
            "/usr/bin/ls",
            "git",
            "pythonpath.txt",
            "",
        ] {
            assert!(
                named(other).is_none(),
                "`{other}` is not a python interpreter's name, and reporting a \
                 python child for one is how a report becomes noise"
            );
        }
    }

    #[test]
    fn a_launcher_that_names_an_interpreter_is_reported_as_uncertain() {
        INTERPRETER.get_or_init(|| None);

        let vector = ["/usr/bin/env", "python3", "app.py"].map(ToString::to_string);
        let candidates = ["/usr/bin/env".to_string()];

        // the child's program is `env`, so nothing about the vector says the
        // child will run python — only that a word of it is an interpreter
        assert_eq!(
            verdict(&candidates, &vector),
            Some(Verdict::Perhaps {
                named: "python3".to_string()
            })
        );
    }

    #[test]
    fn a_command_that_merely_mentions_python_is_not_a_python_child() {
        INTERPRETER.get_or_init(|| None);

        let vector = ["git", "commit", "-m", "port to python 3"].map(ToString::to_string);
        assert_eq!(verdict(&["git".to_string()], &vector), None);

        let shell = ["/bin/sh", "-c", "python app.py"].map(ToString::to_string);
        assert_eq!(
            verdict(&["/bin/sh".to_string()], &shell),
            None,
            "a command handed to a shell is one string, and splitting it to \
             hunt for a word is how `git commit -m 'port to python'` becomes a \
             python child"
        );
    }

    #[test]
    fn a_windows_command_line_names_its_program_by_the_rule_that_applies_to_it() {
        // the two shapes `CommandLineToArgvW` gives the zeroth argument. the
        // quoted one is why splitting on whitespace is not the rule: it would
        // make `C:\Program` the program
        assert_eq!(
            program_of("\"C:\\Program Files\\Python\\python.exe\" app.py").as_deref(),
            Some("C:\\Program Files\\Python\\python.exe")
        );
        assert_eq!(
            program_of("python.exe -c pass").as_deref(),
            Some("python.exe")
        );
        assert_eq!(program_of("").as_deref(), None);
    }

    #[test]
    fn a_child_whose_program_is_a_python_name_is_reported_as_a_different_interpreter() {
        INTERPRETER.get_or_init(|| None);

        let vector = ["/opt/other/python3.13", "worker.py"].map(ToString::to_string);
        assert_eq!(
            verdict(&["/opt/other/python3.13".to_string()], &vector),
            Some(Verdict::AnotherInterpreter {
                named: "python3.13".to_string()
            })
        );
    }
}
