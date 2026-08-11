//! [`Reporting`] sinks for tests that drive a debuggee
//!
//! a running program says three kinds of thing that answer nothing — a
//! logpoint's record, a pause's acknowledgement, and a child process it
//! started. a test that sets no logpoint, arms no pause and starts no child
//! should say so rather than quietly absorb one, because a fact about the
//! program arriving where nobody looks is how a test passes while proving
//! something else

use bpd_core::{LogRecord, Reporting, Spawn};

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
}

/// a sink that keeps every child the program started
///
/// the mirror of [`Logs`], for a test about spawning. a logpoint reaching one
/// is a test that set a breakpoint it did not mean to
#[derive(Debug, Default)]
pub struct Children {
    /// every child, in the order it was reported
    pub started: Vec<Spawn>,
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
}
