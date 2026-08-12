//! `bpd dap` — speak the debug adapter protocol, on stdio or on a socket
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
//!
//! under `--listen` the protocol is on the socket instead, and this process's
//! stdout carries exactly one line: where the adapter bound and what a client
//! has to present. that is why the port can be reported at all — it is the one
//! transport where stdout is not the protocol

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read, Write as _};
use std::sync::{Arc, Mutex};

use bpd_core::python::Capabilities;
use bpd_core::{Addressed, Reporting, Request, Response, SessionId, Stop};
use bpd_dap::{
    Configuration, Failed, Launcher, Listening, ProgramOutput, Reachable, Session, Started, Stream,
};
use bpd_engine::{Debuggee, Launched, Program};

use crate::report_error;

/// `bpd dap` arguments
///
/// there is one, and it chooses a transport rather than configuring a session.
/// everything a session needs is in the `launch` request the client sends,
/// which is where an editor's `launch.json` ends up — a flag for one of those
/// would be a second place to configure the same thing, and the two would
/// disagree
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// listen on this loopback TCP port for one client, instead of speaking on
    /// stdin and stdout
    ///
    /// `0` binds a port the operating system chooses. either way the port that
    /// was really bound, and the token a client must present, are printed on
    /// stdout as one line of json before the first connection is accepted
    ///
    /// the address is not configurable and the port is all this takes: a DAP
    /// message runs the debuggee's own code, so this listens on 127.0.0.1 and
    /// there is nothing to widen it to
    #[arg(long, value_name = "PORT")]
    listen: Option<u16>,
}

pub(crate) fn run(args: &Args) -> std::process::ExitCode {
    let served = match args.listen {
        Some(port) => listen(port).map_err(|error| Box::new(error) as Box<dyn std::error::Error>),
        // nothing else can connect to a pair of pipes somebody spawned, and
        // that is what makes `debugChildren` refusable on this transport rather
        // than half deliverable: a second session would be a second `bpd dap`
        // process, with an engine of its own that this debuggee is not in
        None => bpd_dap::serve(
            &Engine::default(),
            Box::new(std::io::stdin()),
            Box::new(std::io::stdout()),
            &Reachable::Nowhere,
        )
        .map_err(|error| Box::new(error) as Box<dyn std::error::Error>),
    };

    match served {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // stdout is the protocol, or the one line that said where it is, and
            // the client is already gone either way. this goes to stderr —
            // which is where an editor keeps its adapter log
            report_error(error.as_ref());
            std::process::ExitCode::FAILURE
        }
    }
}

/// bind, say where, and serve the one client that presents the token
///
/// the announcement is printed and **flushed** before anything is accepted, so
/// a script that reads a line and then connects cannot lose the race it would
/// have had to run if it had to guess a port
fn listen(port: u16) -> Result<(), bpd_dap::listen::Error> {
    let listening = Listening::bind(port)?;

    println!("{}", listening.announcement());
    if let Err(error) = std::io::stdout().flush() {
        // whoever started this is waiting on that line to know where to
        // connect, and a listener nobody can find is a hang with no cause
        panic!("the endpoint could not be written to stdout, so nothing can learn it: {error}");
    }

    listening.serve(&Engine::default(), &|said| eprintln!("bpd dap: {said}"))
}

/// the engine, as the adapter's launcher
///
/// it holds the debuggee rather than handing one out and forgetting it, because
/// a debugged fork is a second **session of the same debuggee** — two DAP
/// connections, one engine. the mutex is what the two share it through, and
/// [`Attached`] is one connection's view of one session of it
#[derive(Default)]
struct Engine {
    debuggee: Mutex<Option<Arc<Mutex<Debuggee>>>>,
}

/// what a lock on the engine's own field is only ever held to do
const HOLDING: &str =
    "nothing panics holding the debuggee: every path through it is one dispatch or one read";

impl Engine {
    /// the debuggee, once something has launched one
    fn held(&self) -> Result<Arc<Mutex<Debuggee>>, Failed> {
        self.debuggee
            .lock()
            .expect(HOLDING)
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| {
                Failed::from(
                    "nothing has been launched on this adapter yet, so there is no                      session to take up",
                )
            })
    }
}

impl Launcher for Engine {
    fn launch(
        &self,
        configuration: &Configuration,
        output: Arc<dyn ProgramOutput>,
    ) -> Result<Started, Failed> {
        if self.debuggee.lock().expect(HOLDING).is_some() {
            return Err("this adapter already has a program. a second connection to it                         takes up a session of that one, which is what a `startDebugging`                         reverse request asks a client to do"
                .into());
        }
        let interpreter = Capabilities::probe(&configuration.python)?;
        let arguments: Vec<OsString> = configuration
            .args
            .iter()
            .map(|argument| OsString::from(argument.clone()))
            .collect();

        let launched = bpd_engine::launch_piped(
            &interpreter,
            &Program::Script(configuration.program.clone()),
            &arguments,
            move |stdout, stderr| {
                forward(stdout, Stream::Stdout, &output);
                forward(stderr, Stream::Stderr, &output);
            },
        )?;

        Ok(match launched {
            Launched::Stopped(debuggee) => {
                let session = only_session_of(&debuggee)?;
                let held = Arc::new(Mutex::new(debuggee));
                *self.debuggee.lock().expect(HOLDING) = Some(Arc::clone(&held));
                Started::Stopped(Box::new(Attached {
                    debuggee: held,
                    session,
                }))
            }
            Launched::ExitedBeforeStopping(status) => Started::ExitedBeforeStopping {
                code: status.code(),
            },
        })
    }

    fn attach(&self, session: u64) -> Result<Started, Failed> {
        let held = self.held()?;
        let named = std::num::NonZeroU64::new(session)
            .map(SessionId::new)
            .ok_or_else(|| Failed::from("sessions are numbered from one, and 0 is not one"))?;
        {
            let debuggee = held.lock().expect(HOLDING);
            // refused rather than resolved to the nearest, which is the rule
            // every request naming a session already follows
            bpd_core::only_session(&debuggee.sessions(), Some(named), "taking up a session")?;
        }
        Ok(Started::Stopped(Box::new(Attached {
            debuggee: held,
            session: named,
        })))
    }
}

/// the session a freshly launched debuggee holds, which is its only one
fn only_session_of(debuggee: &Debuggee) -> Result<SessionId, Failed> {
    let sessions = debuggee.sessions();
    Ok(bpd_core::only_session(
        &sessions,
        None,
        "the session a launch produced",
    )?)
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

/// one DAP connection's view of one session of a debuggee
///
/// a DAP session **is** a connection, so this is where "the session this
/// connection serves" is written down. a request that names none is the
/// adapter's own — a wait, a resume, the breakpoint set — and it means this
/// connection's session rather than "whichever there is": with a debugged fork
/// open there are two, and the only-session rule would refuse rather than pick
struct Attached {
    debuggee: Arc<Mutex<Debuggee>>,
    session: SessionId,
}

impl Attached {
    fn debuggee(&self) -> std::sync::MutexGuard<'_, Debuggee> {
        self.debuggee.lock().expect(HOLDING)
    }
}

impl Session for Attached {
    fn dispatch(
        &mut self,
        asked: Addressed,
        reporting: &mut dyn Reporting,
    ) -> Result<Response, Failed> {
        let asked = match asked.session {
            Some(_) => asked,
            None => Addressed::to(self.session, asked.request),
        };
        Ok(self.debuggee().dispatch(asked, reporting)?)
    }

    /// the stops **this connection's** session holds
    ///
    /// filtered rather than all of them: another connection's stops are another
    /// program's threads, and a client shown one would be shown a thread it can
    /// neither walk nor resume
    fn held(&self) -> Vec<Stop> {
        self.debuggee()
            .held()
            .into_iter()
            .filter(|stop| stop.session == self.session)
            .collect()
    }

    fn interrupt(&self) -> Result<Box<dyn bpd_dap::Interrupt>, Failed> {
        Ok(Box::new(Reaching(
            self.debuggee().interrupt(Some(self.session))?,
        )))
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
