//! `bpd doctor` — ask an interpreter whether it can be debugged
//!
//! this exits non-zero when the answer is no, so it is usable as a check in a
//! script. the report is printed either way, because "which of my six
//! interpreters can you drive" is the question people actually have

use std::path::PathBuf;
use std::process::ExitCode;

use bpd_core::python::{Capabilities, MINIMUM_SUPPORTED};

use crate::report_error;

/// `bpd doctor` arguments
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// the interpreter to ask, resolved on PATH like any other command
    #[arg(default_value = "python3")]
    interpreter: PathBuf,
}

pub(crate) fn run(args: &Args) -> ExitCode {
    let capabilities = match Capabilities::probe(&args.interpreter) {
        Ok(capabilities) => capabilities,
        Err(error) => {
            report_error(&error);
            return ExitCode::FAILURE;
        }
    };

    report(&capabilities);
    println!();

    if let Err(error) = capabilities.require_debuggable() {
        report_error(&error);
        return ExitCode::FAILURE;
    }

    println!("this interpreter can be debugged");

    // what the interpreter supports is the `PEP 768` field above. this line is
    // about what bpd offers, and those are two different facts: there is no
    // `bpd attach` yet, so an interpreter with the api reaches nothing. saying
    // "attaching is available" on the strength of the interpreter having it
    // named a command that does not exist, which sent a reader to run it
    println!("attaching to a running process is not built yet — see `PEP 768` above");

    ExitCode::SUCCESS
}

fn report(capabilities: &Capabilities) {
    field(
        "interpreter",
        &capabilities.interpreter.display().to_string(),
    );
    field("executable", &capabilities.executable.display().to_string());
    field(
        "version",
        &format!(
            "{} ({}), minimum is {MINIMUM_SUPPORTED}",
            capabilities.version, capabilities.implementation
        ),
    );
    field("build", &build(capabilities));
    field(
        "extension",
        capabilities
            .ext_suffix
            .as_deref()
            .unwrap_or("unknown — this interpreter reports no EXT_SUFFIX"),
    );
    field(
        "PEP 669",
        if capabilities.monitoring {
            "`sys.monitoring` present"
        } else {
            "`sys.monitoring` missing"
        },
    );
    field("PEP 768", &capabilities.remote_debug.to_string());
}

fn build(capabilities: &Capabilities) -> String {
    let mut parts = vec![if capabilities.free_threaded {
        "free-threaded"
    } else {
        "gil"
    }];
    if capabilities.debug_build {
        parts.push("pydebug");
    }
    parts.join(", ")
}

fn field(name: &str, value: &str) {
    println!("{name:<12} {value}");
}
