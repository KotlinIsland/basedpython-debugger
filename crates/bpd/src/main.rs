//! the `bpd` command line
//!
//! only the commands that are fully implemented appear here. a subcommand that
//! parses and then reports that it does not work yet is a placeholder, and this
//! project does not ship those

// a command line tool is the one place where writing to the terminal is the
// entire job
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod doctor;

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
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Doctor(args) => doctor::run(&args),
    }
}
