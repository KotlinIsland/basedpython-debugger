//! the source around a frame's line, and the proof that it is that source
//!
//! a debugger that read the bytes on disk and called them the program's source
//! would be inventing the thing a reader reasons about. files are edited while
//! programs run, and cpython keeps no copy of what it compiled — `linecache`,
//! which is what a traceback uses, has exactly this bug and everyone has been
//! shown the wrong line by it
//!
//! so the file is read **and checked**: it is compiled, and the frame's own code
//! object has to be in what comes out — same qualified name, same first line,
//! same argument count, same names, same variable names, and the same **line
//! table**, which is the thing that maps an offset to a line and therefore the
//! thing being relied on. that is the rule source mapping is already held to:
//! total or absent, with no identity fallback
//!
//! compiling runs none of the program. it is the compiler, on bytes, and a
//! module that would raise on import raises nothing here
//!
//! what is shown is clamped to the **verified code object's own lines**. an edit
//! further down the file leaves this code object identical, so its lines are
//! still proven and lines outside it are not

use bpd_core::{Source, Unverified};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyTuple};

use crate::conditions::capture;

/// the source around one frame's current line
///
/// every way this cannot answer is a [`Unverified`] naming what stood in the
/// way, because "no source" and "source bpd will not vouch for" are different
/// facts and an agent cannot tell them apart from silence
pub(crate) fn around(
    python: Python<'_>,
    frame: &Bound<'_, PyAny>,
    around: u32,
) -> PyResult<Source> {
    let code = frame.getattr("f_code")?;
    let file: String = code.getattr("co_filename")?.extract()?;
    let line: u32 = frame.getattr("f_lineno")?.extract()?;

    let bytes = match std::fs::read(&file) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ok(Source::Unverified {
                why: Unverified::NotAFile {
                    file,
                    reason: error.to_string(),
                },
            });
        }
    };

    let compiled = match compile(python, &bytes, &file) {
        Ok(compiled) => compiled,
        Err(error) => {
            return Ok(Source::Unverified {
                why: Unverified::DoesNotCompile {
                    file,
                    error: capture(python, &error),
                },
            });
        }
    };

    let wanted = Identity::of(&code)?;
    if !matches(&compiled, &wanted)? {
        return Ok(Source::Unverified {
            why: Unverified::NotTheSameCode {
                file,
                function: wanted.qualname,
                first_line: wanted.first_line,
            },
        });
    }

    let (lowest, highest) = extent(&code, wanted.first_line)?;

    // everything above proved the **generated** python: it compiles, and the
    // code object this frame is running is in what came out, so its line table
    // is the one producing the line numbers. what a user of a basedpython build
    // has to read is the `.by` those lines came from, and that file is proved a
    // second way — by the digest the transpiler wrote and `bpd` checked
    let (bytes, file, line, lowest, highest) = match mapped(&file, line, lowest, highest) {
        Ok(None) => (bytes, file, line, lowest, highest),
        Ok(Some(source)) => (
            source.bytes,
            source.file,
            source.line,
            source.lowest,
            source.highest,
        ),
        Err(why) => return Ok(Source::Unverified { why }),
    };

    let all = split(&bytes);
    let total = u32::try_from(all.len()).unwrap_or(u32::MAX);

    // the window is the lines asked for, clamped to the code object that was
    // proved. a line outside it was not verified by anything
    let first = line.saturating_sub(around).max(lowest);
    let last = line.saturating_add(around).min(highest).min(total);
    if first > last {
        // the frame's line is outside its own code object's line table, which
        // nothing in cpython should produce
        return Ok(Source::Unverified {
            why: Unverified::NotTheSameCode {
                file,
                function: wanted.qualname,
                first_line: wanted.first_line,
            },
        });
    }

    let mut lines = Vec::with_capacity((last - first + 1) as usize);
    for number in first..=last {
        let raw = all
            .get((number - 1) as usize)
            .expect("the window is clamped to the number of lines the file has");
        match std::str::from_utf8(raw) {
            Ok(text) => lines.push(text.to_string()),
            // it compiled, so the interpreter read it under an encoding the file
            // declared. deciding that encoding again here would be a second
            // implementation of a rule cpython owns
            Err(_) => {
                return Ok(Source::Unverified {
                    why: Unverified::NotUtf8 { file },
                });
            }
        }
    }

    Ok(Source::Lines {
        first,
        at: line,
        lines,
        total,
    })
}

/// the `.by` behind a generated file, its bytes, and the window in its terms
struct Mapped {
    bytes: Vec<u8>,
    file: String,
    line: u32,
    lowest: u32,
    highest: u32,
}

/// the `.by` a frame of generated python should be shown as, if it is one
///
/// `Ok(None)` is every frame this build did not generate — ordinary python, the
/// standard library, the runner shim — and also a frame sitting on a generated
/// line the transpiler invented. that line has no `.by` behind it, the frame
/// reports the generated location for exactly that reason, and showing the
/// generated python beside it is the same location said once
///
/// the window is the `.by` lines the **proved** code object covers. an edit
/// further down either file leaves this code object identical, so its lines are
/// still the ones running and lines outside it are not
fn mapped(file: &str, line: u32, lowest: u32, highest: u32) -> Result<Option<Mapped>, Unverified> {
    let Some(map) = crate::sources::source_of(file) else {
        return Ok(None);
    };
    let Ok(at) = map.to_source(line) else {
        return Ok(None);
    };

    let source = map.source.display().to_string();
    let bytes = std::fs::read(&map.source).map_err(|error| Unverified::NotAFile {
        file: source.clone(),
        reason: error.to_string(),
    })?;
    // the second proof, and the one `bpd` cannot make from out here: it hashed
    // this file at launch, and a user asking to read it is asking about now. a
    // `.by` edited since the transpile is the failure a source map exists to
    // prevent, and the lines around it would be wrong with total confidence
    if bpd_core::source_map::digest(&bytes) != map.digest {
        return Err(Unverified::NotTheSameSource {
            file: source,
            generated: file.to_string(),
        });
    }

    // one `.by` line becomes several generated ones and some generate none, so
    // the extent is read out of the table rather than mapped at its ends
    let mut extent = (lowest..=highest).filter_map(|generated| map.to_source(generated).ok());
    let first = extent.next().unwrap_or_else(|| {
        unreachable!("the frame's own line is in the code object's extent and it mapped")
    });
    let (mut low, mut high) = (first, first);
    for mapped in extent {
        low = low.min(mapped);
        high = high.max(mapped);
    }

    Ok(Some(Mapped {
        bytes,
        file: source,
        line: at,
        lowest: low,
        highest: high,
    }))
}

/// compile the file's bytes the way the import machinery does
///
/// **bytes**, not text: a source file declares its own encoding under PEP 263
/// and cpython is the thing that reads that declaration. `dont_inherit` is what
/// `importlib._bootstrap_external.source_to_code` passes, so a `__future__`
/// statement in the file decides the flags and nothing else does
fn compile<'py>(python: Python<'py>, bytes: &[u8], file: &str) -> PyResult<Bound<'py, PyAny>> {
    let arguments = PyTuple::new(
        python,
        [
            PyBytes::new(python, bytes).into_any(),
            file.into_pyobject(python)?.into_any(),
            "exec".into_pyobject(python)?.into_any(),
        ],
    )?;
    let keywords = pyo3::types::PyDict::new(python);
    keywords.set_item("dont_inherit", true)?;
    python
        .import("builtins")?
        .getattr("compile")?
        .call(arguments, Some(&keywords))
}

/// what makes one code object the same code as another
///
/// the line table is the load bearing one: it is what maps an offset to a line,
/// so it is what a reported line number comes from. the rest is what makes two
/// different functions that happen to share a line table distinguishable
struct Identity {
    qualname: String,
    first_line: u32,
    argcount: u32,
    names: Vec<String>,
    varnames: Vec<String>,
    linetable: Vec<u8>,
}

impl Identity {
    fn of(code: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(Self {
            qualname: code.getattr("co_qualname")?.extract()?,
            first_line: code.getattr("co_firstlineno")?.extract()?,
            argcount: code.getattr("co_argcount")?.extract()?,
            names: code.getattr("co_names")?.extract()?,
            varnames: code.getattr("co_varnames")?.extract()?,
            linetable: code.getattr("co_linetable")?.extract()?,
        })
    }

    fn is(&self, other: &Self) -> bool {
        self.qualname == other.qualname
            && self.first_line == other.first_line
            && self.argcount == other.argcount
            && self.names == other.names
            && self.varnames == other.varnames
            && self.linetable == other.linetable
    }
}

/// whether the wanted code object is anywhere in a freshly compiled file
///
/// the search is over `co_consts`, recursively, because that is where the
/// compiler puts a nested code object — a method is a constant of its class
/// body, which is a constant of the module
fn matches(compiled: &Bound<'_, PyAny>, wanted: &Identity) -> PyResult<bool> {
    if Identity::of(compiled)?.is(wanted) {
        return Ok(true);
    }
    let kind = compiled.get_type();
    for constant in compiled.getattr("co_consts")?.try_iter()? {
        let constant = constant?;
        if !constant.is_instance(&kind)? {
            continue;
        }
        if matches(&constant, wanted)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// the lowest and highest line this code object has instructions on
///
/// `co_lines` is cpython's own line table walk, in C, and it is the only thing
/// that knows which lines a code object really covers. an entry with no line —
/// which is what compiler generated code produces — carries `None` and is
/// skipped rather than counted as line zero
fn extent(code: &Bound<'_, PyAny>, first_line: u32) -> PyResult<(u32, u32)> {
    let mut lowest = first_line;
    let mut highest = first_line;
    for entry in code.getattr("co_lines")?.call0()?.try_iter()? {
        let entry = entry?;
        let line = entry.get_item(2)?;
        if line.is_none() {
            continue;
        }
        let line: u32 = line.extract()?;
        lowest = lowest.min(line);
        highest = highest.max(line);
    }
    Ok((lowest, highest))
}

/// the file's lines, as cpython splits source: `\n`, `\r\n` and a lone `\r`
///
/// the endings are dropped rather than carried, because what is wanted is the
/// text of a line and an answer that carried them would differ between two
/// checkouts of the same file
fn split(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < bytes.len() {
        let ending = match bytes[at] {
            b'\r' if bytes.get(at + 1) == Some(&b'\n') => 2,
            b'\n' | b'\r' => 1,
            _ => {
                at += 1;
                continue;
            }
        };
        lines.push(&bytes[start..at]);
        at += ending;
        start = at;
    }
    if start < bytes.len() {
        lines.push(&bytes[start..]);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_split_the_way_cpython_splits_source() {
        assert_eq!(split(b"a\nb\nc"), vec![&b"a"[..], b"b", b"c"]);
        assert_eq!(split(b"a\r\nb\rc\n"), vec![&b"a"[..], b"b", b"c"]);
        assert_eq!(split(b""), Vec::<&[u8]>::new());
        // a trailing ending is an ending, not an empty line after it
        assert_eq!(split(b"a\n"), vec![&b"a"[..]]);
    }
}
