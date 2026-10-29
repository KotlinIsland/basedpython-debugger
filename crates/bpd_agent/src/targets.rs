//! the calls a held line is still to make, for a step into one of them
//!
//! read off the frame's own bytecode: every `CALL`, `CALL_KW` and
//! `CALL_FUNCTION_EX` of the line, at or after the instruction the frame is
//! at. each is named by what it calls — `sum`, `self.area`, `json.dumps` — and
//! that name is the source's own text, taken from the call's position: cpython
//! gives a call instruction the span of the whole call expression, and what is
//! called is that span up to its argument list
//!
//! ## the text is proved
//!
//! a label is the source's text at the call's columns, so the source is held to
//! the proof a frame's source is held to — compiled, and the frame's own code
//! object found in what comes out; a `.by` checked against its digest too. a
//! file edited since it was compiled has other text at those columns, and that
//! is refused by name rather than offered as a label
//!
//! ## a module `by run` compiled as its `.by`
//!
//! since `by` a110d7d8 the runner gives each location the author's own `.by`
//! columns — the text they wrote, with a qualifier like `_by_lazy.` in front of
//! a name left out — and a location of code the transpiler wrote the `.by` line
//! with no columns. so the frame's positions are read as they are, the label is
//! the `.by`'s own text, and a call with no columns is the transpiler's and is
//! not offered: `_soundness_iter` around a comprehension's iterable is no call
//! the author can see on the line
//!
//! a build from a `by` before columns — a map with no `COLUMNS` — gave every
//! location the **whole** of its `.by` line, so there the generated python is
//! compiled again and the code object with the same name and the same bytecode
//! is the one whose positions are read. the label is the generated python's
//! text for the call, and the target carries no columns, because the frame the
//! client sees has none

use bpd_core::{StepInTarget, Unverified};
use pyo3::prelude::*;

use crate::source::Proved;

/// the instructions that call
const CALLING: &[&str] = &["CALL", "CALL_KW", "CALL_FUNCTION_EX"];

/// the calls the frame's line is still to make, or why they cannot be named
pub(crate) fn of(
    python: Python<'_>,
    frame: &Bound<'_, PyAny>,
) -> PyResult<Result<Vec<StepInTarget>, Unverified>> {
    let code = frame.getattr("f_code")?;
    let file: String = code.getattr("co_filename")?.extract()?;
    let lasti: u32 = frame
        .getattr("f_lasti")?
        .extract::<i64>()?
        .try_into()
        .unwrap_or(0);
    let line = crate::frames::line_now(frame)?;
    if line == 0 {
        // no line to have calls on, which is the same answer a frame of
        // `by`'s transpiler-written code has for every other question
        return Ok(Ok(Vec::new()));
    }

    let dis = python.import("dis")?;
    let mut calls = Vec::new();
    for instruction in dis.call_method1("get_instructions", (&code,))?.try_iter()? {
        let instruction = instruction?;
        let opname: String = instruction.getattr("opname")?.extract()?;
        let offset: u32 = instruction.getattr("offset")?.extract()?;
        let on: Option<u32> = instruction
            .getattr("positions")?
            .getattr("lineno")?
            .extract()?;
        if CALLING.contains(&opname.as_str()) && offset >= lasti && on == Some(line) {
            calls.push(offset);
        }
    }
    if calls.is_empty() {
        return Ok(Ok(Vec::new()));
    }

    let (spanned, columns_too, only_the_authors) = match crate::source::proved(python, &code)? {
        Err(why) => return Ok(Err(why)),
        // a `.by` whose runner read the map's `COLUMNS`: the code's own
        // positions are the `.by`'s, columns and all, for text the author
        // wrote — and a call the transpiler wrote, `_soundness_iter` around a
        // comprehension's iterable, has none. that call is not one the author
        // can see on the line, and stepping into it is stepping into `by`'s
        // runtime, so it is not offered
        Ok(Proved::Own(bytes)) => {
            let staged = crate::sources::staged_as(&file).is_some();
            (spans(&code, &bytes)?, true, staged)
        }
        // a `.by` from a `by` before columns: every location is the whole line
        Ok(Proved::Generated(bytes)) => {
            (generated_spans(python, &code, &file, &bytes)?, false, false)
        }
    };

    let reported = crate::sources::locate(file, line).line;
    let mut targets = Vec::with_capacity(calls.len());
    for offset in calls {
        let found = spanned.iter().find(|call| call.offset == offset);
        if only_the_authors && !found.is_some_and(|call| call.positioned) {
            continue;
        }
        let (label, columns) = match found.and_then(|call| Some((call, call.text.as_ref()?))) {
            Some((call, text)) => (callee(text), call.columns.filter(|_| columns_too)),
            // the call is in the frame and the proved text has none for it — a
            // position the compiler did not give, or a file in an encoding that
            // is not utf-8. it is still a call, named by where it is rather
            // than by a guess at what it calls
            None => (format!("the call at offset {offset}"), None),
        };
        targets.push(StepInTarget {
            offset,
            label,
            line: reported,
            columns,
        });
    }
    Ok(Ok(targets))
}

/// one call instruction, and the source text its position spans
struct Spanned {
    offset: u32,
    /// whether the instruction has a position with columns at all
    positioned: bool,
    /// the text the position spans, when it is utf-8
    text: Option<String>,
    /// where on its line the call begins and ends, in utf-16 code units
    /// counting from one — DAP's unit for a column — when it is on one line
    columns: Option<(u32, u32)>,
}

/// the calls of the generated python behind a `.by` module, at the offsets the
/// frame's code has — see the module's own documentation
///
/// `bytes` is proved: the frame's code object was found in what they compile
/// to. they are compiled again here plainly, the way the import system would,
/// because those positions are the generated python's own, columns and all
fn generated_spans(
    python: Python<'_>,
    code: &Bound<'_, PyAny>,
    file: &str,
    bytes: &[u8],
) -> PyResult<Vec<Spanned>> {
    let generated = crate::sources::staged_as(file)
        .expect("only a staged module's proof hands back its generated python")
        .generated
        .display()
        .to_string();
    let compiled = crate::source::compile(python, bytes, &generated)?;
    let twin = same_code(&compiled, code)?.unwrap_or_else(|| {
        unreachable!(
            "the generated python was proved by finding this code object in what it \
             compiles to, so compiling it again holds the same bytecode"
        )
    });
    spans(&twin, bytes)
}

/// the code object in `tree` with the same name and the same bytecode as `code`
fn same_code<'py>(
    tree: &Bound<'py, PyAny>,
    code: &Bound<'_, PyAny>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    if tree
        .getattr("co_qualname")?
        .eq(code.getattr("co_qualname")?)?
        && tree.getattr("co_code")?.eq(code.getattr("co_code")?)?
    {
        return Ok(Some(tree.clone()));
    }
    let kind = tree.get_type();
    for constant in tree.getattr("co_consts")?.try_iter()? {
        let constant = constant?;
        if constant.is_instance(&kind)?
            && let Some(found) = same_code(&constant, code)?
        {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// every call of `code`, with the text of `source` its position spans
fn spans(code: &Bound<'_, PyAny>, source: &[u8]) -> PyResult<Vec<Spanned>> {
    let lines: Vec<&[u8]> = source.split(|byte| *byte == b'\n').collect();
    let dis = code.py().import("dis")?;
    let mut spanned = Vec::new();
    for instruction in dis.call_method1("get_instructions", (code,))?.try_iter()? {
        let instruction = instruction?;
        let opname: String = instruction.getattr("opname")?.extract()?;
        if !CALLING.contains(&opname.as_str()) {
            continue;
        }
        let offset = instruction.getattr("offset")?.extract()?;
        let positions = instruction.getattr("positions")?;
        let (Some(first), Some(last), Some(column), Some(end_column)) = (
            positions.getattr("lineno")?.extract::<Option<usize>>()?,
            positions
                .getattr("end_lineno")?
                .extract::<Option<usize>>()?,
            positions
                .getattr("col_offset")?
                .extract::<Option<usize>>()?,
            positions
                .getattr("end_col_offset")?
                .extract::<Option<usize>>()?,
        ) else {
            spanned.push(Spanned {
                offset,
                positioned: false,
                text: None,
                columns: None,
            });
            continue;
        };
        // columns are utf-8 byte offsets into the line, which is cpython's own
        // definition of `col_offset`
        let columns = if first == last {
            first
                .checked_sub(1)
                .and_then(|index| lines.get(index))
                .and_then(|line| Some((utf16(line, column)?, utf16(line, end_column)?)))
        } else {
            None
        };
        spanned.push(Spanned {
            offset,
            positioned: true,
            text: slice(&lines, first, last, column, end_column),
            columns,
        });
    }
    Ok(spanned)
}

/// a byte column of a line as utf-16 code units counting from one, or `None`
/// when it does not fall between two characters
fn utf16(line: &[u8], column: usize) -> Option<u32> {
    let before = std::str::from_utf8(line.get(..column)?).ok()?;
    u32::try_from(before.encode_utf16().count() + 1).ok()
}

/// the text between two positions, one-based lines and byte columns, with the
/// lines kept apart by their own line breaks
fn slice(lines: &[&[u8]], first: usize, last: usize, column: usize, end: usize) -> Option<String> {
    let mut taken = Vec::new();
    for at in first..=last {
        let line = lines.get(at.checked_sub(1)?)?;
        let from = if at == first { column } else { 0 };
        let to = if at == last { end } else { line.len() };
        taken.extend_from_slice(line.get(from..to.min(line.len()))?);
        if at != last {
            taken.push(b'\n');
        }
    }
    String::from_utf8(taken).ok()
}

/// what a call expression calls: its text up to the argument list
///
/// the argument list is the bracket the expression ends in. it is found by
/// reading the text the way python's tokenizer does — a bracket inside a string
/// or a comment is not a bracket — and a text that does not end in one, which a
/// call cpython spans should, is named whole rather than cut somewhere arbitrary
fn callee(call: &str) -> String {
    let named = match argument_list(call).map(|open| call[..open].trim()) {
        Some(named) if !named.is_empty() => named,
        _ => call.trim(),
    };
    named.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// where the bracket that closes `call` opens, when `call` ends in `)`
fn argument_list(call: &str) -> Option<usize> {
    let bytes = call.trim_end().as_bytes();
    if bytes.last() != Some(&b')') {
        return None;
    }
    let mut open = Vec::new();
    let mut matched = None;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'\'' | b'"' => {
                at = past_string(bytes, at)?;
                continue;
            }
            b'#' => {
                at = bytes[at..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(bytes.len(), |end| at + end);
                continue;
            }
            b'(' | b'[' | b'{' => open.push(at),
            b')' | b']' | b'}' => matched = Some(open.pop()?),
            _ => {}
        }
        at += 1;
    }
    matched
}

/// the index just past the string literal whose quote is at `quote`
///
/// its prefix is the letters right before the quote. a backslash keeps the next
/// character from ending the string, raw or not, and an `f` or a `t` makes
/// `{…}` an expression of its own, whose brackets and strings are read the same
/// way
fn past_string(bytes: &[u8], quote: usize) -> Option<usize> {
    let prefix_start = bytes[..quote]
        .iter()
        .rposition(|byte| !byte.is_ascii_alphabetic())
        .map_or(0, |at| at + 1);
    let formatted = bytes[prefix_start..quote]
        .iter()
        .any(|byte| matches!(byte.to_ascii_lowercase(), b'f' | b't'));
    let mark = bytes[quote];
    let triple = bytes[quote..].starts_with(&[mark, mark, mark]);
    let width = if triple { 3 } else { 1 };
    let mut at = quote + width;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'\\' {
            at += 2;
            continue;
        }
        if formatted && byte == b'{' {
            if bytes.get(at + 1) == Some(&b'{') {
                at += 2;
                continue;
            }
            at = past_replacement(bytes, at + 1)?;
            continue;
        }
        if byte == mark && (!triple || bytes[at..].starts_with(&[mark, mark, mark])) {
            return Some(at + width);
        }
        at += 1;
    }
    None
}

/// the index just past the `}` that closes an f-string's replacement field
fn past_replacement(bytes: &[u8], from: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut at = from;
    while at < bytes.len() {
        match bytes[at] {
            b'\'' | b'"' => {
                at = past_string(bytes, at)?;
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b'}' if depth == 0 => return Some(at + 1),
            b')' | b']' | b'}' => depth = depth.checked_sub(1)?,
            _ => {}
        }
        at += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{callee, utf16};

    #[test]
    fn a_call_is_named_by_what_it_calls() {
        assert_eq!(callee("sum(areas, 0.0)"), "sum");
        assert_eq!(callee("self.area()"), "self.area");
        assert_eq!(callee("json.dumps({'a': (1, 2)}, default=f)"), "json.dumps");
        assert_eq!(callee("make()(3)"), "make()");
        assert_eq!(callee("table[k](x)"), "table[k]");
    }

    #[test]
    fn a_bracket_inside_a_string_or_a_comment_is_not_a_bracket() {
        assert_eq!(callee(r#"print("(")"#), "print");
        assert_eq!(callee(r#"f(")")"#), "f");
        assert_eq!(callee(r"g(r'\'(', x)"), "g");
        assert_eq!(callee(r#"h(f"{d[')']}", '''(''')"#), "h");
        assert_eq!(callee("k(\n    1,  # (\n    2)"), "k");
        assert_eq!(callee("obj.method\n    (1)"), "obj.method");
    }

    #[test]
    fn a_column_is_counted_in_utf16_code_units_from_one() {
        let line = "naïve = compute(x)".as_bytes();
        let byte = line
            .windows(7)
            .position(|window| window == b"compute")
            .expect("the call is on the line");
        assert_eq!(utf16(line, byte), Some(9));
        assert_eq!(utf16("😀 = f(1)".as_bytes(), "😀 = ".len()), Some(6));
        assert_eq!(utf16("é".as_bytes(), 1), None, "not between two characters");
    }
}
