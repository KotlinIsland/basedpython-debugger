//! a request the session will not answer, and why
//!
//! separate from a failure of `bpd`'s own machinery, and separate from an
//! expression that raised — that is an answer. this is the session refusing to
//! guess what was meant

use crate::frame::{FrameId, Scope};

/// a request the agent will not answer, and why
///
/// separate from an expression that raised: that is an answer. this is the
/// agent refusing to guess what was meant
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "refused", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Refusal {
    /// the frame id was minted at a stop that is no longer held
    StaleFrame {
        /// what was asked about
        frame: FrameId,
        /// the stops that are held now
        held: Vec<u64>,
    },

    /// the stopped thread's stack is not that deep
    NoSuchFrame {
        /// what was asked about
        frame: FrameId,
        /// how many frames there are
        depth: usize,
    },

    /// that scope of that frame holds no such name
    NoSuchVariable {
        /// which frame
        frame: FrameId,
        /// which scope it was asked for in
        scope: Scope,
        /// the name
        name: String,
        /// the scopes of that frame that do hold it
        elsewhere: Vec<Scope>,
    },

    /// the name is in that scope and the frame does not expose it
    ///
    /// the read says the same thing, in `unreadable`. a write is refused
    /// outright: putting it in the frame's namespace mapping would leave a
    /// value the compiled code never reads and report a change the program did
    /// not receive
    UnreadableVariable {
        /// which frame
        frame: FrameId,
        /// which scope it is in
        scope: Scope,
        /// the name
        name: String,
    },

    /// the request is about a python frame and that frame is a template frame
    ///
    /// a django template frame is synthesised: the interpreter has no frame for
    /// it, so it has no python scopes to read or write and no source bpd can
    /// prove is the code that is running. answering from the
    /// `Node.render_annotated` frame underneath it instead would be reading a
    /// variable from another scope entirely
    NotAPythonFrame {
        /// what was asked about
        frame: FrameId,
        /// what was asked for
        wanted: String,
        /// the python frame underneath it, which does answer
        python: FrameId,
    },

    /// the request is about a template frame and that frame is a python frame
    NotATemplateFrame {
        /// what was asked about
        frame: FrameId,
        /// what it is running instead
        function: String,
    },

    /// no thread is held under that stop number
    ///
    /// several threads can be held at once, so a request that names a stop
    /// names one of them. a stop that has been resumed is gone, and answering
    /// from whichever stop happened to be nearest would be answering a
    /// different question
    NoSuchStop {
        /// the stop that was asked about
        stop: u64,
        /// the stops that are held now
        held: Vec<u64>,
    },

    /// that thread is not one this agent is holding
    ///
    /// resuming a thread that is running is not a no-op to report quietly: the
    /// client believes it is holding something it is not, and the next thing it
    /// waits for will never come
    ThreadNotHeld {
        /// the thread that was named
        thread: u64,
        /// the threads that are held now
        held: Vec<u64>,
    },

    /// the request needs a held thread and there is none
    ///
    /// the agent runs the interpreter's own api to answer this, and it can only
    /// do that on a thread it is holding. asking a program with nothing held
    /// would be a request answered whenever it next happened to stop
    NothingHeld {
        /// what was asked for
        wanted: String,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleFrame { frame, held } => write!(
                formatter,
                "{frame} belongs to a stop that has ended — the stops held now \
                 are {held:?}. a frame id is valid for one stop, because the \
                 frame it named has run on since. ask for the stack again"
            ),
            Self::NoSuchFrame { frame, depth } => write!(
                formatter,
                "{frame} does not exist: the stopped thread's stack is {depth} \
                 frames deep"
            ),
            Self::NoSuchVariable {
                frame,
                scope,
                name,
                elsewhere,
            } => {
                write!(formatter, "`{name}` is not in the {scope} scope of {frame}")?;
                if elsewhere.is_empty() {
                    formatter.write_str(
                        ". it is not in any scope of that frame. writing it \
                         would be accepted by `f_locals` and the program would \
                         never see it, because compiled code reads the fast \
                         locals the compiler gave it and nothing else",
                    )
                } else {
                    formatter.write_str(". it is in the ")?;
                    for (index, scope) in elsewhere.iter().enumerate() {
                        if index > 0 {
                            formatter.write_str(" and ")?;
                        }
                        write!(formatter, "{scope}")?;
                    }
                    formatter.write_str(" scope of it — ask for it there")
                }
            }
            Self::UnreadableVariable { frame, scope, name } => write!(
                formatter,
                "`{name}` is in the {scope} scope of {frame} and that frame does \
                 not expose it: the value lives in a cell only the function \
                 object holds, which is how a class body sees a variable of the \
                 function around it. writing it into the frame's namespace would \
                 leave a value the compiled code never reads"
            ),
            Self::NotAPythonFrame {
                frame,
                wanted,
                python,
            } => write!(
                formatter,
                "{frame} is a django template frame, and {wanted} is about a \
                 python frame. bpd synthesised it from the template node django \
                 is rendering — the interpreter has no frame for it, so it has \
                 no python scopes. its variables are the template context: ask \
                 for that. for the python underneath it, ask about {python}"
            ),
            Self::NotATemplateFrame { frame, function } => write!(
                formatter,
                "{frame} is a python frame running `{function}`, not a django \
                 template frame, so it has no template context. a template frame \
                 appears in the stack above the `Node.render_annotated` frame \
                 that renders it — ask for the stack and pick one"
            ),
            Self::NoSuchStop { stop, held } => write!(
                formatter,
                "stop {stop} is not held — the stops held now are {held:?}. a \
                 stop ends when its thread is resumed, and the thread has run \
                 on since"
            ),
            Self::ThreadNotHeld { thread, held } => write!(
                formatter,
                "thread {thread} is not held — the threads held now are \
                 {held:?}. a stop holds one thread and leaves the rest running, \
                 so a thread bpd never stopped is one it cannot resume"
            ),
            Self::NothingHeld { wanted } => write!(
                formatter,
                "no thread is held, so there is nothing to answer {wanted} on. \
                 the agent runs the interpreter's own api on a thread it is \
                 holding and at no other time — hold one first, by letting the \
                 program run to a breakpoint or by pausing it"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_names_the_frame_and_what_to_do_instead() {
        let frame = FrameId { stop: 1, depth: 2 };
        let cases = [
            (
                Refusal::StaleFrame {
                    frame,
                    held: vec![4],
                },
                vec!["frame 2 of stop 1", "[4]", "ask for the stack again"],
            ),
            (
                Refusal::NoSuchFrame { frame, depth: 2 },
                vec!["frame 2 of stop 1", "2 frames deep"],
            ),
            (
                Refusal::NoSuchVariable {
                    frame,
                    scope: Scope::Local,
                    name: "total".to_string(),
                    elsewhere: vec![Scope::Free, Scope::Global],
                },
                vec!["`total`", "local scope", "free and global"],
            ),
            (
                Refusal::NoSuchVariable {
                    frame,
                    scope: Scope::Local,
                    name: "typo".to_string(),
                    elsewhere: Vec::new(),
                },
                vec!["`typo`", "the program would never see it"],
            ),
            (
                Refusal::UnreadableVariable {
                    frame,
                    scope: Scope::Free,
                    name: "captured".to_string(),
                },
                vec!["`captured`", "free scope", "class body"],
            ),
            (
                Refusal::NotAPythonFrame {
                    frame,
                    wanted: "the variables of a scope".to_string(),
                    python: FrameId { stop: 1, depth: 3 },
                },
                vec![
                    "frame 2 of stop 1",
                    "django template frame",
                    "the template context",
                    "frame 3 of stop 1",
                ],
            ),
            (
                Refusal::NotATemplateFrame {
                    frame,
                    function: "render".to_string(),
                },
                vec!["frame 2 of stop 1", "`render`", "no template context"],
            ),
            (
                Refusal::NoSuchStop {
                    stop: 2,
                    held: vec![5, 6],
                },
                vec!["stop 2 is not held", "[5, 6]"],
            ),
            (
                Refusal::ThreadNotHeld {
                    thread: 11,
                    held: vec![12],
                },
                vec!["thread 11 is not held", "[12]"],
            ),
            (
                Refusal::NothingHeld {
                    wanted: "the breakpoints to resolve".to_string(),
                },
                vec![
                    "no thread is held",
                    "the breakpoints to resolve",
                    // a cause without an action leaves an agent to work out
                    // that it has to hold something first
                    "pausing it",
                ],
            ),
        ];

        for (refusal, expected) in cases {
            let said = refusal.to_string();
            for wanted in expected {
                assert!(said.contains(wanted), "expected {wanted:?} in {said:?}");
            }
        }
    }
}
