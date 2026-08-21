//! reading a code object's instructions, to decide what re-running a line does
//!
//! restarting a frame re-executes a whole **line** of the caller, so whether it
//! is safe is a question about instructions rather than about source. this is
//! where that question is asked, and it is asked entirely **before** anything is
//! attempted — a restart either was refused, or bpd has read the instructions
//! and knows what running them again does
//!
//! ## the lists are allow lists, and that is the whole design
//!
//! it would be shorter to list the instructions that run code of the program —
//! `LOAD_ATTR` for a property, `BINARY_OP` for a dunder, `TO_BOOL` for
//! `__bool__` — and permit the rest. that list fails **open**: an opcode a
//! future interpreter adds is one nobody wrote down, and it would be silently
//! permitted. 3.13 and 3.14 already spell half of these differently — `LOAD_FAST`
//! became `LOAD_FAST_BORROW`, `BINARY_SUBSCR` was folded into `BINARY_OP`,
//! `RETURN_CONST` became `LOAD_CONST` and a `RETURN_VALUE` — so this is not a
//! hypothetical
//!
//! so the lists here are of instructions that provably run **nothing**, and
//! anything else is a refusal naming it. a new opcode makes a restart refuse
//! until somebody has looked at it, which is the direction this project fails in
//!
//! ## it reads the instructions through `dis`
//!
//! rather than decoding `co_code` against a table of opcode numbers. the table
//! is per version and would be one more thing hand-maintained against an
//! interpreter that renumbers opcodes every release; `dis` is the interpreter's
//! own answer about its own bytecode. it costs an import into the debuggee, on
//! the request path and never on the event path

use bpd_core::{Access, Through, Unrestartable, Whose};
use pyo3::prelude::*;

/// instructions that may appear on a line a frame is forced out through
///
/// the line has to **return and do nothing else**, so that moving there executes
/// no statement of the program
///
/// "a load runs nothing" is **not** true of all of these on its own, and saying
/// so was two live defects. three of them carry a condition that is a property of
/// the frame rather than of the opcode, so [`Namespaces`] carries the frame's
/// answer and the walk asks it:
///
/// - `LOAD_GLOBAL` and `LOAD_NAME` are `PyObject_GetItem` unless the mappings
///   behind them are exact dicts — globals **and** builtins, because a miss in
///   one falls through to the other
/// - `LOAD_DEREF` **raises** on a cell that holds nothing. the move binds unbound
///   **locals** to `None` and leaves cells alone — measured on 3.13, 3.14 and
///   3.14t, against a note here that had claimed otherwise
///
/// `LOAD_FAST_CHECK` really is covered by that binding, which is why it stays
/// unconditional
///
/// **`LOAD_COMMON_CONSTANT` carries no condition at all,** unlike every other
/// load here. it is `tstate->interp->common_consts[oparg]` — an index into a
/// twelve-entry array the interpreter owns, holding `None`, `True`, `False`,
/// `""`, `-1`, `AssertionError`, `NotImplementedError` and five builtin types.
/// no mapping, no descriptor, nothing of the program. 3.15 is where it starts
/// carrying a function's implicit `return None`, and its absence cost **14065**
/// of 33030 code objects their exit there — the allow list failing closed,
/// which is the direction it is meant to fail in, but it is still a whole
/// release's worth of ordinary functions
///
/// **`RESUME` is deliberately absent.** it sits on the `def` line rather than on
/// the body's, so a walk that crossed it made the `def` line a candidate for any
/// function whose body is immediately a clean return — and the `def` line comes
/// first in `co_lines` order, so it was the one tried first. measured on 3.13,
/// 3.14 and 3.14t: cpython accepts that jump, lands at offset 0, and
/// re-executing `RESUME` fires a **second `PY_START`** for a frame that was
/// already started. leaving it out costs nothing, because `RESUME` appears only
/// at offset 0 of a plain function and no real return line contains one
///
/// **`CACHE` is dead here and in [`BESIDE_THE_CALL`].** `dis.get_instructions`
/// reports no `CACHE` entry at all on any of the three interpreters — swept over
/// both stdlibs — so nothing ever matches it. it is left in rather than removed
/// because the direction of a wrong guess matters: an allow list that names one
/// opcode too many fails **closed** if `dis` ever starts reporting them, and an
/// allow list missing one refuses shapes it need not
const EXITING: &[&str] = &[
    "RETURN_VALUE",
    "RETURN_CONST",
    "LOAD_CONST",
    "LOAD_COMMON_CONSTANT",
    "LOAD_SMALL_INT",
    "LOAD_FAST",
    "LOAD_FAST_BORROW",
    "LOAD_FAST_CHECK",
    "LOAD_DEREF",
    "LOAD_GLOBAL",
    "NOP",
    "NOT_TAKEN",
    "CACHE",
    "EXTENDED_ARG",
];

/// instructions that may share the caller's line with the call being restarted
///
/// every one of them re-executes when the line does, so every one of them has
/// to run nothing of the program:
///
/// - **loads** read out of storage
/// - **stack shuffles** move slots
/// - `BUILD_TUPLE` and `BUILD_LIST` build a container out of what is already on
///   the stack, and call nothing. `BUILD_MAP` and `BUILD_SET` are deliberately
///   absent — both hash their keys, and `__hash__` is code of the program
/// - **stores** into a name are here because the forced return really does
///   return a value and the rest of the line really does run with it. that is
///   reported as [`bpd_core::Restarting::disturbed`] rather than hidden.
///   `STORE_ATTR` and `STORE_SUBSCR` are absent, because a setter and a
///   `__setitem__` are code of the program
const BESIDE_THE_CALL: &[&str] = &[
    "LOAD_FAST",
    "LOAD_FAST_BORROW",
    "LOAD_FAST_CHECK",
    "LOAD_FAST_LOAD_FAST",
    "LOAD_FAST_BORROW_LOAD_FAST_BORROW",
    "LOAD_CONST",
    "LOAD_COMMON_CONSTANT",
    "LOAD_SMALL_INT",
    "LOAD_GLOBAL",
    "LOAD_DEREF",
    "LOAD_NAME",
    "PUSH_NULL",
    "COPY",
    "SWAP",
    "BUILD_TUPLE",
    "BUILD_LIST",
    "POP_TOP",
    "STORE_FAST",
    "STORE_FAST_STORE_FAST",
    "STORE_FAST_LOAD_FAST",
    "STORE_DEREF",
    "STORE_GLOBAL",
    "STORE_NAME",
    "NOP",
    "NOT_TAKEN",
    "RESUME",
    "CACHE",
    "EXTENDED_ARG",
];

/// the instructions that call something of the program
///
/// what makes a line restartable is that it holds exactly **one** of these, and
/// that the caller is stopped in it. measured on 3.13, 3.14 and 3.14t: a frame
/// suspended in a call has `f_lasti` at the call instruction itself, for all
/// three of these
const CALLING: &[&str] = &["CALL", "CALL_KW", "CALL_FUNCTION_EX"];

/// the call that is refused even when it is the only one on the line
///
/// `f(*args)` compiles to `CALL_FUNCTION_EX`, which unpacks its argument with
/// `PySequence_Tuple` when it is not already a tuple — and that iterates the
/// object, running its `__iter__` and `__next__`. so re-executing the line
/// consumes the iterable a second time, and an `args` that was a generator is
/// **empty** the second time round: the restarted call would be made with
/// different arguments than the one it replaced
///
/// it is here rather than left out of [`CALLING`] because it still has to be
/// recognised as the call — it is where `f_lasti` is, and it is what says the
/// line makes one call rather than none
const UNPACKING: &str = "CALL_FUNCTION_EX";

/// the instructions that end a frame
const RETURNING: &[&str] = &["RETURN_VALUE", "RETURN_CONST"];

/// the instructions whose argument **is** a name
///
/// every list below is consulted by name, and one opcode not on any of them
/// still has its names read: the walk's default arm follows the call's value
/// through a load by asking what its name holds. so this is the union rather
/// than a per-list question, and it is a list rather than a type check because
/// `LOAD_CONST` of a string is a `str` argval that names nothing
const NAMING: &[&str] = &[
    "LOAD_FAST",
    "LOAD_FAST_BORROW",
    "LOAD_FAST_CHECK",
    "LOAD_FAST_LOAD_FAST",
    "LOAD_FAST_BORROW_LOAD_FAST_BORROW",
    "LOAD_FAST_AND_CLEAR",
    "LOAD_GLOBAL",
    "LOAD_NAME",
    "LOAD_DEREF",
    "LOAD_FROM_DICT_OR_DEREF",
    "STORE_FAST",
    "STORE_FAST_STORE_FAST",
    "STORE_FAST_LOAD_FAST",
    "STORE_DEREF",
    "STORE_GLOBAL",
    "STORE_NAME",
];

/// the instructions that write somewhere the callee can also read
///
/// a `STORE_FAST` is the caller's alone and a class body's `STORE_NAME` writes a
/// namespace no callee reads as its globals. these two reach code this analysis
/// never looks at, and a programmer writing one on a call line is writing to
/// shared state deliberately
///
/// **`STORE_NAME` is not on this list and is refused anyway**, by
/// [`shared_with_the_callee`], when the frame's locals are its globals. it is
/// conditional rather than constant, which is the only reason it is not here: a
/// module body's namespace **is** its globals, so `kept = f(1)` at top level
/// writes a global too, and a class body's namespace is its own
const SHARED_WITH_THE_CALLEE: &[&str] = &["STORE_GLOBAL", "STORE_DEREF"];

/// the instructions that write a name
///
/// **a fused instruction's names are not all writes.** `dis` gives one a tuple
/// `argval` and the halves are different operations: `STORE_FAST_LOAD_FAST
/// ('a', 'spare')` writes `a` and **reads** `spare`. counting both as writes put
/// the caller's own parameter in [`bpd_core::Restarting::disturbed`] — a slot
/// the debugger never wrote, reported as holding a value the program never
/// computed. how many of the names each one writes is [`writes`]
const STORING: &[&str] = &[
    "STORE_FAST",
    "STORE_FAST_STORE_FAST",
    "STORE_FAST_LOAD_FAST",
    "STORE_DEREF",
    "STORE_GLOBAL",
    "STORE_NAME",
];

/// how many of an instruction's names it **writes**, in pop order
///
/// the rest it reads. measured on 3.13, 3.14 and 3.14t: `STORE_FAST_STORE_FAST`
/// pops twice and writes both, `STORE_FAST_LOAD_FAST` pops once and then pushes
/// what it loads, and every other store writes the one name it has
fn writes(one: &Instruction) -> usize {
    match one.opname.as_str() {
        "STORE_FAST_STORE_FAST" => 2,
        one if STORING.contains(&one) => 1,
        _ => 0,
    }
}

/// the instructions that read a name
///
/// what says whether the call reads back a name the line is about to store into
const LOADING: &[&str] = &[
    "LOAD_FAST",
    "LOAD_FAST_BORROW",
    "LOAD_FAST_CHECK",
    "LOAD_FAST_LOAD_FAST",
    "LOAD_FAST_BORROW_LOAD_FAST_BORROW",
    "STORE_FAST_LOAD_FAST",
    "LOAD_NAME",
    "LOAD_GLOBAL",
    "LOAD_DEREF",
];

/// the instructions that read a global name
///
/// safe **only** when the globals mapping **and the builtins mapping** are both
/// exact dicts. cpython's `LOAD_GLOBAL` fast path requires both — a name that is
/// not in globals falls through to builtins — and off that path it is
/// `PyObject_GetItem`, which runs `__getitem__` or `__missing__`. measured: a
/// plain-dict globals with a dict-subclass `__builtins__` ran the program's
/// `__missing__` during a forced exit while the answer said `Arranged`
///
/// `LOAD_NAME` is here as well as in [`THROUGH_LOCALS`] because it falls through
/// locals to globals to builtins, so all three have to be plain
const THROUGH_GLOBALS: &[&str] = &["LOAD_GLOBAL", "LOAD_NAME"];

/// the instructions that read or write through the frame's locals mapping
///
/// a module body and a class body use these. a class body built through a
/// `__prepare__` of its own has a mapping whose `__getitem__` and `__setitem__`
/// are the program's code
///
/// `STORE_GLOBAL` is deliberately **not** here: it is `PyDict_SetItem` on
/// `f_globals` directly, which bypasses a subclass's `__setitem__` entirely —
/// measured, and written down because its absence otherwise looks like an
/// oversight
const THROUGH_LOCALS: &[&str] = &["LOAD_NAME", "STORE_NAME"];

/// what the mappings behind a frame's names really are
///
/// the allow lists say which **opcodes** run nothing. two of them run nothing
/// only when the mapping behind them is an exact dict, and that is a property of
/// the frame rather than of the code — so it is read off the frame and carried
/// in here
#[derive(Debug, Clone)]
pub(crate) struct Namespaces {
    /// whether `f_globals` **and** `f_builtins` are both exact dicts
    ///
    /// both, because a `LOAD_GLOBAL` that misses globals falls through to
    /// builtins and the fast path needs the pair
    pub(crate) globals_exact: bool,
    /// what the one that is not an exact dict is, for the refusal to name
    pub(crate) globals: String,
    /// whether `f_locals` is an exact dict
    pub(crate) locals_exact: bool,
    /// whether this frame's locals **are** its globals
    ///
    /// true whenever the frame's namespace **is** the global namespace: a module
    /// body always, an `exec` given only a globals mapping, and a class body
    /// whose `__prepare__` hands back `globals()` — measured on 3.13, 3.14 and
    /// 3.14t, and all three are refused, which is the answer they should get. a
    /// function is never one: it keeps its locals in
    /// slots, and a class body's namespace is its own. it decides whether a
    /// `STORE_NAME` on the call line writes somewhere the **callee** can read,
    /// because a module's namespace is the global namespace
    pub(crate) locals_are_globals: bool,
    /// what it is instead, for the refusal to name
    pub(crate) locals: String,
    /// the global names this frame's mappings hold nothing for
    ///
    /// `LOAD_GLOBAL` **raises `NameError`** for a name that is in neither
    /// globals nor builtins, and that is true with both of them a plain dict —
    /// so it is a separate question from whether the mappings are exact. it is
    /// the same defect the empty cell was: a forced exit that raises into the
    /// program rather than returning
    pub(crate) unresolvable: Vec<String>,
    /// the cells this frame holds nothing in
    ///
    /// `LOAD_DEREF` on one of them **raises**. the allow list used to justify it
    /// with "cpython binds every unbound local as part of the move", which is
    /// true of locals and measured false of cells — so a forced exit through
    /// `return cell` raised into the program, and when the caller had a handler
    /// the rewind fired on that handler's own line and bpd reported success
    pub(crate) unbound_cells: Vec<String>,
    /// the local slots this frame holds nothing in
    ///
    /// only ever consulted for the **caller**, and [`Moved`] is what says so.
    /// `LOAD_FAST_CHECK` reads one of these in the tail that runs after the
    /// forced return and before the rewind, where nothing has bound it yet
    pub(crate) unbound_fasts: Vec<String>,
}

/// whether the frame is moved **before** the instructions being asked about run
///
/// the two paths this analysis serves differ on exactly one question, and it is
/// this one. getting it wrong is not a wording defect: it decides whether
/// `LOAD_FAST_CHECK` can raise
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Moved {
    /// the frame's own exit, which runs **after** the move that reaches it
    ///
    /// that move binds every unbound local: `frame_lineno_set_impl` walks
    /// `co_nlocalsplus` and fills every NULL slot with `None` — measured on
    /// 3.13, 3.14 and 3.14t, and it is the same loop that leaves cells alone,
    /// which is why an unbound **cell** still raises there and an unbound local
    /// does not
    First,
    /// the caller's tail, which runs **before** anything moves the caller
    ///
    /// the forced return lands, the rest of the line runs with it, and only then
    /// does the line event fire that the rewind is made from. so a slot that
    /// holds nothing now holds nothing when `LOAD_FAST_CHECK` reads it, and it
    /// raises `UnboundLocalError` out of the tail — which cost a restart that
    /// was answered `Arranged` and then abandoned as `CallerLeft`, a restart
    /// that was never possible, discovered by attempting it
    NotYet,
}

impl Namespaces {
    /// the mapping this instruction goes through, and **what it does** through
    /// it, when that mapping is not a plain dict
    ///
    /// both halves, because the two are what a refusal has to say and they are
    /// orthogonal: a producer-only split still shipped store wording for a
    /// caller-side `LOAD_GLOBAL`
    fn refuses(&self, one: &Instruction) -> Option<(Access, Through, &str)> {
        let opname = one.opname.as_str();
        if !self.globals_exact && THROUGH_GLOBALS.contains(&opname) {
            return Some((Access::Reads, Through::GlobalsOrBuiltins, &self.globals));
        }
        if !self.locals_exact && THROUGH_LOCALS.contains(&opname) {
            // `LOAD_NAME` is on both lists; only `STORE_NAME` writes
            let access = if WRITES_A_NAME.contains(&opname) {
                Access::Writes
            } else {
                Access::Reads
            };
            return Some((access, Through::Locals, &self.locals));
        }
        None
    }

    /// whether this instruction would **raise** rather than read
    ///
    /// two shapes, and both were found the same way: an allow list entry
    /// justified as "a load runs nothing" that turns out to raise into the
    /// program on the way out
    fn raises(&self, one: &Instruction, moved: Moved) -> Option<String> {
        let held_nowhere = if READS_A_CELL.contains(&one.opname.as_str()) {
            &self.unbound_cells
        } else if READS_A_NAME.contains(&one.opname.as_str()) {
            &self.unresolvable
        } else if READS_A_FAST.contains(&one.opname.as_str()) {
            // the one question [`Moved`] exists to answer. at the frame's own
            // exit the move has already bound this slot to `None`, so asking
            // would refuse a shape that is safe; in the caller's tail nothing
            // has moved yet
            match moved {
                Moved::First => return None,
                Moved::NotYet => &self.unbound_fasts,
            }
        } else {
            return None;
        };
        one.names
            .iter()
            .find(|name| held_nowhere.contains(name))
            .cloned()
    }
}

/// the instructions that write a name through a namespace mapping
const WRITES_A_NAME: &[&str] = &["STORE_NAME"];

/// the instructions that read a cell, which raise when the cell holds nothing
const READS_A_CELL: &[&str] = &["LOAD_DEREF", "LOAD_FROM_DICT_OR_DEREF"];

/// the instructions that read a local slot **and check it first**
///
/// `LOAD_FAST_CHECK` raises `UnboundLocalError` for a slot holding nothing.
/// whether that can happen depends entirely on [`Moved`], which is why it is a
/// list of its own rather than another arm of [`READS_A_CELL`]
const READS_A_FAST: &[&str] = &["LOAD_FAST_CHECK"];

/// the instructions that look a name up in the frame's mappings
///
/// they raise `NameError` when the name is in none of them, which is a question
/// about the **name** rather than about the mappings' types — `LOAD_GLOBAL` on a
/// name bound nowhere raises with globals and builtins both plain dicts
const READS_A_NAME: &[&str] = &["LOAD_GLOBAL", "LOAD_NAME"];

/// one instruction of a code object, as `dis` describes it
struct Instruction {
    offset: u32,
    opname: String,
    /// the opcode's number, which is what `dis.stack_effect` is keyed by
    ///
    /// the name is what every list here is consulted by, and the number is what
    /// the interpreter answers a stack-effect question for. they are the same
    /// instruction asked about two ways
    opcode: u8,
    /// the names it reads or writes, in the order it names them
    ///
    /// empty for an instruction that names nothing — a constant, a jump target,
    /// an operator. which those are is [`NAMING`], read off the **opcode**: the
    /// argval's type cannot answer it, because `LOAD_CONST 'a'` carries a `str`
    /// and names no `a`
    names: Vec<String>,
    /// the raw oparg, which is what `COPY`, `SWAP` and `BUILD_TUPLE` mean by
    /// their argument and what says whether a `LOAD_GLOBAL` pushes a `NULL`
    /// under the value it loads
    ///
    /// `None` for an instruction that takes no argument. read separately from
    /// [`Instruction::names`] because the two are different questions: `COPY 1`
    /// has an argument and no name
    arg: Option<u32>,
}

/// every instruction of a code object, in offset order
///
/// `dis.get_instructions` rather than a decode of `co_code`: the interpreter is
/// the only thing that knows how to read its own bytecode, and it renumbers
/// opcodes between releases
fn instructions(code: &Bound<'_, PyAny>) -> PyResult<Vec<Instruction>> {
    let dis = code.py().import("dis")?;
    let mut read = Vec::new();
    for entry in dis.call_method1("get_instructions", (code,))?.try_iter()? {
        let entry = entry?;
        let opname: String = entry.getattr("opname")?.extract()?;
        read.push(Instruction {
            offset: entry.getattr("offset")?.extract()?,
            opcode: entry.getattr("opcode")?.extract()?,
            names: names_of(&opname, &entry.getattr("argval")?)?,
            arg: entry.getattr("arg")?.extract()?,
            opname,
        });
    }
    Ok(read)
}

/// the names an instruction's `argval` holds, if it holds names at all
///
/// a `str` is one name and a `tuple` of `str` is several — `STORE_FAST_STORE_FAST`
/// carries two, and reading only the string form dropped both of them
///
/// **the opcode decides, not the argument's type.** `LOAD_CONST 'ca'` carries a
/// `str` argval and names nothing, and reading the type alone made it look like
/// a read of a name called `ca` — so `x = f(0); y = 'x'` reported `y` as holding
/// the forced return, in the one field whose purpose is to say which slots a
/// client cannot trust
fn names_of(opname: &str, argval: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    if !NAMING.contains(&opname) {
        return Ok(Vec::new());
    }
    if argval.is_instance_of::<pyo3::types::PyString>() {
        return Ok(vec![argval.extract()?]);
    }
    if argval.is_instance_of::<pyo3::types::PyTuple>() {
        let mut names = Vec::new();
        for item in argval.try_iter()? {
            let item = item?;
            if item.is_instance_of::<pyo3::types::PyString>() {
                names.push(item.extract()?);
            }
        }
        return Ok(names);
    }
    Ok(Vec::new())
}

/// the `co_lines` ranges of a code object, with the lines that have none dropped
///
/// `0` is what cpython gives a module's own `RESUME` and `None` is what it gives
/// an instruction with no source line at all. neither is a line of the file and
/// neither can be jumped to
fn spans(code: &Bound<'_, PyAny>) -> PyResult<Vec<Span>> {
    let mut spans = Vec::new();
    for entry in code.call_method0("co_lines")?.try_iter()? {
        let (start, end, line): (u32, u32, Option<u32>) = entry?.extract()?;
        if let Some(line) = line.filter(|line| *line >= 1) {
            spans.push(Span { start, end, line });
        }
    }
    Ok(spans)
}

/// one contiguous run of instructions that carry one line
struct Span {
    start: u32,
    end: u32,
    line: u32,
}

/// where `frame.f_lineno = line` is **guessed** to put the instruction pointer
///
/// the lowest offset of the line, and that is a guess rather than the rule.
/// cpython's `marklines` marks every `co_lines` range start as a candidate and
/// `frame_setlineno` then picks the candidate whose stack is compatible with
/// where the frame is now — so a jump made from inside a block can land on a
/// later copy. measured on 3.13, 3.14 and 3.14t: jumping to `with cm:` from
/// inside its body landed at 32 while the lowest offset is 2
///
/// an earlier version of this called the lowest offset a measured fact and "the
/// whole of why the unit of this analysis is a span". it is neither
///
/// **nothing depends on this being right any more.** the frame's own exit does
/// not use it at all: [`exit_tails`] names an offset, and
/// [`crate::linetable::move_to`] makes that offset the only destination cpython
/// has to choose between. this is only where the **caller's** span is read from,
/// and that guess is carried on [`CallLine::from`] and compared with the landing
/// at the moment of the rewind — a mismatch abandons the restart rather than
/// resuming into a span nobody read
fn destination(spans: &[Span], line: u32) -> Option<u32> {
    spans
        .iter()
        .filter(|span| span.line == line)
        .map(|span| span.start)
        .min()
}

/// whether `lasti` is inside a block whose exit runs code of the program
///
/// exactly the question "was this frame inside a `with` or a `try`", and it is
/// asked so that the answer can say a `__exit__` and a `finally` **did not run**
/// only when there was one to run. said unconditionally it was a lecture: true
/// of the operation rather than of this use of it, and a caveat repeated at
/// every restart is one a reader learns to skip
///
/// the exception table is the exact answer and there is no public reader for
/// it. this parse is checked against `dis._parse_exception_table` — cpython's
/// own, private, so not usable here — over `with`, `try/finally`, `try/except`
/// and a plain function on 3.13, 3.14 and 3.15, and matches entry for entry
///
/// the format is four varints per entry, six bits to a byte, bit 6 continuing
/// and bit 7 opening an entry, with the offsets in **code units**
pub(crate) fn inside_a_block(code: &Bound<'_, PyAny>, lasti: u32) -> PyResult<bool> {
    let table: Vec<u8> = code.getattr("co_exceptiontable")?.extract()?;
    let mut at = 0;
    while at < table.len() {
        assert!(
            table[at] & 0x80 != 0,
            "an exception table entry opens with the top bit set"
        );
        let mut read = || {
            let mut byte = table[at];
            let mut value = u32::from(byte & 0x3F);
            at += 1;
            while byte & 0x40 != 0 && at < table.len() {
                byte = table[at];
                value = (value << 6) | u32::from(byte & 0x3F);
                at += 1;
            }
            value
        };
        let start = read();
        let length = read();
        let _target = read();
        let _depth = read();
        if lasti >= start * 2 && lasti < (start + length) * 2 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// where a frame can be forced out, highest offset first
///
/// an **offset**, not a line. `frame.f_lineno` will only land on a `co_lines`
/// range start, and cpython fuses a function's implicit `return None` onto the
/// **last statement's** line — so the two instructions that make up that return
/// are a perfectly clean exit sitting in the middle of a range, which no line
/// number names. that is the whole of why an exit used to be rare: measured over
/// each interpreter's own stdlib by `scripts/restart_shapes.py`, code objects
/// with a clean exit go from 20.4% to 65.7% on 3.13, 17.2% to 55.2% on 3.14, and
/// 17.4% to 55.4% on 3.15. [`crate::linetable`] is what reaches one
///
/// ## the offset has to be at abstract stack depth zero
///
/// the move unwinds the frame's stack down to the target's depth and no further.
/// so a target at depth one would have the frame return whatever the unwind
/// happened to leave on top — an intermediate value of the program, returned as
/// if the function had computed it, silently. every candidate here is at depth
/// zero and nothing else is offered
///
/// depth is counted **backwards from the return**, because that is the one point
/// in a code object whose depth cpython states: `mark_stacks` holds
/// `pop_value(next_stack) == EMPTY_STACK` at `RETURN_VALUE`, so exactly one value
/// is on the stack there, and `RETURN_CONST` carries its own and takes none. each
/// step back is `dis.stack_effect`, which is `PyCompile_OpcodeStackEffect` — the
/// same function `mark_stacks` itself falls back to for everything it does not
/// special-case
///
/// ## more than one is offered
///
/// the depth-zero target is compatible with **every** position in the function —
/// `compatible_stack` returns true whenever the target stack is empty, so there
/// is no `incompatible stacks` and no `can't jump into the body of a for loop` to
/// route around. what is left is a target `mark_stacks` never reached, which
/// answers `cannot find bytecode for specified line`, and that is cpython's
/// answer given at the time. so they are tried in order
pub(crate) fn exit_tails(
    code: &Bound<'_, PyAny>,
    namespaces: &Namespaces,
) -> PyResult<Result<Vec<Exit>, Unrestartable>> {
    let read = instructions(code)?;
    let spans = spans(code)?;
    let mut tails: Vec<Exit> = Vec::new();
    let mut blocked_by_a_namespace = None;
    let mut blocked_by_a_name = None;

    for offset in depth_zero_tails(code.py(), &read)? {
        if offset == 0 {
            // the first instruction of a code object is its `RESUME`, so no
            // exit is here — but the table bpd writes to reach an offset keeps
            // a real prefix of the code object's own, and there is no prefix
            // before the first unit. dropped rather than asserted about
            continue;
        }
        let Some(line) = spans
            .iter()
            .find(|span| offset >= span.start && offset < span.end)
            .map(|span| span.line)
        else {
            // an offset the line table gives no line at all, which a client
            // could not be told the exit line of. swept over both stdlibs it is
            // 14 of 23754 tails on 3.13 and 16 of 24216 on 3.14, and every one
            // is the `RETURN_CONST` of an **empty** module body, sitting in the
            // line-0 range cpython gives a module's `RESUME`. those have no
            // caller and are refused before this runs
            continue;
        };
        match walk_to_a_return(&read, offset, namespaces) {
            Walk::Reaches => tails.push(Exit { offset, line }),
            Walk::ThroughANamespace {
                access,
                through,
                namespace,
            } => {
                blocked_by_a_namespace.get_or_insert((line, access, through, namespace));
            }
            Walk::Raises { name } => {
                blocked_by_a_name.get_or_insert((line, name));
            }
            Walk::Runs => unreachable!(
                "a depth-zero tail is a run of exiting instructions ending in a return"
            ),
        }
    }
    tails.sort_unstable_by_key(|exit| std::cmp::Reverse(exit.offset));

    // a tail that would have been an exit but for the mapping behind a name is
    // reported as that rather than as "this function has no clean exit", which
    // would send somebody looking for a `return` they already have
    if tails.is_empty()
        && let Some((line, access, through, namespace)) = blocked_by_a_namespace
    {
        return Ok(Err(Unrestartable::NamespaceIsNotADict {
            whose: Whose::TheFrame,
            access,
            through,
            line,
            namespace,
        }));
    }
    // and the same for a read the frame holds nothing for
    if tails.is_empty()
        && let Some((line, name)) = blocked_by_a_name
    {
        return Ok(Err(Unrestartable::ExitWouldRaise {
            whose: Whose::TheFrame,
            line,
            name,
        }));
    }
    Ok(Ok(tails))
}

/// an offset a frame can be forced out through
pub(crate) struct Exit {
    /// where the interpreter is put, which is where the return's value is loaded
    pub(crate) offset: u32,
    /// the line that offset belongs to, which is what a client is told
    pub(crate) line: u32,
}

/// every offset that runs nothing before returning and sits at stack depth zero
///
/// one per return at most: stepping back from a return over [`EXITING`]
/// instructions, whose stack effects are all pushes or nothing, the depth falls
/// monotonically, so there is a single point at which it reaches zero
fn depth_zero_tails(python: Python<'_>, read: &[Instruction]) -> PyResult<Vec<u32>> {
    let dis = python.import("dis")?;
    let mut found = Vec::new();
    for (index, one) in read.iter().enumerate() {
        if !RETURNING.contains(&one.opname.as_str()) {
            continue;
        }
        // what the return itself takes off the stack. `RETURN_CONST` carries its
        // own value and takes none, so it **is** the depth-zero offset
        let mut depth: i32 = match one.opname.as_str() {
            "RETURN_CONST" => {
                found.push(one.offset);
                continue;
            }
            _ => 1,
        };
        for before in read[..index].iter().rev() {
            let opname = before.opname.as_str();
            if RETURNING.contains(&opname) || !EXITING.contains(&opname) {
                break;
            }
            let Ok(effect) = dis
                .call_method1("stack_effect", (before.opcode, before.arg))
                .and_then(|effect| effect.extract::<i32>())
            else {
                // an opcode the interpreter will not answer for is one nothing
                // here can count, and a tail nobody counted is not offered
                break;
            };
            depth -= effect;
            if depth == 0 {
                found.push(before.offset);
                break;
            }
            if depth < 0 {
                break;
            }
        }
    }
    Ok(found)
}

/// what running on from an offset does before it returns
enum Walk {
    /// it reaches a return having run nothing of the program
    Reaches,
    /// it runs something of the program first
    Runs,
    /// it reads a name the frame holds nothing for, which raises
    Raises { name: String },
    /// it reads or writes through a mapping that is not a plain dict
    ThroughANamespace {
        access: Access,
        through: Through,
        namespace: String,
    },
}

/// walk on from `from` and say what happens before a return
///
/// straight-line, and that is sound rather than an approximation: no instruction
/// on [`EXITING`] branches, so the first one that is not on it ends the walk
/// either way
fn walk_to_a_return(read: &[Instruction], from: u32, namespaces: &Namespaces) -> Walk {
    for one in read.iter().filter(|one| one.offset >= from) {
        if RETURNING.contains(&one.opname.as_str()) {
            return Walk::Reaches;
        }
        if !EXITING.contains(&one.opname.as_str()) {
            return Walk::Runs;
        }
        // a read the frame holds nothing for raises, which is not "running
        // nothing" — it is injecting an exception into the program on the way
        // out. named, so that a function whose every return reads one is told
        // which name rather than that it has no return
        if let Some(name) = namespaces.raises(one, Moved::First) {
            return Walk::Raises { name };
        }
        if let Some((access, through, namespace)) = namespaces.refuses(one) {
            return Walk::ThroughANamespace {
                access,
                through,
                namespace: namespace.to_string(),
            };
        }
    }
    Walk::Runs
}

/// what a caller's line does besides making the call, and whether that is safe
pub(crate) struct CallLine {
    /// the line, in the interpreter's own numbering
    pub(crate) line: u32,
    /// the offset the span was read from, which a rewind is checked against
    pub(crate) from: u32,
    /// the names on it that the forced return's value is written into
    pub(crate) disturbed: Vec<String>,
}
/// what the span does that depends on the **frame** rather than on the opcode
///
/// every instruction here is on [`BESIDE_THE_CALL`], so the opcode alone says it
/// is safe — and for four of them that is not the whole question. a
/// `LOAD_GLOBAL` whose globals is a dict subclass runs the program's
/// `__missing__`; a `STORE_NAME` into a prepared class body runs its
/// `__setitem__`; and a read the frame holds nothing for raises rather than
/// running anything at all
///
/// [`Moved::NotYet`], because this is the caller's tail: it runs before anything
/// moves the caller, so nothing has bound its unbound locals
fn frame_dependent(
    span: &[&Instruction],
    line: u32,
    namespaces: &Namespaces,
) -> Option<Unrestartable> {
    if let Some((access, through, namespace)) = span.iter().find_map(|one| namespaces.refuses(one))
    {
        return Some(Unrestartable::NamespaceIsNotADict {
            whose: Whose::TheCaller,
            access,
            through,
            line,
            namespace: namespace.to_string(),
        });
    }
    // **in order, carrying what the span has already bound.** a read only raises
    // for a name the frame holds nothing for, and the span itself can bind one
    // first: `kept = f(2); copy = kept` in a module body stores `kept` and then
    // reads it back, and at analysis time — before anything moves — `kept` is
    // unbound, so asking the frame alone answered "reading it would raise" for a
    // read that cannot. a false refusal, and one whose reason was not true of
    // the line it named
    let mut bound: Vec<&str> = Vec::new();
    for one in span {
        if let Some(name) = namespaces.raises(one, Moved::NotYet)
            && !bound.iter().any(|had| *had == name)
        {
            return Some(Unrestartable::ExitWouldRaise {
                whose: Whose::TheCaller,
                line,
                name,
            });
        }
        bound.extend(one.names.iter().take(writes(one)).map(String::as_str));
    }
    None
}

/// the first name the tail writes where the **callee** could read it
///
/// everything else in this analysis reasons about names on the caller's line. a
/// global, a cell, or a module body's namespace is read by code it never looks
/// at, so there is nothing to reason with and it refuses instead
fn shared_with_the_callee(
    span: &[&Instruction],
    lasti: u32,
    namespaces: &Namespaces,
) -> Option<String> {
    span.iter()
        .filter(|one| {
            one.offset > lasti
                && (SHARED_WITH_THE_CALLEE.contains(&one.opname.as_str())
                    // a module body's namespace **is** the global namespace, so
                    // its ordinary assignment writes where the callee reads
                    || (namespaces.locals_are_globals && one.opname == "STORE_NAME"))
        })
        .flat_map(|one| one.names.iter().take(writes(one)))
        .next()
        .cloned()
}

/// the first name the call reads before it that the tail writes after it
///
/// `.skip(writes(one))` because a fused instruction's names are not all reads,
/// the same way they are not all writes: `STORE_FAST_LOAD_FAST ('va', 'other')`
/// **writes** `va` and reads `other`, and counting the write half as a read
/// refused a sound restart
///
/// **`disturbed` first, and only then the rest of what the tail binds.** both
/// are refusals and they are not equally informative: a name in `disturbed`
/// definitely takes bpd's forced return, while one only in `written` takes
/// whatever the line itself computed. taking the first match in offset order
/// pointed `x, y = x, f(y)` at `x` — whose read is harmless — when `y` is the
/// name the invented value lands in
fn read_then_written(
    span: &[&Instruction],
    lasti: u32,
    disturbed: &[String],
    written: &[String],
) -> Option<String> {
    let read_before: Vec<&String> = span
        .iter()
        .filter(|one| one.offset < lasti && LOADING.contains(&one.opname.as_str()))
        .flat_map(|one| one.names.iter().skip(writes(one)))
        .collect();
    let found = |bound: &[String]| {
        read_before
            .iter()
            .find(|read| bound.iter().any(|had| &had == *read))
            .map(|read| (*read).clone())
    };
    found(disturbed).or_else(|| found(written))
}

/// read the caller's call, and say whether re-running it is safe
///
/// **the unit is the instruction span from the jump destination to the end of
/// the run holding the call.** not one `co_lines` range and not every range of
/// the line — both are the wrong set, in opposite directions:
///
/// - one **range** misses the other branch of a conditional expression, whose
///   ranges each look like a single clean call on their own
/// - every **range of the line** over-includes the second copy cpython makes of
///   a `finally` body, which counted one call as two; and it under-includes a
///   call split over source lines, whose argument is attributed to the
///   *argument's* line rather than to the call's — so the `LOAD_ATTR` of
///   `got = f(\n    obj.attr\n)` was not seen at all
///
/// the span is what the interpreter really executes: from where the rewind puts
/// the pointer, through the call, to the end of the contiguous run the call is
/// in. past that end is either a new line — which is where the rewind is made
/// from — or a range carrying **no** line at all. `scripts/restart_shapes.py`
/// counts those over the call sites this rule permits, and prints the **whole**
/// run rather than its first opcode — 10 of 7401 on 3.13 and 8 of 7520 on 3.14:
///
/// | run | 3.13 | 3.14 |
/// | --- | --- | --- |
/// | `JUMP_FORWARD` | 8 | 8 |
/// | `EXTENDED_ARG JUMP_FORWARD` | 1 | 0 |
/// | `JUMP_FORWARD PUSH_EXC_INFO` | 1 | 0 |
///
/// none of them runs anything of the program: the `EXTENDED_ARG` prefixes the
/// jump rather than an instruction, and the `PUSH_EXC_INFO` sits behind one, so
/// falling through never reaches it. the opener alone would not have shown
/// either, which is why the script prints the run
///
/// two earlier versions of this note gave figures that were wrong — one set
/// inherited rather than derived, one set derived with a mislabelled
/// denominator. the script exists because a number here that nobody can re-run
/// is the same kind of claim this feature spent seven rounds removing
///
/// it covers both halves of the danger in one bound. before the call is what
/// **re-executes**; after it is what runs with the value of a return the program
/// never made, and then again after the restart
pub(crate) fn call_line(
    code: &Bound<'_, PyAny>,
    lasti: u32,
    namespaces: &Namespaces,
) -> PyResult<Result<CallLine, Unrestartable>> {
    let read = instructions(code)?;
    let spans = spans(code)?;

    let Some(holding) = spans
        .iter()
        .find(|span| lasti >= span.start && lasti < span.end)
    else {
        // a caller stopped at an instruction its own line table does not cover
        // is not a caller anything can be said about, and guessing a nearby line
        // would be guessing which statement runs again
        return Ok(Err(Unrestartable::CallerHasNoLine { lasti }));
    };
    let line = holding.line;
    let Some(from) = destination(&spans, line) else {
        unreachable!("`lasti` is inside a range of this line, so the line has one")
    };
    // `from` is the smallest start of the line and `lasti` is inside one of its
    // ranges, so the span always contains the call
    let to = holding.end;

    let span: Vec<&Instruction> = read
        .iter()
        .filter(|one| one.offset >= from && one.offset < to)
        .collect();

    // **before** anything is counted. a frame is not always entered from a call:
    // measured on 3.13 and 3.14, restarting the frame of a property reached by
    // `got = sink(obj.attr)` has the caller stopped at `LOAD_ATTR`, and a
    // comprehension over a custom iterator has it at `FOR_ITER`. this used to be
    // an assertion, which made an ordinary shape a panic in the debuggee
    let Some(stopped_in) = span.iter().find(|one| one.offset == lasti) else {
        unreachable!("the span runs to the end of the range holding `lasti`")
    };
    if !CALLING.contains(&stopped_in.opname.as_str()) {
        return Ok(Err(Unrestartable::NotEnteredByACall {
            line,
            opcode: stopped_in.opname.clone(),
        }));
    }

    // **before** the return check, because it explains a return the return check
    // would misreport. cpython duplicates a `finally` body, so a line inside one
    // has two copies — and when the caller is stopped in the second, the span
    // starts in the first and swallows both. what gives it away is a call or a
    // return of the *first* copy sitting before the caller's own position:
    // control cannot have flowed from there to here
    //
    // reporting that as `NothingRunsAfterTheCall` told a caller which already
    // binds its result to a name to bind its result to a name
    let copies = spans.iter().filter(|span| span.line == line).count();
    let crossed = span.iter().any(|one| {
        one.offset < lasti
            && (CALLING.contains(&one.opname.as_str()) || RETURNING.contains(&one.opname.as_str()))
    });
    if copies > 1 && holding.start != from && crossed {
        return Ok(Err(Unrestartable::CopiedLine {
            line,
            runs: u32::try_from(copies).expect("a line is not four billion runs"),
        }));
    }

    // about where the caller can be moved **from** rather than about what
    // re-running the span does, so it is its own reason. only after the call:
    // one before it is the copy boundary above
    if span
        .iter()
        .any(|one| one.offset > lasti && RETURNING.contains(&one.opname.as_str()))
    {
        return Ok(Err(Unrestartable::NothingRunsAfterTheCall { line }));
    }

    let calls = span
        .iter()
        .filter(|one| CALLING.contains(&one.opname.as_str()))
        .count();
    if calls != 1 {
        return Ok(Err(Unrestartable::MoreThanOneCall {
            line,
            calls: u32::try_from(calls).expect("a span is not four billion instructions"),
        }));
    }
    if stopped_in.opname == UNPACKING {
        return Ok(Err(Unrestartable::SomethingElseOnTheLine {
            line,
            opcode: UNPACKING.to_string(),
        }));
    }

    if let Some(other) = span
        .iter()
        .find(|one| !BESIDE_THE_CALL.contains(&one.opname.as_str()) && one.offset != lasti)
    {
        return Ok(Err(Unrestartable::SomethingElseOnTheLine {
            line,
            opcode: other.opname.clone(),
        }));
    }

    if let Some(refusal) = frame_dependent(&span, line, namespaces) {
        return Ok(Err(refusal));
    }

    // what the tail writes where the **callee** can see it. everything else here
    // reasons about names on this line; a global or a cell is read by code this
    // analysis never looks at, so there is nothing to reason with
    if let Some(name) = shared_with_the_callee(&span, lasti, namespaces) {
        return Ok(Err(Unrestartable::TailWritesSharedState { line, name }));
    }

    let Some(Written { disturbed, written }) = written_after_the_call(&span, lasti) else {
        return Ok(Err(Unrestartable::SpanNotUnderstood { line }));
    };
    // the store lands **before** the rewind, so a name the span stores is a name
    // the re-executed span reads back — `x = f(x)` would call `f` with the value
    // bpd invented rather than with the program's. refused for the reason
    // `f(*args)` is: the second call would not be the call that was restarted
    //
    // `.skip(writes(one))` because a fused instruction's names are not all
    // reads, the same way they are not all writes: `STORE_FAST_LOAD_FAST
    // ('va', 'other')` **writes** `va` and reads `other`. counting the write
    // half as a read met it against the stored names and refused a sound
    // restart, telling the user their call reads a name the line never reads
    if let Some(name) = read_then_written(&span, lasti, &disturbed, &written) {
        return Ok(Err(Unrestartable::TheCallReadsWhatItStores { line, name }));
    }

    Ok(Ok(CallLine {
        line,
        from,
        disturbed,
    }))
}
/// whether a name is one the call's value is sitting in right now
///
/// `None` — an instruction with no name, a constant or a `PUSH_NULL` — never
/// holds it
fn holds(disturbed: &[String], name: Option<&String>) -> bool {
    name.is_some_and(|name| disturbed.iter().any(|had| had == name))
}

/// the names the forced return's value is written into
///
/// only the stores **after** the call take it. one before already ran with what
/// the program computed, and naming it would be reporting a disturbance that did
/// not happen — a permitted span cannot branch, no jump being on the allow list,
/// so "after" by offset really is after in time
fn written_after_the_call(span: &[&Instruction], lasti: u32) -> Option<Written> {
    // the call left exactly one value on the stack, and this walks the rest of
    // the span to see which stores consume **it** rather than something else.
    // naming every store after the call was near enough for `x = f()` and wrong
    // for two ordinary shapes: `a = f(); b = spare` named `b`, which takes
    // `spare`, and `a, b = f(), spare` stores in the opposite order to the one
    // it is written in, so the name that gets the call's value is not the first
    //
    // a permitted span cannot branch — no jump is on the allow list — so one
    // pass in offset order is the execution order, and this is a simulation of
    // a straight line rather than an analysis
    // the call's value, and **nothing below it**. what the caller pushed before
    // the call is not modelled, and does not need to be: none of it came from
    // the call, so a pop that reaches past this floor consumes a value the
    // program computed itself. that is an answer rather than a gap — seeding one
    // slot and treating a pop past it as unmodellable refused `first, second =
    // spare, f(1)` and `x = [spare, f(2)]`, which are ordinary lines
    let mut stack = vec![true];
    // the names that hold the call's value **right now**, in the order they
    // first took it. a name rather than a slot, because the value moves between
    // the two: `a = f(1); c = a` stores it into `a` and then reads `a` back, and
    // modelling every load as untainted lost it there — `c` held bpd's forced
    // `None` and was not named, and `held = f(seed); seed = held` fed it back to
    // the restarted call as its own argument while bpd answered `Arranged`
    //
    // a bare name, and what makes that safe is the **allow list** rather than
    // any property of names. one code object really can reach the same bare name
    // through two storage classes — PEP 709 inlines a comprehension into its
    // enclosing frame, so `z = [(w := i) for i in range(3)]` at module level
    // gives `<module>` both `STORE_GLOBAL w` and `STORE_FAST w`, and
    // `def f(): global g; g = 1; return [g for g in range(3)]` does the same to
    // `f`. an earlier version of this comment claimed the compiler decides one
    // scope per name and that is simply false
    //
    // every one of those shapes carries `LOAD_FAST_AND_CLEAR`, and `FOR_ITER` or
    // `GET_ITER`, none of which is on `BESIDE_THE_CALL` — so the span is refused
    // before this walk runs. swept over both stdlibs: 0 permitted call sites
    // reach one bare name through two storage classes
    let mut disturbed: Vec<String> = Vec::new();
    // **every** name the tail binds, whatever it binds it to. `disturbed` is
    // the subset holding bpd's own forced return, and that subset is not what
    // decides whether the call can be made again: the tail lands before the
    // rewind either way, so a name it writes with the *program's* value is read
    // back by the re-executed line just the same
    let mut written: Vec<String> = Vec::new();
    // a pop that reaches past the floor yields an untainted value
    macro_rules! pop {
        () => {
            stack.pop().unwrap_or(false)
        };
    }
    // a store either gives a name the call's value or takes it away again —
    // `slot = f(1); slot = other` ends the line holding the program's own value,
    // and naming it would tell a client to distrust a slot it can trust
    macro_rules! bind {
        ($name:expr, $held:expr) => {
            if $held {
                if !disturbed.iter().any(|had| had == $name) {
                    disturbed.push($name.clone());
                }
            } else {
                disturbed.retain(|had| had != $name);
            }
        };
    }
    for one in span.iter().filter(|one| one.offset > lasti) {
        let arg = one.arg.unwrap_or(0) as usize;
        let taken = writes(one);
        if taken > 0 {
            assert!(
                one.names.len() >= taken,
                "`{}` writes {taken} of its names and `dis` gave it {}",
                one.opname,
                one.names.len()
            );
            for name in one.names.iter().take(taken) {
                let held = pop!();
                bind!(name, held);
                if !written.iter().any(|had| had == name) {
                    written.push(name.clone());
                }
            }
            // the load half of a fused store-then-load reads a name, and that
            // name may be one the store half just gave the call's value to —
            // `a = f(1); c = a` fuses into exactly that
            if one.opname == "STORE_FAST_LOAD_FAST" {
                let read = one.names.get(1);
                let held = read.is_some_and(|name| disturbed.iter().any(|had| had == name));
                stack.push(held);
            }
            continue;
        }
        match one.opname.as_str() {
            "POP_TOP" => {
                let _: bool = pop!();
            }
            "COPY" => {
                // `COPY n` duplicates the item `n` down, which is how a chained
                // assignment gives the same value to every name on the line. one
                // below the floor is the caller's own, so untainted
                let held = stack
                    .len()
                    .checked_sub(arg)
                    .and_then(|at| stack.get(at))
                    .copied()
                    .unwrap_or(false);
                stack.push(held);
            }
            "SWAP" => {
                // the one shape that really cannot be modelled: swapping the
                // call's value **below** the floor puts it somewhere this cannot
                // follow, and a later store could still reach it. swapping two
                // untainted values is a no-op for this walk, so it only gives up
                // when it has to. swept over both stdlibs, no permitted call
                // site has a `SWAP` after the call at all
                let top = stack.len().checked_sub(1)?;
                match stack.len().checked_sub(arg) {
                    Some(other) => stack.swap(top, other),
                    None if stack[top] => return None,
                    None => {}
                }
            }
            "BUILD_TUPLE" | "BUILD_LIST" => {
                // the container **is** reachable by a name — the store right
                // after it — so it carries the taint of anything that went into
                // it. modelling it as destroying the value answered `Arranged`
                // with nothing disturbed for `box = [f(), spare]`, while `box`
                // held a list built out of bpd's own forced `None`
                let mut holds = false;
                for _ in 0..arg {
                    holds |= pop!();
                }
                stack.push(holds);
            }
            "LOAD_GLOBAL" => {
                // the low bit of the oparg says it pushes a `NULL` under what it
                // loads. modelling it as one push would put every later `COPY`
                // and `SWAP` one slot out
                //
                // cpython sets that bit when the global is about to be **called**
                // — so in a permitted span it never follows the call, because a
                // second call is refused before this walk runs. swept over both
                // stdlibs: 0 of 7401 permitted sites on 3.13 and 0 of 7520 on
                // 3.14. kept because an allow list that names one case too many
                // fails closed, and pinned by `the_walk_counts_what_each_opcode
                // _leaves_on_the_stack` rather than by a shape no interpreter
                // emits
                if arg & 1 == 1 {
                    stack.push(false);
                }
                let held = holds(&disturbed, one.names.first());
                stack.push(held);
            }
            "LOAD_FAST_LOAD_FAST" | "LOAD_FAST_BORROW_LOAD_FAST_BORROW" => {
                stack.push(holds(&disturbed, one.names.first()));
                stack.push(holds(&disturbed, one.names.get(1)));
            }
            "NOP" | "NOT_TAKEN" | "RESUME" | "CACHE" | "EXTENDED_ARG" => {}
            // every remaining opcode on the allow list pushes one value. a load
            // pushes what its name holds, which is how the call's value is
            // followed when it is read back out of a name it was stored into.
            // `PUSH_NULL` and the constants name nothing and push nothing of the
            // call's, which `holds` answers for free
            _ => {
                let held = holds(&disturbed, one.names.first());
                stack.push(held);
            }
        }
    }
    Some(Written { disturbed, written })
}

/// what the tail of a call line binds, split by **whose value** it binds
///
/// two questions that look like one and are not. `disturbed` answers "which of
/// these hold something bpd invented", which is what a client is told; `written`
/// answers "which of these the re-executed line would read back", which is what
/// decides whether the call can be made a second time at all
struct Written {
    /// the names holding the forced return's value when the line ends
    disturbed: Vec<String>,
    /// every name the tail binds, in the order it first binds them
    written: Vec<String>,
}

/// the bit of `co_flags` saying the code object takes `*args`
const CO_VARARGS: u32 = 0x04;
/// the bit of `co_flags` saying it takes `**kwargs`
const CO_VARKEYWORDS: u32 = 0x08;

/// how many of a code object's `localsplus` slots are its parameters
///
/// they come first and in this order — positional, keyword-only, `*args`,
/// `**kwargs` — which is what lets a slot index alone say whether a fresh call
/// would have bound it
pub(crate) fn parameter_slots(code: &Bound<'_, PyAny>) -> PyResult<usize> {
    let flags: u32 = code.getattr("co_flags")?.extract()?;
    let positional: usize = code.getattr("co_argcount")?.extract()?;
    let keyword_only: usize = code.getattr("co_kwonlyargcount")?.extract()?;
    Ok(positional
        + keyword_only
        + usize::from(flags & CO_VARARGS != 0)
        + usize::from(flags & CO_VARKEYWORDS != 0))
}

/// the parameter this code object writes over, if it writes over one
///
/// a frame reset in place keeps its parameter slots exactly as they are. the
/// arguments are the one thing about a call that cannot be worked out again:
/// the caller's `CALL` **moved** its operands into these slots, so the caller's
/// stack no longer holds them and there is nowhere else they survive
///
/// that is sound only while the frame has not written over them itself. `def
/// f(n): n = n - 1` restarted from below the store would be called with bpd's
/// value rather than the program's, which is the same objection that refuses
/// `x = f(x)` on the rewinding path
///
/// **static, and over the whole code object rather than over what has run.** a
/// frame stopped before its own store would pass a test of what has already
/// happened and then run the store on the second pass, which is the restart
/// getting it wrong one step later
pub(crate) fn rebinds_a_parameter(code: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    let taken = parameter_slots(code)?;
    let parameters: Vec<String> = code
        .getattr("co_varnames")?
        .try_iter()?
        .take(taken)
        .map(|name| name?.extract())
        .collect::<PyResult<_>>()?;
    for one in instructions(code)? {
        // `DELETE_FAST` unbinds rather than rebinds, and loses the argument just
        // as completely
        let written = if one.opname == "DELETE_FAST" {
            one.names.len()
        } else {
            writes(&one)
        };
        if let Some(name) = one
            .names
            .iter()
            .take(written)
            .find(|name| parameters.iter().any(|parameter| parameter == *name))
        {
            return Ok(Some(name.clone()));
        }
    }
    Ok(None)
}

/// the offset a jump to the top of a code object lands at
///
/// **not zero.** the prologue cpython puts before `RESUME` — `MAKE_CELL` for
/// every cell variable, `COPY_FREE_VARS` for a closure — is in a `co_lines`
/// range carrying **no line at all**, measured on 3.13, 3.14 and 3.15 as
/// `(0, 2, None)`. `f_lineno` chooses among marks and a range with no line has
/// none, so offset 0 is not somewhere a frame can be sent
///
/// what it lands on instead is the first range that does carry a line, which is
/// the `RESUME`. that is why a code object with cell variables cannot be reset:
/// its `MAKE_CELL` is on the far side of a door there is no handle on
pub(crate) fn top_offset(code: &Bound<'_, PyAny>) -> PyResult<Option<u32>> {
    Ok(spans(code)?.iter().map(|span| span.start).min())
}

#[cfg(test)]
mod tests {
    use super::{
        BESIDE_THE_CALL, EXITING, Instruction, THROUGH_GLOBALS, THROUGH_LOCALS, WRITES_A_NAME,
        written_after_the_call,
    };

    /// nothing at a frame's own exit writes through a namespace
    ///
    /// what makes `(Whose::TheFrame, Access::Writes)` unreachable, and therefore
    /// what `bpd_core::Access` points at rather than leaving a hole in the table
    /// of rendered refusals. if a store is ever allowed at an exit, that
    /// combination becomes reachable and needs a sentence of its own
    #[test]
    fn an_exit_line_holds_nothing_that_writes_a_name() {
        // `WRITES_A_NAME` rather than `STORING`, which is what decides it:
        // `STORING` is a superset holding `STORE_FAST`, `STORE_DEREF` and
        // `STORE_GLOBAL`, none of which goes through a namespace mapping — so
        // the old form's message was false of three of the opcodes it named,
        // and a new namespace-writing opcode added to `WRITES_A_NAME` and
        // `EXITING` but not to `STORING` would have slipped past it
        for one in EXITING {
            assert!(
                !WRITES_A_NAME.contains(one),
                "`{one}` writes through the frame's namespace mapping and is \
                 allowed at an exit, so a frame can now be refused with \
                 `(TheFrame, Writes)` — a combination `bpd_core::jump` records \
                 as unreachable"
            );
        }
    }

    /// the walk's arithmetic, on spans real bytecode does not produce
    ///
    /// two arms of [`written_after_the_call`] cannot be reached from a python
    /// shape: a `LOAD_GLOBAL` that pushes a `NULL` is one about to be called,
    /// and a second call is refused before the walk runs; and no permitted call
    /// site in either stdlib has a `SWAP` after the call. both were measured, 0
    /// of 7401 sites on 3.13 and 0 of 7520 on 3.14
    ///
    /// so they are pinned here instead. this asserts what the **walk** does with
    /// a stack, not what cpython emits — every claim about what cpython emits is
    /// in `crates/bpd_engine/tests/restarts.rs`, against a real interpreter
    #[test]
    fn the_walk_counts_what_each_opcode_leaves_on_the_stack() {
        let one = |offset: u32, opname: &str, names: &[&str], arg: Option<u32>| Instruction {
            offset,
            opname: opname.to_string(),
            // this walk is by name, and nothing in it asks a stack-effect
            // question — the number is only what `dis.stack_effect` is keyed by
            opcode: 0,
            names: names.iter().map(|name| (*name).to_string()).collect(),
            arg,
        };

        // a `LOAD_GLOBAL` pushing `NULL` leaves **two**, so the `COPY 3` after it
        // reaches the call's value. counting one would copy the `NULL` instead
        // and name nothing
        let null_push = [
            one(0, "CALL", &[], Some(0)),
            one(2, "LOAD_GLOBAL", &["helper"], Some(1)),
            one(4, "COPY", &[], Some(3)),
            one(6, "STORE_FAST", &["got"], Some(0)),
        ];
        let span: Vec<&Instruction> = null_push.iter().collect();
        assert_eq!(
            written_after_the_call(&span, 0).map(|walked| walked.disturbed),
            Some(vec!["got".to_string()]),
            "the `NULL` is a slot, and a later `COPY` counts it"
        );

        // a fused load leaves **two**. counting one would put the `COPY 3` a slot
        // out and name nothing
        let fused = [
            one(0, "CALL", &[], Some(0)),
            one(2, "LOAD_FAST_LOAD_FAST", &["a", "b"], Some(0)),
            one(4, "COPY", &[], Some(3)),
            one(6, "STORE_FAST", &["got"], Some(0)),
        ];
        let span: Vec<&Instruction> = fused.iter().collect();
        assert_eq!(
            written_after_the_call(&span, 0).map(|walked| walked.disturbed),
            Some(vec!["got".to_string()])
        );

        // and a `SWAP` that would put the call's value below the floor is the one
        // thing this gives up on, rather than losing track of it quietly
        let swapped = [
            one(0, "CALL", &[], Some(0)),
            one(2, "SWAP", &[], Some(3)),
            one(4, "STORE_FAST", &["got"], Some(0)),
        ];
        let span: Vec<&Instruction> = swapped.iter().collect();
        assert!(
            written_after_the_call(&span, 0).is_none(),
            "the value goes somewhere this cannot follow, so it refuses"
        );

        // a `COPY` reaching past the floor copies one of the caller's own
        // values, which the call did not produce. answering `true` there would
        // name a slot holding what the program computed — the direction that
        // fails **open**, and the one arm of this walk a python shape cannot
        // reach either way
        let deep_copy = [
            one(0, "CALL", &[], Some(0)),
            one(2, "COPY", &[], Some(4)),
            one(4, "STORE_FAST", &["got"], Some(0)),
        ];
        let span: Vec<&Instruction> = deep_copy.iter().collect();
        let walked = written_after_the_call(&span, 0).expect("the span is modelled");
        assert_eq!(
            walked.disturbed,
            Vec::<String>::new(),
            "below the floor is the program's own, so nothing is disturbed"
        );
        assert_eq!(
            walked.written,
            vec!["got".to_string()],
            "but the tail did bind it, which is what decides the call can be \
             made again"
        );

        // and the name a store takes it away from stops being disturbed: the
        // line ends holding the program's value, so naming it would tell a
        // client to distrust a slot it can trust
        let overwritten = [
            one(0, "CALL", &[], Some(0)),
            one(2, "STORE_FAST", &["slot"], Some(0)),
            one(4, "LOAD_CONST", &[], Some(0)),
            one(6, "STORE_FAST", &["slot"], Some(0)),
        ];
        let span: Vec<&Instruction> = overwritten.iter().collect();
        let walked = written_after_the_call(&span, 0).expect("the span is modelled");
        assert_eq!(
            walked.disturbed,
            Vec::<String>::new(),
            "the second store takes it back off"
        );
        assert_eq!(
            walked.written,
            vec!["slot".to_string()],
            "and the name is still one the tail binds, named once"
        );
    }

    /// the other half of why `Access::Writes` is unreachable from a caller
    ///
    /// a call needs its callee on the stack, and in a frame whose locals are a
    /// namespace mapping the only allow-listed way to get it there is
    /// `LOAD_NAME` — which is on [`THROUGH_LOCALS`] too, so the span always
    /// meets the read before the store. the one shape where `STORE_NAME` comes
    /// first is a literal callee, whose span carries `MAKE_FUNCTION`
    #[test]
    fn a_call_line_cannot_reach_its_callee_without_reading_a_name() {
        // every allow-listed load that could put a callee on the stack in a
        // frame with a namespace mapping. `LOAD_FAST*` needs an optimized frame,
        // which such a frame is not, and `LOAD_DEREF` reaches a cell rather than
        // the mapping — neither can name something the mapping holds
        let reaches_the_mapping: Vec<&&str> = BESIDE_THE_CALL
            .iter()
            .filter(|one| THROUGH_LOCALS.contains(*one) || THROUGH_GLOBALS.contains(*one))
            .collect();
        assert_eq!(
            reaches_the_mapping,
            vec![&"LOAD_GLOBAL", &"LOAD_NAME", &"STORE_NAME"],
            "the set that decides read-before-write on a call line changed"
        );
        // and the one opcode that would let a call line skip the read entirely
        assert!(
            !BESIDE_THE_CALL.contains(&"MAKE_FUNCTION"),
            "a literal callee puts `STORE_NAME` first in the span, so allowing \
             `MAKE_FUNCTION` beside a call makes `(TheCaller, Writes)` reachable \
             and `bpd_core::jump`'s note about it false"
        );
    }
}
