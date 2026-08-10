//! `bpd dap` — speak the debug adapter protocol on stdin and stdout
//!
//! this is the composition root, and it is the whole reason `bpd_dap` depends
//! on `bpd_core` alone: the adapter says what it needs of a session, and this
//! is where `bpd_engine` is put behind it. an adapter that could reach the
//! engine would be an adapter shaped by how the agent happens to report
//! something
//!
//! the program's own stdout and stderr are **pipes**, not this process's. an
//! adapter's stdout is the protocol, and one `print` from the debuggee in the
//! middle of a message would make every message after it unreadable

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read};
use std::sync::Arc;

use bpd_core::python::Capabilities;
use bpd_core::{Reporting, Request, Response, Stop};
use bpd_dap::{Configuration, Failed, Launcher, ProgramOutput, Session, Started, Stream};
use bpd_engine::{Debuggee, Launched};

use crate::report_error;

/// `bpd dap` arguments
///
/// there are none. everything a session needs is in the `launch` request the
/// client sends, which is where an editor's `launch.json` ends up — a flag here
/// would be a second place to configure the same thing, and the two would
/// disagree
#[derive(Debug, clap::Args)]
pub(crate) struct Args {}

pub(crate) fn run(_args: &Args) -> std::process::ExitCode {
    let served = bpd_dap::serve(
        &mut Engine,
        Box::new(std::io::stdin()),
        Box::new(std::io::stdout()),
    );

    match served {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // stdout is the protocol and the client is already gone, so this
            // goes to stderr — which is where an editor keeps its adapter log
            report_error(&error);
            std::process::ExitCode::FAILURE
        }
    }
}

/// the engine, as the adapter's launcher
struct Engine;

impl Launcher for Engine {
    fn launch(
        &mut self,
        configuration: &Configuration,
        output: Arc<dyn ProgramOutput>,
    ) -> Result<Started, Failed> {
        let interpreter = Capabilities::probe(&configuration.python)?;
        let arguments: Vec<OsString> = configuration
            .args
            .iter()
            .map(|argument| OsString::from(argument.clone()))
            .collect();

        let launched = bpd_engine::launch_piped(
            &interpreter,
            &configuration.program,
            &arguments,
            move |stdout, stderr| {
                forward(stdout, Stream::Stdout, &output);
                forward(stderr, Stream::Stderr, &output);
            },
        )?;

        Ok(match launched {
            Launched::Stopped(debuggee) => Started::Stopped(Box::new(Attached(debuggee))),
            Launched::ExitedBeforeStopping(status) => Started::ExitedBeforeStopping {
                code: status.code(),
            },
        })
    }
}

/// copy one of the program's streams to the client, a line at a time
///
/// a line at a time rather than a fixed block, because a block boundary can
/// fall inside a character and `\n` never can — so nothing arrives with a
/// replacement character bpd put there. anything the program wrote that is not
/// utf8 is still replaced, and that is the program's own bytes rather than the
/// reader's cut
fn forward(stream: impl Read + Send + 'static, which: Stream, output: &Arc<dyn ProgramOutput>) {
    let output = Arc::clone(output);
    let started = std::thread::Builder::new()
        .name(format!("bpd-dap-{which:?}"))
        .spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => output.wrote(which, &String::from_utf8_lossy(&line)),
                }
            }
        });

    if let Err(error) = started {
        // the pipe would fill and the program would stop on its next `print`,
        // which is a debuggee that looks hung for a reason nothing reported
        panic!(
            "the debuggee's {which:?} needs a thread to forward it, and one could not be started: {error}"
        );
    }
}

/// a launched debuggee, as the adapter's session
struct Attached(Debuggee);

impl Session for Attached {
    fn dispatch(
        &mut self,
        request: Request,
        reporting: &mut dyn Reporting,
    ) -> Result<Response, Failed> {
        Ok(self.0.dispatch(request, reporting)?)
    }

    fn held(&self) -> Vec<Stop> {
        self.0.held().to_vec()
    }

    fn interrupt(&self) -> Result<Box<dyn bpd_dap::Interrupt>, Failed> {
        Ok(Box::new(Reaching(self.0.interrupt())))
    }
}

/// the engine's interrupt, as the adapter's
struct Reaching(bpd_engine::Interrupt);

impl bpd_dap::Interrupt for Reaching {
    fn deliver(&mut self, request: &Request) -> Result<(), Failed> {
        Ok(self.0.deliver(request)?)
    }

    fn terminate(&mut self) -> Result<(), Failed> {
        Ok(self.0.terminate()?)
    }
}
