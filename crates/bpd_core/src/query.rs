//! describing a stop's state in one call, and comparing two of them
//!
//! ## why it is one call
//!
//! reading one local through a tree walk is `stackTrace`, then `scopes`, then
//! `variables`, then `variables` again — four round trips, each one a tool call
//! and a chunk of an agent's context spent on protocol scaffolding rather than
//! on the program. a [`StateQuery`] says what is wanted and is answered with it
//!
//! nothing here is a second implementation of anything: the query is composed of
//! the same requests the tree walk is made of, so the two cannot disagree
//!
//! ## a snapshot is a value, not a handle
//!
//! DAP's variable reference is a **promise to read something later**, and that
//! is why it goes stale the moment the program runs on. a [`Snapshot`] is the
//! reading, already taken — so its id names an immutable value and stays true
//! for as long as the session lasts, across any number of resumes
//!
//! what does go stale is the [`crate::FrameId`]s inside one, which are the
//! existing per-stop ids and are refused by the existing rule. the snapshot goes
//! on being true; what it names cannot be asked for again
//!
//! ## a diff never invents a change and never hides one
//!
//! "unchanged" is a claim. a value that a bound cut short in either snapshot is
//! reported as [`WhyNot::Elided`] rather than compared, and so is a frame that
//! is running different code in the two — comparing `x` of `f` against `x` of
//! `g` because they are both at depth 0 would be a difference about two
//! different variables

use crate::exception::PythonError;
use crate::frame::{Frame, Scope};
use crate::stop::{Mode, StopReason};
use crate::thread::Where;
use crate::value::{Content, Detail, Entry, Evaluated, Omitted, Value};

/// what to describe about a stop
///
/// deserialised with `deny_unknown_fields` for the reason [`Detail`] is: a
/// misspelled field that quietly took its default would be a part of the
/// question that was asked and never answered
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateQuery {
    /// how many frames to describe, counting from the one that stopped
    ///
    /// the answer says how deep the stack really is either way. the scopes and
    /// the source are read for these frames and no others
    #[serde(default = "StateQuery::frames")]
    pub frames: u32,

    /// which scopes of each described frame to read
    ///
    /// empty reads none. python resolves a name by which scope it is in, so
    /// they are read one at a time and never merged — see
    /// [`crate::Scope`]
    #[serde(default)]
    pub scopes: Vec<Scope>,

    /// expressions to evaluate, each in a frame of this stop
    ///
    /// this runs the program's own code, by request. one that raises is
    /// answered with the exception, which is what it did
    #[serde(default)]
    pub expressions: Vec<Wanted>,

    /// how many lines of source either side of each frame's current line
    ///
    /// `None` reads no source at all. the lines are read **in the debuggee**,
    /// off the filesystem the interpreter read them from, and are shown only
    /// when the file still compiles to the code object the frame is running —
    /// see [`Source`]
    #[serde(default)]
    pub source: Option<u32>,

    /// how much of every value read here may carry
    ///
    /// `budget` is spent across the **whole** query rather than per read: a
    /// query of twenty parts under an eight kilobyte budget spends eight
    /// kilobytes, not a hundred and sixty. what is left when it runs out is
    /// named in `left_out` rather than silently absent
    #[serde(default)]
    pub detail: Detail,
}

impl StateQuery {
    /// how many frames are described when the client does not say
    ///
    /// the frame the program is in, and nothing else. every other frame is a
    /// scope read the client did not ask for, and the byte budget is spending
    /// somebody's context window
    const fn frames() -> u32 {
        1
    }
}

impl Default for StateQuery {
    fn default() -> Self {
        Self {
            frames: Self::frames(),
            scopes: Vec::new(),
            expressions: Vec::new(),
            source: None,
            detail: Detail::default(),
        }
    }
}

/// one expression, and the frame to evaluate it in
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wanted {
    /// the expression, as the client wrote it
    pub expression: String,
    /// how far down the stack to evaluate it, with the frame that stopped at
    /// zero
    #[serde(default)]
    pub frame: u32,
}

/// what a stop was holding, described to the level the query asked for
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct State {
    /// the stop this was read at
    pub stop: u64,
    /// the thread that stop holds
    pub thread: u64,
    /// why that thread is held
    pub reason: StopReason,
    /// the frames that were described, the one that stopped first
    pub frames: Vec<FrameState>,
    /// how deep the stack really is, which is more than `frames` when fewer
    /// were asked for
    pub depth: usize,
    /// what the expressions produced, in the order they were asked for
    pub values: Vec<Answer>,
    /// every part of the query that was not answered, and why
    ///
    /// an agent cannot see the elision a person would notice, so a part that
    /// was not read is named here rather than being absent
    pub left_out: Vec<NotRead>,
    /// how the program was moving while this was read
    ///
    /// in `non_stop` the rest of the program kept running, so this describes a
    /// moment that may never have been one whole state. that is what a snapshot
    /// of it claims, and it is what a diff over two of them can claim
    pub mode: Mode,
    /// what this answer cost against the query's byte budget
    pub bytes: u64,
}

/// one frame, and what the query asked to be read of it
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FrameState {
    /// which frame it is
    pub frame: Frame,
    /// the source around its current line, when the query asked for source
    pub source: Option<Source>,
    /// the scopes that were read, in the order they were asked for
    pub scopes: Vec<ScopeState>,
}

/// what one scope of one frame holds
///
/// [`crate::Variables`] without the mode, which is on the [`State`] around it
/// because every part of one answer was read in the same mode
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScopeState {
    /// which scope
    pub scope: Scope,
    /// the names it holds, in the order the interpreter keeps them
    pub entries: Vec<Entry>,
    /// names of the scope that hold nothing at this line
    pub unbound: Vec<String>,
    /// names of the scope whose value the frame does not expose
    pub unreadable: Vec<String>,
    /// everything this scope's read left out, and why
    pub omitted: Vec<Omitted>,
}

/// what one expression produced
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Answer {
    /// the expression, as the client wrote it
    pub expression: String,
    /// the frame it was evaluated in
    pub frame: u32,
    /// what it did
    pub result: Evaluated,
}

/// the source around a frame's current line, or why there is none
///
/// bpd will not show a line it cannot prove is the line that is running. the
/// file on disk is not evidence on its own — it is edited while a program runs,
/// and a debugger that read the current bytes and called them the program's
/// source would be inventing the thing an agent reasons about
///
/// so the debuggee **compiles the file** and checks that the frame's own code
/// object is in what came out, by qualified name, first line, argument count,
/// flags, names and line table. that is the same rule source mapping is held to:
/// a location is resolved or it errors, and there is no identity fallback
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Source {
    /// the lines, verified to be the code this frame is running
    Lines {
        /// the number of the first line here
        first: u32,
        /// the line the frame is on now
        at: u32,
        /// the lines, without their endings
        lines: Vec<String>,
        /// how many lines the file has
        total: u32,
    },

    /// there is no source bpd is willing to claim, and this is why
    Unverified {
        /// what stood in the way
        why: Unverified,
    },
}

/// why a frame's source is not here
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "unverified", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Unverified {
    /// the code object's filename is not a file
    ///
    /// `<string>`, a frozen module, or a name that is simply not on disk. there
    /// is nothing to read and nothing to check against
    NotAFile {
        /// what `co_filename` says
        file: String,
        /// what reading it said
        reason: String,
    },

    /// the file does not compile now
    ///
    /// which is itself evidence: what is on disk cannot be what the interpreter
    /// compiled, because the interpreter compiled it
    DoesNotCompile {
        /// the file that was read
        file: String,
        /// what compiling it raised
        error: PythonError,
    },

    /// the file compiles and this frame's code object is not in what came out
    ///
    /// the file has been edited since the interpreter read it. the lines on disk
    /// are not the lines that are running
    NotTheSameCode {
        /// the file that was read
        file: String,
        /// the qualified name that was looked for
        function: String,
        /// the first line of the code object that is running
        first_line: u32,
    },

    /// the file's bytes are not utf-8
    ///
    /// it compiled, so the interpreter accepted it under a declared encoding.
    /// showing it as text would mean deciding that encoding a second time, so
    /// the lines are refused rather than guessed at
    NotUtf8 {
        /// the file that was read
        file: String,
    },
}

impl std::fmt::Display for Unverified {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAFile { file, reason } => write!(
                formatter,
                "`{file}` could not be read in the debuggee: {reason}. a code \
                 object whose filename is not a file — `<string>`, a frozen \
                 module, a module in a zip — has no source bpd can check"
            ),
            Self::DoesNotCompile { file, error } => write!(
                formatter,
                "`{file}` does not compile now: {error}. the interpreter \
                 compiled what it is running, so what is on disk is not it — the \
                 file has been edited since"
            ),
            Self::NotTheSameCode {
                file,
                function,
                first_line,
            } => write!(
                formatter,
                "`{file}` compiles and `{function}` at line {first_line} is not \
                 in what came out, so the file has been edited since the \
                 interpreter read it. bpd will not show a line it cannot prove \
                 is the line that is running"
            ),
            Self::NotUtf8 { file } => write!(
                formatter,
                "`{file}` is not utf-8. it compiled, so the interpreter read it \
                 under an encoding it declared — deciding that encoding again \
                 here would be a second implementation of a rule cpython owns"
            ),
        }
    }
}

/// a part of a query that was not answered, and why
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotRead {
    /// which part
    pub part: QueryPart,
    /// why it is not here
    pub why: Omitted,
}

impl std::fmt::Display for NotRead {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} was not read: {}", self.part, self.why)
    }
}

/// one part of a query
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "part", rename_all = "snake_case")]
#[non_exhaustive]
pub enum QueryPart {
    /// one scope of one frame
    Scope {
        /// how far down the stack
        frame: u32,
        /// which scope
        scope: Scope,
    },

    /// one expression
    Expression {
        /// how far down the stack it would have been evaluated
        frame: u32,
        /// the expression, as the client wrote it
        expression: String,
    },

    /// the source around one frame's line
    Source {
        /// how far down the stack
        frame: u32,
    },
}

impl std::fmt::Display for QueryPart {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Scope { frame, scope } => {
                write!(formatter, "the {scope} scope of frame {frame}")
            }
            Self::Expression { frame, expression } => {
                write!(formatter, "`{expression}` in frame {frame}")
            }
            Self::Source { frame } => write!(formatter, "the source around frame {frame}"),
        }
    }
}

/// how a kept state is named
///
/// the stop it was taken at, and a digest of everything in it. the stop is what
/// makes it self-describing — a reader can see which stop it belongs to without
/// asking — and the digest is what makes it **content addressed**: the same
/// state read twice has the same id, and an id names one state or no state at
/// all
///
/// it does not go stale. a [`crate::FrameId`] is valid for one stop because it
/// names a frame that runs on; this names a reading that has already been taken,
/// and nothing the program does afterwards can change it
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotId {
    /// the stop it was taken at
    pub stop: u64,
    /// the digest of everything in it, in lowercase hex
    pub digest: String,
}

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}:{}", self.stop, self.digest)
    }
}

impl std::str::FromStr for SnapshotId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (stop, digest) = text.split_once(':').ok_or_else(|| {
            format!(
                "`{text}` is not a snapshot id. one is the stop it was taken at \
                 and a digest of its contents, written `3:9f2a…`, and it is \
                 given out by the state query rather than composed"
            )
        })?;
        let stop = stop
            .parse()
            .map_err(|_| format!("`{text}` is not a snapshot id: `{stop}` is not a stop number"))?;
        if digest.is_empty() || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "`{text}` is not a snapshot id: `{digest}` is not a hex digest"
            ));
        }
        Ok(Self {
            stop,
            digest: digest.to_string(),
        })
    }
}

impl serde::Serialize for SnapshotId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for SnapshotId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// a state, kept under an id so it can be compared with another
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    /// how to name it later
    pub id: SnapshotId,
    /// what was read
    pub state: State,
}

/// what changed between two snapshots
///
/// the answer is the difference. shipping both states and leaving the comparison
/// to the reader is what this exists instead of
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Difference {
    /// where the first was taken
    pub before: Taken,
    /// where the second was taken
    pub after: Taken,
    /// how the frame chain differs
    pub frames: Frames,
    /// everything that was compared and is not the same
    pub changed: Vec<Changed>,
    /// everything the second snapshot has and the first does not
    pub added: Vec<Appeared>,
    /// everything the first snapshot has and the second does not
    pub removed: Vec<Appeared>,
    /// everything that was compared and is the same, named rather than carried
    pub unchanged: Vec<Subject>,
    /// everything that **could not** be compared, and why
    ///
    /// never counted as unchanged. "unchanged" is a claim, and a value a bound
    /// cut short on either side is not evidence for it
    pub not_compared: Vec<NotCompared>,
}

/// where one side of a difference was read
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Taken {
    /// how it is named
    pub id: SnapshotId,
    /// the stop it was read at
    pub stop: u64,
    /// the thread that stop held
    pub thread: u64,
    /// how the program was moving while it was read
    pub mode: Mode,
    /// whether that stop is still held
    ///
    /// the snapshot is true either way — it is a reading that was taken, not a
    /// promise to take one. what has ended is the ability to ask that stop
    /// anything more: the [`crate::FrameId`]s in it name frames that have run on
    pub stop_has_ended: bool,
}

/// how two snapshots' frame chains differ
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Frames {
    /// how deep the stack was when the first was taken
    pub before_depth: usize,
    /// how deep it was when the second was taken
    pub after_depth: usize,
    /// the described depths where the two are not in the same place
    pub moved: Vec<Moved>,
}

/// one depth of the stack, in both snapshots
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Moved {
    /// how far down the stack
    pub depth: u32,
    /// where it was, or `None` when the first snapshot did not describe it
    pub before: Option<Where>,
    /// where it is, or `None` when the second did not describe it
    pub after: Option<Where>,
}

/// what one comparison is about
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "subject", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Subject {
    /// one name of one scope of one frame
    Variable {
        /// how far down the stack
        frame: u32,
        /// which scope
        scope: Scope,
        /// the name
        name: String,
    },

    /// one expression, evaluated in one frame
    Expression {
        /// how far down the stack it was evaluated
        frame: u32,
        /// the expression, as the client wrote it
        expression: String,
    },
}

impl std::fmt::Display for Subject {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Variable { frame, scope, name } => {
                write!(formatter, "`{name}` in the {scope} scope of frame {frame}")
            }
            Self::Expression { frame, expression } => {
                write!(formatter, "`{expression}` in frame {frame}")
            }
        }
    }
}

/// what one side of a comparison held
///
/// unbound and unreadable are states a name really has, and they are not the
/// same as absent — a local that was unbound and now holds five has changed,
/// and reporting it as having appeared would be a different claim
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "seen", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Seen {
    /// it held a value
    Value {
        /// what it held
        value: Value,
    },
    /// the expression raised
    Raised {
        /// what it raised
        error: PythonError,
    },
    /// the name is in the scope and holds nothing at that line
    Unbound,
    /// the name is in the scope and the frame does not expose it
    Unreadable,
}

impl Seen {
    /// what a scope's read said about one name
    fn of_scope(scope: &ScopeState, name: &str) -> Option<Self> {
        if let Some(entry) = scope.entries.iter().find(|entry| entry.name == name) {
            return Some(Self::Value {
                value: entry.value.clone(),
            });
        }
        if scope.unbound.iter().any(|held| held == name) {
            return Some(Self::Unbound);
        }
        if scope.unreadable.iter().any(|held| held == name) {
            return Some(Self::Unreadable);
        }
        None
    }

    /// what an evaluation produced
    fn of_answer(answer: &Answer) -> Self {
        match &answer.result {
            Evaluated::Value { value } => Self::Value {
                value: value.clone(),
            },
            Evaluated::Raised { error } => Self::Raised {
                error: error.clone(),
            },
        }
    }

    /// the bound that cut this reading short, if one did
    fn elided(&self) -> Option<Omitted> {
        match self {
            Self::Value { value } => elision(value),
            // an exception is carried whole or not at all, and the two states a
            // name can be in carry nothing to cut
            Self::Raised { .. } | Self::Unbound | Self::Unreadable => None,
        }
    }
}

/// one thing that is not the same in the two snapshots
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Changed {
    /// what it is about
    pub subject: Subject,
    /// what the first snapshot held
    pub before: Seen,
    /// what the second holds
    pub after: Seen,
}

/// one thing that is in only one of the two snapshots
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Appeared {
    /// what it is about
    pub subject: Subject,
    /// what the snapshot that has it holds
    pub seen: Seen,
}

/// one thing that could not be compared, and why
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotCompared {
    /// what it is about
    pub subject: Subject,
    /// why the two readings cannot be compared
    pub why: WhyNot,
}

impl std::fmt::Display for NotCompared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} was not compared: {}", self.subject, self.why)
    }
}

/// why two readings of one thing cannot be compared
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "why_not", rename_all = "snake_case")]
#[non_exhaustive]
pub enum WhyNot {
    /// a bound cut the reading short, so neither answer is the whole value
    Elided {
        /// which snapshot was cut
        side: Side,
        /// what was left out of it
        omitted: Omitted,
    },

    /// only one of the two snapshots read it
    ///
    /// the two queries asked for different things. what the other snapshot holds
    /// is unknown rather than absent, and a name reported as added because
    /// nobody looked for it would be a change bpd invented
    NotBothRead {
        /// the snapshot that read it
        side: Side,
    },

    /// that depth of the stack is running different code in the two snapshots
    ///
    /// depth is a position, not an identity. comparing `x` of `f` against `x` of
    /// `g` because both are frame 0 would be a difference about two different
    /// variables
    DifferentCode {
        /// what was at that depth when the first was taken
        before: Where,
        /// what is there now
        after: Where,
    },
}

impl std::fmt::Display for WhyNot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Elided { side, omitted } => {
                match side {
                    Side::Both => formatter.write_str("a bound cut it short in both snapshots"),
                    side => write!(formatter, "a bound cut it short in the {side} snapshot"),
                }?;
                write!(
                    formatter,
                    " — {omitted} — so neither reading is the whole value and \
                     `unchanged` would be a claim about the part that was read. \
                     take both snapshots again with a larger bound"
                )
            }
            Self::NotBothRead { side } => write!(
                formatter,
                "only the {side} snapshot read it, so what the other holds is \
                 unknown rather than absent. query both stops for the same thing"
            ),
            Self::DifferentCode { before, after } => write!(
                formatter,
                "that depth of the stack was {before} and is {after}, so the two \
                 are different code. depth is a position rather than an identity"
            ),
        }
    }
}

/// which snapshot something is about
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// the first
    Before,
    /// the second
    After,
    /// both of them
    Both,
}

impl std::fmt::Display for Side {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Before => "before",
            Self::After => "after",
            Self::Both => "both",
        })
    }
}

/// the first bound that cut a value short, anywhere inside it
///
/// a value with one of these in it is not the whole value, so two of them cannot
/// be told apart from two whole ones that are equal. that is the entire reason a
/// diff has a `not_compared` at all
fn elision(value: &Value) -> Option<Omitted> {
    match &value.content {
        Content::None | Content::Bool { .. } | Content::Float { .. } => None,
        Content::Int { omitted, .. }
        | Content::Str { omitted, .. }
        | Content::Bytes { omitted, .. }
        | Content::Repr { omitted, .. } => omitted.clone(),
        Content::Unread { omitted } => Some(omitted.clone()),
        Content::Sequence { items, omitted, .. } => {
            omitted.clone().or_else(|| items.iter().find_map(elision))
        }
        Content::Mapping {
            entries, omitted, ..
        } => omitted.clone().or_else(|| {
            entries
                .iter()
                .find_map(|pair| elision(&pair.key).or_else(|| elision(&pair.value)))
        }),
        Content::Object {
            attributes,
            omitted,
        } => omitted
            .clone()
            .or_else(|| attributes.iter().find_map(|entry| elision(&entry.value))),
    }
}

/// compare two states, and say what could not be compared rather than guessing
///
/// this is data over data: both states were read by the same session and nothing
/// here touches the program. it lives in the core because both adapters answer
/// with it, and because a diff computed twice in two front ends is two rules
pub fn difference(before: &Snapshot, after: &Snapshot, held: &[u64]) -> Difference {
    let mut difference = Difference {
        before: taken(before, held),
        after: taken(after, held),
        frames: frames_of(&before.state, &after.state),
        changed: Vec::new(),
        added: Vec::new(),
        removed: Vec::new(),
        unchanged: Vec::new(),
        not_compared: Vec::new(),
    };

    compare_expressions(&mut difference, &before.state, &after.state);
    compare_scopes(&mut difference, &before.state, &after.state);
    difference
}

fn taken(snapshot: &Snapshot, held: &[u64]) -> Taken {
    Taken {
        id: snapshot.id.clone(),
        stop: snapshot.state.stop,
        thread: snapshot.state.thread,
        mode: snapshot.state.mode.clone(),
        stop_has_ended: !held.contains(&snapshot.state.stop),
    }
}

fn frames_of(before: &State, after: &State) -> Frames {
    let deepest = before.frames.len().max(after.frames.len());
    let mut moved = Vec::new();
    for depth in 0..deepest {
        let one = before.frames.get(depth).map(|frame| place(&frame.frame));
        let other = after.frames.get(depth).map(|frame| place(&frame.frame));
        if one != other {
            moved.push(Moved {
                depth: u32::try_from(depth).expect("a stack is not four billion frames deep"),
                before: one,
                after: other,
            });
        }
    }
    Frames {
        before_depth: before.depth,
        after_depth: after.depth,
        moved,
    }
}

fn place(frame: &Frame) -> Where {
    Where {
        file: frame.file.clone(),
        line: frame.line,
        function: frame.function.clone(),
    }
}

/// whether two frames are the same code, which is what makes a depth comparable
///
/// the line is deliberately not part of it: a frame that has moved on to another
/// line is the same frame, and that it moved is the whole point of a diff
fn same_code(one: &Frame, other: &Frame) -> bool {
    one.file == other.file && one.function == other.function && one.first_line == other.first_line
}

fn compare_expressions(difference: &mut Difference, before: &State, after: &State) {
    for answer in &before.values {
        let subject = Subject::Expression {
            frame: answer.frame,
            expression: answer.expression.clone(),
        };
        let Some(other) = after
            .values
            .iter()
            .find(|other| other.frame == answer.frame && other.expression == answer.expression)
        else {
            difference.not_compared.push(NotCompared {
                subject,
                why: WhyNot::NotBothRead { side: Side::Before },
            });
            continue;
        };
        // an expression is evaluated in a frame by depth, so the same guard
        // applies to it as to a variable of that frame
        if let Some(why) = incomparable_frame(before, after, answer.frame) {
            difference.not_compared.push(NotCompared { subject, why });
            continue;
        }
        judge(
            difference,
            subject,
            &Seen::of_answer(answer),
            &Seen::of_answer(other),
        );
    }

    for answer in &after.values {
        if before
            .values
            .iter()
            .any(|one| one.frame == answer.frame && one.expression == answer.expression)
        {
            continue;
        }
        difference.not_compared.push(NotCompared {
            subject: Subject::Expression {
                frame: answer.frame,
                expression: answer.expression.clone(),
            },
            why: WhyNot::NotBothRead { side: Side::After },
        });
    }
}

impl State {
    /// the frame at one depth, when this state described it
    fn described(&self, depth: u32) -> Option<&FrameState> {
        self.frames
            .iter()
            .find(|frame| frame.frame.id.depth == depth)
    }
}

/// why one depth cannot be compared between two states, when it cannot
fn incomparable_frame(before: &State, after: &State, depth: u32) -> Option<WhyNot> {
    let one = before.described(depth)?;
    let other = after.described(depth)?;
    if same_code(&one.frame, &other.frame) {
        return None;
    }
    Some(WhyNot::DifferentCode {
        before: place(&one.frame),
        after: place(&other.frame),
    })
}

fn compare_scopes(difference: &mut Difference, before: &State, after: &State) {
    for frame in &before.frames {
        let depth = frame.frame.id.depth;
        for scope in &frame.scopes {
            let Some(other) = after
                .described(depth)
                .and_then(|frame| frame.scopes.iter().find(|read| read.scope == scope.scope))
            else {
                for name in names_of(scope) {
                    difference.not_compared.push(NotCompared {
                        subject: Subject::Variable {
                            frame: depth,
                            scope: scope.scope,
                            name,
                        },
                        why: WhyNot::NotBothRead { side: Side::Before },
                    });
                }
                continue;
            };

            if let Some(why) = incomparable_frame(before, after, depth) {
                for name in names_of(scope) {
                    difference.not_compared.push(NotCompared {
                        subject: Subject::Variable {
                            frame: depth,
                            scope: scope.scope,
                            name,
                        },
                        why: why.clone(),
                    });
                }
                continue;
            }

            compare_names(difference, depth, scope, other);
        }
    }

    // a scope the second snapshot read and the first did not. its names are not
    // additions — nobody looked for them in the first — and calling them one
    // would be a change bpd invented
    for frame in &after.frames {
        let depth = frame.frame.id.depth;
        for scope in &frame.scopes {
            let read_before = before
                .described(depth)
                .is_some_and(|frame| frame.scopes.iter().any(|read| read.scope == scope.scope));
            if read_before {
                continue;
            }
            for name in names_of(scope) {
                difference.not_compared.push(NotCompared {
                    subject: Subject::Variable {
                        frame: depth,
                        scope: scope.scope,
                        name,
                    },
                    why: WhyNot::NotBothRead { side: Side::After },
                });
            }
        }
    }
}

/// every name one scope's read said anything about
fn names_of(scope: &ScopeState) -> Vec<String> {
    scope
        .entries
        .iter()
        .map(|entry| entry.name.clone())
        .chain(scope.unbound.iter().cloned())
        .chain(scope.unreadable.iter().cloned())
        .collect()
}

fn compare_names(difference: &mut Difference, depth: u32, before: &ScopeState, after: &ScopeState) {
    // a scope read that was cut short did not see every name it has, so a name
    // missing from one side may be a name nobody read rather than one that went.
    // only the two bounds that drop a **name** count here: a depth that ran out
    // cuts the values and leaves the listing whole, and that shows up on the
    // values themselves
    let dropped_names =
        |omitted: &&Omitted| matches!(omitted, Omitted::Children { .. } | Omitted::Budget { .. });
    let cut = before
        .omitted
        .iter()
        .find(dropped_names)
        .map(|omitted| (Side::Before, omitted.clone()))
        .or_else(|| {
            after
                .omitted
                .iter()
                .find(dropped_names)
                .map(|omitted| (Side::After, omitted.clone()))
        });

    for name in names_of(before) {
        let subject = Subject::Variable {
            frame: depth,
            scope: before.scope,
            name: name.clone(),
        };
        let one = Seen::of_scope(before, &name).expect("the name came out of this scope");
        match Seen::of_scope(after, &name) {
            Some(other) => judge(difference, subject, &one, &other),
            None => match &cut {
                Some((side, omitted)) => difference.not_compared.push(NotCompared {
                    subject,
                    why: WhyNot::Elided {
                        side: *side,
                        omitted: omitted.clone(),
                    },
                }),
                None => difference.removed.push(Appeared { subject, seen: one }),
            },
        }
    }

    for name in names_of(after) {
        if Seen::of_scope(before, &name).is_some() {
            continue;
        }
        let subject = Subject::Variable {
            frame: depth,
            scope: after.scope,
            name: name.clone(),
        };
        let other = Seen::of_scope(after, &name).expect("the name came out of this scope");
        match &cut {
            Some((side, omitted)) => difference.not_compared.push(NotCompared {
                subject,
                why: WhyNot::Elided {
                    side: *side,
                    omitted: omitted.clone(),
                },
            }),
            None => difference.added.push(Appeared {
                subject,
                seen: other,
            }),
        }
    }
}

/// decide what became of one thing both snapshots read
///
/// the order matters: a reading a bound cut short cannot be called unchanged
/// **or** changed, because the difference might be entirely in what was left out
fn judge(difference: &mut Difference, subject: Subject, before: &Seen, after: &Seen) {
    let cut = match (before.elided(), after.elided()) {
        (Some(omitted), Some(_)) => Some((Side::Both, omitted)),
        (Some(omitted), None) => Some((Side::Before, omitted)),
        (None, Some(omitted)) => Some((Side::After, omitted)),
        (None, None) => None,
    };
    if let Some((side, omitted)) = cut {
        difference.not_compared.push(NotCompared {
            subject,
            why: WhyNot::Elided { side, omitted },
        });
        return;
    }

    if before == after {
        difference.unchanged.push(subject);
    } else {
        difference.changed.push(Changed {
            subject,
            before: before.clone(),
            after: after.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::FrameId;
    use crate::value::Pair;

    fn integer(text: &str) -> Value {
        Value {
            kind: "int".to_string(),
            content: Content::Int {
                text: text.to_string(),
                omitted: None,
            },
        }
    }

    fn frame(depth: u32, function: &str, line: u32) -> Frame {
        Frame {
            id: FrameId { stop: 1, depth },
            file: "/tmp/a.py".to_string(),
            line,
            function: function.to_string(),
            first_line: 1,
        }
    }

    fn state(stop: u64, function: &str, line: u32, entries: Vec<Entry>) -> Snapshot {
        Snapshot {
            id: SnapshotId {
                stop,
                digest: format!("{stop}{stop}"),
            },
            state: State {
                stop,
                thread: 7,
                reason: StopReason::Entry,
                frames: vec![FrameState {
                    frame: frame(0, function, line),
                    source: None,
                    scopes: vec![ScopeState {
                        scope: Scope::Local,
                        entries,
                        unbound: Vec::new(),
                        unreadable: Vec::new(),
                        omitted: Vec::new(),
                    }],
                }],
                depth: 1,
                values: Vec::new(),
                left_out: Vec::new(),
                mode: Mode::NonStop,
                bytes: 0,
            },
        }
    }

    #[test]
    fn a_value_that_really_changed_is_the_difference_and_the_rest_is_named_unchanged() {
        let before = state(
            1,
            "main",
            3,
            vec![
                Entry {
                    name: "total".to_string(),
                    value: integer("1"),
                },
                Entry {
                    name: "steady".to_string(),
                    value: integer("9"),
                },
            ],
        );
        let after = state(
            2,
            "main",
            4,
            vec![
                Entry {
                    name: "total".to_string(),
                    value: integer("2"),
                },
                Entry {
                    name: "steady".to_string(),
                    value: integer("9"),
                },
            ],
        );

        let difference = difference(&before, &after, &[2]);
        assert_eq!(difference.changed.len(), 1, "{difference:?}");
        assert_eq!(
            difference.changed[0].subject,
            Subject::Variable {
                frame: 0,
                scope: Scope::Local,
                name: "total".to_string(),
            }
        );
        assert_eq!(difference.unchanged.len(), 1);
        assert!(difference.not_compared.is_empty(), "{difference:?}");

        // the stop the first was taken at has been resumed, and the snapshot is
        // still an answer. what has ended is asking that stop anything more
        assert!(difference.before.stop_has_ended);
        assert!(!difference.after.stop_has_ended);
        assert_eq!(difference.frames.moved.len(), 1, "the line moved");
    }

    #[test]
    fn a_value_a_bound_cut_short_is_not_compared_rather_than_called_unchanged() {
        let cut = Value {
            kind: "list".to_string(),
            content: Content::Sequence {
                items: vec![integer("1")],
                length: 900,
                omitted: Some(Omitted::Children {
                    length: 900,
                    limit: 1,
                }),
            },
        };
        let entry = vec![Entry {
            name: "items".to_string(),
            value: cut,
        }];
        let difference = difference(
            &state(1, "main", 3, entry.clone()),
            &state(2, "main", 3, entry),
            &[2],
        );

        assert!(
            difference.unchanged.is_empty() && difference.changed.is_empty(),
            "two readings that were both cut short are not evidence of either: \
             {difference:?}"
        );
        let [only] = difference.not_compared.as_slice() else {
            panic!("expected one uncomparable reading: {difference:?}")
        };
        assert!(matches!(
            only.why,
            WhyNot::Elided {
                side: Side::Both,
                ..
            }
        ));
        let said = only.to_string();
        assert!(said.contains("`items`"), "said {said}");
        assert!(said.contains("larger bound"), "said {said}");
    }

    #[test]
    fn an_elision_deep_inside_a_value_still_stops_it_being_compared() {
        // the bound bit four levels down, and the value around it looks whole
        let nested = Value {
            kind: "dict".to_string(),
            content: Content::Mapping {
                entries: vec![Pair {
                    key: integer("1"),
                    value: Value {
                        kind: "Node".to_string(),
                        content: Content::Object {
                            attributes: Vec::new(),
                            omitted: Some(Omitted::Depth { limit: 3 }),
                        },
                    },
                }],
                length: 1,
                omitted: None,
            },
        };
        let entry = vec![Entry {
            name: "graph".to_string(),
            value: nested,
        }];
        let difference = difference(
            &state(1, "main", 3, entry.clone()),
            &state(2, "main", 3, entry),
            &[],
        );
        assert_eq!(difference.not_compared.len(), 1, "{difference:?}");
        assert!(difference.unchanged.is_empty());
    }

    #[test]
    fn the_same_depth_running_different_code_is_not_compared() {
        let difference = difference(
            &state(
                1,
                "main",
                3,
                vec![Entry {
                    name: "x".to_string(),
                    value: integer("1"),
                }],
            ),
            &state(
                2,
                "handler",
                3,
                vec![Entry {
                    name: "x".to_string(),
                    value: integer("2"),
                }],
            ),
            &[2],
        );

        assert!(
            difference.changed.is_empty(),
            "`x` of `main` and `x` of `handler` are different variables: \
             {difference:?}"
        );
        let [only] = difference.not_compared.as_slice() else {
            panic!("expected one uncomparable reading: {difference:?}")
        };
        assert!(matches!(only.why, WhyNot::DifferentCode { .. }));
        assert!(
            only.to_string()
                .contains("position rather than an identity"),
            "said {only}"
        );
    }

    #[test]
    fn a_thing_only_one_snapshot_read_is_not_an_addition() {
        let mut before = state(1, "main", 3, Vec::new());
        before.state.values = vec![Answer {
            expression: "total".to_string(),
            frame: 0,
            result: Evaluated::Value {
                value: integer("1"),
            },
        }];
        let after = state(2, "main", 3, Vec::new());

        let difference = difference(&before, &after, &[2]);
        assert!(difference.added.is_empty() && difference.removed.is_empty());
        let [only] = difference.not_compared.as_slice() else {
            panic!("expected one uncomparable reading: {difference:?}")
        };
        assert!(matches!(
            only.why,
            WhyNot::NotBothRead { side: Side::Before }
        ));
        assert!(only.to_string().contains("unknown rather than absent"));
    }

    #[test]
    fn a_snapshot_id_round_trips_and_anything_else_is_refused_by_shape() {
        let id: SnapshotId = "3:9f2a1c".parse().expect("that is the shape");
        assert_eq!(id.stop, 3);
        assert_eq!(id.to_string(), "3:9f2a1c");

        for wrong in ["9f2a1c", "3:", "x:9f2a", "3:zzz"] {
            let refused = wrong
                .parse::<SnapshotId>()
                .expect_err("that is not the shape");
            assert!(refused.contains(wrong), "said {refused}");
        }
    }
}
