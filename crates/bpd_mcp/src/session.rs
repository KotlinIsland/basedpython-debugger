//! what this adapter needs of a debug session, and nothing more
//!
//! `bpd_mcp` depends on `bpd_core` alone, for the reason `bpd_dap` does: an
//! adapter that could reach `bpd_engine` would be an adapter shaped by how the
//! agent happens to report something, and how the agent reports something would
//! become what an agent using bpd sees. so the engine arrives through these
//! traits, and the `bpd` binary is where the two are put together
//!
//! the shape is narrower than DAP's. there is no `Interrupt` here, and that is
//! the whole difference between the two front ends: every control tool carries a
//! deadline, so the server is never blocked on a program indefinitely and never
//! needs a second thread to reach one that is running

use std::path::PathBuf;
use std::sync::Arc;

use bpd_core::{Reporting, Request, Response, Stop};

/// something a session could not do, as an adapter has to render it
///
/// boxed on purpose. a front end that matched on the *kind* of failure would be
/// deciding something, and a decision belongs in the core where both front ends
/// get it. what an adapter legitimately does with a failure is report it, and
/// [`describe`] is how — with every cause behind it, since the engine's failures
/// carry the socket or the process that actually went wrong
pub type Failed = Box<dyn std::error::Error + Send + Sync + 'static>;

/// a debug session: something that answers the capability surface
pub trait Session {
    /// answer one request
    ///
    /// `reporting` takes what the debuggee says while it runs, which is not the
    /// answer to anything — a logpoint's record, and the acknowledgement of a
    /// pause
    fn dispatch(
        &mut self,
        request: Request,
        reporting: &mut dyn Reporting,
    ) -> Result<Response, Failed>;

    /// the stops held right now, in the order the session learned of them
    ///
    /// a stop holds one thread and the rest of the program keeps running, so
    /// several can be outstanding at once. a tool that names no stop is answered
    /// against this through [`bpd_core::only_stop`]
    fn held(&self) -> Vec<Stop>;

    /// end the debuggee
    ///
    /// the last resort rather than a resume: a program that is running cannot be
    /// asked anything, so a client that wants to be finished with one has
    /// nothing else to say
    fn terminate(&mut self) -> Result<(), Failed>;
}

/// which of its own streams the program wrote to
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// the program's stdout
    Stdout,
    /// the program's stderr
    Stderr,
}

impl std::fmt::Display for Stream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        })
    }
}

/// where the debuggee's own output goes
///
/// the server's stdout **is** the protocol, so the program cannot share it: one
/// `print` in the middle of a message and the line the client reads is not json.
/// the launcher hands the program's streams here instead, and the server carries
/// what they said on the answer to whatever control tool let the program run
///
/// `Send + Sync`, and taking `&self`, because a stream is forwarded by a thread
/// of its own — stdout and stderr are separate pipes and reading one must never
/// wait on the other
pub trait ProgramOutput: Send + Sync + 'static {
    /// the program wrote a line
    fn wrote(&self, stream: Stream, text: &str);
}

/// what a `launch` tool call asked for
#[derive(Debug, Clone)]
pub struct Configuration {
    /// the script to run
    pub program: PathBuf,
    /// the interpreter, resolved on `PATH` like any command
    pub python: String,
    /// arguments for the program, exactly as it receives them
    pub args: Vec<String>,
}

/// something that starts a debuggee
pub trait Launcher {
    /// start a program and return it held before its first statement
    ///
    /// `output` is where the program's own stdout and stderr go, and it has to
    /// be reading them before anything waits on the process: a pipe nobody
    /// reads fills up, and a process whose pipe is full stops
    fn launch(
        &mut self,
        configuration: &Configuration,
        output: Arc<dyn ProgramOutput>,
    ) -> Result<Started, Failed>;
}

/// what came of a launch
///
/// deliberately closed, for the reason `bpd_engine::Launched` is: a third
/// outcome is something the adapter has to decide about rather than absorb
pub enum Started {
    /// the program is attached and held before its first statement
    Stopped(Box<dyn Session>),

    /// the program finished before it could be stopped
    ///
    /// a program that does not compile never reaches its first statement. it has
    /// already said why on its own stderr, in the interpreter's own words, so
    /// what is left to report is that it ended and with what
    ExitedBeforeStopping {
        /// the exit code, or `None` when a signal ended it
        code: Option<i32>,
    },
}

/// a failure and every cause behind it, on one line
///
/// an agent shown only the outermost link is shown "the control connection to
/// the agent failed" with nothing about which failure that was
pub fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    let mut described = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        described.push_str(": ");
        described.push_str(&cause.to_string());
        source = cause.source();
    }
    described
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("the outer thing failed")]
    struct Outer {
        #[source]
        source: Inner,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("the socket was closed")]
    struct Inner;

    #[test]
    fn a_description_carries_every_cause_behind_the_failure() {
        assert_eq!(
            describe(&Outer { source: Inner }),
            "the outer thing failed: the socket was closed"
        );
    }
}
