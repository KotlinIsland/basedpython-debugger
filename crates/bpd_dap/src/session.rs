//! what this adapter needs of a debug session, and nothing more
//!
//! `bpd_dap` depends on `bpd_core` alone. an adapter that could reach
//! `bpd_engine` would be an adapter shaped by how the agent happens to report
//! something, and how the agent reports something would become what a DAP
//! client sees — so the engine arrives through these traits instead, and the
//! `bpd` binary is where the two are put together
//!
//! the shape is deliberately narrow: answer a [`Request`], say which stops are
//! held, and hand out something that reaches the program while an answer is
//! being waited for. everything else a debugger does is a `Request`, which is
//! the whole point of the capability surface being data

use bpd_core::{Addressed, Reporting, Request, Response, Stop};

/// something a session could not do, as an adapter has to render it
///
/// boxed on purpose. a front end that matched on the *kind* of failure would be
/// deciding something, and a decision belongs in the core where both front ends
/// get it. what an adapter legitimately does with a failure is show it, and
/// [`crate::describe`] is how — with every cause behind it, since the engine's
/// failures carry the socket or the process that actually went wrong
pub type Failed = Box<dyn std::error::Error + Send + Sync + 'static>;

/// where a **second** session of this adapter's debuggee can be reached
///
/// DAP's answer to a debuggee that became two processes is the `startDebugging`
/// reverse request: the adapter asks the client to start a second debug session.
/// that only works if the second session can reach the same engine — a client
/// that spawned this adapter on a pipe would start another `bpd dap` process,
/// and another process has another engine, with no [`Session`] of the first one
/// in it
///
/// so this is the difference between the two transports, and it is a difference
/// in what is **possible** rather than in what is configured. a debugged child
/// is refused on a transport that cannot carry a second session, with the reason
/// and what to run instead — a client that was told to start a session it cannot
/// have would leave the child held for ever, which is the hang this whole
/// feature is arranged around
#[derive(Debug, Clone)]
pub enum Reachable {
    /// nothing else can connect: this adapter is on the pipes it was spawned with
    Nowhere,

    /// a client can open a second connection to this adapter
    At {
        /// the loopback address to connect to
        host: String,
        /// the port
        port: u16,
        /// the header a connection presents its token in
        header: &'static str,
        /// the token, which is this listener's and is the same for every session
        ///
        /// one token rather than one per child, and deliberately: a second token
        /// is a second lifetime to get wrong, and the connection that would use
        /// it is being asked for by an adapter the client has **already**
        /// authenticated to with this one
        token: String,
    },
}

/// a debug session: something that answers the capability surface
///
/// `Send` because a second connection is served on a thread of its own, and
/// what it serves is a session of the same debuggee
pub trait Session: Send {
    /// answer one request, addressed to the session it is for
    ///
    /// the address is [`bpd_core::Addressed`] rather than a bare [`Request`]
    /// because a stop is only unique within one debugged process. the adapter
    /// puts one on every request it makes; a request that names a session this
    /// is not is refused rather than answered here
    ///
    /// `reporting` takes what the debuggee says while it runs, which is not the
    /// answer to anything — a logpoint's record, and the acknowledgement of a
    /// pause armed through an [`Interrupt`]
    fn dispatch(
        &mut self,
        asked: Addressed,
        reporting: &mut dyn Reporting,
    ) -> Result<Response, Failed>;

    /// the stops held right now, in the order the session learned of them
    ///
    /// a stop holds one thread and the rest of the program keeps running, so a
    /// second thread can stop while a first is held — and it arrives on the
    /// connection rather than as the answer to anything. the adapter compares
    /// this against what it has already told the client about, which is how a
    /// `stopped` event gets emitted for a thread nobody asked about
    fn held(&self) -> Vec<Stop>;

    /// a handle that reaches the program while this session is waiting on it
    fn interrupt(&self) -> Result<Box<dyn Interrupt>, Failed>;
}

/// a handle that reaches a debuggee that is running
///
/// an event driven front end spends most of a session waiting for the program
/// to do something, and the two things it may then be asked — pause, and stop —
/// are precisely the two that are about a program which is running. so they are
/// on a handle of their own, which the adapter moves to the thread that reads
/// its client
pub trait Interrupt: Send {
    /// send a request without waiting for the answer to it
    ///
    /// only [`Request::Pause`] can be sent to a running program. the
    /// acknowledgement arrives at the [`Reporting`] sink of whatever the
    /// session is waiting on, because that is where the reading end is
    fn deliver(&mut self, request: &Request) -> Result<(), Failed>;

    /// end the debuggee
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

/// where the debuggee's own output goes
///
/// the adapter's stdout **is** the protocol, so the program cannot share it:
/// one `print` in the middle of a message and every message after it is
/// unreadable. the launcher hands the program's streams here instead, and the
/// adapter turns each line into an `output` event
///
/// `Send + Sync`, and taking `&self`, because a stream is forwarded by a thread
/// of its own — stdout and stderr are separate pipes and reading one must never
/// wait on the other
pub trait ProgramOutput: Send + Sync + 'static {
    /// the program wrote a line
    fn wrote(&self, stream: Stream, text: &str);
}

/// something that starts a debuggee from a client's launch configuration
///
/// `&self` and `Sync` because more than one connection can be served at once:
/// a debugged fork is a second session of the **same** debuggee, so the two
/// connections reach one of these and it is what holds the thing they share
pub trait Launcher: Sync {
    /// start a program and return it held before its first statement
    ///
    /// `output` is where the program's own stdout and stderr go, and it has to
    /// be reading them before anything waits on the process: a pipe nobody
    /// reads fills up, and a process whose pipe is full stops
    fn launch(
        &self,
        configuration: &crate::Configuration,
        output: std::sync::Arc<dyn ProgramOutput>,
    ) -> Result<Started, Failed>;

    /// take up a session this launcher's debuggee already holds
    ///
    /// what a connection that arrived because of a `startDebugging` reverse
    /// request asks for. it starts nothing: the process is already there, it is
    /// already **held** — a debugged fork at the line that forked, and a
    /// debugged `exec` at its own interpreter startup — and what this hands back
    /// is a second view of the one debuggee
    ///
    /// # errors
    ///
    /// when nothing has been launched yet, and when the id names no session this
    /// debuggee holds. an id that resolved to the nearest session would be a
    /// connection driving a program nobody named
    fn attach(&self, session: u64) -> Result<Started, Failed>;
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
    /// a program that does not compile never reaches its first statement. it
    /// has already said why on its own stderr, in the interpreter's own words,
    /// so what is left to report is that it ended and with what
    ExitedBeforeStopping {
        /// the exit code, or `None` when a signal ended it
        code: Option<i32>,
    },
}

/// a failure and every cause behind it, on one line
///
/// the engine reports a socket or a process failure as a chain, and a client
/// that is shown only the outermost link is shown "the control connection to
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
