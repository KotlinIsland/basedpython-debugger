//! the calls a held line is still to make, for a step into one of them
//!
//! read off the frame's own bytecode: every `CALL`, `CALL_KW` and
//! `CALL_FUNCTION_EX` of the line, at or after the instruction the frame is
//! at. each is named by what it calls — `sum`, `self.area`, `json.dumps` — and
//! that name is the source's own text, taken from the call's position: cpython
//! gives a call instruction the span of the whole call expression, and what is
//! called is that span up to its argument list
//!
//! ## a module `by run` compiled as its `.by`
//!
//! the runner moves every location of such a module onto the `.by` line it
//! came from and gives it the **whole** of that line, so the frame's own
//! positions have no columns to read a call from. the bytecode is the generated
//! python's, unchanged, so the generated python is compiled again — the way
//! [`crate::source`] proves a frame — and the code object with the same name
//! and the same bytecode is the one whose positions are read. the label is
//! then the generated python's text for the call, and the target carries no
//! columns, because the frame the client sees has none

use bpd_core::StepInTarget;
use pyo3::prelude::*;

/// the instructions that call
const CALLING: &[&str] = &["CALL", "CALL_KW", "CALL_FUNCTION_EX"];

/// the calls the frame's line is still to make
pub(crate) fn of(python: Python<'_>, frame: &Bound<'_, PyAny>) -> PyResult<Vec<StepInTarget>> {
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
        return Ok(Vec::new());
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
        return Ok(Vec::new());
    }

    let reported = crate::sources::locate(file.clone(), line).line;
    let (text_of, columns_too) = match crate::sources::staged_as(&file) {
        Some(table) => (generated_positions(python, &code, &table)?, false),
        None => (own_positions(&code, &file)?, true),
    };

    let mut targets = Vec::with_capacity(calls.len());
    for offset in calls {
        let found = text_of.iter().find(|call| call.offset == offset);
        let (label, columns) = match found {
            Some(call) => (
                callee(&call.text),
                columns_too.then_some((call.column + 1, call.end_column + 1)),
            ),
            // the call is in the frame and has no text the source gives it —
            // a file that is not there to read. it is still a call, named by
            // where it is rather than by a guess at what it calls
            None => (format!("the call at offset {offset}"), None),
        };
        targets.push(StepInTarget {
            offset,
            label,
            line: reported,
            columns,
        });
    }
    Ok(targets)
}

/// one call instruction, and the source text its position spans
struct Spanned {
    offset: u32,
    text: String,
    column: u32,
    end_column: u32,
}

/// the calls of `code`, with their text read from the file it was compiled from
fn own_positions(code: &Bound<'_, PyAny>, file: &str) -> PyResult<Vec<Spanned>> {
    let Ok(source) = std::fs::read(file) else {
        return Ok(Vec::new());
    };
    spans(code, &source)
}

/// the calls of the generated python behind a `.by` module, at the offsets the
/// frame's code has — see the module's own documentation
fn generated_positions(
    python: Python<'_>,
    code: &Bound<'_, PyAny>,
    table: &bpd_core::MappedFile,
) -> PyResult<Vec<Spanned>> {
    let Ok(source) = std::fs::read(&table.generated) else {
        return Ok(Vec::new());
    };
    let keywords = pyo3::types::PyDict::new(python);
    keywords.set_item("dont_inherit", true)?;
    let Ok(compiled) = python.import("builtins")?.getattr("compile")?.call(
        (
            pyo3::types::PyBytes::new(python, &source),
            table.generated.display().to_string(),
            "exec",
        ),
        Some(&keywords),
    ) else {
        // the generated python does not compile now, so it is not what the
        // frame is running and its text names nothing
        return Ok(Vec::new());
    };
    match same_code(&compiled, code)? {
        Some(twin) => spans(&twin, &source),
        None => Ok(Vec::new()),
    }
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
            continue;
        };
        // columns are utf-8 byte offsets into the line, which is cpython's own
        // definition of `col_offset`
        let Some(text) = slice(&lines, first, last, column, end_column) else {
            continue;
        };
        spanned.push(Spanned {
            offset: instruction.getattr("offset")?.extract()?,
            text,
            column: u32::try_from(column).unwrap_or(u32::MAX),
            end_column: u32::try_from(end_column).unwrap_or(u32::MAX),
        });
    }
    Ok(spanned)
}

/// the text between two positions, one-based lines and byte columns
fn slice(lines: &[&[u8]], first: usize, last: usize, column: usize, end: usize) -> Option<String> {
    let mut taken = Vec::new();
    for at in first..=last {
        let line = lines.get(at.checked_sub(1)?)?;
        let from = if at == first { column } else { 0 };
        let to = if at == last { end } else { line.len() };
        taken.extend_from_slice(line.get(from..to.min(line.len()))?);
        if at != last {
            taken.push(b' ');
        }
    }
    String::from_utf8(taken).ok()
}

/// what a call expression calls: its text up to the argument list
///
/// the argument list is the bracket the expression ends in, found by walking
/// back to the `(` that opens it. a text that does not end in one — which a
/// call cpython spans should — is named whole rather than cut somewhere
/// arbitrary
fn callee(call: &str) -> String {
    let call = call.split_whitespace().collect::<Vec<_>>().join(" ");
    if !call.ends_with(')') {
        return call;
    }
    let mut depth = 0usize;
    for (at, character) in call.char_indices().rev() {
        match character {
            ')' | ']' | '}' => depth += 1,
            '(' | '[' | '{' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let named = call[..at].trim();
                    return if named.is_empty() {
                        call
                    } else {
                        named.to_string()
                    };
                }
            }
            _ => {}
        }
    }
    call
}

#[cfg(test)]
mod tests {
    use super::callee;

    #[test]
    fn a_call_is_named_by_what_it_calls() {
        assert_eq!(callee("sum(areas, 0.0)"), "sum");
        assert_eq!(callee("self.area()"), "self.area");
        assert_eq!(callee("json.dumps({'a': (1, 2)}, default=f)"), "json.dumps");
        assert_eq!(callee("make()(3)"), "make()");
        assert_eq!(callee("table[k](x)"), "table[k]");
    }
}
