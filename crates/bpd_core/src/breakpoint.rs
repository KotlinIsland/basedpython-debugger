//! breakpoints: what the client asked for, and what is actually behind it
//!
//! the rule the whole module serves is that `bpd` never reports a breakpoint as
//! set unless there is a code object and an offset behind it. a request is a
//! [`SourceBreakpoint`], what became of it is a [`Resolved`], and the half of
//! that which says nothing will stop is an [`Unbound`] carrying why

use std::num::NonZeroU32;
use std::path::PathBuf;

use crate::exception::PythonError;

/// a breakpoint as the client asked for it
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceBreakpoint {
    /// the client's identity for this breakpoint, echoed in every report about it
    pub id: u32,
    /// the file the user named, spelled however they spelled it
    pub file: PathBuf,
    /// the line the user asked for
    pub line: u32,
    /// a python expression that has to be true before anything happens
    ///
    /// compiled once, when the breakpoint is set, and evaluated in the frame
    /// that reached the line. an expression that does not compile makes the
    /// breakpoint [`Unbound`], because a breakpoint whose condition can never
    /// be answered can never fire
    #[serde(default)]
    pub condition: Option<String>,
    /// how many qualifying hits to wait for
    ///
    /// a hit qualifies when the condition was true, or when there is no
    /// condition. a hit whose condition raised does not count, because it did
    /// not answer
    #[serde(default)]
    pub hits: Option<HitCondition>,
    /// produce a log record instead of stopping
    ///
    /// the text is emitted as it is written, except that `{...}` is a python
    /// expression evaluated in the frame and converted with `str()`. `{{` and
    /// `}}` are a literal brace
    #[serde(default)]
    pub log: Option<String>,
    /// the breakpoint that has to be hit before this one is armed at all
    ///
    /// "stop in the handler, but only after the request that set this flag came
    /// through". the id of another breakpoint in the **same set** — the set is
    /// replaced whole on every request, so there is nothing else it could name
    ///
    /// until that one has a qualifying hit, this one is bound and **not armed**:
    /// its location has no `LINE` events at all, so it costs nothing rather than
    /// being watched and ignored. what that means for what it can count is in
    /// [`HitCondition`] — a hit before arming is one the interpreter never
    /// reported, so it cannot be counted
    ///
    /// arming is **per process and permanent**. per process because the
    /// interpreter's local events are per code object rather than per thread, so
    /// a per-thread sequence would mean watching the location on every thread
    /// and discarding what the others saw — the cost this is supposed not to
    /// have. permanent because the case it is for is a flag being set once, and
    /// a sequence that re-arms is a different feature rather than a setting on
    /// this one
    #[serde(default)]
    pub after: Option<u32>,
}

impl SourceBreakpoint {
    /// a breakpoint that stops on every pass over the line
    pub fn at(id: u32, file: impl Into<PathBuf>, line: u32) -> Self {
        Self {
            id,
            file: file.into(),
            line,
            condition: None,
            hits: None,
            log: None,
            after: None,
        }
    }

    /// the same, armed only once the breakpoint `after` has been hit
    #[must_use]
    pub fn after(mut self, after: u32) -> Self {
        self.after = Some(after);
        self
    }

    /// stop only when `condition` is true
    #[must_use]
    pub fn when(mut self, condition: impl Into<String>) -> Self {
        self.condition = Some(condition.into());
        self
    }

    /// stop only on the hits `hits` selects
    #[must_use]
    pub const fn counting(mut self, hits: HitCondition) -> Self {
        self.hits = Some(hits);
        self
    }

    /// log `template` instead of stopping
    #[must_use]
    pub fn logging(mut self, template: impl Into<String>) -> Self {
        self.log = Some(template.into());
        self
    }
}

/// which of a breakpoint's qualifying hits it acts on
///
/// deliberately closed, and deliberately not DAP's `hitCondition` string. that
/// string means different things in different clients, and a debugger that
/// guesses which one a client meant is a debugger that stops on the wrong pass
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "hits", rename_all = "snake_case")]
pub enum HitCondition {
    /// only the nth qualifying hit, and nothing after it
    Exactly {
        /// which hit
        count: NonZeroU32,
    },
    /// the nth qualifying hit and every one after it
    AtLeast {
        /// the first hit that acts
        count: NonZeroU32,
    },
    /// every nth qualifying hit
    Every {
        /// the interval
        count: NonZeroU32,
    },
}

/// one thing a logpoint had to say
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogRecord {
    /// the breakpoint that produced it
    pub breakpoint: u32,
    /// the `co_filename` of the code object that was running
    pub file: String,
    /// the line it was produced on
    pub line: u32,
    /// the interpreter's identity for the thread that produced it
    pub thread: u64,
    /// which qualifying hit of that breakpoint this was, counting from one
    pub hit: u64,
    /// the log message, with every `{...}` replaced by `str()` of its value
    pub message: String,
}

/// what became of one requested breakpoint
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Resolved {
    /// the id the client gave it
    pub id: u32,
    /// whether there is a code object behind it, and where
    pub binding: Binding,
    /// the breakpoint this one is still waiting for, if it is waiting
    ///
    /// beside the binding rather than inside it, because it is a different
    /// question. [`Binding`] says whether the interpreter **has** somewhere to
    /// stop; this says whether it is watching it yet, and a breakpoint can be
    /// bound to a real line and not armed
    ///
    /// `None` is the ordinary breakpoint, armed the moment it binds. `Some` is
    /// one that named [`SourceBreakpoint::after`], and reporting it as plainly
    /// bound would leave a user waiting at a line the interpreter is not
    /// watching, with the debugger having said it was
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<u32>,
}

/// whether a breakpoint has a code object and an offset behind it
///
/// deliberately closed. there is no third state between "the interpreter will
/// stop here" and "it will not, and here is why", and `#[non_exhaustive]` would
/// invite a client to absorb one into a catch-all arm — which is how a
/// breakpoint ends up displayed as neither set nor refused
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "binding", rename_all = "snake_case")]
pub enum Binding {
    /// the interpreter will stop here
    Bound {
        /// the executable line it sits on
        ///
        /// not necessarily the line that was asked for — a request on a blank
        /// line, a comment or an elided `pass` moves to the next executable
        /// line, and this is where it went
        line: u32,
        /// every code object that holds that line, and where in each
        sites: Vec<Site>,
        /// how the condition will be answered on every hit
        evaluation: Evaluation,
    },

    /// the interpreter will stop here, while a django template renders
    ///
    /// a separate variant rather than a [`Site`] with the python fields left
    /// blank, because there is no code object and no offset behind a template
    /// breakpoint and there never will be: django compiles a template to a tree
    /// of `Node` objects, not to python. what is behind it is a node, reached
    /// through `Node.render_annotated`
    BoundInTemplate {
        /// the template line it sits on
        ///
        /// not necessarily the line that was asked for. a line with no node
        /// that renders through `Node.render_annotated` — a blank line, or one
        /// that is nothing but literal text — moves to the next line that has
        /// one, and this is where it went
        line: u32,
        /// the node classes django compiled that line to, in tree order
        ///
        /// more than one when a line holds more than one tag. reported for the
        /// reason [`Site::offset`] is: the client can see what is really behind
        /// its request rather than being told it worked
        nodes: Vec<String>,
        /// how the condition will be answered on every hit
        evaluation: Evaluation,
    },

    /// the interpreter will stop here, in the python a `.by` was transpiled to
    ///
    /// a separate variant rather than a [`Self::Bound`] whose `line` quietly
    /// means a different file, for the reason [`Self::BoundInTemplate`] is one:
    /// the two locations are both real and neither stands in for the other.
    /// `line` is the `.by` line the user asked about, `generated` is where the
    /// interpreter will really stop, and [`Site`] describes code objects of the
    /// generated python because that is the only place code objects exist
    ///
    /// a client that showed only `line` is showing the truth. one that showed
    /// only `generated` is too. what neither of them is doing is inventing a
    /// third location out of the two, which is what a single field would have
    /// left room for
    BoundInSource {
        /// the `.by` line it sits on
        ///
        /// not necessarily the line that was asked for. two things move it: a
        /// `.by` line the transpiler generated nothing for — a blank line, a
        /// comment — moves to the next one it did, and the generated line that
        /// produces may itself move on to the next executable one. this is
        /// where it ended up, read back out of the map rather than assumed
        line: u32,
        /// where in the generated python the interpreter will stop
        generated: crate::source_map::Located,
        /// every code object of the generated python that holds that line
        sites: Vec<Site>,
        /// how the condition will be answered on every hit
        evaluation: Evaluation,
    },

    /// nothing will stop, and this is why
    Unbound {
        /// what stood in the way
        reason: Unbound,
    },
}

/// why a breakpoint's `after` can never arm it
///
/// three ways, and they are told apart because the thing to do about each is
/// different: a typo'd id is fixed by naming a breakpoint that exists, and a
/// cycle is fixed by breaking it somewhere the client has to choose
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "no_arming", rename_all = "snake_case")]
pub enum NoArming {
    /// no breakpoint in the set has that id
    ///
    /// the set is replaced whole on every request, so there is nowhere else it
    /// could have meant — a breakpoint that was in the *previous* set is not in
    /// this program's any more
    NoSuchBreakpoint,

    /// it named itself, so it would have to be hit before it could be hit
    Itself,

    /// the chain it is in comes back to it
    Cycle {
        /// the ids the chain runs through, starting and ending at this one
        through: Vec<u32>,
    },
}

impl std::fmt::Display for NoArming {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchBreakpoint => formatter.write_str(
                "no breakpoint in this set has that id. the set is replaced \
                 whole on every request, so one that was in an earlier set is \
                 not in this program's any more",
            ),
            Self::Itself => formatter.write_str(
                "it named itself, so it would have to be hit before it could be \
                 hit",
            ),
            Self::Cycle { through } => {
                formatter.write_str("the chain comes back to it: ")?;
                for (at, id) in through.iter().enumerate() {
                    if at > 0 {
                        formatter.write_str(" after ")?;
                    }
                    write!(formatter, "{id}")?;
                }
                formatter.write_str(
                    ". nothing in a cycle is ever armed, because every link is \
                     waiting for one behind it",
                )
            }
        }
    }
}

/// how a breakpoint's condition is answered
///
/// reported for the same reason [`Site::offset`] is: the client can see what is
/// actually behind its request, rather than being told it worked
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evaluation {
    /// there is no condition, so every hit qualifies
    Always,

    /// `name <op> literal`, compared natively against the frame's fast locals
    ///
    /// the interpreter answers it instead when `name` is not a local of the
    /// frame that hit the line, because resolving a global the way `LOAD_NAME`
    /// does is the interpreter's job and reimplementing it is how a debugger
    /// reads a variable from the wrong scope. the answer is the same either
    /// way, which is what `crates/bpd_engine/tests/conditions.rs` pins
    Comparison,

    /// compiled once when the breakpoint was set, and evaluated by the
    /// interpreter against the frame on every hit
    Expression,
}

/// one code object a breakpoint is armed in
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Site {
    /// `co_qualname` — which function, lambda, comprehension or class body
    pub qualname: String,
    /// `co_firstlineno`, which separates two code objects with the same name
    pub first_line: u32,
    /// the first instrumentable bytecode offset for the bound line
    ///
    /// a line covers several offsets, and a stop anywhere but the first would
    /// land mid-statement
    pub offset: u32,
}

/// why a breakpoint has nothing behind it
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "unbound", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Unbound {
    /// the path does not name a file on disk
    Unresolvable {
        /// the path as the client gave it
        file: PathBuf,
        /// what the filesystem said
        reason: String,
        /// whether the interpreter has code carrying exactly that filename
        ///
        /// true is the shape of a module loaded from a zip archive, a frozen
        /// module, or a string handed to `exec` — the name is real, the file
        /// is not
        loaded_under_that_name: bool,
    },

    /// the path is a file, and nothing in the process has read it as code
    ///
    /// there are two ways a file becomes bindable and neither has happened: the
    /// interpreter has compiled no python from it, and no django template has
    /// been parsed from it
    NotLoaded {
        /// the path as the client gave it
        file: PathBuf,
        /// whether django's template machinery is loaded in this process
        ///
        /// it decides which of the two routes is worth naming. with django in
        /// the process an unloaded `.html` is a template that has not been
        /// rendered yet; without it, nothing in the process could ever have
        /// parsed one
        templates_available: bool,
    },

    /// django has parsed the template and no node of it renders at that line
    ///
    /// the analogue of [`Unbound::NoExecutableLine`], and a separate reason
    /// because "executable" is not what a template line is. django compiles a
    /// template to `Node` objects and renders each through
    /// `Node.render_annotated` — but `TextNode` **overrides** that method, so a
    /// line of nothing but literal text produces no event and cannot hold a
    /// breakpoint
    NoRenderedNode {
        /// the path as the client gave it
        file: PathBuf,
        /// the line that was asked for
        requested: u32,
        /// the last line of the template a node renders on, if it has one
        last_rendered: Option<u32>,
    },

    /// the interpreter has run code from the file, but never the file itself
    ///
    /// binding walks down from the code object the file's module compiled to,
    /// so without it only part of the file is visible — and every answer taken
    /// from a partial view is wrong in a way that looks right. so nothing is
    /// answered from one
    PartiallyLoaded {
        /// the path as the client gave it
        file: PathBuf,
    },

    /// the file is loaded and has no executable line at or after the one asked for
    NoExecutableLine {
        /// the path as the client gave it
        file: PathBuf,
        /// the line that was asked for
        requested: u32,
        /// the last line of that file the interpreter can stop on, if it has one
        last_executable: Option<u32>,
    },

    /// the condition does not compile
    ///
    /// nothing about the file is wrong. a breakpoint whose condition cannot be
    /// answered can never fire, so it is refused now — with the interpreter's
    /// own words — rather than at some line the user is waiting on
    ConditionInvalid {
        /// the expression as the client wrote it
        condition: String,
        /// what the interpreter said about it
        error: PythonError,
    },

    /// it waits for a breakpoint that can never hit it
    ///
    /// nothing about the file is wrong. the same rule
    /// [`Self::ConditionInvalid`] follows: a breakpoint that can never fire is
    /// refused **now**, rather than at a line somebody sits waiting on. a
    /// sequence whose first link is missing never arms anything after it, so
    /// every breakpoint in the chain is refused rather than the one that named
    /// it
    NeverArms {
        /// the breakpoint id it named
        after: u32,
        /// why that one can never arm it
        why: NoArming,
    },

    /// the file is basedpython and bpd was given no source map for this program
    ///
    /// a `.by` is never what the interpreter runs. without the map that says
    /// which generated line the request means, there is nothing to bind to and
    /// nothing to guess from — the alternative would be binding to a `.py` of
    /// the same name and hoping the lines line up, which is the identity
    /// fallback the source mapping rule exists to forbid
    NoSourceMap {
        /// the path as the client gave it
        file: PathBuf,
    },

    /// the source map cannot place that `.by` line in any generated python
    ///
    /// the map was loaded and verified, and it has no generated line for this
    /// one. every reason it can have is a fact the map itself carries rather
    /// than a limit of the search — see [`crate::source_map::Unmapped`]
    Unmappable {
        /// why the map could not place it
        reason: crate::source_map::Unmapped,
    },

    /// the `.by` line was placed, and nothing in the generated python holds it
    ///
    /// the ordinary reasons, one level down. a `.by` breakpoint that cannot bind
    /// because the module has not been imported yet fails for exactly the reason
    /// a python one does, and flattening that into a reason of its own would be
    /// two vocabularies for one fact. what is added is where the search really
    /// happened, because a user reading "not loaded" about a file in a temporary
    /// directory needs to know why bpd was looking there
    InGeneratedPython {
        /// the `.by` file, as the client gave it
        file: PathBuf,
        /// the line of it that was asked for
        requested: u32,
        /// where in the generated python that line is
        generated: crate::source_map::Located,
        /// what stood in the way there
        reason: Box<Unbound>,
    },

    /// the log message cannot be used
    LogMessageInvalid {
        /// the template as the client wrote it
        log: String,
        /// the embedded expression at fault, when one of them is
        expression: Option<String>,
        /// what is wrong with it
        reason: String,
    },
}

impl Unbound {
    /// whether this breakpoint may still bind, or is refused for good
    ///
    /// the one question a client acts on differently. DAP has a `reason` on an
    /// unverified breakpoint that is either `pending` — not bound yet, may bind
    /// later — or `failed`, and an editor is entitled to stop hoping about the
    /// second one
    ///
    /// it lives here rather than in an adapter because **which refusals are
    /// temporary is the core's fact**. an adapter that matched the one variant
    /// it happened to know about got it wrong the moment a reason arrived
    /// wrapped: [`Self::InGeneratedPython`] is an ordinary reason one level
    /// down, so a `.by` breakpoint waiting for its module reported `failed`
    /// where the identical `.py` one reported `pending`, and both bound on
    /// import
    ///
    /// the recursion is the point rather than an implementation detail —
    /// `InGeneratedPython` is the only variant that wraps another today, and
    /// the second one to be added is exactly what this is written against
    #[must_use]
    #[expect(
        clippy::match_same_arms,
        reason = "the arms are listed one per variant **because** their bodies \
                  agree today. collapsing them into a `_` is what let a wrapped \
                  reason be classified by nobody, and it is what would let the \
                  next variant be called permanent without a decision"
    )]
    pub fn will_bind_later(&self) -> bool {
        match self {
            // the file is not loaded, and loading it is a thing that happens
            Self::NotLoaded { .. } => true,

            // whatever stood in the way in the generated python, asked one
            // level down. the wrapper says where bpd looked, not what stopped it
            Self::InGeneratedPython { reason, .. } => reason.will_bind_later(),

            // and everything else is settled. **listed rather than caught**,
            // because a `_` arm would classify the next variant as permanent
            // without anybody deciding that — and the cost of being wrong is an
            // editor that stops hoping about a breakpoint which would have bound
            //
            // the file is not there, or is not a file at all
            Self::Unresolvable { .. } => false,
            // django has parsed the template already; the line renders nothing
            Self::NoRenderedNode { .. } => false,
            // only part of the file is visible, and nothing answers from a
            // partial view — see the variant's own doc
            Self::PartiallyLoaded { .. } => false,
            // the file is loaded and has no such line
            Self::NoExecutableLine { .. } => false,
            // a condition that does not compile never will
            Self::ConditionInvalid { .. } => false,
            // the breakpoint it waits for can never arm it
            Self::NeverArms { .. } => false,
            // no map was given for this program
            Self::NoSourceMap { .. } => false,
            // the map is loaded and cannot place the line: its own answer, and
            // nothing arriving later changes it
            Self::Unmappable { .. } => false,
            // the log message is malformed
            Self::LogMessageInvalid { .. } => false,
        }
    }
}

/// [`Unbound::NotLoaded`], which has two routes into a file and neither taken
///
/// a `.html` in a process that has django in it is a template that has not been
/// rendered yet, and that binds later. the same file in a process without
/// django is a file nothing could ever have parsed
fn not_loaded(
    formatter: &mut std::fmt::Formatter<'_>,
    file: &std::path::Path,
    templates_available: bool,
) -> std::fmt::Result {
    write!(
        formatter,
        "the interpreter has not loaded any code from `{}`. it will bind if \
         that file is imported later",
        file.display()
    )?;
    if templates_available {
        formatter.write_str(
            ". django is in this process and has not parsed a template from it \
             either — a template breakpoint binds the first time django loads \
             the template, so this one binds when something renders it",
        )
    } else {
        Ok(())
    }
}

/// [`Unbound::NoRenderedNode`], and the two ways a template line reaches no node
fn no_rendered_node(
    formatter: &mut std::fmt::Formatter<'_>,
    file: &std::path::Path,
    requested: u32,
    last_rendered: Option<u32>,
) -> std::fmt::Result {
    write!(
        formatter,
        "django has parsed `{}` and no node of it renders at or after line \
         {requested}",
        file.display()
    )?;
    match last_rendered {
        Some(last) => write!(
            formatter,
            ". the last line that renders one is line {last}. a line of nothing \
             but literal text is not one of them, and neither is anything an \
             `{{% extends %}}` puts out of reach: django renders neither \
             through the method bpd watches"
        ),
        None => formatter.write_str(
            ". no line of it renders through the method bpd watches. a template \
             of nothing but literal text is one way that happens, and so is one \
             whose `{% extends %}` leaves django rendering none of its own \
             nodes",
        ),
    }
}

/// [`Unbound::NoSourceMap`], and what would have produced one
fn no_source_map(
    formatter: &mut std::fmt::Formatter<'_>,
    file: &std::path::Path,
) -> std::fmt::Result {
    write!(
        formatter,
        "`{}` is basedpython source, and bpd has no source map for this \
         program. the interpreter never runs a `.by` — it runs the python `by` \
         transpiled it to — so without the map that says which generated line \
         this one is, there is nothing to bind to. run the program with `bpd \
         by`, which transpiles it and hands bpd the map `by run` wrote",
        file.display()
    )
}

/// [`Unbound::InGeneratedPython`], which is an ordinary reason one level down
///
/// both locations, in the order a person reads them: the line they asked about,
/// where `by` put it, and then the reason as it would read for any python file.
/// a user meeting "not loaded" about a path in a temporary directory needs the
/// middle clause to make sense of the last one
fn in_generated_python(
    formatter: &mut std::fmt::Formatter<'_>,
    file: &std::path::Path,
    requested: u32,
    generated: &crate::source_map::Located,
    reason: &Unbound,
) -> std::fmt::Result {
    write!(
        formatter,
        "line {requested} of `{}` is line {} of `{}`, which `by` transpiled it \
         to, and {reason}",
        file.display(),
        generated.line,
        generated.file.display()
    )
}

impl std::fmt::Display for Unbound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeverArms { after, why } => write!(
                formatter,
                "it is armed only once breakpoint {after} has been hit, and \
                 {why}. a breakpoint that can never arm can never fire, so it \
                 is refused now rather than at a line somebody is waiting on"
            ),
            Self::Unresolvable {
                file,
                reason,
                loaded_under_that_name,
            } => {
                write!(
                    formatter,
                    "`{}` is not a file on disk: {reason}",
                    file.display()
                )?;
                if *loaded_under_that_name {
                    write!(
                        formatter,
                        ". the interpreter has loaded code under exactly that \
                         name, so it came from somewhere that is not the \
                         filesystem — a zip archive, a frozen module, or a \
                         string passed to `exec`. bpd binds breakpoints to \
                         files it can identify on disk, and cannot identify \
                         that one"
                    )?;
                }
                Ok(())
            }
            Self::NotLoaded {
                file,
                templates_available,
            } => not_loaded(formatter, file, *templates_available),
            Self::NoRenderedNode {
                file,
                requested,
                last_rendered,
            } => no_rendered_node(formatter, file, *requested, *last_rendered),
            Self::PartiallyLoaded { file } => write!(
                formatter,
                "the interpreter has run code from `{}` but never the file \
                 itself, so bpd has seen only part of it and cannot say where a \
                 breakpoint in it would go. a module first reached from inside \
                 a breakpoint's condition does this, because the interpreter \
                 reports no code object created while a monitoring callback is \
                 running. import it somewhere the program itself runs, or take \
                 the condition that imports it off",
                file.display()
            ),
            Self::NoExecutableLine {
                file,
                requested,
                last_executable,
            } => {
                write!(
                    formatter,
                    "`{}` has no executable line at or after line {requested}",
                    file.display()
                )?;
                match last_executable {
                    Some(last) => write!(formatter, ". the last one is line {last}"),
                    None => write!(formatter, ". it has no executable lines at all"),
                }
            }
            Self::NoSourceMap { file } => no_source_map(formatter, file),
            Self::Unmappable { reason } => write!(formatter, "{reason}"),
            Self::InGeneratedPython {
                file,
                requested,
                generated,
                reason,
            } => in_generated_python(formatter, file, *requested, generated, reason),
            Self::ConditionInvalid { condition, error } => write!(
                formatter,
                "the condition `{condition}` does not compile: {error}. a \
                 breakpoint whose condition cannot be answered can never fire, \
                 so it is not set"
            ),
            Self::LogMessageInvalid {
                log,
                expression,
                reason,
            } => {
                write!(formatter, "the log message `{log}` cannot be used")?;
                match expression {
                    Some(expression) => {
                        write!(formatter, ": `{{{expression}}}` {reason}")
                    }
                    None => write!(formatter, ": {reason}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_unbound_reason_says_what_to_do_about_it() {
        let file = PathBuf::from("/tmp/program.py");
        let cases = [
            (
                Unbound::Unresolvable {
                    file: file.clone(),
                    reason: "Not a directory (os error 20)".to_string(),
                    loaded_under_that_name: true,
                },
                "zip archive",
            ),
            (
                Unbound::NotLoaded {
                    file: file.clone(),
                    templates_available: false,
                },
                "imported later",
            ),
            (
                Unbound::NotLoaded {
                    file: file.clone(),
                    templates_available: true,
                },
                "when something renders it",
            ),
            (
                Unbound::NoRenderedNode {
                    file: file.clone(),
                    requested: 4,
                    last_rendered: Some(2),
                },
                "the last line that renders one is line 2",
            ),
            (
                Unbound::NoRenderedNode {
                    file: file.clone(),
                    requested: 4,
                    last_rendered: None,
                },
                "no line of it renders through the method bpd watches",
            ),
            (
                Unbound::PartiallyLoaded { file: file.clone() },
                "never the file itself",
            ),
            (
                Unbound::NoExecutableLine {
                    file: file.clone(),
                    requested: 40,
                    last_executable: Some(12),
                },
                "the last one is line 12",
            ),
            (
                Unbound::NoExecutableLine {
                    file: file.clone(),
                    requested: 40,
                    last_executable: None,
                },
                "no executable lines at all",
            ),
            (
                Unbound::NoSourceMap { file: file.clone() },
                "run the program with `bpd by`",
            ),
            (
                Unbound::Unmappable {
                    reason: crate::source_map::Unmapped::NoGeneratedLine {
                        file: file.clone(),
                        requested: 4,
                        last_mapped: Some(2),
                    },
                },
                "the last line it generated anything for is line 2",
            ),
            (
                Unbound::InGeneratedPython {
                    file,
                    requested: 7,
                    generated: crate::source_map::Located {
                        file: PathBuf::from("/tmp/build/program.py"),
                        line: 15,
                    },
                    reason: Box::new(Unbound::NoExecutableLine {
                        file: PathBuf::from("/tmp/build/program.py"),
                        requested: 15,
                        last_executable: Some(12),
                    }),
                },
                "line 7 of `/tmp/program.py` is line 15 of `/tmp/build/program.py`",
            ),
        ];

        for (reason, expected) in cases {
            let said = reason.to_string();
            assert!(
                said.contains("/tmp/program.py"),
                "a refusal must name the file, got {said}"
            );
            assert!(said.contains(expected), "expected {expected:?} in {said:?}");
        }
    }

    #[test]
    fn a_refused_condition_or_log_message_quotes_the_thing_that_is_wrong() {
        let cases = [
            (
                Unbound::ConditionInvalid {
                    condition: "value ==".to_string(),
                    error: PythonError {
                        kind: "SyntaxError".to_string(),
                        message: "invalid syntax".to_string(),
                        traceback: Vec::new(),
                    },
                },
                ["value ==", "SyntaxError: invalid syntax", "can never fire"],
            ),
            (
                Unbound::LogMessageInvalid {
                    log: "count is {".to_string(),
                    expression: None,
                    reason: "there is a `{` that is never closed".to_string(),
                },
                ["count is {", "never closed", "cannot be used"],
            ),
            (
                Unbound::LogMessageInvalid {
                    log: "count is {1 +}".to_string(),
                    expression: Some("1 +".to_string()),
                    reason: "does not compile: SyntaxError: invalid syntax".to_string(),
                },
                ["{1 +}", "does not compile", "cannot be used"],
            ),
        ];

        for (reason, expected) in cases {
            let said = reason.to_string();
            for wanted in expected {
                assert!(said.contains(wanted), "expected {wanted:?} in {said:?}");
            }
        }
    }

    /// every `Unbound` this test knows how to build, so the sweep below is
    /// about the enum rather than about three variants somebody picked
    fn every_reason() -> Vec<(Unbound, bool)> {
        use std::path::PathBuf;
        // every variant, not six of eleven. the comment used to say this and the
        // list did not do it — and `will_bind_later` had a `_` arm underneath,
        // so a new variant would have been classified permanent by nobody
        vec![
            (
                Unbound::NotLoaded {
                    file: PathBuf::from("/tmp/app.py"),
                    templates_available: false,
                },
                true,
            ),
            (
                Unbound::Unresolvable {
                    file: PathBuf::from("/tmp/app.py"),
                    reason: "no such file".to_string(),
                    loaded_under_that_name: false,
                },
                false,
            ),
            (
                Unbound::PartiallyLoaded {
                    file: PathBuf::from("/tmp/app.py"),
                },
                false,
            ),
            (
                Unbound::NoExecutableLine {
                    file: PathBuf::from("/tmp/app.py"),
                    requested: 9,
                    last_executable: Some(4),
                },
                false,
            ),
            (
                Unbound::NoSourceMap {
                    file: PathBuf::from("/src/app.by"),
                },
                false,
            ),
            (
                Unbound::Unmappable {
                    reason: crate::source_map::Unmapped::NotInTheMap {
                        file: PathBuf::from("/src/app.by"),
                    },
                },
                false,
            ),
            (
                Unbound::NoRenderedNode {
                    file: PathBuf::from("/tmp/index.html"),
                    requested: 3,
                    last_rendered: Some(9),
                },
                false,
            ),
            (
                Unbound::ConditionInvalid {
                    condition: "x ==".to_string(),
                    error: PythonError {
                        kind: "SyntaxError".to_string(),
                        message: "invalid syntax".to_string(),
                        traceback: Vec::new(),
                    },
                },
                false,
            ),
            (
                Unbound::NeverArms {
                    after: 3,
                    why: NoArming::NoSuchBreakpoint,
                },
                false,
            ),
            (
                Unbound::LogMessageInvalid {
                    log: "{x".to_string(),
                    expression: None,
                    reason: "unbalanced brace".to_string(),
                },
                false,
            ),
        ]
    }

    /// the same reason, wrapped in the one variant that wraps
    fn in_generated(reason: Unbound) -> Unbound {
        use std::path::PathBuf;
        Unbound::InGeneratedPython {
            file: PathBuf::from("/src/app.by"),
            requested: 5,
            generated: crate::source_map::Located {
                file: PathBuf::from("/tmp/build/app.py"),
                line: 86,
            },
            reason: Box::new(reason),
        }
    }

    #[test]
    fn a_wrapped_reason_is_as_temporary_as_the_reason_inside_it() {
        // the fact this is about: `InGeneratedPython` says where bpd looked,
        // not what stopped it. a `.by` breakpoint waiting for its module is
        // waiting for exactly what the `.py` one is
        for (reason, temporary) in every_reason() {
            let bare = reason.will_bind_later();
            assert_eq!(
                bare, temporary,
                "unwrapped, this is the wrong answer: {reason:?}"
            );

            let wrapped = in_generated(reason.clone());
            assert_eq!(
                wrapped.will_bind_later(),
                bare,
                "wrapping changed the answer, and the wrapper is not what stood \
                 in the way: {wrapped:?}"
            );

            // and again, because one level of unwrapping is the fix somebody
            // reaches for first and it is not what this says
            let twice = in_generated(in_generated(reason));
            assert_eq!(
                twice.will_bind_later(),
                bare,
                "a second wrapper is where an unwrap that only went one deep \
                 would stop: {twice:?}"
            );
        }
    }
}
