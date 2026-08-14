//! `bpd launch` — run a program with the debugger attached
//!
//! today it stops before the first statement and resumes immediately, because
//! there is nothing yet that could be told about the stop or asked what to do
//! next. what it proves is the part that everything else needs: the agent
//! attaches, the program is genuinely held before it has run, and letting it go
//! produces a run indistinguishable from a bare one
//!
//! indistinguishable is meant literally, and is checked in
//! `crates/bpd/tests/launch_parity.rs`: the same stdout, the same stderr, the
//! same exit code, and the same `sys.argv`, `sys.path[0]` and `__main__` — for
//! each of the three ways an interpreter can be entered, which differ from one
//! another in exactly those values
//!
//! `--debug-children` is the one thing that changes what the program can see,
//! and it changes it by exactly the four names
//! `a_program_launched_with_debug_children_can_tell_exactly_as_much_and_no_more`
//! enumerates. what it changes here is that there is more than one session to
//! drive: a debugged child arrives **held**, and this command is the only thing
//! that could let it go — see [`every_session`]

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitCode, ExitStatus};
use std::time::Duration;

use bpd_core::python::Capabilities;
use bpd_core::{Addressed, Forwarded, Request, Response, Running};
use bpd_engine::{Debuggee, Launched, Program};

use crate::report_error;

/// `bpd launch` arguments
///
/// the three launch forms are one **required** group, so a launch that names no
/// program at all is a parse error rather than something an interpreter is
/// started to discover
///
/// they are exclusive **structurally** rather than by a check, and that is
/// worth saying because it looks like a missing one. `-m` and `-c` each take
/// everything after their own value, exactly as the interpreter's own option
/// parsing does — `python -m pkg -c x` runs `pkg` with `-c x` as its arguments,
/// and so does this. so no two of the three can ever be populated at once, and
/// a conflict rule over them would be a rule that can never fire
#[derive(Debug, clap::Args)]
#[command(group = clap::ArgGroup::new("form")
    .required(true)
    .args(["module", "command", "script"]))]
pub(crate) struct Args {
    /// the interpreter to run under, resolved on PATH like any other command
    #[arg(long, default_value = "python3")]
    python: PathBuf,

    /// run a module as `__main__`, the way `python -m` does, and everything
    /// after it as the program's own arguments
    #[arg(
        short = 'm',
        value_name = "MODULE",
        num_args = 1..,
        allow_hyphen_values = true
    )]
    module: Vec<OsString>,

    /// run source as `__main__`, the way `python -c` does, and everything after
    /// it as the program's own arguments
    #[arg(
        short = 'c',
        value_name = "SOURCE",
        num_args = 1..,
        allow_hyphen_values = true
    )]
    command: Vec<OsString>,

    /// debug the program's children too, as sessions of their own, instead of
    /// reporting them and leaving them alone
    #[arg(long)]
    debug_children: bool,

    /// the program to run, and its arguments exactly as it would receive them
    #[arg(
        value_name = "SCRIPT",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    script: Vec<OsString>,
}

impl Args {
    /// the program to run and the arguments that belong to it
    ///
    /// clap has already established that exactly one of the three is populated,
    /// so the only thing left to decide is a module name or a command source
    /// that is not text. the interpreter would reject those too — a module name
    /// is an identifier and a command is source — and saying so here names the
    /// value rather than letting python fail on something it was handed
    fn program(&self) -> Result<(Program, &[OsString]), String> {
        if let Some((module, arguments)) = self.module.split_first() {
            return Ok((Program::Module(text(module, "a module name")?), arguments));
        }
        if let Some((source, arguments)) = self.command.split_first() {
            return Ok((Program::Command(text(source, "a command")?), arguments));
        }
        let (script, arguments) = self.script.split_first().unwrap_or_else(|| {
            unreachable!("the form group is required, so one of the three holds at least one value")
        });
        Ok((Program::Script(PathBuf::from(script)), arguments))
    }
}

/// an argument the interpreter can only receive as text
#[expect(
    clippy::unnecessary_debug_formatting,
    reason = "this message is about bytes that are not text, and `Display` \
              would replace exactly the ones that caused it"
)]
fn text(value: &OsString, what: &str) -> Result<String, String> {
    value
        .to_str()
        .map(ToString::to_string)
        .ok_or_else(|| format!("{what} has to be valid utf-8, and {value:?} is not"))
}

/// what `bpd launch` does with the things a running program says
///
/// two of the four cannot happen here and say so: `bpd launch` sets no
/// breakpoints, so nothing can log, and it arms no pause. a child being
/// reported can happen to any program at all, and a child **joining** can
/// happen only when `--debug-children` asked for one
struct Watching {
    /// whether `--debug-children` was given
    ///
    /// what makes a session that joins a thing this command asked for. without
    /// it nothing puts the child token in the debuggee's environment and
    /// nothing tells a fork to reconnect, so a session arriving is a session
    /// nothing asked for — and that says so rather than being handled
    children: bool,
}

impl bpd_core::Reporting for Watching {
    fn logged(&mut self, record: bpd_core::LogRecord) {
        unreachable!("no breakpoints were set, and the agent logged {record:?}")
    }

    fn pausing(&mut self, running: Vec<u64>) {
        unreachable!("no pause was armed, and the agent acknowledged one naming {running:?}")
    }

    /// the program started a child, and `bpd` is debugging the parent
    ///
    /// on stderr, prefixed, because it is the debugger talking and not the
    /// program. it is written as it arrives rather than at the end: a
    /// supervisor that starts a child and then waits for ever is exactly the
    /// program this is worth knowing about, and it never reaches an end
    #[expect(
        clippy::print_stderr,
        reason = "the whole point of this sink is to put the notice where a \
                  person running `bpd launch` in a terminal will see it"
    )]
    fn spawned(&mut self, child: bpd_core::Spawn) {
        eprintln!("bpd: {child}");
    }

    /// this interpreter hides a whole way of starting a child
    ///
    /// the same stream and the same prefix as a child itself, because it is the
    /// same subject and a person reading one needs the other beside it
    #[expect(
        clippy::print_stderr,
        reason = "a limit nobody is told about is a limit that reads as an \
                  absence of children — see above"
    )]
    fn blind_to(&mut self, blindspot: bpd_core::Blindspot) {
        eprintln!("bpd: {blindspot}");
    }

    /// another agent joined this debuggee
    ///
    /// with `--debug-children` this is a child of the program, **held** before
    /// it has run anything, and it is said as it arrives for the reason a child
    /// itself is: the program it is a child of may be a supervisor that never
    /// reaches an end
    ///
    /// without the flag nothing asked for it. `bpd launch` sends no
    /// [`bpd_core::Request::DebugChildren`], so the child token is never put
    /// back into the debuggee's environment and no fork is told to reconnect —
    /// a session arriving anyway is a state this command has no account of, and
    /// it says so rather than handling it
    #[expect(
        clippy::print_stderr,
        reason = "a held process nobody is told about is the one thing child \
                  debugging must never produce — see above"
    )]
    fn attached(&mut self, session: bpd_core::SessionId) {
        assert!(
            self.children,
            "`bpd launch` was not asked to debug the program's children, and \
             {session} joined this debuggee"
        );
        eprintln!(
            "bpd: {session} joined — a child of this program is being debugged, \
             and is held before it has run anything"
        );
    }
}

impl Watching {
    /// a session is held, and this command is what has to let it go
    ///
    /// said as it happens, on the stream the debugger's own words go on. a
    /// child that arrives held and is let go without a word would be a program
    /// that ran under bpd with a step nobody can see in its history
    #[expect(
        clippy::print_stderr,
        reason = "the same stream and the same prefix as every other thing the \
                  debugger says about a child"
    )]
    fn held(&mut self, stop: &bpd_core::Stop) {
        assert!(
            self.children,
            "`bpd launch` sets no breakpoints, arms no pause and asked for no \
             child to be debugged, and {stop:?} arrived"
        );
        eprintln!(
            "bpd: {} is held — {}. `bpd launch` has nothing to hold a program \
             in, so it is let go",
            stop.session,
            held_at(&stop.reason)
        );
    }
}

/// where a session that joined is held, in the words the stop carries
///
/// the two a child arrives with, and nothing invented for either: a fork is
/// held at the line it forked on, and an `exec`'d child has no line to be held
/// at because none of its program has been compiled yet
fn held_at(reason: &bpd_core::StopReason) -> String {
    match reason {
        bpd_core::StopReason::Forked { parent, file, line } => {
            format!("forked from pid {parent}, at `{file}` line {line}")
        }
        bpd_core::StopReason::Started { parent } => format!(
            "started by pid {parent}, at its own interpreter startup — none of \
             its program has been compiled yet, so it has no line and no stack"
        ),
        // `bpd launch` arms no breakpoint, no step and no pause, so nothing
        // else here can hold a thread. `StopReason` is non-exhaustive though,
        // and a stop this does not have words for is still a stop — so it says
        // what arrived rather than claiming it could not have
        other => format!("{other:?}"),
    }
}

pub(crate) fn run(args: &Args) -> ExitCode {
    let (program, arguments) = match args.program() {
        Ok(program) => program,
        Err(message) => {
            eprintln!("error: {message}");
            return ExitCode::FAILURE;
        }
    };

    let capabilities = match Capabilities::probe(&args.python) {
        Ok(capabilities) => capabilities,
        Err(error) => {
            report_error(&error);
            return ExitCode::FAILURE;
        }
    };

    let launched = match bpd_engine::launch(&capabilities, &program, arguments) {
        Ok(launched) => launched,
        Err(error) => {
            report_error(&error);
            return ExitCode::FAILURE;
        }
    };

    let mut watching = Watching {
        children: args.debug_children,
    };

    let finished = match launched {
        // nothing is added here on purpose. the program already failed in the
        // interpreter's own words, and a line of bpd's on top would be a line
        // that is not there without the debugger
        Launched::ExitedBeforeStopping(status) => Ok(status),
        Launched::Stopped(mut debuggee) => {
            if args.debug_children {
                // asked for while the program is held at its entry stop, which
                // is before it has run a line and therefore before it could
                // have made a child. a setting that arrived later would leave
                // whichever children came first undebugged, silently
                match debuggee.debug_children(true) {
                    Ok(true) => {}
                    Ok(false) => unreachable!(
                        "the agent answers with what is set, and turning child \
                         debugging on is refused by name where it cannot be \
                         true rather than answered `false`"
                    ),
                    // the program has run nothing, so refusing here is refusing
                    // before the program ran rather than in the middle of it —
                    // the rule an unsupported interpreter is refused by
                    Err(error) => {
                        report_error(&error);
                        return ExitCode::FAILURE;
                    }
                }
                every_session(&mut debuggee, &mut watching)
            } else {
                alone(&mut debuggee, &mut watching)
            }
        }
    };

    match finished {
        Ok(status) => ExitCode::from(exit_code(status)),
        Err(error) => {
            report_error(&error);
            ExitCode::FAILURE
        }
    }
}

/// let the program go and wait for it to end, with nothing else to attend to
///
/// the default, and the whole of what `bpd launch` used to do
fn alone(
    debuggee: &mut Debuggee,
    watching: &mut Watching,
) -> Result<ExitStatus, bpd_engine::Error> {
    match debuggee.run(watching) {
        // `bpd launch` sets no breakpoints, so nothing in the program can
        // stop it and nothing can change what a breakpoint resolves to.
        // both are stated rather than absorbed, because a stop nobody
        // handles is a debuggee left hanging
        Ok(Running::Exited {
            status,
            rebound,
            output,
        }) => {
            assert!(
                rebound.is_empty(),
                "no breakpoints were set, and the agent reported {rebound:?}"
            );
            // this launch leaves the debuggee's streams inherited, so its output
            // never passed through bpd and there is nothing that could still be
            // carrying it. anything else here is the engine reporting a pipe on
            // a run that has none
            assert_eq!(
                output,
                Forwarded::Everything,
                "`bpd launch` inherits the program's streams, and the engine \
                 reported {output:?}"
            );
            Ok(status)
        }
        Ok(Running::Stopped { stop, .. }) => {
            unreachable!("no breakpoints were set, and the debuggee stopped for {stop:?}")
        }
        // the only stop `bpd launch` makes is the entry one, and it resumes
        // it before the program runs a line, so nothing can still be held
        // `bpd launch` waits without a deadline: it has nothing to report a
        // timeout to, and the program's own exit is what it is waiting for
        Ok(Running::StillRunning { waited, .. }) => {
            unreachable!(
                "this wait carries no deadline and was answered after \
                 {waited:?} with the program still running"
            )
        }
        Ok(Running::Finishing { threads, .. }) => {
            unreachable!("nothing was held, and the debuggee ended holding {threads:?}")
        }
        // `bpd launch` starts the process itself and holds its child, so
        // its exit is bpd's to read. this is the answer for a session that
        // connected to bpd's listener instead, and `bpd launch` waits on
        // the one it launched
        Ok(Running::Ended { .. }) => {
            unreachable!(
                "the program bpd launched ended without an exit status, and \
                 bpd holds its child"
            )
        }
        Err(error) => Err(error),
    }
}

/// how long one turn of [`every_session`] spends on one session
///
/// it bounds the **rotation** and nothing else. a wait is a peek with a
/// deadline, so a session with something to say is answered the instant it says
/// it; what this decides is how long a second session waits for its turn while
/// the first is silent
const TURN: Duration = Duration::from_millis(20);

/// let every session of this debuggee go, and wait for all of them to end
///
/// what `--debug-children` needs and the reason the flag did not exist before.
/// a debugged child **stops** — a fork at the line that forked, an `exec`'d
/// child at its own startup — and a stop that nothing resumes is a hung
/// program. `bpd launch` has no ui to hold one in and sets no breakpoints, so
/// what it does with a child is what it does with the program itself: says
/// where it is, and lets it go
///
/// so every session is driven the same way, in a rotation: resume whatever it
/// is holding, wait a turn for what it does next, and move on. one session at a
/// time because a wait is addressed to one — what a wait on another session's
/// connection would find has nowhere to go in its answer — and the rotation is
/// what stops a program that is blocked in `waitpid` on a held child from being
/// waited on for ever while the child waits to be let go
///
/// it returns when **every** session has ended, and the status it returns is the
/// launched program's. leaving earlier would close the connection to a child
/// that is still running, which the agent reads as the debugger vanishing and
/// answers by ending the process — the debugger killing a program it was asked
/// to watch. so `bpd launch --debug-children` outlives a child that outlives its
/// parent, and that is a real difference from a bare run of the same program:
/// [child processes](../../../docs/development/subprocesses.md) says so out loud
///
/// # a child that arrives after the last session has ended
///
/// it cannot be one that is left held, and the reason is where a session is
/// accepted: the engine takes a connection off the listener from **inside a
/// wait**, and a wait is only ever made on a session that is still open. so a
/// child the engine has taken is a child in [`Debuggee::sessions`], and this
/// loop reads that list afresh every turn and drives every one of them to its
/// end
///
/// a child whose connection had not been taken by then is attached to nothing.
/// it is waiting for a handshake, and when this process exits it gets what any
/// child that cannot reach the debugger gets: a line on its own stderr naming
/// the endpoint and the failure, and a run with the tool taken off it. the
/// shape that produces one is a parent that exits immediately after forking —
/// so what it costs is a child that runs undebugged and says so, rather than a
/// process nobody can resume
fn every_session(
    debuggee: &mut Debuggee,
    watching: &mut Watching,
) -> Result<ExitStatus, bpd_engine::Error> {
    let launched = match debuggee.sessions().as_slice() {
        [only] => *only,
        open => unreachable!(
            "child debugging was asked for while the program was held at its \
             entry stop, and this debuggee already held {open:?}"
        ),
    };
    let mut ended: Vec<bpd_core::SessionId> = Vec::new();
    let mut status = None;

    loop {
        let live: Vec<bpd_core::SessionId> = debuggee
            .sessions()
            .into_iter()
            .filter(|session| !ended.contains(session))
            .collect();
        if live.is_empty() {
            return Ok(status.unwrap_or_else(|| {
                unreachable!(
                    "the launched session is one of these and bpd holds its \
                     child, so its end carried the status this exits with"
                )
            }));
        }

        for session in live {
            // a resume is refused for a program with nothing held — the agent
            // answers on a thread it is holding, and a parent blocked in
            // `waitpid` on a child bpd has not let go of has none. so which of
            // the two this is comes from what is held rather than from a guess
            let holding = debuggee.held().iter().any(|stop| stop.session == session);
            let request = if holding {
                Request::Run {
                    deadline: Some(TURN),
                }
            } else {
                Request::Wait {
                    deadline: Some(TURN),
                }
            };

            let ran = match debuggee.dispatch(Addressed::to(session, request), watching)? {
                Response::Ran(ran) => ran,
                other => unreachable!("a run of {session} was answered with {other:?}"),
            };
            match ran {
                Running::Stopped { stop, rebound } => {
                    assert!(
                        rebound.is_empty(),
                        "no breakpoints were set, and the agent reported {rebound:?}"
                    );
                    watching.held(&stop);
                }
                // the program is running, or it has reached its end with a
                // thread still held. neither is anything to do here and neither
                // is a hang: an interpreter cannot finalize while a thread is
                // held, and the next turn of this loop is what lets it go —
                // `held` is what decides between a resume and a wait, and it
                // holds every stop that has arrived
                Running::StillRunning { .. } | Running::Finishing { .. } => {}
                Running::Exited { status: exit, .. } => {
                    assert_eq!(
                        session, launched,
                        "bpd did not start that process and cannot have read \
                         {exit} off it"
                    );
                    status = Some(exit);
                    ended.push(session);
                }
                // a session that joined is a process bpd is not the parent of,
                // so it ends without an exit status. inventing one is the whole
                // reason this is a variant of its own
                Running::Ended { .. } => {
                    assert_ne!(
                        session, launched,
                        "bpd holds the child it launched, so its end carries an \
                         exit status"
                    );
                    ended.push(session);
                }
            }
        }
    }
}

/// the debuggee's exit, as this process should report it
///
/// a program run under `bpd` exits the way it would have. a signal becomes
/// `128 + signal`, which is the number a shell reports for a signalled child,
/// so a script wrapping either one sees the same value
fn exit_code(status: ExitStatus) -> u8 {
    if let Some(code) = status.code() {
        return u8::try_from(code).unwrap_or_else(|_| {
            // unreachable on unix, where the kernel already truncated it. on
            // windows an exit code is a full i32 and there is nowhere to put it
            eprintln!("bpd: the program exited with {code}, which does not fit in an exit code");
            1
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return u8::try_from(128 + signal).unwrap_or(1);
        }
    }

    unreachable!("an exit status is either a code or a signal, and this was {status}")
}
