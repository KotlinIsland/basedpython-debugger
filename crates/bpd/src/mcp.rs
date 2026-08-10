//! `bpd mcp` — speak the model context protocol on stdin and stdout
//!
//! the composition root for the agent front end, and the whole reason `bpd_mcp`
//! depends on `bpd_core` alone: the server says what it needs of a session, and
//! this is where `bpd_engine` is put behind it
//!
//! the program's own stdout and stderr are **pipes**, not this process's. this
//! server's stdout is the protocol, and one `print` from the debuggee in the
//! middle of a line would make that line unreadable json

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read};
use std::sync::Arc;
use std::thread::JoinHandle;

use bpd_core::python::Capabilities;
use bpd_core::{Reporting, Request, Response, Stop};
use bpd_engine::{Debuggee, Launched};
use bpd_mcp::{Configuration, Failed, Launcher, ProgramOutput, Session, Started, Stream};

use crate::report_error;

/// `bpd mcp` arguments
///
/// there are none. everything a session needs arrives in the `launch` tool
/// call, which is where an agent says what to debug — a flag here would be a
/// second place to configure the same thing, and the two would disagree
#[derive(Debug, clap::Args)]
pub(crate) struct Args {}

pub(crate) fn run(_args: &Args) -> std::process::ExitCode {
    let served = bpd_mcp::serve(
        &mut Engine,
        Box::new(std::io::stdin()),
        Box::new(std::io::stdout()),
    );

    match served {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // stdout is the protocol and the client is already gone, so this
            // goes to stderr — which is where a host keeps its server log
            report_error(&error);
            std::process::ExitCode::FAILURE
        }
    }
}

/// the engine, as the server's launcher
struct Engine;

impl Launcher for Engine {
    fn launch(
        &mut self,
        configuration: &Configuration,
        output: Arc<dyn ProgramOutput>,
    ) -> Result<Started, Failed> {
        let interpreter = Capabilities::probe(std::path::Path::new(&configuration.python))?;
        let arguments: Vec<OsString> = configuration
            .args
            .iter()
            .map(|argument| OsString::from(argument.clone()))
            .collect();

        let forwarding = Arc::new(std::sync::Mutex::new(Vec::new()));
        let launched =
            bpd_engine::launch_piped(&interpreter, &configuration.program, &arguments, {
                let forwarding = Arc::clone(&forwarding);
                move |stdout, stderr| {
                    let mut threads = forwarding
                        .lock()
                        .expect("nothing panics holding the forwarders: it is one push");
                    threads.push(forward(stdout, Stream::Stdout, &output));
                    threads.push(forward(stderr, Stream::Stderr, &output));
                }
            })?;

        Ok(match launched {
            Launched::Stopped(debuggee) => Started::Stopped(Box::new(Attached(debuggee))),
            Launched::ExitedBeforeStopping(status) => {
                // the forwarders are joined before this returns, so the
                // interpreter's own words about why the program never started
                // are on the answer rather than arriving after it. a client
                // told nothing here has no way to find out what happened
                let mut threads = forwarding
                    .lock()
                    .expect("nothing panics holding the forwarders: it is one drain");
                for thread in threads.drain(..) {
                    if thread.join().is_err() {
                        return Err("a thread forwarding the debuggee's output panicked, so \
                                    what the program said about why it did not start is lost"
                            .into());
                    }
                }
                Started::ExitedBeforeStopping {
                    code: status.code(),
                }
            }
        })
    }
}

/// copy one of the program's streams to the server, a line at a time
///
/// a line at a time rather than a fixed block, because a block boundary can fall
/// inside a character and `\n` never can — so nothing arrives with a replacement
/// character bpd put there
fn forward(
    stream: impl Read + Send + 'static,
    which: Stream,
    output: &Arc<dyn ProgramOutput>,
) -> JoinHandle<()> {
    let output = Arc::clone(output);
    std::thread::Builder::new()
        .name(format!("bpd-mcp-{which}"))
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
        })
        .unwrap_or_else(|error| {
            // the pipe would fill and the program would stop on its next
            // `print`, which is a debuggee that looks hung for a reason nothing
            // reported
            panic!(
                "the debuggee's {which} needs a thread to forward it, and one \
                 could not be started: {error}"
            )
        })
}

/// a launched debuggee, as the server's session
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

    fn terminate(&mut self) -> Result<(), Failed> {
        Ok(self.0.interrupt().terminate()?)
    }
}
