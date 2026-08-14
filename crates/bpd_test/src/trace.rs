//! what a test asked the debuggee for and what happened, in the order it did
//!
//! a test that drives one stop reports a failure well enough on its own: there
//! is one stop, and printing it says everything. a test that drives **several**
//! does not. a step that lands in the wrong place is the shape of failure this
//! exists for, and the thing that diagnoses one is not the landing — it is the
//! sequence that reached it, because a step is only wrong relative to where it
//! started and what was asked
//!
//! so this records as it goes rather than reconstructing afterwards. a
//! reconstruction can only use what the test still holds, and the whole problem
//! is that the interesting part is what it has already dropped
//!
//! it is a [`Reporting`] sink as well as a recorder, because the things a
//! program says that answer nothing belong in the same sequence: a logpoint's
//! record between two steps is evidence about which line ran. it still refuses
//! them the way [`crate::reporting::Unreported`] does — a report nothing
//! expected is a program doing something the test is not about — and the refusal
//! now carries the sequence that led there

use std::fmt;

use bpd_core::{Blindspot, LogRecord, Reporting, Running, SessionId, Spawn, StepKind, Stop};
use bpd_engine::Debuggee;

/// one thing that happened, as it is rendered in the sequence
#[derive(Debug)]
enum Entry {
    /// the test asked the debuggee for something
    Asked(String),
    /// the debuggee answered a resume with an outcome
    Ran(String),
    /// the program said something nobody asked for
    Said(String),
}

impl fmt::Display for Entry {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        // the arrow says which direction it went, which is the thing a reader
        // is scanning for: a landing that does not match the ask above it is
        // the failure, and putting them in one column hides exactly that
        match self {
            Self::Asked(what) => write!(out, "  -> {what}"),
            Self::Ran(what) => write!(out, "  <- {what}"),
            Self::Said(what) => write!(out, "   . {what}"),
        }
    }
}

/// the sequence a test built, ready to print in a failure
///
/// hand it to [`Self::run`] and [`Self::stepped`] instead of driving the
/// debuggee directly, and every assertion message that includes it says how the
/// program got where it is
#[derive(Debug, Default)]
pub struct Trace {
    happened: Vec<Entry>,
}

impl Trace {
    /// resume everything and record what the program did
    ///
    /// the same call the debuggee offers, with the outcome written down. a test
    /// that called `Debuggee::run` directly would get the outcome and leave no
    /// trace of it, which is the gap this type is for
    ///
    /// # errors
    ///
    /// whatever the debuggee answers with, unchanged — a failed resume is the
    /// test's to report, and swallowing one here would hide it behind a trace
    pub fn run(&mut self, debuggee: &mut Debuggee) -> bpd_engine::Result<Running> {
        self.happened.push(Entry::Asked("run".to_string()));
        let outcome = debuggee.run(self)?;
        self.saw(&outcome);
        Ok(outcome)
    }

    /// wait for what a request already set in motion, and record it
    ///
    /// # errors
    ///
    /// whatever the debuggee answers with, unchanged
    pub fn wait(&mut self, debuggee: &mut Debuggee) -> bpd_engine::Result<Running> {
        self.happened.push(Entry::Asked("wait".to_string()));
        let outcome = debuggee.wait(self)?;
        self.saw(&outcome);
        Ok(outcome)
    }

    /// step the only held thread, wait for it to land, and record both halves
    ///
    /// **the ask and the landing together**, which is the pair a stepping
    /// failure is read from. a landing on its own says where the program is; it
    /// does not say that the step which produced it was an `over` rather than an
    /// `into`, and that is usually the bug
    ///
    /// # panics
    ///
    /// when the step does not land on a stop, naming the whole sequence — a
    /// program that ran to its end instead of stopping is the case where "what
    /// led here" is the only thing worth having
    pub fn stepped(&mut self, debuggee: &mut Debuggee, kind: StepKind) -> Stop {
        // `StepKind` already renders as "step over" — naming it again here
        // would read as two words for one thing
        self.happened.push(Entry::Asked(kind.to_string()));
        let stepping = debuggee.the_step(kind);
        match stepping {
            Ok(threads) => self
                .happened
                .push(Entry::Ran(format!("stepping {threads:?}"))),
            Err(error) => panic!("the {kind} was refused: {error}\n{self}"),
        }

        match self.wait(debuggee) {
            Ok(Running::Stopped { stop, .. }) => stop,
            Ok(other) => panic!("the {kind} did not land, and answered {other:?}\n{self}"),
            Err(error) => panic!("the wait for the {kind} failed: {error}\n{self}"),
        }
    }

    /// write down an outcome the test produced some other way
    ///
    /// for a resume this type does not wrap. the sequence is only worth having
    /// if it is complete, so a test doing something of its own says so here
    /// rather than leaving a hole nobody can see
    pub fn saw(&mut self, running: &Running) {
        self.happened.push(Entry::Ran(rendered(running)));
    }

    /// write down something the test asked for that this type does not wrap
    pub fn asked(&mut self, what: impl fmt::Display) {
        self.happened.push(Entry::Asked(what.to_string()));
    }

    /// whether anything at all has been recorded
    ///
    /// what a test asserts on to know the trace is really being filled, rather
    /// than believing a printed sequence that is empty for a reason nobody
    /// noticed
    pub fn is_empty(&self) -> bool {
        self.happened.is_empty()
    }

    /// how many things have happened
    pub fn len(&self) -> usize {
        self.happened.len()
    }

    /// record an unasked report and then refuse it, with the sequence
    ///
    /// the refusal is [`crate::reporting::Unreported`]'s, and the difference is
    /// the trace: a program that logged between two steps did something the test
    /// is not about, and *which two steps* is the part that says why
    fn refuse(&mut self, what: &str) -> ! {
        self.happened.push(Entry::Said(what.to_string()));
        panic!("nothing here expected the program to say {what}\n{self}")
    }
}

/// one outcome, short enough to read in a column
///
/// a stop renders as where it is and why, because that is what a stepping
/// failure is compared against. the rest render as what they are — a test whose
/// program exited where a stop was expected needs the exit and not its fields
fn rendered(running: &Running) -> String {
    match running {
        Running::Stopped { stop, .. } => format!("stopped: {} {:?}", stop.thread, stop.reason),
        Running::Exited { status, output, .. } => format!("exited {status}, output {output:?}"),
        Running::Ended { .. } => "ended, with no exit status bpd can read".to_string(),
        Running::Finishing { threads, .. } => format!("finishing, still holding {threads:?}"),
        Running::StillRunning { waited, .. } => format!("still running after {waited:?}"),
    }
}

impl fmt::Display for Trace {
    /// the sequence, one thing a line, oldest first
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.happened.is_empty() {
            // said rather than printed as nothing. an empty trace under a
            // failure means the test drove the debuggee some other way, and a
            // reader who is not told that goes looking for a bug in the program
            return write!(
                out,
                "nothing was recorded: this test drove the debuggee directly"
            );
        }
        writeln!(out, "what led here, oldest first:")?;
        for (at, entry) in self.happened.iter().enumerate() {
            writeln!(out, "{:>3}. {entry}", at + 1)?;
        }
        Ok(())
    }
}

impl Reporting for Trace {
    fn logged(&mut self, record: LogRecord) {
        self.refuse(&format!("a log record: {record:?}"))
    }

    fn pausing(&mut self, running: Vec<u64>) {
        self.refuse(&format!("a pause is armed, naming {running:?}"))
    }

    fn spawned(&mut self, child: Spawn) {
        self.refuse(&format!("it started a child: {child}"))
    }

    fn blind_to(&mut self, blindspot: Blindspot) {
        self.refuse(&format!("this interpreter has a blind spot: {blindspot}"))
    }

    fn attached(&mut self, session: SessionId) {
        self.refuse(&format!("{session} joined this debuggee"))
    }
}
