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

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitCode, ExitStatus};

use bpd_core::Running;
use bpd_core::python::Capabilities;
use bpd_engine::{Launched, Program};

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

    let finished = match launched {
        // nothing is added here on purpose. the program already failed in the
        // interpreter's own words, and a line of bpd's on top would be a line
        // that is not there without the debugger
        Launched::ExitedBeforeStopping(status) => Ok(status),
        Launched::Stopped(mut debuggee) => match debuggee
            .run(|record| unreachable!("no breakpoints were set, and the agent logged {record:?}"))
        {
            // `bpd launch` sets no breakpoints, so nothing in the program can
            // stop it and nothing can change what a breakpoint resolves to.
            // both are stated rather than absorbed, because a stop nobody
            // handles is a debuggee left hanging
            Ok(Running::Exited { status, rebound }) => {
                assert!(
                    rebound.is_empty(),
                    "no breakpoints were set, and the agent reported {rebound:?}"
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
            Err(error) => Err(error),
        },
    };

    match finished {
        Ok(status) => ExitCode::from(exit_code(status)),
        Err(error) => {
            report_error(&error);
            ExitCode::FAILURE
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
