//! `bpd-release` — assemble the layout a released `bpd` is shipped as, and
//! check one
//!
//! it is a separate binary from `bpd` on purpose. this is a build-time tool and
//! `bpd` is a debugger: a `package` subcommand would appear in `bpd --help` for
//! every person who ever installs one, describing something only this
//! repository's release ever does
//!
//! **nothing here publishes.** it reads files that exist and writes a directory,
//! and what happens to that directory afterwards is not a decision this makes

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// assemble the layout a released `bpd` is shipped as, and check one
#[derive(Debug, Parser)]
#[command(name = "bpd-release", version, about, long_about = None)]
enum Command {
    /// build the layout out of a binary and one agent per interpreter tag
    Assemble {
        /// the `bpd` binary to ship
        #[arg(long)]
        binary: PathBuf,

        /// an agent for one tag, as `<tag>=<path>` — given once per tag
        ///
        /// the tag is what the interpreter says it is: `3.13`, `3.14`, `3.14t`
        #[arg(long = "agent", value_name = "TAG=PATH", required = true)]
        agents: Vec<String>,

        /// where to build it, which must not already hold anything
        #[arg(long)]
        out: PathBuf,
    },

    /// read a layout back and check it is still what its manifest says
    Verify {
        /// the layout's root
        layout: PathBuf,
    },
}

#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "this is a command line tool and its two streams are its whole \
              interface: what it built on stdout, and why it would not on stderr"
)]
fn main() -> ExitCode {
    match run(Command::parse()) {
        Ok(said) => {
            println!("{said}");
            ExitCode::SUCCESS
        }
        // the reason, on stderr, in the words the refusal carries. a packaging
        // step that failed with an exit code and nothing else is one somebody
        // reproduces by hand to find out what happened
        Err(refused) => {
            eprintln!("bpd-release: {refused}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<String, bpd_release::Refused> {
    match command {
        Command::Assemble {
            binary,
            agents,
            out,
        } => {
            let agents = bpd_release::agents(&agents)?;
            let manifest = bpd_release::assemble(&binary, &agents, &out)?;
            let tags: Vec<String> = manifest.tags.iter().map(ToString::to_string).collect();
            Ok(format!(
                "assembled {} into {}, carrying an agent for {}",
                manifest.files.len(),
                out.display(),
                tags.join(", ")
            ))
        }
        Command::Verify { layout } => {
            let manifest = bpd_release::verify(&layout)?;
            let tags: Vec<String> = manifest.tags.iter().map(ToString::to_string).collect();
            Ok(format!(
                "{} is what its manifest says: {} file(s), an agent for {}",
                layout.display(),
                manifest.files.len(),
                tags.join(", ")
            ))
        }
    }
}
