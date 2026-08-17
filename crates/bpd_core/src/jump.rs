//! moving a frame's instruction pointer, and what that does to the frame
//!
//! two operations share this vocabulary. **set next statement** moves the frame
//! that is executing to another line of the code it is running; **restart
//! frame** moves it to the first line of its code object, so the frame runs
//! again from the top. they are the same act on the interpreter — an assignment
//! to `frame.f_lineno` — and they differ in where the line comes from
//!
//! ## where the program is afterwards is derived, never waited for
//!
//! **no `LINE` event is delivered for the line a jump moves to.** measured on
//! 3.13, 3.14 and 3.15: jumping from the third statement of a three-statement
//! body back to the first runs `A, B, A, B, C` while the events are
//! `A, B, C, B, C`. the destination really is where the frame is and it really
//! does run — the event for it is simply not sent
//!
//! so [`Jumped::at`] is read **off the frame** after the assignment, and a
//! debugger that waited to be told would report the line after the one it moved
//! to. the same fact is why [`Jump::Moved::unannounced`] exists: a breakpoint on
//! the destination line does not fire for the pass the jump lands in, and a
//! client that was not told would watch a program run past a breakpoint it can
//! see is set
//!
//! ## what a jump does to the frame besides moving it
//!
//! cpython binds **every unbound local of the frame to `None`** as part of the
//! jump, and warns that it did — `RuntimeWarning: assigning None to 2 unbound
//! locals`. that is a change to the program's own state, made because the
//! debugger was asked to move, and [`Jump::Moved::bound_to_none`] is what says
//! which names it happened to. they are read back out of the frame afterwards
//! rather than predicted
//!
//! ## what a jump does not do
//!
//! it does not run the cleanup of any block it leaves. measured on 3.13, 3.14
//! and 3.15: jumping out of a `with` body does not call `__exit__`, and jumping
//! out of a `try` does not run its `finally`. cpython accepts both — this is not
//! a refusal it makes — so what is on the other side of a jump is a program with
//! a context manager still open. `bpd` does not pretend otherwise and does not
//! undo it: the frames a jump skips were not executed, and the effects they
//! would have had did not happen

use crate::exception::PythonError;
use crate::stop::Mode;
use crate::thread::Where;

/// what a jump did, and where the frame is now
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Jumped {
    /// where the frame is **now**, read off the frame after the attempt
    ///
    /// after a refusal this is where it still is, read the same way rather than
    /// assumed from the fact that the jump did not happen
    pub at: Where,
    /// what became of it
    pub outcome: Jump,
    /// how the program was moving while this was done
    pub mode: Mode,
}

/// what became of a jump
///
/// deliberately closed, and deliberately not a `bool`: a jump that cpython
/// refused carries cpython's own reason, and a caller that was handed `false`
/// would have to invent one
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "jumped", rename_all = "snake_case")]
pub enum Jump {
    /// the frame moved
    Moved {
        /// the line it was on before
        from: u32,
        /// locals that held nothing before the jump and hold `None` after it
        ///
        /// cpython's doing rather than `bpd`'s: assigning to `f_lineno` binds
        /// every unbound local of the frame to `None` and warns that it did.
        /// read back out of the frame after the jump, so this is what the frame
        /// really holds rather than what it was expected to
        bound_to_none: Vec<String>,
        /// breakpoints on the destination line that will **not** fire for this
        /// pass
        ///
        /// no `LINE` event is delivered for the line a jump moves to, so a
        /// breakpoint bound there is not offered the destination's own
        /// execution of it. it is still set, and it fires the next time the line
        /// runs
        unannounced: Vec<u32>,
    },

    /// cpython refused it, and the frame did not move
    Refused {
        /// the line that was asked for
        wanted: u32,
        /// cpython's own refusal, with its reason intact
        ///
        /// `can't jump into the body of a for loop`,
        /// `can only jump from a 'line' trace event`, `line 3 comes before the
        /// current code block`. every one of them names something the caller can
        /// act on, and rewriting them into a message of `bpd`'s would lose that
        error: PythonError,
    },
}

/// why a frame cannot be re-entered from the top
///
/// only about **restart frame**. set next statement moves to a line the caller
/// named, and whether that line is reachable is cpython's answer rather than
/// this one
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "unrestartable", rename_all = "snake_case")]
pub enum Unrestartable {
    /// the frame is one its driver sends into rather than one that is called
    ///
    /// a generator, a coroutine or an async generator. the first instruction of
    /// such a code object is the `RESUME` that `send`, `throw` and `await` enter
    /// at, not the top of the body — so moving there is not "run it again".
    /// measured on 3.13, 3.14 and 3.15: a generator restarted this way is
    /// **over**, and the very next `next()` raises `StopIteration` without it
    /// having yielded anything
    Suspendable {
        /// which of the three it is, in the words `co_flags` distinguishes
        kind: Suspendable,
    },

    /// no instruction of the code object carries a line of the source
    ///
    /// the destination of a restart is the line of the code object's first
    /// instruction that has one. a code object whose whole line table is
    /// synthetic has no such line, and there is nothing to move to — reporting
    /// `co_firstlineno` instead would be reporting a line the frame cannot be
    /// positioned at
    NoFirstLine,
}

/// what kind of frame its driver sends into
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Suspendable {
    /// `CO_GENERATOR`
    Generator,
    /// `CO_COROUTINE`
    Coroutine,
    /// `CO_ASYNC_GENERATOR`
    AsyncGenerator,
}

impl std::fmt::Display for Suspendable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Generator => "a generator",
            Self::Coroutine => "a coroutine",
            Self::AsyncGenerator => "an async generator",
        })
    }
}

impl std::fmt::Display for Unrestartable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Suspendable { kind } => write!(
                formatter,
                "it is {kind}, and the first instruction of such a code object is \
                 the `RESUME` its driver sends into rather than the top of the \
                 body. measured on 3.13, 3.14 and 3.15, moving there ends the \
                 frame instead of running it again — the next `next()` raises \
                 `StopIteration` and nothing was yielded. set the next statement \
                 to a line of the body instead, which is the same jump without \
                 that instruction"
            ),
            Self::NoFirstLine => formatter.write_str(
                "no instruction of its code object carries a line of the source, \
                 so there is no line to move to. `co_firstlineno` is where the \
                 code was written rather than a position the frame can be put at, \
                 and moving to it would be moving somewhere nothing said was \
                 there",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_that_cannot_be_restarted_says_what_to_do_instead() {
        let suspendable = Unrestartable::Suspendable {
            kind: Suspendable::Generator,
        }
        .to_string();
        assert!(suspendable.contains("a generator"), "{suspendable}");
        assert!(
            suspendable.contains("set the next statement"),
            "a refusal has to name the operation that does work, and said \
             {suspendable}"
        );

        let unlined = Unrestartable::NoFirstLine.to_string();
        assert!(unlined.contains("co_firstlineno"), "{unlined}");
    }

    #[test]
    fn a_refused_jump_carries_cpythons_own_words() {
        // the whole reason the outcome is an enum rather than a bool. cpython
        // supplies a reason a caller can act on, and it is not paraphrased
        let refused = Jump::Refused {
            wanted: 12,
            error: PythonError {
                kind: "ValueError".to_string(),
                message: "can't jump into the body of a for loop".to_string(),
                traceback: Vec::new(),
            },
        };
        let Jump::Refused { error, .. } = &refused else {
            panic!("it was built as a refusal")
        };
        assert_eq!(
            error.to_string(),
            "ValueError: can't jump into the body of a for loop"
        );
    }

    /// the json a client reads a refusal out of, pinned
    ///
    /// `Jumped` goes out whole on `bpd/moved`, so this shape *is* the wire. it is
    /// pinned because getting it wrong on the reading side is silent: an `error`
    /// read as a string rather than as the object it is yields nothing, which
    /// turns a refusal into a move that disturbed nothing and reports it as
    /// nothing at all — and a client that asked bpd to stop narrating has no
    /// other channel for it. the intellij plugin did exactly that
    #[test]
    fn a_refusal_serialises_as_a_tagged_outcome_and_an_error_object() {
        let jumped = Jumped {
            at: Where {
                file: "/tmp/p/bain.by".to_string(),
                line: 2,
                function: "work".to_string(),
            },
            outcome: Jump::Refused {
                wanted: 4,
                error: bpd_core_error(),
            },
            mode: Mode::NonStop,
        };

        let json = serde_json::to_value(&jumped).expect("Jumped serialises");
        let outcome = &json["outcome"];

        // the tag is what says which case this is. a reader that infers it from
        // whether some other key parsed has no way to tell a refusal it cannot
        // read from a move
        assert_eq!(outcome["jumped"], "refused");
        assert_eq!(outcome["wanted"], 4);
        // an object, never a string
        assert!(outcome["error"].is_object(), "was {}", outcome["error"]);
        assert_eq!(outcome["error"]["kind"], "ValueError");
        assert_eq!(
            outcome["error"]["message"],
            "can't jump into the body of a for loop"
        );
    }

    /// and the other case, so the two are pinned against each other
    #[test]
    fn a_move_serialises_with_the_lists_a_client_acts_on() {
        let jumped = Jumped {
            at: Where {
                file: "/tmp/p/bain.by".to_string(),
                line: 1,
                function: "work".to_string(),
            },
            outcome: Jump::Moved {
                from: 3,
                bound_to_none: vec!["later".to_string()],
                unannounced: vec![7],
            },
            mode: Mode::NonStop,
        };

        let json = serde_json::to_value(&jumped).expect("Jumped serialises");
        assert_eq!(json["outcome"]["jumped"], "moved");
        assert_eq!(json["outcome"]["bound_to_none"][0], "later");
        assert_eq!(json["outcome"]["unannounced"][0], 7);
    }

    fn bpd_core_error() -> crate::PythonError {
        crate::PythonError {
            kind: "ValueError".to_string(),
            message: "can't jump into the body of a for loop".to_string(),
            traceback: Vec::new(),
        }
    }
}
