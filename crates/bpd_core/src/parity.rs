//! the capability surface, enumerated, so the parity rule can be a test
//!
//! the rule is that no capability exists in one front end and not the other. it
//! was a policy someone had to remember for as long as a capability was a
//! *method*, because rust cannot enumerate methods. as data it can be checked,
//! and this module is what a check is written against:
//!
//! - [`surface`] is one [`Request`] of every variant
//! - [`Facet`] names the capabilities that are **not** variants, because a rule
//!   that only enumerates variants misses a capability carried in a field
//! - [`Reach`] is how a front end says it gets at one, including saying that it
//!   cannot
//!
//! that is the half about what a client **asks for**. the other half is what the
//! debugger **says**, and it was held by a trait with no default bodies — which
//! forces an implementation to exist and is satisfied by an empty one. so it is
//! enumerated here too:
//!
//! - [`Told`] is one of every fact the debugger produces that a client did not
//!   ask for: the [`Reporting`] methods, and the outcomes of a running program
//! - [`Carried`] is how a front end says it passes one on, including saying that
//!   it cannot
//! - [`say`] and [`ran`] build one of each, so that a check can hand them to an
//!   adapter and then go looking for them in what the client was really told
//!
//! nothing here knows what DAP or MCP are. each adapter writes its own
//! `reach_of` and `carriage_of` — exhaustive matches with no catch-all arm, so a
//! capability or a report added to the core is a compile error there rather than
//! one that front end silently does not have — and the parity test compares the
//! two answers

use std::num::{NonZeroU32, NonZeroU64};
use std::process::ExitStatus;
use std::time::Duration;

use crate::breakpoint::{LogRecord, SourceBreakpoint};
use crate::frame::{FrameId, Scope};
use crate::query::{SnapshotId, StateQuery, Wanted};
use crate::script::{Budget, Script, Step};
use crate::session::{Forwarded, Reporting, Request, Running, SessionId, Threads};
use crate::spawn::{Blindspot, Spawn, Verdict};
use crate::stop::{Reported, StepKind, StopReason};
use crate::thread::Which;
use crate::value::Detail;

/// how a front end gets at one capability
///
/// [`Reach::Unreachable`] is the variant that makes this worth having. a front
/// end whose protocol genuinely cannot express a capability says so **here**,
/// with the reason, rather than leaving a gap that reads as an oversight — and
/// the parity test can then tell a named, justified exception apart from a
/// capability nothing can reach at all
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// a request or tool of the front end's own protocol maps onto it
    Direct(&'static str),

    /// the front end makes it without being asked, and this is when
    OnItsOwn(&'static str),

    /// this front end cannot use it in this shape, because it is a composition
    ///
    /// not a gap. the capability is reachable — the *combination* is what the
    /// protocol cannot use
    Composed {
        /// the capabilities it is a composition of, by [`Request::name`]
        of: &'static [&'static str],
        /// why the composed form is unusable here
        why: &'static str,
    },

    /// this front end's protocol cannot express it at all
    ///
    /// the reason is not decoration. a capability that no front end can reach is
    /// a capability nobody has, and the only thing that separates the two is a
    /// stated reason that someone had to write down
    Unreachable {
        /// why the protocol cannot carry it
        why: &'static str,
    },
}

impl Reach {
    /// whether this front end can get at the capability at all
    pub const fn reaches(&self) -> bool {
        !matches!(self, Self::Unreachable { .. })
    }
}

/// a capability carried **inside** a [`Request`] rather than being one
///
/// the parity rule is about capabilities, and not every capability is a variant.
/// a hit condition is a field of [`SourceBreakpoint`]; the bounds on how much of
/// a value is read are a field of three requests. a front end can implement
/// every variant and still not offer either, and enumerating variants would
/// never find it
///
/// deliberately closed and deliberately short. each entry is one an adapter has
/// had to answer for, and adding one means going and answering for it in both
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Facet {
    /// a breakpoint's typed hit condition — [`crate::HitCondition`]
    HitCondition,

    /// how much of a value one request may read — [`Detail`]
    ValueBounds,

    /// which session a request is for — [`crate::Addressed`]
    ///
    /// carried beside a [`Request`] rather than inside one, and a capability
    /// all the same: a front end that never said which session it meant would
    /// be a front end that cannot reach a second one, and the two of them
    /// drifting over that is exactly what the parity rule is for
    Session,

    /// where the interpreter really is, for a frame reported in `.by` source —
    /// [`crate::Mapping`]
    ///
    /// carried on the **answer** to a stack rather than in the request, which
    /// is the same problem one step along: a front end can implement every
    /// variant, report a mapped frame's `.by` location, and drop the generated
    /// one — and a user who does not believe the debugger then has no way to
    /// see what it saw. enumerating variants would never find it
    GeneratedLocation,

    /// the debuggee running on a **terminal the front end's client owns**
    ///
    /// not carried by any request at all, which is why it is here rather than
    /// being one: a front end that has taken the program's output over has
    /// taken bpd's own streams for itself, so the debuggee gets pipes and an
    /// empty stdin — and a program that reads its input, or asks `isatty()`,
    /// behaves differently because of it. whether a front end can put a real
    /// terminal back is a capability of the front end, and a rule that only
    /// enumerated requests would never find it
    ///
    /// it is the one facet that is not a payload. every other entry here is a
    /// shape a protocol either has a field for or has not; this one is a thing
    /// the client has to **have**
    Terminal,

    /// replacing a file's code **under a frame that is running it** —
    /// [`Request::ReplaceCode`]
    ///
    /// a field of one request, and a capability all the same: it is the only
    /// way to reach a replacement the ordinary rule refuses, and it trades the
    /// guarantee that the process never runs two versions of one function for
    /// a report of every frame that will. a front end without it cannot offer
    /// the trade, and one that took the flag and dropped the report would be
    /// making the trade **for** its user without saying so
    LiveReplacement,

    /// the breakpoint a breakpoint waits for — [`SourceBreakpoint::after`]
    ///
    /// a field of a breakpoint, like a hit condition, and a capability all the
    /// same: without it a front end can set every breakpoint and offer no
    /// sequence at all. the **report** is the half that is easy to drop — a
    /// waiting breakpoint is bound, so a front end that carried the request and
    /// not the answer would show a breakpoint as set on a line the interpreter
    /// is not watching, which is the one thing a debugger must not do
    Sequenced,

    /// where the task a stack is inside was created — [`crate::Stack`]
    ///
    /// carried on the **answer** to a stack, which is what makes it a facet
    /// rather than a request: a front end can implement every variant, show the
    /// frames, and drop the one thing that says who scheduled them. the running
    /// frames are all a severed chain leaves behind, so a front end without this
    /// shows a stack that is true and says nothing about who is responsible
    Scheduling,

    /// that the record of where a task was created **stops short** —
    /// [`crate::Stack::scheduling_cut`]
    ///
    /// its own entry rather than part of [`Self::Scheduling`], because it is
    /// separately droppable and the argument for that one applies again one
    /// level down: a front end can carry every scheduling frame and leave out
    /// the one bit saying they are not all of them. the frames a bounded record
    /// drops are the **outermost**, so what is left reads as a task scheduled
    /// from the middle of a call chain, and a reader has no way to tell that
    /// apart from a task really scheduled there
    SchedulingCut,

    /// how much of each step a recording keeps — [`crate::Depth`]
    ///
    /// a field of [`Request::Record`], and a capability all the same: without it
    /// a front end can start a recording and never ask for the half of it that
    /// says what a variable was. it is also the one field here that a user picks
    /// on **price** — the depths differ by hundreds of times a bare run — so a
    /// front end that hard-coded one would be choosing that for its user without
    /// saying so
    RecordingDepth,
}

impl Facet {
    /// every facet, for a test that has to cover all of them
    pub const ALL: [Self; 10] = [
        Self::HitCondition,
        Self::ValueBounds,
        Self::Session,
        Self::GeneratedLocation,
        Self::Terminal,
        Self::LiveReplacement,
        Self::Sequenced,
        Self::Scheduling,
        Self::SchedulingCut,
        Self::RecordingDepth,
    ];

    /// what to call this capability in a message about it
    pub const fn name(self) -> &'static str {
        match self {
            Self::HitCondition => "a breakpoint's hit condition",
            Self::ValueBounds => "the bounds on how much of a value is read",
            Self::Session => "naming the session a request is for",
            Self::GeneratedLocation => "the generated python behind a `.by` frame",
            Self::Terminal => "running the debuggee on a terminal the client owns",
            Self::LiveReplacement => "replacing code under a frame that is running it",
            Self::Sequenced => "a breakpoint that waits for another one to be hit",
            Self::Scheduling => "where the task a stack is inside was created",
            Self::SchedulingCut => "that a scheduling record does not reach the program's entry",
            Self::RecordingDepth => "how much of each step a recording keeps",
        }
    }
}

/// one thing the debugger says that no client asked it for
///
/// the other direction from [`Request`], and it needed enumerating for the same
/// reason. a report was held by [`Reporting`], which has no default bodies — so
/// an implementation has to **exist**, and an empty one satisfies that. nothing
/// failed if a front end took a report and dropped it on the floor
///
/// it is deliberately not a [`Facet`]. a facet is a capability carried inside a
/// request, so it is still something a client asks for, and [`Reach`] is about
/// how one asks. these are the opposite: the debugger produced a fact and the
/// question is whether it got **out**. what proves one is different too — a
/// capability is proved by watching what the session was asked, and a report by
/// watching what the client was told — and putting two proofs behind one name is
/// how one of them comes to be skipped
///
/// the outcomes of a running program are here beside the [`Reporting`] methods
/// because a front end has the same problem with both: a fact arrived while
/// nobody was asking, and it either reaches the client or it does not.
/// [`Running`] is how the fact crosses the session boundary, not how it reaches
/// a person
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Told {
    /// a logpoint produced a record — [`Reporting::logged`]
    Logged,

    /// a pause is armed, and these threads were running python — [`Reporting::pausing`]
    Pausing,

    /// the program started a child process — [`Reporting::spawned`]
    Spawned,

    /// there is a way of starting a child this interpreter hides — [`Reporting::blind_to`]
    BlindSpot,

    /// a debugged fork joined as a session of its own — [`Reporting::attached`]
    Attached,

    /// a thread stopped — [`Running::Stopped`]
    Stopped,

    /// the program exited, and bpd read what with — [`Running::Exited`]
    Exited,

    /// the program ran to its end with threads still held — [`Running::Finishing`]
    Finishing,

    /// the program is over and its exit is not bpd's to read — [`Running::Ended`]
    Ended,

    /// a deadline passed and the program is still running — [`Running::StillRunning`]
    StillRunning,
}

impl Told {
    /// every one of them, for a test that has to cover all of them
    pub const ALL: [Self; 10] = [
        Self::Logged,
        Self::Pausing,
        Self::Spawned,
        Self::BlindSpot,
        Self::Attached,
        Self::Stopped,
        Self::Exited,
        Self::Finishing,
        Self::Ended,
        Self::StillRunning,
    ];

    /// what to call this in a message about it
    pub const fn name(self) -> &'static str {
        match self {
            Self::Logged => "a logpoint's record",
            Self::Pausing => "a pause armed while the program ran",
            Self::Spawned => "a child the program started",
            Self::BlindSpot => "a way of starting a child this interpreter hides",
            Self::Attached => "a debugged fork joining as a session of its own",
            Self::Stopped => "a thread stopping",
            Self::Exited => "the program exiting",
            Self::Finishing => "the program ending with threads still held",
            Self::Ended => "the program being over with no exit bpd can read",
            Self::StillRunning => "a deadline passing with the program still running",
        }
    }

    /// whether it arrives through [`Reporting`] rather than as the outcome of a
    /// run
    ///
    /// what [`say`] makes, as against what [`ran`] makes
    pub const fn unasked(self) -> bool {
        match self {
            Self::Logged | Self::Pausing | Self::Spawned | Self::BlindSpot | Self::Attached => true,
            Self::Stopped | Self::Exited | Self::Finishing | Self::Ended | Self::StillRunning => {
                false
            }
        }
    }

    /// which one a resumed program's outcome is
    ///
    /// exhaustive and with no catch-all arm, so an outcome added to [`Running`]
    /// is a compile error here — and naming it is what puts it in [`ALL`], which
    /// is what every front end then has to answer for
    ///
    /// [`ALL`]: Self::ALL
    pub const fn of(outcome: &Running) -> Self {
        match outcome {
            Running::Stopped { .. } => Self::Stopped,
            Running::Exited { .. } => Self::Exited,
            Running::Finishing { .. } => Self::Finishing,
            Running::Ended { .. } => Self::Ended,
            Running::StillRunning { .. } => Self::StillRunning,
        }
    }
}

/// how a front end passes on something the debugger said
///
/// the sibling of [`Reach`], and it needs its own words because the two front
/// ends differ here in a way they do not differ about requests. DAP has an event
/// stream and sends a report as it happens; MCP has no push at all, so a report
/// is kept and handed over on the answer to the next call
///
/// [`Carried::Pulled`] is a legitimate answer and it is the one that has to be
/// watched. a front end that kept a report and never handed it over looks
/// exactly like one that carries it properly, right up until somebody reads what
/// the next answer really held — which is why an adapter's claim here is checked
/// against a real conversation rather than believed
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Carried {
    /// the front end sends it when it happens, and this is what it sends
    Pushed(&'static str),

    /// there is nowhere to send it, so it is kept and rides on the next answer
    ///
    /// this is where it turns up
    Pulled(&'static str),

    /// this front end's protocol cannot carry it at all
    ///
    /// the reason is not decoration, for the reason it is not on
    /// [`Reach::Unreachable`]: a fact no front end passes on is a fact the
    /// debugger established and threw away, and the only thing that separates
    /// that from a limit of one protocol is a stated reason someone had to write
    Nowhere {
        /// why the protocol cannot carry it
        why: &'static str,
    },
}

impl Carried {
    /// whether this front end passes the fact on at all
    pub const fn carries(&self) -> bool {
        !matches!(self, Self::Nowhere { .. })
    }
}

/// what the reports [`say`] and the outcomes [`ran`] build carry
///
/// a front end renders a fact in its own shape, so what proves the fact reached
/// a client is a piece of the **payload** turning up in what the client was
/// told. these are the distinctive pieces, and they are here rather than in each
/// adapter's test so that the two front ends are checked against the same facts
pub mod mark {
    /// what the log record [`super::say`] makes says
    pub const LOGGED: &str = "a logpoint said this";

    /// the thread the pause [`super::say`] makes names as running python
    pub const RUNNING: u64 = 909_090;

    /// the file the child [`super::say`] makes is about to run
    pub const CHILD: &str = "worker.py";

    /// what a report of a child bpd is **taking up** says, whatever renders it
    ///
    /// the child [`super::say`] makes is one this session was told to debug, so
    /// a front end that dropped that half of the report would be one whose user
    /// reads `bpd is not debugging it` and then watches a session join. it is
    /// the wording rather than a field because the CLI and DAP carry the report
    /// as a sentence and have nowhere else to put it
    pub const TAKING_UP: &str = "bpd was asked to debug this program's children";

    /// the sentence every blind spot ends up saying, whatever renders it
    pub const BLIND_TO: &str = "silence here does not mean";

    /// what an exit whose output is **still being written** says, whatever
    /// renders it
    ///
    /// the exit [`super::ran`] holds is one of these, because it is the half a
    /// front end can drop without anything failing: an ordinary exit says
    /// nothing extra, so an adapter that ignored it would look right until the
    /// one run where the order of the output cannot be trusted
    pub const STILL_WRITING: &str = "still being written";

    /// the session the debugged fork [`super::say`] makes arrived as
    pub const JOINED: u64 = 424_242;

    /// the thread still held as the program of [`super::ran`] ends
    pub const HELD_AT_THE_END: u64 = 909_091;

    /// what the program of [`super::ran`] exits with
    pub const EXIT_CODE: i32 = 91;

    /// how long [`super::ran`]'s deadline was waited out for
    pub const WAITED_MS: u64 = 505_050;
}

/// what a report is called, worked out by being handed one
///
/// [`Reporting`] has no default bodies, so a method added to it is a compile
/// error **here** as well as in every front end — and the only thing this can do
/// about one is name it, which is what puts it in [`Told::ALL`]. rust cannot
/// enumerate methods; it can insist that every one of them is written down
///
/// it is not a front end and reports nothing to anybody. it exists so that
/// [`say`] can be checked against the trait rather than against somebody's
/// memory of the trait
#[derive(Debug, Default)]
pub struct Naming {
    /// what it was handed, in order, each with the part of the payload that
    /// makes it recognisable
    pub heard: Vec<(Told, String)>,
}

impl Reporting for Naming {
    fn logged(&mut self, record: LogRecord) {
        self.heard.push((Told::Logged, record.message));
    }

    fn pausing(&mut self, running: Vec<u64>) {
        self.heard.push((Told::Pausing, format!("{running:?}")));
    }

    fn spawned(&mut self, child: Spawn) {
        // the whole sentence rather than the vector, because the vector is the
        // half of the report a front end cannot get wrong. what it can get wrong
        // is whether the child is being taken up — see [`mark::TAKING_UP`]
        self.heard.push((Told::Spawned, child.to_string()));
    }

    fn blind_to(&mut self, blindspot: Blindspot) {
        self.heard.push((Told::BlindSpot, blindspot.to_string()));
    }

    fn attached(&mut self, session: SessionId) {
        self.heard.push((Told::Attached, session.to_string()));
    }
}

/// hand a front end one report of every kind [`Reporting`] carries
///
/// what a coverage test drives an adapter with, so that "this front end surfaces
/// it" stops being a claim and becomes something that either turns up in the
/// transcript or does not. the payloads live here rather than in each adapter's
/// test because two tests inventing their own are two tests that can come to
/// check different things
///
/// [`Naming`] is what keeps this complete: a method added to the trait breaks
/// that implementation, naming it grows [`Told::ALL`], and the test below then
/// fails until this says it too
pub fn say(to: &mut dyn Reporting) {
    to.logged(LogRecord {
        breakpoint: 1,
        file: "/tmp/fake.py".to_string(),
        line: 3,
        thread: mark::RUNNING,
        hit: 1,
        message: mark::LOGGED.to_string(),
    });
    to.pausing(vec![mark::RUNNING]);
    // taken up, because that is the half a front end can drop without anything
    // failing: the child is reported either way, and only a report that carries
    // the setting can avoid contradicting the session that then joins
    to.spawned(Spawn {
        event: "_posixsubprocess.fork_exec".to_string(),
        executable: Some("/usr/bin/python3.14".to_string()),
        arguments: vec!["/usr/bin/python3.14".to_string(), mark::CHILD.to_string()],
        verdict: Verdict::ThisInterpreter,
        taking_up: true,
    });
    to.blind_to(Blindspot::MultiprocessingSpawn {
        interpreter: "3.13".to_string(),
    });
    to.attached(SessionId::new(
        NonZeroU64::new(mark::JOINED).expect("the joined session is not zero"),
    ));
}

/// one outcome of every kind a resumed program has
///
/// the sibling of [`say`] for the facts that arrive as [`Running`] rather than
/// through [`Reporting`]. an adapter's fake answers a wait with one of these, and
/// what the client was then told is what says whether the front end really
/// carries it
///
/// [`Running::Stopped`] carries a stop of nowhere in particular, because an
/// adapter driving a conversation has stops of its own and the interesting thing
/// about that outcome is not the payload
pub fn ran() -> Vec<Running> {
    vec![
        Running::Stopped {
            stop: Reported {
                stop: 1,
                thread: mark::RUNNING,
                reason: StopReason::Entry,
                holding: Vec::new(),
            }
            .in_session(SessionId::new(
                NonZeroU64::new(1).expect("the first session is not zero"),
            )),
            rebound: Vec::new(),
        },
        // held open, for the reason `say` makes a child that is being taken up:
        // it is the half a front end can drop with nothing failing. an exit
        // whose output had all arrived says nothing extra, so a front end that
        // ignored the field would look right on every ordinary program and be
        // silent on the one where the order cannot be trusted
        Running::Exited {
            status: exited_with(mark::EXIT_CODE),
            rebound: Vec::new(),
            output: Forwarded::StillHeldOpen,
        },
        Running::Finishing {
            threads: vec![mark::HELD_AT_THE_END],
            rebound: Vec::new(),
        },
        Running::Ended {
            rebound: Vec::new(),
        },
        Running::StillRunning {
            waited: Duration::from_millis(mark::WAITED_MS),
            rebound: Vec::new(),
        },
    ]
}

/// the outcome [`ran`] holds of one kind
///
/// what an adapter's fake answers a wait with when the conversation has reached
/// the point that outcome is what it is driving
///
/// # panics
///
/// when `told` is a report rather than an outcome, which is a caller asking for
/// something [`ran`] was never going to hold
pub fn ran_as(told: Told) -> Running {
    assert!(
        !told.unasked(),
        "`{}` arrives through `Reporting` rather than as the outcome of a run, \
         and `say` is what makes one",
        told.name()
    );
    ran()
        .into_iter()
        .find(|outcome| Told::of(outcome) == told)
        .unwrap_or_else(|| unreachable!("`ran` holds one outcome of every kind"))
}

/// an exit status, which has no portable constructor
fn exited_with(code: i32) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        ExitStatus::from_raw(code << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt as _;
        ExitStatus::from_raw(code.unsigned_abs())
    }
}

/// one request of every variant the core defines
///
/// an adapter's `reach_of` is what makes a new variant impossible to ignore;
/// this is what a coverage test drives an adapter against
pub fn surface() -> Vec<Request> {
    let frame = FrameId { stop: 1, depth: 0 };
    vec![
        Request::SetBreakpoints {
            breakpoints: vec![SourceBreakpoint::at(1, "a.py", 1)],
        },
        Request::SetExceptionBreakpoints {
            raised: false,
            uncaught: true,
        },
        Request::DebugChildren { on: true },
        Request::Run { deadline: None },
        Request::Wait { deadline: None },
        Request::Resume { which: Which::All },
        Request::Step {
            stop: 1,
            kind: StepKind::Over,
        },
        Request::Pause,
        Request::Threads {
            settle: Threads::SETTLE,
        },
        Request::StopTheWorld {
            stop: 1,
            settle: Threads::SETTLE,
        },
        Request::Stack { stop: 1, top: None },
        Request::Variables {
            frame,
            scope: Scope::Local,
            detail: Detail::default(),
        },
        Request::TemplateContext {
            frame,
            detail: Detail::default(),
        },
        Request::Evaluate {
            frame,
            expression: "1".to_string(),
            detail: Detail::default(),
        },
        Request::SetVariable {
            frame,
            scope: Scope::Local,
            name: "x".to_string(),
            value: "1".to_string(),
            detail: Detail::default(),
        },
        Request::SetNextStatement { frame, line: 2 },
        // the default, and the other two are reached through the same request
        // rather than through a second one — a front end that carries the mode
        // carries all three
        Request::RestartFrame {
            frame,
            again: crate::Again::Either,
        },
        // asked for under a live frame, because that is the half a front end
        // can drop with nothing failing: a replacement is made either way, and
        // only a front end that carries the flag can offer the trade at all
        Request::ReplaceCode {
            even_under_a_live_frame: true,
            file: std::path::PathBuf::from("a.py"),
        },
        Request::Query {
            stop: 1,
            query: StateQuery {
                scopes: vec![Scope::Local],
                expressions: vec![Wanted {
                    expression: "1".to_string(),
                    frame: 0,
                }],
                source: Some(2),
                ..StateQuery::default()
            },
        },
        Request::Diff {
            before: SnapshotId {
                stop: 1,
                digest: "00".to_string(),
            },
            after: SnapshotId {
                stop: 2,
                digest: "01".to_string(),
            },
        },
        Request::RunScript {
            stop: 1,
            script: Script {
                steps: vec![Step::StepOver],
                budget: Budget {
                    steps: NonZeroU32::new(1).expect("1 is not zero"),
                    wall_ms: NonZeroU64::new(1).expect("1 is not zero"),
                    bytes: NonZeroU32::new(1024).expect("1024 is not zero"),
                },
            },
        },
    ]
    .into_iter()
    .chain(asked_about_a_program(frame))
    .collect()
}

/// the four that ask a question about a running program rather than steer it
///
/// lifted out because `surface` is at clippy's line bound, and grouped because
/// they are the four that were **missing** from it. every parity assertion
/// iterates that list, so while they were absent `Facts`, `Record`, `Trail` and
/// `Retainers` sat outside the comparison between the two front ends, outside
/// the `JUSTIFIED` check, and outside the one that makes a gap say what stands
/// in the way — with all of it passing green
fn asked_about_a_program(frame: FrameId) -> Vec<Request> {
    vec![
        Request::Facts {
            frame,
            names: vec!["x".to_string()],
            limit: crate::fact::Limit::default(),
        },
        Request::Record {
            on: true,
            depth: crate::Depth::default(),
        },
        Request::Trail,
        Request::Retainers {
            frame,
            expression: "x".to_string(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn the_surface_holds_one_request_of_every_kind_and_no_kind_twice() {
        // this used to test only the second half of its own name. the surface
        // was missing four variants — `Facts`, `Record`, `Trail`, `Retainers` —
        // and since every parity assertion iterates it, all four were outside
        // the comparison between the front ends, outside the `JUSTIFIED` check
        // and outside the "says what stands in the way" check. a test whose
        // name promises more than its body checks is worse than no test, and it
        // sat inside the mechanism that enforces this project's central rule
        let mut held: Vec<Option<&str>> = vec![None; Request::KINDS];
        for request in surface() {
            let at = request.ordinal();
            assert!(
                at < Request::KINDS,
                "`{}` has ordinal {at} and `Request::KINDS` is {}. a variant was \
                 given an ordinal and the count was not raised with it",
                request.name(),
                Request::KINDS
            );
            assert!(
                held[at].is_none(),
                "the surface names `{}` twice",
                request.name()
            );
            held[at] = Some(request.name());
        }

        let missing: Vec<usize> = held
            .iter()
            .enumerate()
            .filter_map(|(at, held)| held.is_none().then_some(at))
            .collect();
        assert!(
            missing.is_empty(),
            "the surface is missing the variants with ordinals {missing:?}, so \
             every parity assertion that iterates it skips them silently. add \
             one of each to `surface`"
        );
    }

    #[test]
    fn every_facet_is_named_once() {
        let names: BTreeSet<&str> = Facet::ALL.iter().map(|facet| facet.name()).collect();
        assert_eq!(names.len(), Facet::ALL.len(), "two facets share a name");
    }

    #[test]
    fn everything_the_debugger_says_is_named_once() {
        let names: BTreeSet<&str> = Told::ALL.iter().map(|told| told.name()).collect();
        assert_eq!(names.len(), Told::ALL.len(), "two of them share a name");
    }

    #[test]
    fn saying_one_of_every_report_reaches_every_method_of_the_trait() {
        // the link between the trait and the data. `Naming` implements
        // `Reporting`, which has no default bodies, so a method added there is a
        // compile error in it — and this is what then fails until `Told::ALL`
        // and `say` have both been told about the new one
        let mut naming = Naming::default();
        say(&mut naming);

        let heard: Vec<Told> = naming.heard.iter().map(|(told, _)| *told).collect();
        let unasked: Vec<Told> = Told::ALL
            .into_iter()
            .filter(|told| told.unasked())
            .collect();
        assert_eq!(
            heard, unasked,
            "`say` has to make one report of every kind the trait carries, in \
             the order they are enumerated"
        );

        // and each of them really carried its payload. a report built empty
        // would reach a front end with nothing in it to find afterwards
        for (told, payload) in &naming.heard {
            assert!(
                !payload.trim().is_empty(),
                "the report `say` made of `{}` carries nothing recognisable",
                told.name()
            );
        }
    }

    #[test]
    fn running_a_program_has_one_outcome_of_every_kind_and_no_kind_twice() {
        let told: Vec<Told> = ran().iter().map(Told::of).collect();
        let outcomes: Vec<Told> = Told::ALL
            .into_iter()
            .filter(|told| !told.unasked())
            .collect();
        assert_eq!(
            told, outcomes,
            "`ran` has to hold one outcome of every kind, in the order they are \
             enumerated"
        );

        for told in outcomes {
            assert_eq!(Told::of(&ran_as(told)), told);
        }
    }

    #[test]
    fn the_marks_a_report_carries_are_the_words_the_reports_really_use() {
        // a mark is what each front end's coverage test goes looking for, so one
        // that had drifted from the wording would be a test that passes while
        // the front end says something else entirely
        let mut naming = Naming::default();
        say(&mut naming);

        let (_, spawned) = naming
            .heard
            .iter()
            .find(|(told, _)| *told == Told::Spawned)
            .unwrap_or_else(|| unreachable!("`say` makes one report of every kind"));
        assert!(
            spawned.contains(mark::CHILD),
            "the child report said {spawned}"
        );
        assert!(
            spawned.contains(mark::TAKING_UP),
            "`say` makes a child bpd is taking up, and the sentence it renders \
             to does not say so: {spawned}"
        );

        let (_, blind) = naming
            .heard
            .iter()
            .find(|(told, _)| *told == Told::BlindSpot)
            .unwrap_or_else(|| unreachable!("`say` makes one report of every kind"));
        assert!(
            blind.contains(mark::BLIND_TO),
            "the blind spot said {blind}"
        );
    }

    #[test]
    fn the_marks_a_report_carries_are_all_different() {
        // what a front end's coverage test looks for. two that were the same
        // would let one report stand in as evidence for another
        let marks = [
            mark::RUNNING.to_string(),
            mark::JOINED.to_string(),
            mark::HELD_AT_THE_END.to_string(),
            mark::EXIT_CODE.to_string(),
            mark::WAITED_MS.to_string(),
        ];
        let distinct: BTreeSet<&String> = marks.iter().collect();
        assert_eq!(
            marks.len(),
            distinct.len(),
            "two marks are the same: {marks:?}"
        );
    }
}
