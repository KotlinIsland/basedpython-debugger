//! compiling a module of a basedpython build the way `by run` compiles it
//!
//! `by run`'s runner, `_by_runner.py`, does not leave a staged module to python's
//! loader. it compiles the generated python from its text, and then moves the
//! whole code object tree onto the `.by`: every code object is named after the
//! `.by` path, and every location in it is the `.by` line the generated line came
//! from. a line the transpiler wrote on its own has no `.by` line, and is line
//! `0`. the columns depend on the build: a runner from `by` a110d7d8 on reads the
//! map's `COLUMNS` and gives a location the author's own `.by` columns, or none
//! for code the transpiler wrote (see [`Relining::by_span`]); an older runner,
//! whose map has no `COLUMNS`, spans the whole line. that is what the program reads its own locations
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

use bpd_core::source_map::{MappedFile, Run};
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
        columns: table.columns.as_deref(),
        generated_widths: widths(generated),
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
    /// the map's column runs, when the map has `COLUMNS` — a build from a `by`
    /// whose runner gives a location the author's own columns
    columns: Option<&'a [Vec<Run>]>,
    /// the generated python's lines, in bytes
    generated_widths: Vec<u32>,
}

/// one location: lines, and columns when there are any
type Location = (u32, u32, Option<u32>, Option<u32>);

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

    /// the `.by` location of a span of the generated python whose text is the
    /// author's, or `None` for one the transpiler wrote
    ///
    /// the runner's `_by_span`, line for line, and it has to be: a location
    /// that differed from the runner's in one column is a line table that
    /// proves nothing about the frame's. lines are one-based and columns byte
    /// offsets, as python's own positions are. a span is the author's when every
    /// byte of it is in a run — copied text, or a qualifier like `_by_lazy.` in
    /// front of a name — and the copied pieces follow one another in the `.by`
    fn by_span(
        &self,
        columns: &[Vec<Run>],
        position: (Option<u32>, Option<u32>, Option<u32>, Option<u32>),
    ) -> Option<Location> {
        let (Some(line), Some(end_line), Some(column), Some(end_column)) = position else {
            return None;
        };
        if end_line < line {
            return None;
        }
        let mut pieces: Vec<(u32, u32, u32)> = Vec::new();
        for generated in line..=end_line {
            let runs: &[Run] = generated
                .checked_sub(1)
                .and_then(|index| columns.get(index as usize))
                .map_or(&[], Vec::as_slice);
            let mut at = if generated == line { column } else { 0 };
            let stop = if generated == end_line {
                end_column
            } else {
                *generated
                    .checked_sub(1)
                    .and_then(|index| self.generated_widths.get(index as usize))?
            };
            for run in runs {
                if at >= stop {
                    break;
                }
                if run.end <= at {
                    continue;
                }
                if run.start > at {
                    return None;
                }
                let covered = run.end.min(stop);
                if let Some((by_line, by_column)) = run.from {
                    pieces.push((by_line + 1, by_column + (at - run.start), covered - at));
                }
                at = covered;
            }
            if at < stop {
                return None;
            }
        }
        let (first_line, first_column, _) = *pieces.first()?;
        for pair in pieces.windows(2) {
            let (line_before, column_before, length) = pair[0];
            let (line_after, column_after, _) = pair[1];
            let follows = (line_after == line_before && column_after == column_before + length)
                || (line_after == line_before + 1
                    && column_after == 0
                    && column_before + length == self.width(line_before));
            if !follows {
                return None;
            }
        }
        let (last_line, last_column, last_length) = *pieces.last()?;
        Some((
            first_line,
            last_line,
            Some(first_column),
            Some(last_column + last_length),
        ))
    }

    /// where one instruction of the generated python is in the `.by`, as the
    /// runner's `_relined_one` decides it
    fn located(
        &self,
        position: (Option<u32>, Option<u32>, Option<u32>, Option<u32>),
    ) -> Option<Location> {
        let line = position.0?;
        let Some(columns) = self.columns else {
            let at = self.by_line(line);
            return Some((at, at, Some(0), Some(self.width(at))));
        };
        if let Some(span) = self.by_span(columns, position) {
            return Some(span);
        }
        let at = self.by_line(line);
        Some((at, at, None, None))
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
        let mut lines: Vec<Option<Location>> = Vec::new();
        for position in code.call_method0("co_positions")?.try_iter()? {
            lines.push(self.located(position?.extract()?));
        }

        let keywords = PyDict::new(python);
        keywords.set_item("co_filename", named)?;
        keywords.set_item("co_firstlineno", first)?;
        keywords.set_item("co_consts", PyTuple::new(python, consts)?)?;
        keywords.set_item(
            "co_linetable",
            PyBytes::new(python, &Self::table(first, &lines)),
        )?;
        code.call_method("replace", (), Some(&keywords))
    }

    /// a location table for one `.by` location per code unit
    ///
    /// a run of up to [`RUN`] code units at one location is one entry. an entry
    /// with a location is the long form, a column of none written as `0`; one
    /// without is the form for no location
    fn table(first: u32, lines: &[Option<Location>]) -> Vec<u8> {
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
                Some((line, end_line, column, end_column)) => {
                    table.push(0x80 | (LONG_FORM << 3) | length);
                    signed(&mut table, i64::from(line) - previous);
                    varint(&mut table, u64::from(end_line - line));
                    varint(&mut table, column.map_or(0, |column| u64::from(column) + 1));
                    varint(
                        &mut table,
                        end_column.map_or(0, |column| u64::from(column) + 1),
                    );
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

    /// the runner's `_by_span`, run by an interpreter over every span of a
    /// small build, against this port of it over the same spans
    ///
    /// against the runner's **own** text — the function is cut out of the
    /// runner `by` a110d7d8 writes and run as it is — because a port checked
    /// against a second port proves the two agree with each other
    /// column runs as the map writes them, which is a python literal
    fn as_python(columns: &[Vec<Run>]) -> String {
        let lines: Vec<String> = columns
            .iter()
            .map(|line| {
                let runs: Vec<String> = line
                    .iter()
                    .map(|run| match run.from {
                        Some((line, column)) => {
                            format!("({}, {}, {line}, {column})", run.start, run.end)
                        }
                        None => format!("({}, {}, None, None)", run.start, run.end),
                    })
                    .collect();
                format!("[{}]", runs.join(", "))
            })
            .collect();
        format!("[{}]", lines.join(", "))
    }

    /// what an interpreter the agent matches prints for `script`
    fn run_python(script: &str) -> String {
        let interpreter = bpd_test::agent::matching_interpreter();
        let ran = std::process::Command::new(interpreter.to_start())
            .args(["-I", "-c", script])
            .output()
            .expect("the interpreter runs");
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        String::from_utf8(ran.stdout).expect("the answers are text")
    }

    #[test]
    fn every_span_is_located_exactly_as_the_runners_own_by_span_locates_it() {
        let generated = "\
import types
def main() -> None:
    xs = [_by_lazy.classify(s) for s in _soundness_iter([1, 2], int)]
    total = add(
        1, 2)
";
        let by = "\
def main() -> None:
    xs = [classify(s) for s in [1, 2]]
    total = add(
        1, 2)
";
        let run = |start, end, from| Run { start, end, from };
        let columns = vec![
            Vec::new(),
            vec![run(0, 19, Some((0, 0)))],
            vec![
                run(0, 10, Some((1, 0))),
                run(10, 19, None),
                run(19, 40, Some((1, 10))),
                run(56, 62, Some((1, 31))),
                run(68, 69, Some((1, 37))),
            ],
            vec![run(0, 15, Some((2, 0)))],
            vec![run(0, 13, Some((3, 0)))],
        ];
        let lines = [None, Some(0), Some(1), Some(2), Some(3)];
        let relining = Relining {
            lines: &lines,
            widths: widths(by.as_bytes()),
            columns: Some(&columns),
            generated_widths: widths(generated.as_bytes()),
        };

        let runner = bpd_test::basedpython::RUNNER_WITH_COLUMNS;
        let start = runner
            .find("def _by_span(")
            .expect("the runner defines `_by_span`");
        let length = runner[start..]
            .find("\n\n\ndef ")
            .expect("another definition follows it");
        let function = &runner[start..start + length];
        let script = format!(
            "{function}\n\
             columns = {columns}\n\
             generated_widths = {generated_widths:?}\n\
             by_widths = {by_widths:?}\n\
             width = lambda line: by_widths[line - 1] if 0 < line <= len(by_widths) else 0\n\
             for line in range(1, len(generated_widths) + 1):\n\
             \x20   for end_line in range(line, min(line + 1, len(generated_widths)) + 1):\n\
             \x20       for column in range(0, generated_widths[line - 1] + 1):\n\
             \x20           for end_column in range(0, generated_widths[end_line - 1] + 1):\n\
             \x20               print(line, end_line, column, end_column, _by_span(columns, generated_widths, width, line, end_line, column, end_column))\n",
            columns = as_python(&columns),
            generated_widths = relining.generated_widths,
            by_widths = relining.widths,
        );
        let mut compared = 0;
        let mut located = 0;
        for answer in run_python(&script).lines() {
            let mut words = answer.splitn(5, ' ');
            let mut number = || -> u32 {
                words
                    .next()
                    .and_then(|word| word.parse().ok())
                    .expect("four numbers lead the line")
            };
            let (line, end_line, column, end_column) = (number(), number(), number(), number());
            let theirs = words.next().expect("and the answer follows them");
            let ours = match relining.by_span(
                &columns,
                (Some(line), Some(end_line), Some(column), Some(end_column)),
            ) {
                None => "None".to_string(),
                Some((line, end_line, Some(column), Some(end_column))) => {
                    located += 1;
                    format!("({line}, {end_line}, {column}, {end_column})")
                }
                Some(other) => {
                    panic!("a span is located with both columns or not at all: {other:?}")
                }
            };
            assert_eq!(
                ours, theirs,
                "the span from {line}:{column} to {end_line}:{end_column}"
            );
            compared += 1;
        }
        assert!(
            compared > 5_000,
            "every span of the build was compared: {compared}"
        );
        assert!(
            located > 100,
            "and many of them are the author's: {located}"
        );
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
