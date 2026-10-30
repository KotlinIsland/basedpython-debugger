//! the `bpd` command line
//!
//! only the commands that are fully implemented appear here. a subcommand that
//! parses and then reports that it does not work yet is a placeholder, and this
//! project does not ship those

// a command line tool is the one place where writing to the terminal is the
// entire job
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod cache;
mod dap;
mod doctor;
mod launch;
mod mcp;
mod signals;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// a debugger for python and basedpython
#[derive(Debug, Parser)]
#[command(name = "bpd", version, about, long_about = None)]
struct Cli {
    /// what to do
    #[command(subcommand)]
    command: Command,
}

/// the implemented commands
#[derive(Debug, Subcommand)]
enum Command {
    /// report whether an interpreter can be debugged, and why not if it cannot
    Doctor(doctor::Args),

    /// run a program with the debugger attached
    Launch(launch::Args),

    /// speak the debug adapter protocol on stdin and stdout, for an editor
    Dap(dap::Args),

    /// speak the model context protocol on stdin and stdout, for an ai agent
    Mcp(mcp::Args),

    /// show what the agent staging cache is holding, and reclaim it
    Cache(cache::Args),
}

/// print a failure and every cause behind it
///
/// shared by the commands so a refusal reads the same wherever it came from
fn report_error(error: &dyn std::error::Error) {
    eprintln!("error: {error}");
    let mut source = error.source();
    while let Some(cause) = source {
        eprintln!("  caused by: {cause}");
        source = cause.source();
    }
}

fn main() -> ExitCode {
    let command = Cli::parse().command;
    // the two commands that start a program in their own place: `launch`, and
    // `dap`, which `by run` starts as its program. what is sent to end them is
    // passed on to it — see `signals`
    if matches!(command, Command::Launch(_) | Command::Dap(_))
        && let Err(error) = signals::install()
    {
        report_error(&error);
        return ExitCode::FAILURE;
    }
    match command {
        Command::Doctor(args) => doctor::run(&args),
        Command::Launch(args) => launch::run(&args),
        Command::Dap(args) => dap::run(&args),
        Command::Mcp(args) => mcp::run(&args),
        Command::Cache(args) => cache::run(&args),
    }
}
