//! `bpd mcp` — speak the model context protocol on stdin and stdout
//!
//! the composition root for the agent front end, and the whole reason `bpd_mcp`
//! depends on `bpd_core` alone: the server says what it needs of a session, and
//! this is where `bpd_engine` is put behind it
//!
//! the program's own stdout and stderr are **pipes**, not this process's. this
//! server's stdout is the protocol, and one `print` from the debuggee in the
//! middle of a line would make that line unreadable json
//!
//! its stdin is **`/dev/null`**, for the mirror image of that reason: this
//! server's stdin is the protocol, and a debuggee reading it took the client's
//! next message out of the stream. `input()` in a debuggee raises `EOFError`,
//! and there is no tool that writes to one — see
//! [the MCP server](../../../docs/development/mcp.md)

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read};
use std::sync::Arc;
use std::thread::JoinHandle;

use bpd_core::python::Capabilities;
use bpd_core::{Addressed, Exit, Joined, Reporting, Response, SessionId, Stop};
use bpd_engine::{Debuggee, Forwarders, Launched, Program};
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

        // handed to the engine rather than held here: what a program wrote is
        // waited for before the program is reported over, and that is the
        // engine's rule for every front end rather than one this server keeps
        // for itself
        let launched = bpd_engine::launch_piped(
            &interpreter,
            &Program::Script(configuration.program.clone()),
            &arguments,
            move |stdout, stderr| {
                Forwarders::on(vec![
                    forward(stdout, Stream::Stdout, &output),
                    forward(stderr, Stream::Stderr, &output),
                ])
            },
        )?;

        Ok(match launched {
            Launched::Stopped(debuggee) => Started::Stopped(Box::new(Attached(debuggee))),
            // the interpreter's own words about why the program never started
            // are already on the answer rather than arriving after it: the
            // engine waited for them before handing this back
            Launched::ExitedBeforeStopping(status) => Started::ExitedBeforeStopping {
                code: status.code(),
            },
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
        asked: Addressed,
        reporting: &mut dyn Reporting,
    ) -> Result<Response, Failed> {
        Ok(self.0.dispatch(asked, reporting)?)
    }

    fn held(&self) -> Vec<Stop> {
        self.0.held()
    }

    fn sessions(&self) -> Vec<Joined> {
        self.0.joined()
    }

    fn ended(&self, session: Option<SessionId>) -> Option<Exit> {
        self.0.exit_of(session)
    }

    fn terminate(&mut self, session: Option<SessionId>) -> Result<(), Failed> {
        Ok(self.0.interrupt(session)?.terminate()?)
    }
}
