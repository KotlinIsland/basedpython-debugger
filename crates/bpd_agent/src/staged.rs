//! compiling a module of a basedpython build the way `by run` compiles it
//!
//! `by run`'s runner, `_by_runner.py`, does not leave a staged module to python's
//! loader. it compiles the generated python from its text, and then moves the
//! whole code object tree onto the `.by`: every code object is named after the
//! `.by` path, and every location in it is the `.by` line the generated line came
//! from, spanning that whole line. a line the transpiler wrote on its own has no
//! `.by` line, and is line `0`. that is what the program reads its own locations
//! back from — a traceback, a warning, `inspect` — and it is what the interpreter
//! reports to a debugger
//!
//! so anything bpd compiles for a module of the build has to come out the same
//! way, or it is a different program:
//!
//! - a code replacement assigns what this compiles to `function.__code__`, and a
//!   function that came back named after the generated python would report every
//!   location after the edit in a file the user never wrote, and bind no `.by`
//!   breakpoint again
//! - the source shown beside a frame is proved by compiling it and finding the
//!   frame's own code object, **line table included**, in what comes out
//!
//! the second is why this is the runner's construction exactly and not merely an
//! equivalent one: a line table that differed in a column the runner chose would
//! prove nothing about the one the frame is running. the runner's own words for
//! the format are its `_relined`; the format itself is cpython's
//! `Objects/locations.md`, and only its 3.11-and-later form is here because bpd
//! runs on 3.13 and later
//!
//! `crates/bpd_test/src/by_runner.py` is that runner, captured from `by run`, and
//! the tests that run a build through it are what hold this to it

use bpd_core::source_map::MappedFile;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyTuple};

/// the most code units one location entry covers
const RUN: usize = 8;

/// the location entry that holds any line and column: line delta, end line
/// delta, column + 1, end column + 1
const LONG_FORM: u8 = 14;

/// the location entry for code units with no location at all
const NO_LOCATION: u8 = 15;

/// the generated python of `table`, compiled as the `.by` it was transpiled from
///
/// `generated` is the bytes of the generated file, compiled under `compiled_as`
/// the way python's loader would — bytes, so the file's own encoding
/// declaration decides, and `dont_inherit`, so its own `__future__` imports do.
/// `source` is the `.by`'s bytes, which only say how wide each line is.
/// `named` is the filename every code object ends up with: the `.by`, spelled
/// the way the code this is to stand beside spells it
pub(crate) fn compile<'py>(
    python: Python<'py>,
    generated: &[u8],
    compiled_as: &str,
    named: &str,
    table: &MappedFile,
    source: &[u8],
) -> PyResult<Bound<'py, PyAny>> {
    let arguments = PyTuple::new(
        python,
        [
            PyBytes::new(python, generated).into_any(),
            compiled_as.into_pyobject(python)?.into_any(),
            "exec".into_pyobject(python)?.into_any(),
        ],
    )?;
    let keywords = PyDict::new(python);
    keywords.set_item("dont_inherit", true)?;
    let code = python
        .import("builtins")?
        .getattr("compile")?
        .call(arguments, Some(&keywords))?;

    let relining = Relining {
        lines: &table.lines,
        widths: widths(source),
    };
    relining.apply(&code, named)
}

/// the width of every line of a file, in bytes, the way the runner measures one
///
/// the runner iterates the file opened in binary, which splits after each `\n`,
/// and strips the line ending — `\r` included — off each piece
fn widths(source: &[u8]) -> Vec<u32> {
    source
        .split_inclusive(|byte| *byte == b'\n')
        .map(|line| {
            let end = line
                .iter()
                .rposition(|byte| *byte != b'\r' && *byte != b'\n')
                .map_or(0, |last| last + 1);
            u32::try_from(end).unwrap_or(u32::MAX)
        })
        .collect()
}

/// one build file's table, and the widths of the `.by` it points into
struct Relining<'a> {
    /// indexed by zero-based generated line, the zero-based `.by` line
    lines: &'a [Option<u32>],
    /// the `.by`'s lines, in bytes
    widths: Vec<u32>,
}

impl Relining<'_> {
    /// the `.by` line a generated line becomes: one-based, and `0` for a line
    /// with no `.by` line behind it or a line the table does not cover
    fn by_line(&self, line: u32) -> u32 {
        line.checked_sub(1)
            .and_then(|index| self.lines.get(index as usize).copied().flatten())
            .map_or(0, |source| source + 1)
    }

    /// how wide a `.by` line is, and `0` for line `0` or one past the end
    fn width(&self, line: u32) -> u32 {
        line.checked_sub(1)
            .and_then(|index| self.widths.get(index as usize).copied())
            .unwrap_or(0)
    }

    /// `code` and every code object it holds, named `named`, on `.by` lines
    fn apply<'py>(&self, code: &Bound<'py, PyAny>, named: &str) -> PyResult<Bound<'py, PyAny>> {
        let python = code.py();
        let kind = code.get_type();

        let mut consts = Vec::new();
        for constant in code.getattr("co_consts")?.try_iter()? {
            let constant = constant?;
            consts.push(if constant.is_instance(&kind)? {
                self.apply(&constant, named)?
            } else {
                constant
            });
        }

        let first = self.by_line(code.getattr("co_firstlineno")?.extract()?);
        let mut lines: Vec<Option<u32>> = Vec::new();
        for position in code.call_method0("co_positions")?.try_iter()? {
            let (line, _, _, _): (Option<u32>, Option<u32>, Option<u32>, Option<u32>) =
                position?.extract()?;
            lines.push(line.map(|line| self.by_line(line)));
        }

        let keywords = PyDict::new(python);
        keywords.set_item("co_filename", named)?;
        keywords.set_item("co_firstlineno", first)?;
        keywords.set_item("co_consts", PyTuple::new(python, consts)?)?;
        keywords.set_item(
            "co_linetable",
            PyBytes::new(python, &self.table(first, &lines)),
        )?;
        code.call_method("replace", (), Some(&keywords))
    }

    /// a location table for one `.by` line per code unit
    ///
    /// a run of up to [`RUN`] code units on one line is one entry. an entry with
    /// a line is the long form, spanning the whole of that line on it; one
    /// without is the form for no location
    fn table(&self, first: u32, lines: &[Option<u32>]) -> Vec<u8> {
        let mut table = Vec::new();
        let mut previous = i64::from(first);
        let mut start = 0;
        while start < lines.len() {
            let line = lines[start];
            let mut end = start + 1;
            while end < lines.len() && end - start < RUN && lines[end] == line {
                end += 1;
            }
            let length = u8::try_from(end - start - 1)
                .unwrap_or_else(|_| unreachable!("a run is at most {RUN} code units"));
            match line {
                None => table.push(0x80 | (NO_LOCATION << 3) | length),
                Some(line) => {
                    table.push(0x80 | (LONG_FORM << 3) | length);
                    signed(&mut table, i64::from(line) - previous);
                    varint(&mut table, 0);
                    varint(&mut table, 1);
                    varint(&mut table, u64::from(self.width(line)) + 1);
                    previous = i64::from(line);
                }
            }
            start = end;
        }
        table
    }
}

/// a variable-length unsigned integer, six bits at a time, low bits first
fn varint(table: &mut Vec<u8>, mut value: u64) {
    while value >= 64 {
        table.push(0x40 | u8::try_from(value & 63).unwrap_or_else(|_| unreachable!()));
        value >>= 6;
    }
    table.push(u8::try_from(value).unwrap_or_else(|_| unreachable!()));
}

/// a signed one: the magnitude shifted up one, with the sign in the low bit
fn signed(table: &mut Vec<u8>, value: i64) {
    let magnitude = value.unsigned_abs();
    varint(
        table,
        if value < 0 {
            (magnitude << 1) | 1
        } else {
            magnitude << 1
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_measured_without_its_ending_the_way_a_binary_file_splits() {
        assert_eq!(widths(b"ab\ncd\r\n\nlast"), vec![2, 2, 0, 4]);
        assert_eq!(widths(b"one\n"), vec![3]);
        assert!(widths(b"").is_empty());
    }

    #[test]
    fn a_signed_delta_keeps_its_sign_in_the_low_bit() {
        let mut table = Vec::new();
        signed(&mut table, -3);
        signed(&mut table, 3);
        varint(&mut table, 64);
        assert_eq!(table, vec![7, 6, 0x40, 1]);
    }
}
