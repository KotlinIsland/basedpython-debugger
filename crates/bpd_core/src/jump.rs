//! moving a frame's instruction pointer, and what that does to the frame
//!
//! two operations, and they are **not** the same act. **set next statement**
//! moves the frame that is executing to another line of the code it is running:
//! one assignment to `frame.f_lineno`, and nothing is resumed. **restart frame**
//! runs a frame again, by forcing it to *return* and rewinding its **caller** to
//! the call — two of those assignments, with the program running in between, so
//! that the frame which comes back is one the interpreter built rather than the
//! old one put back at its top. see [`Restarting`]
//!
//! everything below is about the jump, and is true of both: a restart is made of
//! two of them
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

/// what restarting a frame arranged, and what is going to run again
///
/// **restart frame does not move the frame it names.** it forces that frame to
/// **return** — by moving it to a line of its own code that is only loads and a
/// return — and then rewinds the **caller** to the line the call was made from,
/// so that the caller makes the call again and the interpreter builds a frame
/// that was never run before. that is what makes the locals genuinely fresh
///
/// **it does not run the cleanup of a block it leaves.** forcing the frame out
/// is an assignment to `f_lineno` like any other, and the module note above
/// applies to it unchanged: measured on 3.13 and 3.14 with a plain class context
/// manager, a restart from inside a `with` gives two `__enter__` and **one**
/// `__exit__`, and one from inside a `try/finally` runs the body twice and the
/// `finally` once. an earlier version of this claimed the opposite, on a
/// `@contextlib.contextmanager` fixture whose `finally` runs when the generator
/// is collected rather than when the block is left
///
/// the unit is the caller's **line**, not the call. everything on that line
/// runs again, which is why a line carrying anything observable besides the one
/// call is refused rather than restarted — see [`Unrestartable`]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Restarting {
    /// the frame that was forced out, and where it was when it was
    pub frame: Where,

    /// the line of its own code object it was moved to, so that it returns
    ///
    /// a line whose whole instruction range is loads and a return, so moving
    /// there executes nothing of the program. cpython fuses a function's
    /// implicit `return None` onto the last statement's line, which is why such
    /// a line often does not exist and why its absence is a refusal
    pub exit_line: u32,

    /// the caller, at the line it is going to run again
    ///
    /// its `line` is the call line: the whole of it re-executes, and the frame
    /// the restart produces is the one that line's call builds
    pub caller: Where,

    /// the names the forced return's value is written into before the rewind
    ///
    /// **not always the caller's own locals.** `STORE_GLOBAL` is allowed beside
    /// the call, so `global stashed; stashed = f(1)` puts one of these in the
    /// **module's** globals and the caller's locals are empty. an earlier
    /// version of this said "the names the caller's line binds", and a client resolving
    /// that against a Locals scope found nothing and concluded the value was
    /// untouched
    ///
    /// the forced return is a **real** return with a real value, so the rest of
    /// the call line runs with it: `got = f(x)` stores it in `got`. that is a
    /// value the program never computed, sitting in a live frame, and it is
    /// here because nothing else in a session would ever mention it
    ///
    /// it is overwritten when the restarted call returns for real. until then
    /// it is what the caller holds, and a client reading the caller's locals
    /// during the restarted run is reading this rather than the program's
    pub disturbed: Vec<String>,

    /// locals of the forced-out frame that held nothing before the move and
    /// hold `None` after it
    ///
    /// cpython's doing rather than `bpd`'s, exactly as for any other jump. read
    /// back out of the frame rather than predicted. it is reported even though
    /// the frame is about to die, because the frame is **live** at the moment
    /// the answer is made and a client reading its locals then reads these
    pub bound_to_none: Vec<String>,

    /// breakpoints this restart passes over, on either of the two lines it
    /// moves to
    ///
    /// no `LINE` event is delivered for the line a jump moves to, and a restart
    /// makes **two** jumps. so it is both the [`Restarting::exit_line`], whose
    /// loads and return really do execute, and the caller's call line the rewind
    /// lands on. a breakpoint on either was passed over by a line the program
    /// ran, and it is still set and fires the next time that line runs
    ///
    /// one list rather than two: they are breakpoint ids, and what a client does
    /// with them is look them up
    pub unannounced: Vec<u32>,

    /// how the program was moving while this was arranged
    pub mode: Mode,
}

impl Restarting {
    /// what a restart did that neither protocol has a field for
    ///
    /// **here** rather than in each adapter, because two of these are claims
    /// about the program's state and a front end that worded one differently
    /// would be a front end somebody trusts differently. it was written three
    /// times — twice in the DAP adapter and once in MCP's renderer — and the
    /// sentence about cleanup existed on only one of the three paths
    ///
    /// each front end still adds its own: DAP says nothing about waiting for a
    /// stop because its `stopped` event is the wait, and MCP has to say it
    /// because an agent would otherwise ask this stop another question
    #[must_use]
    pub fn told(&self) -> Vec<String> {
        let mut told = vec![
            format!(
                "`{}` was forced to return through line {}, and the thread has \
                 been let go. `{}` will run line {} again — the **whole** line, \
                 which is why anything else on it is refused rather than \
                 restarted",
                self.frame.function, self.exit_line, self.caller.function, self.caller.line,
            ),
            "the forced return runs **no block cleanup**: moving the frame out \
             is an `f_lineno` jump, so a `with` the frame was inside gets no \
             `__exit__` and a `try` gets no `finally`"
                .to_string(),
            "**but the frame dies, and what dies with it is finalised.** \
             anything the forced-out frame was the last holder of is released at \
             that moment — a moment the program never reached — and finalising \
             can run arbitrary code of the program: a `__del__`, or the \
             `GeneratorExit` thrown into a suspended generator, which runs its \
             `finally` and the `__exit__` of any `with` inside it. bpd does not \
             enumerate what that will be, and nothing can refuse it: it is what \
             forcing any frame out does"
                .to_string(),
        ];
        if !self.disturbed.is_empty() {
            told.push(format!(
                "{:?} hold the forced return's value until the restarted call \
                 finishes — a value the program never computed. they are names \
                 `{}` binds in its **own** locals: its slots if it is a \
                 function, its namespace if it is a module, class or `exec` \
                 body. a line that writes a global or a cell after the call is \
                 refused rather than restarted, so none of these is one",
                self.disturbed, self.caller.function,
            ));
        }
        if !self.unannounced.is_empty() {
            told.push(format!(
                "breakpoint(s) {:?} are on a line this restart moves to and will \
                 not fire for the pass it lands in — no line event is delivered \
                 for the line a jump moves to. they are still set",
                self.unannounced,
            ));
        }
        if !self.bound_to_none.is_empty() {
            told.push(format!(
                "{:?} held nothing before the frame was forced out and hold \
                 `None` now — cpython binds every unbound local when a frame \
                 moves",
                self.bound_to_none,
            ));
        }
        told
    }
}

/// what became of a restart
///
/// deliberately not a `bool` and deliberately not [`Jumped`]: the two outcomes
/// leave the program in **different** states. an arranged restart has already
/// forced a frame out and the thread has been let go to finish it; a refused
/// one moved nothing and the thread is still held exactly where it was
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "restarted", rename_all = "snake_case")]
pub enum Restarted {
    /// the frame was forced out and the caller will make the call again
    ///
    /// the thread has been **let go** to finish it, and where it gets to
    /// arrives as a stop of its own: [`crate::StopReason::Restarted`] when the
    /// fresh frame is entered, and [`crate::StopReason::RestartAbandoned`] when
    /// it could not be finished after the frame had already gone — carrying
    /// which of [`crate::Abandoned`]'s reasons it was, a list that is
    /// `non_exhaustive` and is not enumerated here for that reason
    ///
    /// **those two are not exhaustive, and that is a known gap.** anything else
    /// that holds this thread first — a breakpoint, an exception, a pause, a
    /// stopped world — takes the restart off, and then neither arrives. closing
    /// it needs a report the debugger makes without being asked, which is a
    /// [`crate::Told`] and a `carriage_of` arm in both front ends; until then
    /// the sentence says so rather than reading as a disjunction it is not
    Arranged(Restarting),

    /// cpython refused to move the frame to any of its exit lines
    ///
    /// no code of the program ran: a refused assignment to `f_lineno` moves
    /// nothing and binds nothing — measured on 3.13, 3.14 and 3.14t — so the
    /// thread is still held where it was
    ///
    /// **not quite "nothing happened",** which is what this said. deciding a
    /// refusal reads the frame's bytecode, and reading it imports `dis` into the
    /// debuggee — with `opcode` and `_opcode` behind it. no frame moved and no
    /// name of the program's changed, but `sys.modules` did, and a debuggee can
    /// see that. it is disclosed in [`WHAT_READING_THE_BYTECODE_COSTS`] rather
    /// than glossed, because the alternative — importing `dis` at agent startup
    /// so a refusal really does change nothing — charges every session for a
    /// feature most of them never use
    Refused {
        /// every exit line that was offered to cpython, in the order they were
        /// tried
        ///
        /// more than one because a function can have several returns and
        /// cpython accepts a move to some of them and not others — a `return`
        /// inside a `for` body answers `can't jump into the body of a for loop`
        /// while a `return` at the top level of the same function is fine
        tried: Vec<u32>,
        /// cpython's own refusal of the last one, with its reason intact
        error: PythonError,
    },
}

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

/// what deciding a restart costs the debuggee even when it is refused
///
/// one sentence, in one place, because it belongs on both front ends and a
/// second copy is how the two drift. it is a **disclosure**, not a warning: the
/// import is how the bytecode gets read at all, and reading it is what lets a
/// refusal be decided before anything moves
///
/// **deliberately not a list.** an earlier version named `dis`, `opcode` and
/// `_opcode`, and the real delta is 13 modules on 3.13 and 15 on 3.14 — a
/// closed list in a sentence whose whole point is "a program can tell" is the
/// same defect as no sentence at all. the floor it does claim is pinned by
/// `the_disclosure_does_not_undercount_what_reading_the_bytecode_imports`
pub const WHAT_READING_THE_BYTECODE_COSTS: &str = "reading the frame's bytecode imports `dis` into the debuggee, and \
     everything `dis` imports with it — more than a dozen modules, and which \
     ones depends on the interpreter. no frame moved and no name of the \
     program's changed, but `sys.modules` did, and a program that inspects it \
     can tell";

/// why a frame cannot be restarted
///
/// only about **restart frame**. set next statement moves to a line the caller
/// named, and whether that line is reachable is cpython's answer rather than
/// this one
///
/// every one of these is decided **before anything is attempted**, off the
/// bytecode of the frame and of its caller. that is the whole shape of this
/// feature: a restart either runs none of the program's code because it was
/// refused, or it is one bpd has read the instructions for and knows what
/// re-running them does. there is no case where it finds out halfway
///
/// "cannot disturb the program" is what this said, and reading the bytecode
/// imports `dis` into the debuggee — see [`WHAT_READING_THE_BYTECODE_COSTS`]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "unrestartable", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Unrestartable {
    /// the frame is one its driver sends into rather than one that is called
    ///
    /// a generator, a coroutine or an async generator. what rules all three out
    /// is that `f_back` is **whoever resumed the frame**, which need not be
    /// what produced it — and the whole mechanism is "make the caller's line
    /// build the frame again"
    ///
    /// measured on 3.13, 3.14 and 3.14t, and both halves are worse than a
    /// missed opportunity:
    ///
    /// - a **generator** forced out returns, so `next(it)` raises
    ///   `StopIteration` — which propagates out of the caller instead of
    ///   reaching a line event, and the program dies. the caller's line
    ///   `second = next(it)` passes every other test this enum makes, so
    ///   nothing else would have caught it
    /// - a **coroutine** driven by a task has `f_back` inside the event loop —
    ///   `asyncio.events.Handle._run` — and rewinding *that* line answered
    ///   `InvalidStateError: __step(): already done`. the loop, not the
    ///   program's frame, is what got rewound
    ///
    /// a coroutine awaited in place *does* restart correctly, and it is refused
    /// with the rest because nothing in the frame distinguishes it from the
    /// task case: both are a coroutine frame whose `f_back` is a python frame
    Suspendable {
        /// which of the three it is, in the words `co_flags` distinguishes
        kind: Suspendable,
    },

    /// no line of its code object is only loads and a return
    ///
    /// a frame is forced out by moving it to a line that **returns and does
    /// nothing else**. cpython fuses a function's implicit `return None` onto
    /// the last statement's line, so a function with no explicit `return` very
    /// often has no such line — and moving there would *execute that
    /// statement*, which is a side effect nobody asked for
    NoCleanExit,

    /// nothing bpd would report called it
    ///
    /// the mechanism rewinds the **caller** — that is where the fresh frame
    /// comes from. the outermost frame of the program has no caller but bpd's
    /// own bootstrap, and there is nothing above it to run the call again
    NoCaller,

    /// something else on the caller's line would run again with it
    ///
    /// the rewind re-executes the caller's whole line, so **everything** on it
    /// runs a second time. what is permitted there is an allow list of
    /// instructions that provably run no code of the program — loads, stack
    /// shuffles, and a store into the names the caller's line binds — and anything else is
    /// refused by name
    ///
    /// an allow list rather than a list of the dangerous ones, deliberately: an
    /// opcode a future interpreter adds is refused rather than silently
    /// permitted. measured shapes, all of them ordinary lines:
    ///
    /// | the line | what is on it | why |
    /// | --- | --- | --- |
    /// | `x = f(obj.attr)` | `LOAD_ATTR` | a property runs user code |
    /// | `x = f(d[k])` | `BINARY_OP` | `__getitem__` runs user code |
    /// | `x = f(k + 1)` | `BINARY_OP` | `__add__` runs user code |
    /// | `if f(k):` | `TO_BOOL` | `__bool__` runs user code |
    /// | `obj.slot = f(k)` | `STORE_ATTR` | a setter runs, with the forced return's value |
    SomethingElseOnTheLine {
        /// the caller's line the call is on
        line: u32,
        /// the instruction that is not on the allow list, by `opname`
        opcode: String,
    },

    /// the caller's line makes more than one call, and all of them would re-run
    ///
    /// `[f(), f()]`, `sorted(items, key=f)`, a conditional expression with a
    /// call in each branch. the unit of a rewind is the line, so a line with two
    /// calls on it re-runs a call that had **already completed** — measured: the
    /// caller's own list gains an entry, and a sort re-runs its key function for
    /// every element it had already compared
    ///
    /// counted over **every** range the line has rather than the one the caller
    /// is stopped in. `co_lines` yields a range per contiguous run, and a
    /// conditional expression has one per branch — each of which looks like an
    /// ordinary single call on its own
    ///
    /// a comprehension is **not** one of these: PEP 709 inlines it into the
    /// enclosing frame, so its call really is the only `CALL` on the line, and
    /// what refuses it is the `GET_ITER` and `LOAD_FAST_AND_CLEAR` the construct
    /// puts there — see [`Unrestartable::SomethingElseOnTheLine`]
    MoreThanOneCall {
        /// the caller's line the call is on
        line: u32,
        /// how many calls that line makes
        calls: u32,
    },

    /// the call is the last thing the caller does, so no line event follows it
    ///
    /// the rewind can only be driven from a `LINE` event: cpython answers
    /// `can only jump from a 'line' trace event` to anything else, so
    /// `PY_RETURN` cannot drive it. when a `RETURN` lies **after the call** in
    /// the span, execution never enters another line of the caller and there is
    /// nowhere the rewind could be made from
    ///
    /// after the call, specifically. one **before** it means the span crosses a
    /// copy boundary, which is [`Unrestartable::CopiedLine`] and a different
    /// fact — reporting that as this one told a caller that already binds its
    /// result to a name to bind its result to a name
    ///
    /// the same restriction is what makes the dangerous case unreachable rather
    /// than merely unlikely: with the caller suspended in a C call — the
    /// `sorted(items, key=f)` shape — no line event fires in it either, so a
    /// rewind cannot happen in the middle of one
    NothingRunsAfterTheCall {
        /// the caller's line the call is on
        line: u32,
    },

    /// the call reads back a name the same line stores into
    ///
    /// `x = f(x)`. the forced return really returns, so the rest of the line
    /// runs and `x` holds a value the program never computed **before** the
    /// rewind — and the re-executed line then calls `f` with bpd's invention
    /// rather than with the program's value. the restarted call would not be the
    /// call that was restarted
    TheCallReadsWhatItStores {
        /// the caller's line the call is on
        line: u32,
        /// the name that is both read and written
        name: String,
    },

    /// the line has more than one copy of its instructions
    ///
    /// cpython **duplicates** a `finally` body: one copy for the normal path and
    /// one for the exception path. so one call inside a `finally` has two `CALL`
    /// instructions, in two `co_lines` ranges of the same line — and when the
    /// caller is stopped in the second, the span the analysis would read starts
    /// in the first and swallows both
    ///
    /// the shape is genuinely restartable. what bpd cannot do is say **which
    /// copy a rewind would land in**: cpython picks the destination by stack
    /// depth rather than by offset, and that is interpreter internals this
    /// project will not hand-maintain. so it is refused, and the reason is that
    /// rather than something about the call
    CopiedLine {
        /// the caller's line the call is on
        line: u32,
        /// how many separate runs of instructions the line's code is in
        ///
        /// `co_lines` ranges, not copies of a body — the two coincide for a
        /// duplicated `finally` and for very little else. calling this `copies`
        /// is what let the sentence assert a `finally` that, in most of the
        /// cases that reach it, is not there: a call split over source lines, a
        /// `with` header, a loop header and a decorator all put a line's code in
        /// more than one run
        runs: u32,
    },

    /// every way out of the frame reads a name that would raise
    ///
    /// a forced exit has to **return**, and two reads on the allow list raise
    /// instead when the frame does not hold what they name: `LOAD_DEREF` on a
    /// cell that holds nothing, and `LOAD_GLOBAL` on a name in neither globals
    /// nor builtins. cpython binds a frame's unbound **locals** to `None` as
    /// part of a move and leaves both of those alone
    ///
    /// this is what a function whose every `return` reads one gets, instead of
    /// [`Unrestartable::NoCleanExit`] — which would send somebody looking for a
    /// `return` they can see they already have
    ExitWouldRaise {
        /// which frame's line this is about
        whose: Whose,
        /// the line, in the frame [`Whose`] names — one that would otherwise
        /// have been an exit, or the caller's own call line
        line: u32,
        /// the name whose read raises
        name: String,
    },

    /// the caller is stopped at an instruction with no line of the source
    ///
    /// there is no line to rewind to, and picking a nearby one would be picking
    /// which statement runs again
    /// **no producer test, and the reason is that nothing can produce it.** the
    /// caller is found by locating `f_lasti` in the caller's own `co_lines`, and
    /// a frame that is executing a call is executing an instruction its own line
    /// table covers — cpython emits a line-table entry for every instruction it
    /// can stop at. so this is the branch that fires if that stops being true,
    /// and it names the offset rather than guessing a nearby line, because a
    /// guess would be guessing which statement runs again
    CallerHasNoLine {
        /// the instruction offset the caller is stopped at
        lasti: u32,
    },

    /// a namespace the span reads or writes is not a plain dictionary
    ///
    /// **both halves of this are program code, and only one of them was
    /// checked.** a `STORE_NAME` into a class body built through a `__prepare__`
    /// of its own runs that mapping's `__setitem__`. and a `LOAD_GLOBAL` whose
    /// globals is a **dict subclass** falls off cpython's exact-dict fast path
    /// and calls `PyObject_GetItem`, running `__getitem__` or `__missing__` —
    /// measured: a callee exiting through `return LATE` ran the program's
    /// `__missing__` during the forced exit, and the answer said `Arranged`
    ///
    /// so a span that reads a global or a name, or writes one, is permitted only
    /// when the mapping behind it is an **exact** dict
    NamespaceIsNotADict {
        /// which frame's line this is about
        whose: Whose,
        /// whether the instruction reads through the namespace or writes to it
        access: Access,
        /// which of the frame's mappings it is
        through: Through,
        /// the line, in the frame [`Whose`] names
        line: u32,
        /// what the namespace is, by type name
        namespace: String,
    },
    /// the tail writes a name the restarted call could read, and bpd cannot see
    /// whether it does
    ///
    /// [`Restarting::disturbed`] and the read guard both track names **on the
    /// caller's line**, and neither sees state the *callee* reads that the tail
    /// writes and the caller's line never reads back. measured: `got, G = f(),
    /// 99` with `global G` and an `f` that reads `G` answered `Arranged` and
    /// reported `Restarted`, and the restarted call read `99` where every
    /// undebugged run reads `1` — the program's own output changed and nothing
    /// said so. a cell shared with a closure does the same
    ///
    /// so `STORE_GLOBAL` and `STORE_DEREF` after the call are refused outright,
    /// and so is **any** `STORE_NAME` when the caller's locals are its globals —
    /// which is a module body, and also an `exec` given only globals or a class
    /// body whose `__prepare__` returns `globals()` — measured, and all three
    /// are the same hazard. bpd would have to read the
    /// callee's code, and everything it calls, to do better, and a debugger that
    /// guesses at that is worse than one that says it cannot know
    ///
    /// **what this costs, measured over both stdlibs.** a tail that writes a
    /// global or a cell is 143 of 7401 permitted call sites on 3.13 and 150 of
    /// 7520 on 3.14. the module-body rule is the larger one and wants its own
    /// denominator rather than that one: of the 1128 permitted sites that are
    /// **in** a module body on 3.13, 520 are refused and 608 still restart; on
    /// 3.14 it is 537 of 1156, with 619 restarting. a little under half, and a
    /// majority of module-level calls are unaffected — an earlier version of
    /// this said "very nearly every module-level restart", which was asserted
    /// rather than measured and is wrong by about a factor of two
    ///
    /// the module-body case is refused rather than excused because the
    /// alternative was to treat it and `got, G = f(), 99` on **likelihood rather
    /// than on kind**, and "unlikely" is not a justification this project takes.
    /// what survives is any module-level line that stores nothing **after** the
    /// call: a bare call statement, one whose result is discarded, a decorator,
    /// and `f(w := 3)`, whose store lands before the call
    TailWritesSharedState {
        /// the caller's line the call is on
        line: u32,
        /// the name the tail writes that the callee might read
        name: String,
    },

    /// the frame was not entered by a call, so there is no call to make again
    ///
    /// a restart rewinds the **caller** to the call it made. a frame reached
    /// another way has no such instruction: measured on 3.13 and 3.14, the frame
    /// of a property reached by `got = sink(obj.attr)` has its caller stopped at
    /// `LOAD_ATTR`, a comprehension over a custom iterator has it at `FOR_ITER`,
    /// and an `await` has it at `SEND`
    ///
    /// separate from [`Unrestartable::SomethingElseOnTheLine`] because the two
    /// were one variant and one sentence, and that sentence ended "put the call
    /// on a line of its own, with its arguments already in locals" — advice for
    /// a line that already looks like that, about a call that does not exist
    ///
    /// **`SEND` is deliberately not named here.** an `await` does stop the
    /// caller at `SEND`, but the frame it enters is a coroutine and
    /// [`Unrestartable::Suspendable`] refuses it before this analysis runs; the
    /// frame an `await` reaches that could produce this is `__await__`, entered
    /// at `GET_AWAITABLE`. and it is not true that nothing the user writes
    /// changes it: `next(it)` restarts where a `for` loop's `FOR_ITER` does not,
    /// and a property getter restarts once its `fget` is on a line of its own.
    /// **`type(obj).attr.fget(obj)` is not that line** — it is two calls, so it
    /// is refused as [`Unrestartable::MoreThanOneCall`]; an earlier version of
    /// this message offered it as the remedy and it did not work
    NotEnteredByACall {
        /// the caller's line it is stopped on
        line: u32,
        /// the opcode the caller is stopped at, which is how it entered
        opcode: String,
    },

    /// the span does something with the call's value that bpd cannot follow
    ///
    /// `disturbed` is worked out by walking the rest of the line and tracking
    /// which stores consume the value the call left on the stack. the walk knows
    /// every opcode the allow list permits, and it models the stack from the
    /// call's own value down — what the caller pushed **before** the call is not
    /// modelled, and does not need to be, because none of it came from the call
    ///
    /// so what it gives up on is a `SWAP` it cannot follow: one that would put
    /// the call's value below that floor, where a later store could still reach
    /// it and this could not see, and one on an empty modelled stack, where
    /// there is no top to swap at all. swept over both stdlibs, no permitted call site
    /// has a `SWAP` after the call at all — 0 of 7401 on 3.13 and 0 of 7520 on
    /// 3.14 — so this is a door left locked rather than one anybody walks into
    ///
    /// **an earlier version blamed the allow list and the walk drifting apart,**
    /// and fired on about one permitted call site in a hundred: it seeded the
    /// stack with one slot and treated any pop past it as unmodellable, so
    /// `first, second = spare, f(1)` was refused. nothing had drifted — a value
    /// below the floor is an answer, not a gap
    ///
    /// refused rather than guessed, because the alternative is naming the wrong
    /// slots as holding a value the program never computed, which is the thing
    /// `disturbed` exists to say
    SpanNotUnderstood {
        /// the caller's line the call is on
        line: u32,
    },
}

/// which frame's line a refusal is about
///
/// two of the refusals below are produced **twice**: once about a line the
/// frame would be forced out through, and once about the line its caller makes
/// the call from. they are the same fact about bytecode and completely
/// different things to be told, and for a while they shared one message written
/// for the caller — so a user refused because of a `LOAD_GLOBAL` on the frame's
/// own `return` was told the caller's line stored into a `__prepare__` namespace
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Whose {
    /// a line the frame would be forced out through
    TheFrame,
    /// the line the caller makes the call from, which the rewind re-executes
    TheCaller,
}

/// which of a frame's mappings a refusal is about
///
/// the third axis, and it was found the way the first two were: one arm served
/// two mappings and told the user about the wrong one. a caller-side **read**
/// refused because a class body's `__prepare__` namespace is not a plain dict
/// was closed with "cpython's fast path for a global needs the globals **and**
/// the builtins to be plain dicts" — true, and about neither mapping in play,
/// because the globals and the builtins were both plain dicts and the
/// `OrderedDict` was `f_locals`
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Through {
    /// `f_globals` or `f_builtins`, which `LOAD_GLOBAL` reads as a pair
    GlobalsOrBuiltins,
    /// `f_locals`, which a module body and a class body keep their names in
    Locals,
}

/// what an instruction does through a namespace
///
/// **orthogonal to [`Whose`], and that is the point.** the first attempt at this
/// split used the producer alone, which moved the defect instead of removing it:
/// a caller-side refusal raised by a `LOAD_GLOBAL` still got wording about a
/// store running `__setitem__` under a `__prepare__` class body. where the line
/// came from and what the instruction does are two axes, and the falsity lived
/// on the second
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// `LOAD_GLOBAL` or `LOAD_NAME` — runs `__getitem__` or `__missing__`
    Reads,
    /// `STORE_NAME` — runs `__setitem__`
    Writes,
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
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per refusal, and every one of them is a whole \
                  sentence naming what stood in the way and what would work \
                  instead. splitting them out would put half of a message \
                  somewhere nobody reading the variant would find it — the same \
                  reason `Refusal`'s own `Display` carries this"
    )]
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Suspendable { kind } => write!(
                formatter,
                "it is {kind}, and a restart works by making the caller run the \
                 call again — but `f_back` of such a frame is whoever **resumed** \
                 it, which need not be what produced it. measured on 3.13, 3.14 \
                 and 3.14t: forcing a generator out makes the caller's `next()` \
                 raise `StopIteration`, which leaves the caller instead of \
                 reaching a line event, and a coroutine driven by a task has \
                 `f_back` inside `asyncio.events.Handle._run`, where the rewind \
                 answered `InvalidStateError: __step(): already done`. set the \
                 next statement to a line of the body instead, which re-executes \
                 it without needing the caller"
            ),
            Self::NoCleanExit => formatter.write_str(
                "no line of its code object is only loads and a return, so there \
                 is no way to make it return without running a statement of the \
                 program. cpython fuses a function's implicit `return None` onto \
                 the **last statement's** line, so a function with no explicit \
                 `return` usually has none — give it one, or set the next \
                 statement to a line of the body instead",
            ),
            Self::NoCaller => formatter.write_str(
                "nothing bpd would report called it, and a restart is the caller \
                 making the call again. this is the outermost frame of the \
                 program, so there is no frame above it to re-run anything — set \
                 the next statement to a line of it instead",
            ),
            Self::TailWritesSharedState { line, name } => write!(
                formatter,
                "the caller's line {line} writes `{name}` after the call, and it \
                 writes it somewhere the callee can see — a global, a cell \
                 shared with a closure, or any name at all when the caller's \
                 namespace **is** the global namespace — a module body, or an \
                 `exec` or class body made to share one. the \
                 tail runs before the rewind, so the restarted call would run \
                 with the new value; whether it reads `{name}` at all is inside \
                 the callee, and bpd does not read the callee's code to guess. \
                 write it before the call, or into a local the callee cannot \
                 reach. a call from inside a function restarts; from a module \
                 body it depends on the line, and a little over half of them do"
            ),
            Self::NotEnteredByACall { line, opcode } => write!(
                formatter,
                "the caller is stopped at `{opcode}` on line {line}, not at a \
                 call — so this frame was not entered by one, and a restart has \
                 nothing to rewind the caller **to**. a property getter reached \
                 as `obj.attr` is entered through `LOAD_ATTR`, and a \
                 comprehension's iterator through `FOR_ITER`\n\n\
                 what changes it is reaching the same code **through a call**, \
                 on a line that makes only one: `seen = next(it)` restarts where \
                 a `for` loop's `FOR_ITER` does not, and a property getter \
                 restarts if its `fget` is hoisted to a line of its own first — \
                 `type(obj).attr.fget(obj)` on one line is two calls and is \
                 refused for that instead. use set next statement inside the \
                 frame if you do not want to rewrite the line"
            ),
            // `CALL_FUNCTION_EX` is the call itself rather than something
            // sharing its line, so the generic remedy is false of it: the call
            // in `got = f(*args)` **is** on a line of its own with its argument
            // already in a local, and it is still refused
            Self::SomethingElseOnTheLine { line, opcode } if opcode == "CALL_FUNCTION_EX" => {
                write!(
                    formatter,
                    "the caller's line {line} calls with `*args` or `**kwargs`, \
                     which `CALL_FUNCTION_EX` unpacks by **iterating** what it \
                     is given — so the second call would not be the call that \
                     was restarted: a generator or a one-shot iterator is empty \
                     by then, and bpd cannot tell one of those from a list. this \
                     is the call itself rather than something sharing its line, \
                     so moving it will not help. passing the arguments \
                     positionally is what makes it restartable"
                )
            }
            Self::SomethingElseOnTheLine { line, opcode } => write!(
                formatter,
                "the caller's line {line} runs `{opcode}`, which is not something \
                 bpd can prove runs no code of the program — and a restart \
                 re-executes that **whole line**, so it would run again. an \
                 attribute is a property, a subscript is a `__getitem__`, and an \
                 operator is a dunder. put the call on a line of its own, with \
                 its arguments already in locals, and it can be restarted"
            ),
            Self::MoreThanOneCall { line, calls } => write!(
                formatter,
                "the caller's line {line} makes {calls} calls, and a restart \
                 re-executes the whole line — so it would re-run the ones that \
                 had already finished. measured: the sibling calls of `[f(), \
                 f()]` really do run again, and a comprehension re-runs every \
                 iteration because PEP 709 inlines it into the caller's own \
                 frame. put the call on a line of its own and it can be \
                 restarted"
            ),
            Self::NothingRunsAfterTheCall { line } => write!(
                formatter,
                "the call is the last thing the caller does on line {line}, so no \
                 line event follows it — and cpython answers `can only jump from \
                 a 'line' trace event` to a rewind driven from anywhere else. \
                 there is nowhere the caller could be moved from. give the caller \
                 a statement after the call — binding the result to a name is \
                 enough — and it can be restarted"
            ),
            Self::TheCallReadsWhatItStores { line, name } => write!(
                formatter,
                "the caller's line {line} reads `{name}` before the call and \
                 writes it after, and the tail of the line runs **before** the \
                 rewind. so by the time the line is re-executed `{name}` holds \
                 what the tail put there rather than what it held when the call \
                 was made, and bpd cannot say the second call would be the call \
                 that was restarted. it does not matter whose value the tail \
                 puts there: `x = f(x)` reads back what bpd forced the frame to \
                 return, and `a, x = f(x), other` reads back the program's own \
                 `other`. bpd refuses on the read-and-write rather than on where \
                 the value goes, so a line that writes `{name}` back the value it \
                 already had is refused too — that is a refusal of a restart \
                 that would have been sound, and it is the direction this errs \
                 in. write the result to a name the call does not read and it \
                 can be restarted"
            ),
            Self::CopiedLine { line, runs } => write!(
                formatter,
                "the caller's line {line} has its instructions in {runs} \
                 separate runs, and the caller is not stopped in the first. \
                 cpython picks the destination of a move by stack depth rather \
                 than by offset, so bpd cannot say which run a rewind would land \
                 in, and it will not guess. a line gets more than one run from a \
                 call split over several source lines, from a `with` or loop \
                 header, and from a `finally`, whose body cpython duplicates — \
                 bpd cannot tell which of those this is. putting the call on a \
                 line of its own is what makes it restartable"
            ),
            Self::ExitWouldRaise {
                whose: Whose::TheFrame,
                line,
                name,
            } => write!(
                formatter,
                "line {line} is one of the lines it could be forced out through, \
                 and it reads `{name}`, which the frame holds nothing for. a read \
                 like that **raises** rather than returning, so forcing the frame \
                 out would inject an exception into the program instead of \
                 leaving it — and no other line was usable either, or one of them \
                 would have been taken. cpython binds a frame's unbound locals to \
                 `None` when it moves, and leaves empty cells and unbound globals \
                 alone"
            ),
            Self::ExitWouldRaise {
                whose: Whose::TheCaller,
                line,
                name,
            } => write!(
                formatter,
                "the caller's line {line} reads `{name}`, which the caller holds \
                 nothing for, and a restart runs the whole of that line a second \
                 time — so the re-execution **raises**. where on the line the \
                 read sits decides whether it raises before the call is made \
                 again or after, and neither is a restart"
            ),
            Self::CallerHasNoLine { lasti } => write!(
                formatter,
                "the caller is stopped at offset {lasti}, which its own line \
                 table gives no line of the source. there is nothing to rewind \
                 it to, and picking a nearby line would be picking which \
                 statement runs again"
            ),
            // a write through a namespace is `STORE_NAME`, which only ever
            // goes through `f_locals` — there is no store on the allow list
            // that writes through globals, and `STORE_GLOBAL` is absent from
            // `THROUGH_LOCALS` because `PyDict_SetItem` bypasses `__setitem__`
            Self::SpanNotUnderstood { line } => write!(
                formatter,
                "bpd could not follow what line {line} does with the value the \
                 call returns, so it cannot say which of the caller's names \
                 would hold a value the program never computed. that is a gap in \
                 bpd rather than something to change about the program — please \
                 report the line, and set the next statement by hand meanwhile"
            ),
            Self::NamespaceIsNotADict {
                whose: Whose::TheCaller,
                access: Access::Writes,
                through: _,
                line,
                namespace,
            } => write!(
                formatter,
                "the caller's line {line} stores into a namespace that is a \
                 `{namespace}` rather than a plain dict, so the store runs that \
                 mapping's own `__setitem__` — code of the program, handed the \
                 value of a return the program never made. a class body built \
                 through `__prepare__` is where this comes from"
            ),
            Self::NamespaceIsNotADict {
                whose: Whose::TheCaller,
                access: Access::Reads,
                through: Through::GlobalsOrBuiltins,
                line,
                namespace,
            } => write!(
                formatter,
                "the caller's line {line} reads a global through a namespace \
                 that is a `{namespace}` rather than a plain dict, so the read \
                 runs that mapping's own `__getitem__` or `__missing__` — code \
                 of the program, run again every time the line is. cpython's \
                 fast path for a global needs the globals **and** the builtins \
                 to be plain dicts, and a miss in one falls through to the other"
            ),
            Self::NamespaceIsNotADict {
                whose: Whose::TheCaller,
                access: Access::Reads,
                through: Through::Locals,
                line,
                namespace,
            } => write!(
                formatter,
                "the caller's line {line} reads a name through the frame's own \
                 locals, which are a `{namespace}` rather than a plain dict, so \
                 the read runs that mapping's own `__getitem__` or \
                 `__missing__` — code of the program, run again every time the \
                 line is. a class body built through `__prepare__` is where a \
                 frame gets locals that are not a plain dict"
            ),
            Self::NamespaceIsNotADict {
                whose: Whose::TheFrame,
                access,
                through,
                line,
                namespace,
            } => write!(
                formatter,
                "line {line} is one of the lines it could be forced out through, \
                 and {} a name there goes through {} — so it runs that mapping's \
                 own {}, which is code of the program. {}",
                match access {
                    Access::Reads => "reading",
                    // unreachable today: nothing that writes a name is allowed
                    // on an exit line, which `bpd_agent::bytecode` asserts. said
                    // rather than left to a wildcard, so that allowing one is a
                    // sentence somebody has to write
                    Access::Writes => "writing",
                },
                match access {
                    Access::Reads => "`__getitem__` or `__missing__`",
                    // and the dunder has to move with the verb. it did not: the
                    // write arm said `__getitem__`, so allowing a store on an
                    // exit line would have shipped a false sentence rather than
                    // forced somebody to write a true one
                    Access::Writes => "`__setitem__`",
                },
                match through {
                    Through::GlobalsOrBuiltins =>
                        format!("a namespace that is a `{namespace}` rather than a plain dict"),
                    Through::Locals => format!(
                        "the frame's own locals, which are a `{namespace}` rather than a plain \
                         dict"
                    ),
                },
                match through {
                    Through::GlobalsOrBuiltins =>
                        "cpython's fast path for a global needs the globals **and** the builtins \
                         to be plain dicts, and a miss in one falls through to the other",
                    Through::Locals =>
                        "a class body built through `__prepare__` is where a frame gets locals \
                         that are not a plain dict",
                }
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **every variant, across every discriminant it carries, rendered and read**
    ///
    /// the method finding rather than a finding of its own. two refusals are
    /// produced from more than one place and carried one message written for
    /// one of them, and every review round found a fresh sentence that was false
    /// for a producer nobody had printed. reasoning about whether a message is
    /// true is what kept failing; producing it and reading it is what this is
    ///
    /// so a combination that cannot arise is named here with the reason, rather
    /// than left out — the same standard the guards are held to
    #[test]
    // **what the table leaves out, and why.** the rows below cover every variant
    // and, for the ones that carry discriminants, every combination that arises
    // — with one exception, argued rather than omitted:
    //
    // - `(TheFrame, Writes)` is unreachable: a frame is forced out through a
    //   line of loads and a return, and nothing that writes through a namespace
    //   is allowed on one. the list that guarantees it is the agent's `EXITING`,
    //   which this crate cannot see, so the assertion lives beside that list in
    //   `bpd_agent::bytecode::an_exit_line_holds_nothing_that_writes_a_name`
    // - `(*, Writes, GlobalsOrBuiltins)` is structurally impossible: the globals
    //   branch of `refuses` returns `Access::Reads` unconditionally, because
    //   `STORE_GLOBAL` goes through `PyDict_SetItem` and runs no `__setitem__`
    //
    // `(TheCaller, Writes)` **does** arise and has a row. an earlier note here
    // argued it could not, on the grounds that a call must load its callee and
    // every allow-listed way to do that in a namespace frame is `LOAD_NAME`,
    // which `refuses` meets first. that is false: `refuses` checks globals
    // before locals, so a `global f` inside a prepared class body loads through
    // `LOAD_GLOBAL` — which is on `THROUGH_GLOBALS` only — and the first
    // refusing instruction is the `STORE_NAME`. measured on 3.13 and 3.14
    //
    // nothing forces a row to exist. `Display`'s match makes a new **variant** a
    // compile error and a new **discriminant** of an existing one is not — it
    // falls into an arm that already compiles, which is how a wording written
    // for one producer went out under another
    fn every_refusal_renders_a_sentence_that_is_true_of_its_own_case() {
        // a row silently deleted is a producer nobody renders again, and the
        // table is data so nothing else would notice. the number is here to be
        // changed **deliberately** — every variant, plus one row per extra
        // discriminant combination that arises
        assert_eq!(
            every_refusal().len(),
            22,
            "a row was added or removed; check it is one that arises and say so \
             in the note above"
        );
        for (reason, wanted, forbidden) in every_refusal() {
            // **the composed refusal**, not the inner clause. a user never sees
            // `Unrestartable`'s own `Display` — it arrives wrapped in
            // `Refusal::NotRestartable`'s preamble, which says "rewinds its
            // caller to the call". asserting a `forbidden` against the clause
            // alone let a row forbid "caller" and pass while the string a user
            // reads contained it, which is the guarantee this table exists to
            // hold. the `wanted` entries are checked against the same string for
            // the mirror reason: one satisfied by the preamble is not a check
            let said = crate::Refusal::NotRestartable {
                frame: crate::FrameId { stop: 1, depth: 2 },
                function: "work".to_string(),
                reason: reason.clone(),
            }
            .to_string();
            for expected in wanted {
                assert!(
                    said.contains(expected),
                    "expected {expected:?} in {reason:?}\n  said {said:?}"
                );
            }
            for wrong in forbidden {
                assert!(
                    !said.contains(wrong),
                    "{reason:?} is not that: {wrong:?} in {said:?}"
                );
            }
        }
    }

    /// one of every refusal, with what its sentence must and must not say
    ///
    /// data rather than a body, because the list **is** the test: a producer
    /// whose wording nobody printed is how four false claims shipped
    ///
    /// **nothing forces a row to exist.** `Display`'s match makes a new variant
    /// a compile error, and a new *discriminant* of an existing one is not — it
    /// falls into an arm that already compiles, which is exactly how a wording
    /// written for one producer went out under another. so a discriminant added
    /// below has to be added here by hand, and the count that follows is what
    /// somebody comparing the two reads first
    #[expect(
        clippy::too_many_lines,
        reason = "one entry per refusal per discriminant, which is the point of \
                  it — a shorter list is a producer nobody rendered"
    )]
    fn every_refusal() -> Vec<(Unrestartable, Vec<&'static str>, Vec<&'static str>)> {
        vec![
            (
                Unrestartable::Suspendable {
                    kind: Suspendable::Generator,
                },
                vec!["a generator", "StopIteration", "set the next statement"],
                vec![],
            ),
            (
                Unrestartable::Suspendable {
                    kind: Suspendable::Coroutine,
                },
                vec!["a coroutine", "set the next statement"],
                vec![],
            ),
            (
                Unrestartable::Suspendable {
                    kind: Suspendable::AsyncGenerator,
                },
                vec!["an async generator", "set the next statement"],
                vec![],
            ),
            (
                Unrestartable::NoCleanExit,
                vec!["loads and a return", "implicit `return None`"],
                vec![],
            ),
            (
                Unrestartable::NoCaller,
                vec!["outermost frame", "set the next statement"],
                vec![],
            ),
            (
                Unrestartable::SomethingElseOnTheLine {
                    line: 12,
                    opcode: "LOAD_ATTR".to_string(),
                },
                vec!["line 12", "`LOAD_ATTR`"],
                vec![],
            ),
            (
                Unrestartable::MoreThanOneCall { line: 4, calls: 2 },
                vec!["line 4", "2 calls"],
                vec![],
            ),
            (
                Unrestartable::NothingRunsAfterTheCall { line: 9 },
                vec!["line 9", "can only jump from a 'line' trace event"],
                vec![],
            ),
            (
                Unrestartable::TheCallReadsWhatItStores {
                    line: 7,
                    name: "seed".to_string(),
                },
                vec![
                    "line 7",
                    "`seed`",
                    "a name the call does not read",
                    // the half this guard was missing: the rule is the
                    // read-then-write, not whose value ends up in the name
                    "it does not matter whose value the tail puts there",
                    // and it must admit the direction it errs in, rather than
                    // asserting the call definitely changes
                    "would have been sound",
                ],
                vec![],
            ),
            (
                // the discriminant the table's own doc warns about: a new one on
                // an existing variant is not a compile error, so it falls into
                // an arm that already compiles — which is how this one shipped
                // advice that its own case cannot follow
                Unrestartable::SomethingElseOnTheLine {
                    line: 16,
                    opcode: "CALL_FUNCTION_EX".to_string(),
                },
                vec!["line 16", "*args", "iterating", "positionally"],
                vec!["put the call on a line of its own"],
            ),
            (
                Unrestartable::CopiedLine { line: 44, runs: 2 },
                vec!["line 44", "2 separate runs", "a line of its own"],
                // it must not assert the cause it cannot know. most lines that
                // reach this are not in a `finally` at all — swept over both
                // stdlibs, 3723 of 4161 on 3.13 and 3758 of 4219 on 3.14
                vec!["move it out of the `finally`"],
            ),
            (
                Unrestartable::TailWritesSharedState {
                    line: 23,
                    name: "SEEN".to_string(),
                },
                vec![
                    "line 23",
                    "`SEEN`",
                    "shared with a closure",
                    "before the call",
                ],
                // it must not claim to know the callee reads it — that is the
                // thing it refuses **because** it cannot know
                vec!["the callee reads", "would read"],
            ),
            (
                Unrestartable::NotEnteredByACall {
                    line: 17,
                    opcode: "LOAD_ATTR".to_string(),
                },
                vec![
                    "line 17",
                    "`LOAD_ATTR`",
                    "not entered by one",
                    // it must say what *does* work, because something does — and
                    // must not offer the one-line `fget` form, which is two
                    // calls and refused for that instead
                    "hoisted to a line of its own",
                ],
                // the remedy the shared sentence used to end with, which this
                // case cannot follow: the call it names does not exist
                vec![
                    // the shared remedy, which is what this variant was split
                    // out to stop it giving. "a line of its own" alone is too
                    // broad now: hoisting the `fget` to one is the real remedy
                    "put the call on a line of its own",
                    "arguments already in locals",
                    // both falsified by its own examples, and both were there
                    "no way to write the line",
                    "`SEND`",
                    // and the one-line `fget` form, which is two calls
                    "`type(obj).attr.fget(obj)` restarts",
                ],
            ),
            (
                Unrestartable::SpanNotUnderstood { line: 11 },
                vec!["line 11", "could not follow", "gap in bpd"],
                // it must not tell the user to change their program: there is
                // nothing wrong with it, and the walk is bpd's to fix
                vec!["and it can be restarted", "put the call"],
            ),
            (
                Unrestartable::CallerHasNoLine { lasti: 42 },
                vec!["offset 42", "no line of the source"],
                vec![],
            ),
            (
                // the frame's own exit line, blocked by a **read** through the
                // globals pair. it is one of possibly several blocked lines, not
                // the only way out — the agent records the first it meets
                Unrestartable::NamespaceIsNotADict {
                    whose: Whose::TheFrame,
                    access: Access::Reads,
                    through: Through::GlobalsOrBuiltins,
                    line: 4,
                    namespace: "Missing".to_string(),
                },
                vec!["line 4", "`Missing`", "__missing__", "builtins"],
                vec![
                    "the only way out",
                    "stores",
                    "__setitem__",
                    "__prepare__",
                    "the caller's line",
                    "own locals",
                ],
            ),
            (
                // the same line blocked through the frame's **locals**. the
                // globals sentence would be false here: a class body's locals can
                // be a `__prepare__` mapping with globals and builtins both plain
                Unrestartable::NamespaceIsNotADict {
                    whose: Whose::TheFrame,
                    access: Access::Reads,
                    through: Through::Locals,
                    line: 4,
                    namespace: "OrderedDict".to_string(),
                },
                vec!["line 4", "`OrderedDict`", "__missing__", "__prepare__"],
                vec!["builtins", "the only way out", "the caller's line"],
            ),
            (
                // the caller's line, blocked by a **read** through the globals
                // pair. this is the case the producer-only split still shipped
                // the store wording for
                Unrestartable::NamespaceIsNotADict {
                    whose: Whose::TheCaller,
                    access: Access::Reads,
                    through: Through::GlobalsOrBuiltins,
                    line: 8,
                    namespace: "Missing".to_string(),
                },
                vec![
                    "the caller's line",
                    "line 8",
                    "`Missing`",
                    "__missing__",
                    "builtins",
                ],
                vec!["stores into", "__setitem__", "__prepare__"],
            ),
            (
                // and the caller's line blocked by a read through its **locals**,
                // which is what a class body making the call really produces. the
                // globals-and-builtins sentence named neither mapping in play
                Unrestartable::NamespaceIsNotADict {
                    whose: Whose::TheCaller,
                    access: Access::Reads,
                    through: Through::Locals,
                    line: 8,
                    namespace: "OrderedDict".to_string(),
                },
                vec![
                    "the caller's line",
                    "line 8",
                    "`OrderedDict`",
                    "__missing__",
                    "__prepare__",
                ],
                vec!["builtins", "stores into", "__setitem__"],
            ),
            (
                // and the caller's line blocked by a **store**, which is the only
                // case the original wording was ever true of
                Unrestartable::NamespaceIsNotADict {
                    whose: Whose::TheCaller,
                    access: Access::Writes,
                    through: Through::Locals,
                    line: 3,
                    namespace: "OrderedDict".to_string(),
                },
                vec![
                    "the caller's line",
                    "line 3",
                    "`OrderedDict`",
                    "__setitem__",
                    "__prepare__",
                ],
                vec!["__missing__"],
            ),
            (
                Unrestartable::ExitWouldRaise {
                    whose: Whose::TheFrame,
                    line: 9,
                    name: "cell".to_string(),
                },
                vec!["line 9", "`cell`", "raises"],
                // it is the first blocked line the agent met, not proof that
                // every other one reads the same name — and it is the **frame's**
                // exit line, so it must not be described as the caller's. the
                // preamble says "rewinds its caller to the call", which is why
                // this forbids the clause rather than the word
                vec!["every line", "the caller's line"],
            ),
            (
                Unrestartable::ExitWouldRaise {
                    whose: Whose::TheCaller,
                    line: 11,
                    name: "NEVER_BOUND".to_string(),
                },
                vec!["the caller's line", "line 11", "`NEVER_BOUND`", "raises"],
                // the span is scanned both sides of the call, so a raising read
                // **after** it does not stop the call being made again
                vec!["would not reach the call"],
            ),
        ]
    }

    /// the json a client reads an arranged restart out of, pinned
    ///
    /// `Restarted` goes out whole, so this shape *is* the wire. it is pinned
    /// for the reason a refused jump's is: a reader that cannot tell the two
    /// outcomes apart would render a program whose frame was forced out and a
    /// program nothing happened to as the same thing, and those are opposite
    /// states
    #[test]
    fn an_arranged_restart_and_a_refused_one_serialise_as_different_tags() {
        let arranged = Restarted::Arranged(Restarting {
            frame: Where {
                file: "/tmp/p/app.py".to_string(),
                line: 9,
                function: "work".to_string(),
            },
            exit_line: 11,
            caller: Where {
                file: "/tmp/p/app.py".to_string(),
                line: 20,
                function: "main".to_string(),
            },
            disturbed: vec!["got".to_string()],
            bound_to_none: vec!["later".to_string()],
            unannounced: vec![20],
            mode: Mode::NonStop,
        });
        let json = serde_json::to_value(&arranged).expect("Restarted serialises");
        assert_eq!(json["restarted"], "arranged");
        assert_eq!(json["exit_line"], 11);
        assert_eq!(json["caller"]["line"], 20);
        assert_eq!(json["disturbed"][0], "got");

        let refused = Restarted::Refused {
            tried: vec![11, 14],
            error: bpd_core_error(),
        };
        let json = serde_json::to_value(&refused).expect("Restarted serialises");
        assert_eq!(json["restarted"], "refused");
        assert_eq!(json["tried"][1], 14);
        // an object, never a string, for the reason a refused jump's is
        assert!(json["error"].is_object(), "was {}", json["error"]);
        assert_eq!(json["error"]["kind"], "ValueError");
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

    fn bpd_core_error() -> PythonError {
        PythonError {
            kind: "ValueError".to_string(),
            message: "can't jump into the body of a for loop".to_string(),
            traceback: Vec::new(),
        }
    }
}
