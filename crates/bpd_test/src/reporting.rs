//! [`Reporting`] sinks for tests that drive a debuggee
//!
//! a running program says several kinds of thing that answer nothing — every
//! one of them is a `bpd_core::Told`, and a logpoint's record, a pause's
//! acknowledgement and a child process it started are three. a test that sets no
//! logpoint, arms no pause and starts no child should say so rather than quietly
//! absorb one, because a fact about the program arriving where nobody looks is
//! how a test passes while proving something else

use bpd_core::{Blindspot, LogRecord, Reporting, SessionId, Spawn};

/// a sink nothing is supposed to reach, which panics naming what did
///
/// this is what most tests want. a program that logs, pauses or starts a child
/// in a test that expected none of those has done something the test is not
/// about, and the failure says which
pub struct Unreported;

impl Reporting for Unreported {
    fn logged(&mut self, record: LogRecord) {
        panic!("no logpoint was set, and the agent sent {record:?}")
    }

    fn pausing(&mut self, running: Vec<u64>) {
        panic!("no pause was armed, and the agent acknowledged one naming {running:?}")
    }

    fn spawned(&mut self, child: Spawn) {
        panic!("this program was not expected to start a child, and it started {child}")
    }

    fn blind_to(&mut self, blindspot: Blindspot) {
        panic!("this interpreter announced a blind spot nothing here is about: {blindspot}")
    }

    fn attached(&mut self, session: SessionId) {
        panic!("nothing here debugs a forked child, and {session} joined this debuggee")
    }
}

/// a sink that keeps every log record, for a test about logpoints
///
/// it keeps all of them, so it is for a logpoint that fires a countable number
/// of times. a test about a logpoint on a hot line needs a sink of its own
#[derive(Debug, Default)]
pub struct Logs {
    /// every record, in the order it arrived
    pub records: Vec<LogRecord>,
}

impl Reporting for Logs {
    fn logged(&mut self, record: LogRecord) {
        self.records.push(record);
    }

    fn pausing(&mut self, running: Vec<u64>) {
        panic!("no pause was armed, and the agent acknowledged one naming {running:?}")
    }

    fn spawned(&mut self, child: Spawn) {
        panic!("this program was not expected to start a child, and it started {child}")
    }

    fn blind_to(&mut self, blindspot: Blindspot) {
        panic!("this interpreter announced a blind spot nothing here is about: {blindspot}")
    }

    fn attached(&mut self, session: SessionId) {
        panic!("nothing here debugs a forked child, and {session} joined this debuggee")
    }
}

/// a sink for a test in which a **second session** joins the debuggee
///
/// a debugged fork opens a connection of its own, and the engine says so as it
/// happens. it is the one report that must not be absorbed: the session that
/// joined is **held**, so a test that ignored it would be a test leaving a
/// stopped process behind
#[derive(Debug, Default)]
pub struct Joining {
    /// every session that joined, in the order they arrived
    pub joined: Vec<SessionId>,
}

impl Reporting for Joining {
    fn logged(&mut self, record: LogRecord) {
        panic!("no logpoint was set, and the agent sent {record:?}")
    }

    fn pausing(&mut self, running: Vec<u64>) {
        panic!("no pause was armed, and the agent acknowledged one naming {running:?}")
    }

    fn spawned(&mut self, child: Spawn) {
        panic!("this program was not expected to start a child, and it started {child}")
    }

    fn blind_to(&mut self, blindspot: Blindspot) {
        panic!("this interpreter announced a blind spot nothing here is about: {blindspot}")
    }

    fn attached(&mut self, session: SessionId) {
        self.joined.push(session);
    }
}

/// a sink that keeps every child the program started
///
/// the mirror of [`Logs`], for a test about spawning. a logpoint reaching one
/// is a test that set a breakpoint it did not mean to
#[derive(Debug, Default)]
pub struct Children {
    /// every child, in the order it was reported
    pub started: Vec<Spawn>,
    /// every way of starting one this interpreter does not let bpd see
    ///
    /// a test that asserts on `started` has to be able to tell "no child" from
    /// "no child bpd can see", or it is a test that passes on a blind spot
    pub unseen: Vec<Blindspot>,
    /// every session that joined the debuggee, in the order they arrived
    ///
    /// a debugged fork is one. it is **held** when it arrives, so a test that
    /// ignored these would be a test that left a stopped process behind
    pub joined: Vec<SessionId>,
}

impl Reporting for Children {
    fn logged(&mut self, record: LogRecord) {
        panic!("no logpoint was set, and the agent sent {record:?}")
    }

    fn pausing(&mut self, running: Vec<u64>) {
        panic!("no pause was armed, and the agent acknowledged one naming {running:?}")
    }

    fn spawned(&mut self, child: Spawn) {
        self.started.push(child);
    }

    fn blind_to(&mut self, blindspot: Blindspot) {
        self.unseen.push(blindspot);
    }

    /// a debugged fork opened a session of its own
    ///
    /// kept beside the children rather than a panic: `Children` is the sink for
    /// a test about a program that starts them, and one that forks under
    /// `debug_children` produces both — the report of the fork, and the session
    /// the child then opened
    fn attached(&mut self, session: SessionId) {
        self.joined.push(session);
    }
}
