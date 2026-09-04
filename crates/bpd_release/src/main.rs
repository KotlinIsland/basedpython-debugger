//! `bpd-release` — assemble the layout a released `bpd` is shipped as, and
//! check one
//!
//! it is a separate binary from `bpd` on purpose. this is a build-time tool and
//! `bpd` is a debugger: a `package` subcommand would appear in `bpd --help` for
//! every person who ever installs one, describing something only this
//! repository's release ever does
//!
//! **nothing here publishes.** it reads files that exist and writes a directory.
//! uploading one is `.github/workflows/release.yaml`, which runs this on five
//! platforms and takes what it produced — there is no network call in this
//! program to reach for

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

    /// write a verified layout out as a wheel, so pip can deliver it
    ///
    /// one wheel per **platform**, tagged `py3-none-<platform>`, carrying every
    /// agent the layout holds. the binary is not a python extension and the
    /// agents are loaded by the debuggee, so nothing here is tied to the
    /// interpreter that installs it
    Wheel {
        /// the layout's root, as `assemble` built it
        #[arg(long)]
        layout: PathBuf,

        /// the distribution name pip will know it by
        ///
        /// the default is the name in `pyproject.toml`, which is the project
        /// pypi holds — it is an argument at all so that a fork publishing
        /// under its own name does not have to patch the tree to do it
        #[arg(long, default_value = bpd_release::wheel::DISTRIBUTION)]
        distribution: String,

        /// the version to ship it as
        #[arg(long)]
        version: String,

        /// the platform tag, written the way pip writes one
        ///
        /// `macosx_11_0_arm64`, `manylinux_2_17_x86_64`, `win_amd64`. it is
        /// taken rather than detected because what manylinux level a binary
        /// satisfies is a fact about the toolchain that built it
        #[arg(long)]
        platform: String,

        /// the directory to write the wheel into
        #[arg(long)]
        out: PathBuf,
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
        Command::Wheel {
            layout,
            distribution,
            version,
            platform,
            out,
        } => {
            let built = bpd_release::wheel(&layout, &distribution, &version, &platform, &out)?;
            Ok(format!(
                "wrote {}, carrying {} file(s) as {}",
                built.path.display(),
                built.contents.len(),
                built.tag
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
