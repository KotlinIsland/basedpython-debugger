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
//! ## which events, measured rather than assumed — **and they differ by release**
//!
//! recorded by a hook that kept every event of every name, on 3.13.15, 3.14.7
//! and 3.15, on posix:
//!
//! | what the program does                   | 3.13                  | 3.14 and 3.15                            |
//! | --------------------------------------- | --------------------- | ---------------------------------------- |
//! | `subprocess.run([...])`                 | `subprocess.Popen`    | `subprocess.Popen`, `_posixsubprocess.fork_exec` |
//! | `subprocess.run(..., close_fds=False)`  | `subprocess.Popen`, `os.posix_spawn` | `subprocess.Popen`, `os.posix_spawn` |
//! | `multiprocessing`, `spawn`              | **nothing at all**    | `_posixsubprocess.fork_exec`             |
//! | `multiprocessing`, `forkserver`         | **nothing at all**    | `_posixsubprocess.fork_exec`, `os.fork`  |
//! | `multiprocessing`, `fork`               | `os.fork`             | `os.fork`                                |
//! | `os.fork()`                             | `os.fork`             | `os.fork`                                |
//! | `os.execv`                              | `os.exec`             | `os.exec`                                |
//! | `os.posix_spawn`                        | `os.posix_spawn`      | `os.posix_spawn`                         |
//! | `os.spawnv`                             | `os.fork`, then `os.exec` in the child | the same                |
//!
//! **`_posixsubprocess.fork_exec` only became an audit event in 3.14.** that is
//! the whole reason there are two lists below, and it is the same kind of
//! release-to-release change as 3.14 splitting `BRANCH` into `BRANCH_LEFT` and
//! `BRANCH_RIGHT` — the capability is the same, and the name the interpreter
//! raises it under is not. it is not a capability ladder: what `bpd` reports is
//! identical on both, except for the one thing 3.13 genuinely cannot say
//!
//! three things follow, and none of them is obvious:
//!
//! - **on 3.14 and later, `subprocess.Popen` is not watched.** it fires for the
//!   same child as `_posixsubprocess.fork_exec`, so watching both reports every
//!   ordinary subprocess twice
//! - **on 3.13 it has to be**, because it is the only event a `subprocess` child
//!   raises there. that reopens the double against `os.posix_spawn`, which fires
//!   beside it whenever `close_fds=False` lets `subprocess` take that path — so
//!   the pair is deduplicated, by `AFTER_SUBPROCESS_POPEN`, and
//!   `a_child_started_the_posix_spawn_way_is_reported_once` is what proves it
//! - **`multiprocessing`'s `spawn` and `forkserver` cannot be seen at all on
//!   3.13.** it reaches a child through `multiprocessing.util.spawnv_passfds`,
//!   which calls `_posixsubprocess.fork_exec` — silent there. this is a blind
//!   spot on a supported interpreter, and the one thing this feature must never
//!   do is be quietly silent, so it is **announced**: see [`announce_blindspot`]
//!
//! ## why only the process that attached reports
//!
//! a fork inherits the hook, and it inherits both descriptors of the control
//! connection. two processes writing length-prefixed frames into one socket
//! desynchronise it, and the engine reports that as a message it does not
//! understand — the debugger blaming its own protocol for the program having
//! forked
//!
//! so the pid is recorded at attach and compared here. a forked child stays
//! silent, and the fork that made it is reported by the parent
//!
//! [`crate::forks`] is the other half of the same rule, and the stronger one: a
//! forked child gives the session up before it runs a line, so nothing in it
//! could write a frame even if this comparison were removed. the comparison
//! stays because it is the cheaper statement of the same thing and because it
//! is what decides *which* process reports, which is a question the detach does
//! not answer
//!
//! the consequence is a stated limit rather than a silence: an `os.exec`
//! inside a forked child is not reported, and the `os.fork` that made it is

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use bpd_core::{Blindspot, Spawn, Verdict};
use bpd_protocol::message::FromAgent;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};

use crate::attach;
use crate::cells::ForkCell;

/// the audit events that make a process, on 3.14 and later
///
/// `subprocess.Popen` is deliberately absent: the event beneath it fires for
/// the same child, so watching both reports every ordinary subprocess twice.
/// `os.system` is absent for a different reason — it hands a whole command line
/// to a shell, and what a shell does with one is not knowable from the vector
#[cfg(not(windows))]
const MAKING_A_PROCESS: &[&CStr] = &[
    c"_posixsubprocess.fork_exec",
    c"os.posix_spawn",
    c"os.exec",
    c"os.fork",
];

/// the same on 3.13, where `_posixsubprocess.fork_exec` raises nothing
///
/// `subprocess.Popen` takes its place, because there it is the only event a
/// `subprocess` child raises at all. `import` is watched for one reason and one
/// only — [`announce_blindspot`], which needs to know when `multiprocessing`
/// arrives
#[cfg(not(windows))]
const MAKING_A_PROCESS_BEFORE_314: &[&CStr] = &[
    c"subprocess.Popen",
    c"os.posix_spawn",
    c"os.exec",
    c"os.fork",
    c"import",
];

/// the events that make a process on windows, on every supported release
///
/// the list is per platform because the events are: nothing on windows raises
/// `_posixsubprocess.fork_exec`, and nothing on posix raises
/// `_winapi.CreateProcess`. there is no `os.fork` here and there cannot be one,
/// and `subprocess.Popen` is absent for the reason it is on posix — windows
/// raises it beside `_winapi.CreateProcess` for the same child
///
/// it does not change with the release, because `_winapi.CreateProcess` has
/// been an audit event since PEP 578 landed in 3.8 — long before this project's
/// minimum. `multiprocessing`'s spawn method goes through it here, so the 3.13
/// blind spot below is a posix one
#[cfg(windows)]
const MAKING_A_PROCESS: &[&CStr] = &[c"_winapi.CreateProcess", c"os.exec"];

/// the same on 3.13, which on windows is the same list
#[cfg(windows)]
const MAKING_A_PROCESS_BEFORE_314: &[&CStr] = MAKING_A_PROCESS;

/// the events this interpreter is watched for, chosen once at attach
static WATCHED: OnceLock<&'static [&'static CStr]> = OnceLock::new();

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

/// the blind spot this interpreter has, whether or not it has been said
///
/// `None` on an interpreter that has none. it is what [`BLINDSPOT`] holds when
/// it is fresh, which is at attach and again in a forked child that opened a
/// session of its own — the blind spot is a property of the interpreter, and a
/// second session on the same interpreter has it too
static OF_THIS_INTERPRETER: OnceLock<Option<Blindspot>> = OnceLock::new();

/// the blind spot still to be announced on this session
///
/// taken out when it is said, so it is said once per session — a program that
/// imports `multiprocessing` in fifty modules has one blind spot, not fifty
///
/// in a [`ForkCell`] for the reason the session's other state is: a forked child
/// that opens a session of its own runs this hook again, and a mutex a thread
/// the fork did not keep was holding would never be released. the child replaces
/// the cell rather than taking it, and what it finds is
/// [`OF_THIS_INTERPRETER`] — the same interpreter, a session that has not been
/// told
static BLINDSPOT: ForkCell<Option<Blindspot>> = ForkCell::new(not_yet_said);

/// what the blind spot is before anything has said it, in any session
fn not_yet_said() -> Option<Blindspot> {
    OF_THIS_INTERPRETER.get().cloned().flatten()
}

thread_local! {
    /// whether the last watched event on this thread was `subprocess.Popen`
    ///
    /// this is the deduplication, and it exists only for 3.13. there,
    /// `subprocess.Popen` is watched because it is the only event an ordinary
    /// `subprocess` child raises — but `subprocess` takes a `posix_spawn` path
    /// when `close_fds=False` lets it, and then `os.posix_spawn` fires **as
    /// well**, for the same child
    ///
    /// `subprocess.py` raises its event and then calls one of the two, on the
    /// same thread, with nothing else watched in between. so "the previous
    /// watched event on this thread" identifies the pair exactly, and no
    /// counting or matching of arguments is needed. per thread rather than
    /// global, because two threads each starting a child would otherwise
    /// suppress each other's
    ///
    /// on 3.14 and later `subprocess.Popen` is not watched at all, so this is
    /// never set and the whole path is inert
    static AFTER_SUBPROCESS_POPEN: Cell<bool> = const { Cell::new(false) };
}

/// cpython's C audit hook signature
///
/// a non-zero return makes the audited operation **fail**. this one always
/// returns success
///
/// the return value is not the only influence a hook has, and it is worth
/// writing down that it was checked rather than assumed. the argument tuple is
/// immutable but its *contents* are not, and `subprocess.Popen` raises the
/// event with the argument list it then goes on to use — measured on 3.13, 3.14
/// and 3.15, appending to that list puts the extra argument in the child, and
/// the caller's own list is untouched because what is audited is already a copy
///
/// so a hook could rewrite a spawn invisibly, and this one must not. it is not
/// a channel for propagating a session into a child either: it works only
/// because of where cpython happens to raise the event relative to where it
/// reads the list, which is an implementation detail no document promises. a
/// debugger that changed a program's child process on the strength of that
/// would be guessing, in the one place a wrong guess is unrecoverable
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
    let sys = PyModule::import(python, "sys")?;
    let executable: String = sys.getattr("executable")?.extract()?;
    INTERPRETER
        .set(resolve(&executable))
        .unwrap_or_else(|_| unreachable!("the agent installs the audit hook once"));
    ATTACHED.store(std::process::id(), Ordering::Relaxed);

    let version = sys.getattr("version_info")?;
    let major: u8 = version.getattr("major")?.extract()?;
    let minor: u8 = version.getattr("minor")?.extract()?;
    watch_what_this_interpreter_raises(major, minor);

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

/// choose the watch list, and the blind spot that comes with it
///
/// the split is at 3.14, where `_posixsubprocess.fork_exec` became an audit
/// event. below it that event is silent, so `subprocess.Popen` is watched in
/// its place — which covers `subprocess` and does **not** cover
/// `multiprocessing`, because `multiprocessing` never goes near `subprocess`
///
/// the blind spot that leaves is recorded here rather than discovered later, so
/// that the thing which says it cannot see a child is set up at the same moment
/// as the thing that sees them
fn watch_what_this_interpreter_raises(major: u8, minor: u8) {
    let before_314 = (major, minor) < (3, 14);

    WATCHED
        .set(if before_314 {
            MAKING_A_PROCESS_BEFORE_314
        } else {
            MAKING_A_PROCESS
        })
        .unwrap_or_else(|_| unreachable!("the agent installs the audit hook once"));

    // windows reaches a `multiprocessing` spawn child through
    // `_winapi.CreateProcess`, which has been an audit event since 3.8 — so the
    // blind spot is a posix one and claiming it anywhere else would be bpd
    // reporting a limit it does not have
    let here = if cfg!(windows) || !before_314 {
        None
    } else {
        Some(Blindspot::MultiprocessingSpawn {
            interpreter: format!("{major}.{minor}"),
        })
    };
    OF_THIS_INTERPRETER
        .set(here.clone())
        .unwrap_or_else(|_| unreachable!("the agent installs the audit hook once"));
    *lock() = here;
}

/// this process is the one that reports its children now
///
/// the hook is inherited by a forked child and compares the pid against the one
/// recorded at attach, which is what keeps a child that gave the session up off
/// the parent's socket. a child that opened a session of **its own** is a
/// debuggee, and its children are its session's to report — so it records itself
///
/// an atomic store and nothing else, because it is called from a fork handler
#[cfg(unix)]
pub(crate) fn now_this_process() {
    ATTACHED.store(std::process::id(), Ordering::Relaxed);
    // the blind spot is the interpreter's, so this session has it too and has
    // not been told about it. the cell is replaced rather than emptied for the
    // reason every other one a fork handler touches is — see [`BLINDSPOT`]
    BLINDSPOT.abandon();
}

/// the blind spot still to be announced
fn lock() -> std::sync::MutexGuard<'static, Option<Blindspot>> {
    BLINDSPOT
        .get()
        .lock()
        .expect("nothing panics holding the blind spot: every path through it is a take or a set")
}

/// say, once, that this interpreter hides a whole way of starting a child
///
/// the trigger is `multiprocessing` being imported, and that is the whole point
/// of it. announcing at attach would put a warning on every 3.13 launch of every
/// program, most of which never start a child at all — and a warning everybody
/// learns to skip is one nobody reads when it matters. announcing when the
/// module arrives is the first moment at which such a child becomes possible
///
/// it is deliberately **not** waited for and deliberately not a refusal. the
/// interpreter is supported and the feature works on it, apart from this; what
/// would be wrong is letting the silence read as "no child was started"
fn announce_blindspot(module: &str) {
    if module != "multiprocessing" && !module.starts_with("multiprocessing.") {
        return;
    }
    let Some(blindspot) = lock().take() else {
        return;
    };
    attach::send(&FromAgent::BlindTo { blindspot });
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
    let watched = WATCHED
        .get()
        .unwrap_or_else(|| unreachable!("the hook is not installed before `install` ran"));
    if !watched.contains(&name) {
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
        let name = name.to_string_lossy();

        // `import` is watched only on the interpreters that have the blind
        // spot, and only ever means this. it is not a child and must not be
        // read as one
        if name == "import" {
            let module = arguments
                .as_ref()
                .and_then(|arguments| arguments.get_item(0).ok())
                .and_then(|module| text(&module))
                .unwrap_or_default();
            announce_blindspot(&module);
            return;
        }

        // the `subprocess.Popen` / `os.posix_spawn` pair, which is one child
        let after_subprocess = AFTER_SUBPROCESS_POPEN.replace(name == "subprocess.Popen");
        if after_subprocess && name == "os.posix_spawn" {
            return;
        }

        if let Some(child) = describe(&name, arguments.as_ref()) {
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
