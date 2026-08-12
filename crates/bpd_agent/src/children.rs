//! how a child that was **`exec`'d** finds the session, and what it does when
//! it has
//!
//! a fork inherits memory, so a forked child is born holding the endpoint and
//! the token — [`crate::forks`] is the whole of what it needs. an `exec` is the
//! opposite: the child is a fresh interpreter with none of this process's
//! memory in it, and the only things it inherits are the **environment** and the
//! file descriptors. so the agent has to be *found*, and the only channels into
//! a `python …` command line bpd did not write are the environment and the files
//! the interpreter reads at startup
//!
//! it is `PYTHONPATH` plus a `sitecustomize`, and there is no second candidate.
//! what the alternatives cost is measured in
//! [child processes](../../../docs/development/subprocesses.md); the short of it
//! is that `PYTHONSTARTUP` does not run for `-c`, `-m` or a script, that an
//! audit hook can rewrite a child's arguments only through an undocumented
//! detail of where cpython raises the event, and that monkeypatching
//! `subprocess` — which is what debugpy does — is ruled out by the python
//! support policy
//!
//! ## what that costs, stated rather than hidden
//!
//! this is the one feature in bpd that a program **can** see. with it on, the
//! debuggee's environment holds `PYTHONPATH` ending in a directory of bpd's and
//! the three names in [`bpd_protocol::env::CHILD`], and its `sys.path` ends in
//! that same directory. the mirrors in `crates/bpd/tests/launch_parity.rs`
//! enumerate exactly that and fail on a fourth thing
//!
//! **off is the default and stays the default.** debugpy defaults its
//! equivalent to on, and that is the one thing in its design not to copy: a
//! child that is debugged *stops*, and a setting that produced stopped processes
//! without being asked for would be a debugger that hangs programs by default
//!
//! ## appended, never prepended
//!
//! the agent's own staged directory is *prepended* to `PYTHONPATH` at launch,
//! and `a_program_that_reads_its_own_import_path_finds_no_debugger_on_it` exists
//! because a directory searched before everything else is the debugger deciding
//! what the program imports. this one goes on the **end**, where it cannot
//! shadow a module of the program's own — and the directory holds one file, so
//! there is nothing in it to shadow with but `sitecustomize` itself
//!
//! ## `sys.path` moves with `PYTHONPATH`
//!
//! the directory is appended to *both*. `PYTHONPATH` is a statement about where
//! this interpreter and its children import from, and one naming a directory
//! this interpreter's `sys.path` does not have is a lie about this process —
//! programs read it back, and several rebuild the variable out of `sys.path`,
//! which would drop the channel on the way to a child. it also makes
//! `import sitecustomize` in the debuggee reach the real file rather than
//! nothing, and that import is a no-op because [`entered`] is idempotent
//!
//! ## a non-python child, and a python grandchild
//!
//! a child that is not python — the `/bin/sh` behind `shell=True`, `git`, `ls` —
//! inherits the variables and **ignores** them, because nothing but an
//! interpreter reads `PYTHONPATH`. that is inert. a **grandchild** that is
//! python inherits them too and attaches, which is the feature working through a
//! shell rather than an exception to it: `sh -c "python worker.py"` is a python
//! child, and it is one this reaches where the audit hook's report deliberately
//! cannot see it
//!
//! an interpreter started with `-E`, `-I` or `-S` reaches none of this — the
//! first two ignore `PYTHONPATH` and the third does not import `site` — and a
//! child of one runs exactly as it would have

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use bpd_core::StopReason;
use pyo3::prelude::*;
use pyo3::types::{PyList, PyModule};

use crate::{attach, events, forks, frames, run, session, spawns};

/// how `PYTHONPATH` is spelled on this platform
const SEPARATOR: &str = if cfg!(windows) { ";" } else { ":" };

/// everything an `exec`'d child of this process has to be told
///
/// held for the life of the process because the ask can arrive at any stop, and
/// the values are gone from the environment by then — the agent takes its own
/// variables out before a line of the program runs, which is what keeps the
/// **off** case indistinguishable from a bare run
struct Channel {
    /// where a child connects, which is the session's own listener
    endpoint: String,
    /// the token a child presents, which is **not** the session token
    token: String,
    /// the staged directory holding the agent
    agent: String,
    /// the staged directory holding the `sitecustomize` a child is entered
    /// through
    site: String,
}

static CHANNEL: OnceLock<Channel> = OnceLock::new();

/// whether the environment currently carries the channel
static OPEN: AtomicBool = AtomicBool::new(false);

/// `PYTHONPATH` exactly as it was when the channel was opened
///
/// **absent is not the same as set and empty**, and a program that reads
/// `os.environ` can tell the two apart — so what is put back when child
/// debugging is turned off is what was there, rather than a value reconstructed
/// by taking a suffix off a string. `None` therefore means "there was none",
/// and whether there is anything to put back at all is [`OPEN`]'s to say
static RESTORE: Mutex<Option<String>> = Mutex::new(None);

/// remember what a child would have to be told, before the environment is
/// cleared
///
/// called from the agent's entry point, in the one window where the launcher's
/// variables are still readable
///
/// where the agent itself is staged is read off the module's own `__file__`
/// rather than taken from the launcher, for the reason
/// [`crate::bpd_agent::main`] reads it there too: what a child is pointed at is
/// the directory the agent really came from
pub(crate) fn remember(
    python: Python<'_>,
    endpoint: &str,
    token: &str,
    site: &str,
) -> PyResult<()> {
    CHANNEL
        .set(Channel {
            endpoint: endpoint.to_owned(),
            token: token.to_owned(),
            agent: staged_agent(python)?,
            site: site.to_owned(),
        })
        .unwrap_or_else(|_| unreachable!("the agent's entry point runs once"));
    Ok(())
}

/// the directory this agent was imported from
fn staged_agent(python: Python<'_>) -> PyResult<String> {
    let file: String = PyModule::import(python, "bpd_agent")?
        .getattr("__file__")?
        .extract()?;
    Ok(std::path::Path::new(&file)
        .parent()
        .unwrap_or_else(|| unreachable!("`{file}` is a file, so it has a directory"))
        .display()
        .to_string())
}

fn channel() -> &'static Channel {
    CHANNEL.get().unwrap_or_else(|| {
        unreachable!("the entry point remembers the channel before the program runs")
    })
}

fn restoring() -> std::sync::MutexGuard<'static, Option<String>> {
    RESTORE.lock().expect(
        "nothing panics holding the saved `PYTHONPATH`: every path through it is a read or a write",
    )
}

/// put the channel into this program's environment, or take it back out
///
/// the two halves are exact inverses, and that is the whole of what makes
/// turning child debugging **off** mean something: a debuggee that had it on and
/// then off has the environment and the `sys.path` it started with, not one that
/// merely no longer works
///
/// idempotent in both directions. asking for what is already the case touches
/// nothing, so a front end that sets it at every stop does not append the
/// directory twice
pub(crate) fn announce(python: Python<'_>, on: bool) -> PyResult<()> {
    if OPEN.swap(on, Ordering::SeqCst) == on {
        return Ok(());
    }
    let channel = channel();

    let environ = PyModule::import(python, "os")?.getattr("environ")?;
    let path = PyModule::import(python, "sys")?.getattr("path")?;
    let path = path.cast::<PyList>()?;

    if on {
        let inherited: Option<String> = environ
            .call_method1("get", ("PYTHONPATH", python.None()))?
            .extract()?;
        (*restoring()).clone_from(&inherited);

        let appended = inherited.map_or_else(
            || channel.site.clone(),
            |inherited| format!("{inherited}{SEPARATOR}{}", channel.site),
        );
        environ.set_item("PYTHONPATH", appended)?;
        environ.set_item(bpd_protocol::env::CHILD_ENDPOINT, &channel.endpoint)?;
        environ.set_item(bpd_protocol::env::CHILD_TOKEN, &channel.token)?;
        environ.set_item(bpd_protocol::env::CHILD_AGENT, &channel.agent)?;
        path.append(&channel.site)?;
        return Ok(());
    }

    match restoring().take() {
        Some(inherited) => environ.set_item("PYTHONPATH", inherited)?,
        None => {
            environ.call_method1("pop", ("PYTHONPATH", python.None()))?;
        }
    }
    for name in bpd_protocol::env::CHILD {
        environ.call_method1("pop", (*name, python.None()))?;
    }
    // the program is free to have rebuilt its own `sys.path`, and taking an
    // entry out of a list that no longer holds it raises. what is being undone
    // is bpd's own append, so its absence is the state that was wanted
    if path.contains(&channel.site)? {
        path.call_method1("remove", (&channel.site,))?;
    }
    Ok(())
}

/// this process is a child that was `exec`'d, entered through the staged
/// `sitecustomize`
///
/// it runs at interpreter startup, from `site`, before `__main__` exists and
/// before a line of the program has been compiled. so what it produces is the
/// child's **entry** stop: nothing of the program has run, and there is no line
/// to name because there is no code object yet
///
/// nothing here raises for a reason the *program* did not cause. a child that
/// could not be debugged says so on its own stderr and then runs exactly as it
/// would have, because killing a worker over a debugger that could not reach it
/// would be the debugger changing what the program did — the same rule a forked
/// child that cannot reconnect follows
pub(crate) fn entered(python: Python<'_>) -> PyResult<()> {
    forget_the_agents_directory(python)?;

    // a program that imports `sitecustomize` by hand reaches here, and so does
    // the parent — whose own `sys.path` ends in the directory holding it. a
    // process that already has a session does not open a second one
    if attach::attached() {
        return Ok(());
    }

    let (Ok(endpoint), Ok(token)) = (
        std::env::var(bpd_protocol::env::CHILD_ENDPOINT),
        std::env::var(bpd_protocol::env::CHILD_TOKEN),
    ) else {
        // the directory is on this interpreter's path and the variables are
        // not, which is what a program that cleared its own environment leaves.
        // it is not a child bpd was asked to debug
        return Ok(());
    };

    if let Err(error) = crate::verify(python) {
        return said(
            python,
            &format!(
                "this program started an interpreter the staged agent was not \
                 built for, so the child is not being debugged and is running \
                 as it would have without bpd. {}",
                error.value(python)
            ),
        );
    }

    if let Err(error) = attach::attach(&endpoint, &token) {
        return said(
            python,
            &format!(
                "this program started a child, and the child could not open a \
                 debug session of its own on {endpoint}: {error}. the child is \
                 not being debugged and is running as it would have without bpd"
            ),
        );
    }

    if let Err(error) = arm_as_a_debuggee(python) {
        return said(
            python,
            &format!(
                "this program started a child, and the child reached the debugger \
                 and could not instrument itself: {}. it is running as it would \
                 have without bpd",
                error.value(python)
            ),
        );
    }
    Ok(())
}

/// claim the tool, arm the events, and hold this child before it runs anything
fn arm_as_a_debuggee(python: Python<'_>) -> PyResult<()> {
    crate::claim(python)?;
    crate::arm(python)?;

    // this process's children are this session's to report, and its forks are
    // its own to debug: the channel is in the environment it inherited, so a
    // grandchild reaches the same engine whatever this one decides
    spawns::install(python)?;
    spawns::now_this_process();
    forks::install(python)?;
    forks::debug_children(true);
    OPEN.store(true, Ordering::SeqCst);

    // the `sitecustomize` frame this is running in is bpd's own, and no stack
    // may ever report it. remembering it as the bootstrap is what stops the
    // walk there, so the entry stop below carries **no** frames — which is the
    // truth about a process that has not begun its program
    frames::remember_bootstrap(python)?;
    // there is no `run::enter` in a child and so no entry gate to match, and
    // this says the entry stop has already happened rather than leaving a
    // second one to be looked for on every code object
    run::mark_stopped_at_entry();

    session::stop(
        python,
        events::thread_ident(python)?,
        StopReason::Started {
            parent: forks::parent_process(),
        },
    )
}

/// take the agent's staged directory back off this child's import path
///
/// the `sitecustomize` puts it in front of `sys.path` for exactly one import
/// and this is what undoes it — here rather than there, so that the directory is
/// gone before the child is **held**, and a client that walks the child's path
/// while it waits sees what the program will see
fn forget_the_agents_directory(python: Python<'_>) -> PyResult<()> {
    let staged = staged_agent(python)?;
    let path = PyModule::import(python, "sys")?.getattr("path")?;
    let path = path.cast::<PyList>()?;
    if path.contains(&staged)? {
        path.call_method1("remove", (&staged,))?;
    }
    Ok(())
}

/// say why this child is not being debugged, on its own stderr
///
/// the child shares the program's stderr, so this reaches whoever is watching
/// the program. it is prefixed `bpd:` because it is the debugger talking and not
/// the program — and it is written through `sys.stderr` rather than through
/// rust's, so that it interleaves with the program's own output the way every
/// other line on that stream does
fn said(python: Python<'_>, reason: &str) -> PyResult<()> {
    PyModule::import(python, "sys")?
        .getattr("stderr")?
        .call_method1("write", (format!("bpd: {reason}\n"),))?;
    Ok(())
}
