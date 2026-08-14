//! replacing the code a live process is running with the code on disk
//!
//! a program is edited while it runs. the file on disk stops being the code the
//! process is executing, and cpython says nothing — a traceback is rendered by
//! `linecache` reading the file *now*, so an edited file is shown with current
//! text against old line numbers. `bpd` already refuses to show source it cannot
//! prove: a frame's file is compiled and the frame's own code object has to be
//! in what comes out, or the answer is [`crate::Unverified::NotTheSameCode`]
//!
//! this is that comparison inverted. a mismatch is what makes a replacement
//! worth offering, and the same evidence decides whether one can be made
//!
//! ## what a replacement is
//!
//! a set of assignments to `function.__code__`, and nothing else. **the top
//! level is never re-run**, no name is bound or unbound, and no object is
//! created. every function object in the process that was running this file's
//! old code runs its new code afterwards, including the ones no namespace still
//! points at — a decorator's captured original, a closure a factory handed out,
//! a method reached through a `staticmethod`
//!
//! that is the whole mechanism, and it is why a class needs nothing of its own:
//! a method **is** a function object in the class dictionary, so rebinding its
//! code is seen by every instance that already exists, immediately, with no
//! dictionary written to
//!
//! ## the rule, which is what makes it total
//!
//! the file is compiled and the resulting tree is compared, code object by code
//! object, against the tree the process is running. a replacement is applicable
//! exactly when **every difference between the two is inside the body of a
//! function that exists in both and takes the same arguments**
//!
//! any other difference is refused, because applying it would need something a
//! code swap cannot do:
//!
//! | the difference | what applying it would need |
//! | --- | --- |
//! | the module body's own instructions, names or constants | re-running the top level |
//! | a function or class added or removed | binding or unbinding a module name |
//! | a class body's own instructions | writing the class dictionary |
//! | a function's parameters | callers in flight expecting the old ones |
//!
//! re-running the top level is not a reload, it is running the program a second
//! time: its imports, its calls and its registrations all happen again, and
//! every name it binds becomes a **new object** that anything already holding
//! the old one will never see. so `bpd` does not do it, and says so
//!
//! ## it is never applied partially
//!
//! a process half way between two versions of a module produces evidence about
//! neither. every refusal is collected before anything is written, so a
//! replacement that cannot be made whole changes nothing at all — and
//! [`Replacement::Refused`] carries **all** of what stood in the way rather than
//! the first, because a client fixing them one at a time is a client running
//! this seventeen times
//!
//! ## the frames
//!
//! a frame keeps its own reference to the code object it is running.
//! **measured on 3.13, 3.14, 3.15 and 3.14t**: assigning `function.__code__`
//! while a frame of that function is in flight is accepted, the frame runs the
//! old code to completion, and the next call gets the new one. nothing crashes
//! and nothing is corrupted — this is not the `f_lineno` trap, which really does
//! abort the interpreter, and it must never be described as one
//!
//! it is refused all the same, and the reason is the rule above rather than
//! safety. between the assignment and that frame returning, the process is
//! running two versions of one function at once: a stack showing one file whose
//! frames behave two different ways is evidence about neither version. so a
//! replacement requires that no frame anywhere in the process is running any of
//! the file's code — not on a thread, and not suspended inside a generator, a
//! coroutine or an async generator waiting to be sent into

use std::path::PathBuf;

use crate::breakpoint::Resolved;
use crate::exception::PythonError;
use crate::jump::Suspendable;
use crate::stop::Mode;

/// what a replacement did to the process
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Replaced {
    /// the file that was asked about, as the client named it
    pub file: PathBuf,
    /// what became of it
    pub outcome: Replacement,
    /// how the program was moving while this was done
    ///
    /// [`Mode::NonStop`] is the ordinary one, and it bounds what the refusals
    /// below are: whether another thread is running this file's code is read
    /// from a **sample** of the threads that are not held. stopping the world
    /// first is what makes it a reading of all of them
    pub mode: Mode,
}

/// what became of a replacement
///
/// deliberately closed, and deliberately not a `bool`: a replacement that was
/// not made carries what stood in the way, and a caller handed `false` would
/// have to invent one
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "replaced", rename_all = "snake_case")]
pub enum Replacement {
    /// every function object in the process now runs the code on disk
    Applied {
        /// the functions whose code changed, and what changed about each
        ///
        /// empty means the file on disk already **was** what the process is
        /// running: nothing needed replacing, which is a different fact from
        /// nothing being replaceable
        changed: Vec<Rebound>,
        /// `co_qualname` of the file's functions whose code is unchanged
        unchanged: Vec<String>,
        /// breakpoints whose binding the replacement changed
        ///
        /// a breakpoint was bound to a code object nothing will execute any
        /// more, and it is rebound against the code that is running now. this
        /// is the second thing a replacement changes about the process, and a
        /// client that was not told would be watching a line it can see is
        /// armed and never reached
        rebound: Vec<Resolved>,

        /// frames that go on running the code this replaced
        ///
        /// **empty unless the caller asked for it.** without
        /// `even_under_a_live_frame` a frame running the code is a refusal, so
        /// an applied replacement had none — and this being empty is then the
        /// ordinary guarantee rather than an absence of information
        ///
        /// with it, this is what that guarantee was traded for, and it is the
        /// whole of what was traded: every frame that will finish on the old
        /// code, named. see [`StillRunning`] for what it does and does not say
        still_running: Vec<StillRunning>,
    },

    /// nothing was changed, and this is everything that stood in the way
    Refused {
        /// all of it, rather than the first
        because: Vec<Unreplaceable>,
    },
}

/// one function whose code was replaced
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rebound {
    /// `co_qualname` of the code that was replaced
    pub function: String,
    /// the line its code began on before
    pub was_at: u32,
    /// the line its code begins on now
    ///
    /// different from `was_at` when an edit above it moved it down the file.
    /// that is the change a debugger normally gets wrong: the line numbers a
    /// stack reports come from the code object, and until this the process's
    /// were the old file's
    pub now_at: u32,
    /// how many function objects in the process were running that code
    ///
    /// one for an ordinary function. more when a decorator kept the original,
    /// when a closure factory handed several out, or when the same function is
    /// bound under two names — every one of them was rebound, which is the
    /// point of counting them rather than assuming one
    pub objects: u32,
}

/// why a replacement was not made
///
/// every variant names the thing that blocked it, because "cannot" without a
/// name is a client guessing which of its edits to undo
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "unreplaceable", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Unreplaceable {
    /// there is nothing on disk to compile
    NotAFile {
        /// what was asked about
        file: PathBuf,
        /// what the filesystem said
        reason: String,
    },

    /// the file on disk does not compile
    ///
    /// compiling runs none of the program — it is the compiler, on bytes — so
    /// this is a syntax error in the edit and nothing of the process was touched
    DoesNotCompile {
        /// what was asked about
        file: PathBuf,
        /// the compiler's own error
        error: PythonError,
    },

    /// the interpreter has compiled nothing from this file
    NotLoaded {
        /// what was asked about
        file: PathBuf,
    },

    /// the interpreter has compiled this file more than once
    ///
    /// two module-level code objects for one file, which is what `runpy`, a
    /// re-import after `del sys.modules[...]`, or an explicit `compile` of the
    /// same path produces. which live function object belongs to which copy is
    /// not answerable from the file, and answering it by resemblance is the
    /// thing this project refuses
    CompiledMoreThanOnce {
        /// what was asked about
        file: PathBuf,
        /// how many module-level code objects there are for it
        copies: u32,
    },

    /// only part of the file's code has ever been seen
    ///
    /// the same limit binding a breakpoint has: the whole tree is only reachable
    /// when the module's own code object was registered, and a partial view
    /// answers every question about a file wrongly and plausibly
    PartiallyLoaded {
        /// what was asked about
        file: PathBuf,
    },

    /// the module body itself is different code
    ///
    /// applying it would mean **re-running the top level**, which is running the
    /// program a second time rather than reloading it
    TopLevelChanged {
        /// what was asked about
        file: PathBuf,
        /// what is different about it
        differences: Vec<Divergence>,
    },

    /// a class body itself is different code
    ///
    /// the methods of a class are function objects and their code is replaceable
    /// like any other. the class **body** is the code that built the class, and
    /// applying a change to it would mean writing the class dictionary of a type
    /// whose instances already exist
    ClassLayoutChanged {
        /// `co_qualname` of the class
        class: String,
        /// what is different about it
        differences: Vec<Divergence>,
    },

    /// a function takes different arguments than the callers in flight expect
    SignatureChanged {
        /// `co_qualname` of the function
        function: String,
        /// the parameters it takes now, as the process has them
        was: String,
        /// the parameters the file on disk gives it
        now: String,
    },

    /// a frame is running code of this file
    Running {
        /// `co_qualname` of the code the frame is running
        function: String,
        /// where that frame is
        frame: LiveFrame,
    },

    /// a changed function holds two nested code objects of the same name
    ///
    /// a function whose own body changed has its nested code objects — a
    /// closure, a lambda, a generator expression — matched to the new ones by
    /// `co_qualname`, because their positions moved with the edit. two lambdas
    /// written in one function share a `co_qualname`, and there is then nothing
    /// that says which new one replaces which old one. picking either would be a
    /// coin toss over which body a live closure runs
    Ambiguous {
        /// `co_qualname` of the function that holds them
        function: String,
        /// the name that names more than one of them
        nested: String,
    },

    /// live objects run a nested function the file no longer defines
    ///
    /// a closure handed out by a factory whose body has since dropped it. the
    /// module's own names are covered by [`Unreplaceable::TopLevelChanged`],
    /// which sees a `def` disappear; this is the same fact one level in, where
    /// the only thing that can see it is the heap
    Orphaned {
        /// `co_qualname` of what the file no longer defines
        function: String,
        /// how many function objects in the process still run it
        objects: u32,
    },

    /// a live function object's closure does not fit the code on disk
    ///
    /// cpython requires a function's cell count and its code's free variable
    /// count to agree, and refuses the assignment otherwise. it is checked here,
    /// before anything is written, because a refusal that arrived half way
    /// through would be the partial application this whole feature refuses
    ClosureChanged {
        /// `co_qualname` of the function
        function: String,
        /// the cells the live function object holds
        cells: u32,
        /// the free variables the code on disk wants
        wanted: u32,
    },
}

/// what is different about a body that has to be identical
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "differs", rename_all = "snake_case")]
pub enum Divergence {
    /// the functions and classes it defines
    Defines {
        /// `co_qualname` of what the file on disk defines and the process has not
        added: Vec<String>,
        /// `co_qualname` of what the process has and the file on disk does not
        removed: Vec<String>,
    },

    /// the names it reads or writes
    Names {
        /// what the file on disk names and the process does not
        added: Vec<String>,
        /// what the process names and the file on disk does not
        removed: Vec<String>,
    },

    /// the instructions it runs
    ///
    /// the catch-all, and the one that carries the literal values a body uses:
    /// what is compared is the **resolved** instruction stream, so a load of
    /// `5` where the process loads `7` is a different instruction
    Instructions,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Defines { added, removed } => {
                formatter.write_str("it defines different things")?;
                names(formatter, added, removed)
            }
            Self::Names { added, removed } => {
                formatter.write_str("it reads or writes different names")?;
                names(formatter, added, removed)
            }
            Self::Instructions => formatter.write_str("it runs different instructions"),
        }
    }
}

/// the added and removed halves of a difference, when there are any
fn names(
    formatter: &mut std::fmt::Formatter<'_>,
    added: &[String],
    removed: &[String],
) -> std::fmt::Result {
    if !added.is_empty() {
        write!(formatter, " — {added:?} is new")?;
    }
    if !removed.is_empty() {
        write!(
            formatter,
            "{} {removed:?} is gone",
            if added.is_empty() { " —" } else { " and" }
        )?;
    }
    Ok(())
}

/// a frame that will finish on the code a replacement replaced
///
/// only ever produced when the caller asked for a replacement under a live
/// frame. it is the cost of that, stated: until this frame returns, the process
/// runs two versions of one function
///
/// **it is true when it is made and not afterwards.** a frame here returns on
/// its own schedule and nothing reports when one has, so this says which frames
/// were on the old code at the instant of the replacement. a caller reading it
/// as the state of the process *now* is reading a list that has been going out
/// of date since it was written — which is why the ordinary answer is a refusal
/// and this one has to be asked for
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StillRunning {
    /// `co_qualname` of the code it is running
    pub function: String,
    /// where the frame is
    pub frame: LiveFrame,
}

impl std::fmt::Display for StillRunning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "`{}` was replaced while {} — that frame finishes on the code it \
             started with, so until it returns the process is running two \
             versions of one function",
            self.function, self.frame
        )
    }
}

/// where a frame that is running the code was found
///
/// two different facts, and a client shown one as the other would be told a
/// thread is busy when what is really holding the replacement up is an
/// abandoned generator nobody is iterating
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum LiveFrame {
    /// a thread is executing it
    Thread {
        /// which thread
        thread: u64,
        /// the line the frame is on
        line: u32,
        /// the stop holding that thread, when `bpd` is holding it
        ///
        /// `None` is a thread that is running, and the sighting is then a
        /// sample — see [`Replaced::mode`]
        held: Option<u64>,
    },

    /// a generator, coroutine or async generator holds a frame of it
    ///
    /// not on any thread's stack, and it will run the old code the moment
    /// anything sends into it
    Suspended {
        /// which of the three it is
        kind: Suspendable,
        /// the line the frame is on
        line: u32,
        /// whether it has run at all yet
        ///
        /// `false` is one that was created and never advanced. it will start at
        /// the top of the old code the first time it is
        started: bool,
    },
}

impl std::fmt::Display for LiveFrame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Thread { thread, line, held } => {
                write!(formatter, "thread {thread} is executing line {line} of it")?;
                match held {
                    Some(stop) => write!(formatter, ", held by stop {stop}"),
                    None => formatter.write_str(", and is running"),
                }
            }
            Self::Suspended {
                kind,
                line,
                started,
            } => write!(
                formatter,
                "{kind} {} at line {line}",
                if *started {
                    "is suspended"
                } else {
                    "was created and never advanced, and would start"
                }
            ),
        }
    }
}

impl std::fmt::Display for Unreplaceable {
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per refusal, and every one of them is a whole \
                  sentence about what stood in the way and what to do instead. \
                  splitting them out would put half of a message somewhere \
                  nobody reading the variant would find it"
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAFile { file, reason } => write!(
                formatter,
                "`{}` is not a file the debuggee can read: {reason}. the file is \
                 read on the debuggee's own filesystem, which is where the \
                 interpreter read it from",
                file.display()
            ),
            Self::DoesNotCompile { file, error } => write!(
                formatter,
                "`{}` does not compile: {error}. nothing of the process was \
                 touched — compiling runs none of the program, so this is the \
                 edit rather than the debuggee",
                file.display()
            ),
            Self::NotLoaded { file } => write!(
                formatter,
                "the interpreter has compiled nothing from `{}`, so there is no \
                 code of it to replace. a module that has not been imported has \
                 no live function objects, and importing it is the program's \
                 business rather than the debugger's",
                file.display()
            ),
            Self::CompiledMoreThanOnce { file, copies } => write!(
                formatter,
                "the interpreter has compiled `{}` {copies} times, so there are \
                 {copies} versions of its code alive at once — `runpy`, a \
                 re-import after `del sys.modules[...]` and an explicit \
                 `compile` of the same path all do this. which live function \
                 object belongs to which copy is not answerable from the file, \
                 and answering it by resemblance is what a debugger must never \
                 do. restart the process",
                file.display()
            ),
            Self::Orphaned { function, objects } => write!(
                formatter,
                "{objects} function objects in the process run `{function}`, and \
                 the file on disk no longer defines it — a closure handed out by \
                 a factory whose body has since dropped it. replacing everything \
                 around them would leave them running code that is in no version \
                 of the file, which is the half-replaced process this refuses to \
                 make. restart the process, or put it back"
            ),
            Self::PartiallyLoaded { file } => write!(
                formatter,
                "bpd has seen only part of the code compiled from `{}` — its \
                 module-level code object was never registered, so the tree \
                 under it cannot be walked. a partial view answers every \
                 question about a file wrongly and plausibly, so nothing is \
                 answered from one",
                file.display()
            ),
            Self::TopLevelChanged { file, differences } => {
                write!(
                    formatter,
                    "the module body of `{}` is different code now",
                    file.display()
                )?;
                each(formatter, differences)?;
                formatter.write_str(
                    ". applying that would mean re-running the top level, which \
                     is running the program a second time rather than reloading \
                     it: its imports, its calls and its registrations would all \
                     happen again, and every name it binds would become a new \
                     object that anything already holding the old one would \
                     never see. restart the process, or put the edit inside a \
                     function body",
                )
            }
            Self::ClassLayoutChanged { class, differences } => {
                write!(formatter, "the body of class `{class}` is different code")?;
                each(formatter, differences)?;
                formatter.write_str(
                    ". a method is a function object and its code is replaceable \
                     like any other, but the class **body** is the code that \
                     built the class — applying a change to it would mean \
                     writing the dictionary of a type whose instances already \
                     exist, and those instances were built by the old body. \
                     restart the process",
                )
            }
            Self::SignatureChanged { function, was, now } => write!(
                formatter,
                "`{function}` takes `{now}` on disk and `{was}` in the process. \
                 a call already in flight was made against `{was}`, and the \
                 callers that have not been reached yet were compiled against it \
                 too — replacing the body under a different parameter list would \
                 make them pass arguments the function no longer has. restart \
                 the process, or keep the parameters and change the body"
            ),
            Self::Running { function, frame } => write!(
                formatter,
                "`{function}` is being run right now: {frame}. replacing its code \
                 would be accepted by cpython — a frame holds its own reference \
                 to the code object, so the frame in flight would run the old \
                 code to completion and the next call would get the new one — \
                 and that is exactly what is refused: until that frame returns \
                 the process is running two versions of one function, and a \
                 stack whose frames behave two different ways is evidence about \
                 neither. let it return first"
            ),
            Self::Ambiguous { function, nested } => write!(
                formatter,
                "the body of `{function}` changed, and it holds more than one \
                 nested code object named `{nested}` — two lambdas written in \
                 one function share a name. matching them to the new ones by \
                 position is not available once the body moved, so there is \
                 nothing that says which new one replaces which old one, and \
                 picking either would be a coin toss over which body a live \
                 closure runs. give them names by making them `def`s, or restart \
                 the process"
            ),
            Self::ClosureChanged {
                function,
                cells,
                wanted,
            } => write!(
                formatter,
                "`{function}` holds {cells} closure cells and the code on disk \
                 wants {wanted} free variables. cpython refuses that assignment, \
                 and it is checked before anything is written rather than found \
                 half way through — the enclosing function's variables are what \
                 changed, so replace it from the outside or restart the process"
            ),
        }
    }
}

/// the differences of one body, as a list a sentence can hold
fn each(formatter: &mut std::fmt::Formatter<'_>, differences: &[Divergence]) -> std::fmt::Result {
    for (index, difference) in differences.iter().enumerate() {
        formatter.write_str(if index == 0 { ": " } else { ", and " })?;
        write!(formatter, "{difference}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(
        clippy::too_many_lines,
        reason = "one case per refusal, which is what makes a refusal that \
                  nobody checked the wording of visible here"
    )]
    #[test]
    fn a_refusal_names_the_thing_that_blocked_it_and_what_to_do_about_it() {
        let cases = [
            (
                Unreplaceable::TopLevelChanged {
                    file: PathBuf::from("/tmp/victim.py"),
                    differences: vec![Divergence::Defines {
                        added: vec!["helper".to_string()],
                        removed: Vec::new(),
                    }],
                },
                vec![
                    "/tmp/victim.py",
                    "\"helper\"] is new",
                    // the roadmap's own objection, and the thing a caller has to
                    // understand before it asks again
                    "running the program a second time",
                    "restart the process",
                ],
            ),
            (
                Unreplaceable::ClassLayoutChanged {
                    class: "Widget".to_string(),
                    differences: vec![Divergence::Instructions],
                },
                vec!["Widget", "different instructions", "instances already"],
            ),
            (
                Unreplaceable::SignatureChanged {
                    function: "handle".to_string(),
                    was: "(request)".to_string(),
                    now: "(request, timeout)".to_string(),
                },
                vec!["handle", "(request, timeout)", "in flight"],
            ),
            (
                Unreplaceable::Running {
                    function: "worker".to_string(),
                    frame: LiveFrame::Thread {
                        thread: 12,
                        line: 40,
                        held: Some(3),
                    },
                },
                vec![
                    "worker",
                    "thread 12",
                    "stop 3",
                    // the reason is the two-version process, and it must never
                    // be given as crash prevention: cpython accepts this, and a
                    // reason that is false is worse than no reason
                    "two versions of one function",
                ],
            ),
            (
                Unreplaceable::Running {
                    function: "counter".to_string(),
                    frame: LiveFrame::Suspended {
                        kind: Suspendable::Generator,
                        line: 9,
                        started: false,
                    },
                },
                vec!["a generator", "never advanced", "line 9"],
            ),
            (
                Unreplaceable::Ambiguous {
                    function: "build".to_string(),
                    nested: "build.<locals>.<lambda>".to_string(),
                },
                vec!["build", "<lambda>", "coin toss"],
            ),
            (
                Unreplaceable::ClosureChanged {
                    function: "inner".to_string(),
                    cells: 1,
                    wanted: 2,
                },
                vec!["inner", "1 closure cells", "2 free variables"],
            ),
            (
                Unreplaceable::PartiallyLoaded {
                    file: PathBuf::from("/tmp/half.py"),
                },
                vec!["/tmp/half.py", "module-level code object"],
            ),
            (
                Unreplaceable::NotLoaded {
                    file: PathBuf::from("/tmp/never.py"),
                },
                vec!["/tmp/never.py", "compiled nothing"],
            ),
            (
                Unreplaceable::NotAFile {
                    file: PathBuf::from("/tmp/gone.py"),
                    reason: "No such file or directory".to_string(),
                },
                vec!["/tmp/gone.py", "No such file or directory"],
            ),
            (
                Unreplaceable::DoesNotCompile {
                    file: PathBuf::from("/tmp/broken.py"),
                    error: PythonError {
                        kind: "SyntaxError".to_string(),
                        message: "invalid syntax".to_string(),
                        traceback: Vec::new(),
                    },
                },
                vec!["SyntaxError", "runs none of the program"],
            ),
        ];

        for (refusal, expected) in cases {
            let said = refusal.to_string();
            for wanted in expected {
                assert!(said.contains(wanted), "expected {wanted:?} in {said:?}");
            }
        }
    }

    #[test]
    fn no_refusal_is_justified_as_stopping_a_crash() {
        // measured on 3.13, 3.14, 3.15 and 3.14t: assigning `__code__` under a
        // live frame is accepted and nothing crashes. the neighbouring `f_lineno`
        // assignment really does abort the interpreter, and that finding must
        // never leak into a message here — a reason that is false is worse than
        // no reason at all
        let said = Unreplaceable::Running {
            function: "worker".to_string(),
            frame: LiveFrame::Thread {
                thread: 1,
                line: 2,
                held: None,
            },
        }
        .to_string();
        for forbidden in ["crash", "abort", "fatal", "segfault", "corrupt"] {
            assert!(
                !said.contains(forbidden),
                "the refusal claims {forbidden:?}, which cpython does not do: {said}"
            );
        }
        assert!(
            said.contains("would be accepted by cpython"),
            "the refusal has to say that cpython permits this, or a reader will \
             assume it does not, and said {said}"
        );
    }
}
