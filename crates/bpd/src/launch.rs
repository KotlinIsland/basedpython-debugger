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
//! same exit code, and the same `sys.argv`, `sys.path[0]` and `__main__`

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitCode, ExitStatus};

use bpd_core::Running;
use bpd_core::python::Capabilities;
use bpd_engine::Launched;

use crate::report_error;

/// `bpd launch` arguments
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// the interpreter to run under, resolved on PATH like any other command
    #[arg(long, default_value = "python3")]
    python: PathBuf,

    /// the program to run
    script: PathBuf,

    /// arguments for the program, exactly as it would receive them
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    program_arguments: Vec<OsString>,
}

pub(crate) fn run(args: &Args) -> ExitCode {
    let capabilities = match Capabilities::probe(&args.python) {
        Ok(capabilities) => capabilities,
        Err(error) => {
            report_error(&error);
            return ExitCode::FAILURE;
        }
    };

    let launched = match bpd_engine::launch(&capabilities, &args.script, &args.program_arguments) {
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
