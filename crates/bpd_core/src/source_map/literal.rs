//! reading the two tables out of `_by_sourcemap.py` without running it
//!
//! the file is python, and the obvious way to read it is to let python read it.
//! that is not what happens here, for two reasons that are both about when:
//!
//! - a breakpoint is set **before** the program is launched, and there is no
//!   interpreter to ask yet. a map that could only be read once the debuggee
//!   existed would be a map that arrived after the answer it is needed for
//! - importing it would put a module in the debuggee's `sys.modules` that bpd
//!   put there, and what a program can tell about being debugged is a thing this
//!   project keeps to zero and tests for
//!
//! so the tables are read here, from the bytes. what makes that safe rather than
//! a second-rate python parser is that it accepts a **literal subset** and
//! nothing else: strings, integers, `None`, lists, tuples, dicts and comments.
//! anything else in the file — a name, an operator, a call, an f-string — is a
//! [`ParseError`] naming the line, the column and what was found. it never
//! guesses at a construct it does not know, which is the same rule the map
//! itself is held to

use std::path::PathBuf;

/// a python literal, in the subset a source map is written in
#[derive(Debug, Clone, PartialEq, Eq)]
enum Literal {
    /// a string
    Str(String),
    /// a non-negative integer
    Int(u32),
    /// `None`
    None,
    /// `[...]`
    List(Vec<Literal>),
    /// `(...)`
    Tuple(Vec<Literal>),
    /// `{...}`, in the order it was written
    Dict(Vec<(Literal, Literal)>),
}

impl Literal {
    /// what to call this in a message about meeting the wrong one
    const fn describe(&self) -> &'static str {
        match self {
            Self::Str(_) => "a string",
            Self::Int(_) => "an integer",
            Self::None => "None",
            Self::List(_) => "a list",
            Self::Tuple(_) => "a tuple",
            Self::Dict(_) => "a dict",
        }
    }
}

/// why a map file could not be read
///
/// every variant carries the line and column, because the thing a person does
/// with one of these is open the file there
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseError {
    /// something that is not a literal of the subset
    #[error("line {line}, column {column}: expected {expected}, found {found}")]
    Unexpected {
        /// the line it was found on, counting from one
        line: u32,
        /// the column it was found at, counting from one
        column: u32,
        /// what would have been a literal here
        expected: &'static str,
        /// what is there instead
        found: String,
    },

    /// the file ended in the middle of something
    #[error("the file ends in the middle of {inside}")]
    Truncated {
        /// what was still open
        inside: &'static str,
    },

    /// a table the reader needs is not in the file
    #[error(
        "there is no `{name}` in it. a source map carries `SOURCEMAP`, \
         `DIGESTS` and `COLUMNS`, and one written without all three either \
         cannot be checked against the files it describes or is from a `by` \
         older than this bpd reads"
    )]
    MissingTable {
        /// the table that is not there
        name: &'static str,
    },

    /// a table is there and is not the shape a table is
    #[error("`{name}` is {found}, and a source map's tables are dicts")]
    NotATable {
        /// the table
        name: &'static str,
        /// what it is instead
        found: &'static str,
    },

    /// a name the map may carry is there and is not the shape it is
    #[error("`{name}` is {found}, and {expected}")]
    MalformedName {
        /// the name
        name: &'static str,
        /// what it is
        found: String,
        /// what it is written as
        expected: &'static str,
    },

    /// an entry of a table is a list, as a `by` before 422bbe56 wrote one
    ///
    /// formats move freely until basedpython 0.0.1, and this bpd reads only
    /// the packed strings `by` 422bbe56 and later write. the program is
    /// transpiled again by running it with a newer `by`
    #[error(
        "the `{name}` entry for `{key}` is a list, which is how a `by` older \
         than 422bbe56 wrote its tables. this bpd reads the packed strings a \
         newer `by` writes: run the program with a newer `by`, so that it is \
         transpiled again"
    )]
    OlderBy {
        /// the table the entry is in
        name: &'static str,
        /// the key it is under
        key: String,
    },

    /// an entry of a table is not the shape an entry is
    #[error("the `{name}` entry for `{key}` is {found}, and {expected}")]
    MalformedEntry {
        /// the table the entry is in
        name: &'static str,
        /// the key it is under
        key: String,
        /// what it is
        found: String,
        /// what an entry of that table looks like
        expected: &'static str,
    },
}

/// one file pair, as the two tables between them describe it
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    /// the generated python, as the map spells it
    pub(super) generated: PathBuf,
    /// the `.by` it came from, as the map spells it
    pub(super) source: PathBuf,
    /// indexed by zero-based generated line, holding the zero-based `.by` line
    pub(super) lines: Vec<Option<u32>>,
    /// the digests of the pair, when `DIGESTS` carries one for it
    pub(super) digests: Option<Digests>,
    /// the column runs of each generated line — see `MappedFile::columns`
    pub(super) columns: Vec<Vec<super::Run>>,
}

/// the two digests one entry of `DIGESTS` carries, algorithm prefix and all
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Digests {
    /// of the `.by` bytes the transpiler read
    pub(super) source: String,
    /// of the python bytes it wrote
    pub(super) generated: String,
}

/// what a source map says about the files of its build that are `by`'s own
///
/// both names are newer than the two tables, and a map from a `by` that writes
/// neither is read as one that says nothing: no launcher named, no file of
/// `by`'s named. that is the map's own silence rather than a guess about it
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Own {
    /// `LAUNCHER`: the runner `by run` starts, which runs the program as
    /// `__main__`. `None` when the map does not name one — a `by build` tree
    /// writes `None`, and an older `by` writes nothing
    pub(super) launcher: Option<PathBuf>,
    /// `BY_CODE`: every python file `by` wrote into the tree that is not the
    /// program's — the runner, the runtime's helpers, its plugins, the map
    pub(super) by_code: Vec<PathBuf>,
}

/// what a map says: the entries of its tables, and what it names of `by`'s own
///
/// the text is parsed once, and both are read off the one parse
pub(super) fn read(text: &str) -> Result<(Vec<Entry>, Own), ParseError> {
    let assignments = Reader::new(text).module()?;
    Ok((tables(&assignments)?, own(&assignments)?))
}

/// read `LAUNCHER` and `BY_CODE`, as [`Own`] describes them
fn own(assignments: &[(String, Literal)]) -> Result<Own, ParseError> {
    let named = |name: &str| {
        assignments
            .iter()
            .find(|(assigned, _)| assigned == name)
            .map(|(_, value)| value)
    };
    let launcher = match named("LAUNCHER") {
        None | Some(Literal::None) => None,
        Some(Literal::Str(path)) => Some(PathBuf::from(path)),
        Some(other) => {
            return Err(ParseError::MalformedName {
                name: "LAUNCHER",
                found: other.describe().to_owned(),
                expected: "it is the runner's path as a string, or `None`",
            });
        }
    };
    let by_code = match named("BY_CODE") {
        None => Vec::new(),
        Some(Literal::List(paths)) => {
            let mut files = Vec::with_capacity(paths.len());
            for path in paths {
                let Literal::Str(path) = path else {
                    return Err(ParseError::MalformedName {
                        name: "BY_CODE",
                        found: format!("a list holding {}", path.describe()),
                        expected: "it is a list of paths, each a string",
                    });
                };
                files.push(PathBuf::from(path));
            }
            files
        }
        Some(other) => {
            return Err(ParseError::MalformedName {
                name: "BY_CODE",
                found: other.describe().to_owned(),
                expected: "it is a list of paths, each a string",
            });
        }
    };
    Ok(Own { launcher, by_code })
}

/// read `CARRIED` out of `_by_project.py`: each hand-written `.py` `by run`
/// copied into the tree, as its path relative to the tree, with the project
/// file it is a copy of
///
/// the runner compiles a carried copy **as** that project file, so its frames
/// and its code objects are named after the file the user edits and never after
/// the copy. a project file with no `CARRIED` is one from a `by` that copied
/// nothing, or wrote no such table, and carries nothing
pub(super) fn carried(text: &str) -> Result<Vec<(PathBuf, PathBuf)>, ParseError> {
    let assignments = Reader::new(text).module()?;
    let Some((_, value)) = assignments.iter().find(|(name, _)| name == "CARRIED") else {
        return Ok(Vec::new());
    };
    let malformed = |found: String| ParseError::MalformedName {
        name: "CARRIED",
        found,
        expected: "it maps each copy's path in the tree to the project file, both strings",
    };
    let Literal::Dict(entries) = value else {
        return Err(malformed(value.describe().to_owned()));
    };
    entries
        .iter()
        .map(|entry| match entry {
            (Literal::Str(staged), Literal::Str(source)) => {
                Ok((PathBuf::from(staged), PathBuf::from(source)))
            }
            (key, value) => Err(malformed(format!(
                "a dict with {} for {}",
                value.describe(),
                key.describe()
            ))),
        })
        .collect()
}

/// the entries `SOURCEMAP` and `DIGESTS` describe between them, in file order
///
/// an entry of `SOURCEMAP` with no `DIGESTS` beside it keeps a `None` digest
/// rather than being dropped: which entry could not be verified is what the
/// caller refuses by name, and one that quietly went missing would look like a
/// file the map was never about
fn tables(assignments: &[(String, Literal)]) -> Result<Vec<Entry>, ParseError> {
    let sourcemap = table(assignments, "SOURCEMAP")?;
    let all_digests = table(assignments, "DIGESTS")?;

    let mut entries = Vec::with_capacity(sourcemap.len());
    for (key, value) in sourcemap {
        let generated = path_key("SOURCEMAP", key)?;
        let Literal::Tuple(parts) = value else {
            return Err(ParseError::MalformedEntry {
                name: "SOURCEMAP",
                key: generated.display().to_string(),
                found: value.describe().to_owned(),
                expected: "an entry is `(path, [lines])`",
            });
        };
        let [Literal::Str(source), lines] = &parts[..] else {
            return Err(ParseError::MalformedEntry {
                name: "SOURCEMAP",
                key: generated.display().to_string(),
                found: format!("a tuple of {} values", parts.len()),
                expected: "an entry is `(path, lines)`",
            });
        };
        let malformed = |found: String| ParseError::MalformedEntry {
            name: "SOURCEMAP",
            key: generated.display().to_string(),
            found,
            expected: "the line table is one string of integers and `-` joined by `,`",
        };
        let line_table = match lines {
            Literal::Str(packed) => packed_lines(packed, &malformed)?,
            Literal::List(_) => {
                return Err(ParseError::OlderBy {
                    name: "SOURCEMAP",
                    key: generated.display().to_string(),
                });
            }
            other => return Err(malformed(other.describe().to_owned())),
        };
        let digests = digest_for(all_digests, key)?;
        // asked for after the line table, so that a map an older `by` wrote,
        // which has no `COLUMNS` and holds lists, is refused as that `by`'s
        let all_columns = table(assignments, "COLUMNS")?;
        let columns = columns_for(all_columns, key, &generated, line_table.len())?;
        entries.push(Entry {
            generated,
            source: PathBuf::from(source),
            lines: line_table,
            digests,
            columns,
        });
    }
    table(assignments, "COLUMNS")?;
    Ok(entries)
}

/// a line table packed into one string, as `by` 422bbe56 writes it:
/// `"-,0,1"`, the zero-based `.by` line of each generated line or `-` for one
/// the transpiler wrote, joined by `,`. the empty string is a file of no lines
fn packed_lines(
    packed: &str,
    malformed: &impl Fn(String) -> ParseError,
) -> Result<Vec<Option<u32>>, ParseError> {
    if packed.is_empty() {
        return Ok(Vec::new());
    }
    packed
        .split(',')
        .map(|line| match line {
            "-" => Ok(None),
            line => packed_number(line)
                .map(Some)
                .ok_or_else(|| malformed(format!("a packed line table holding `{line}`"))),
        })
        .collect()
}

/// a number in a packed string: decimal digits with no leading zero but `0`'s
/// own, which is all `by` writes there. `str::parse` would take a sign and a
/// leading zero as well, and no map was written with either
fn packed_number(text: &str) -> Option<u32> {
    let digits = text.bytes().all(|byte| byte.is_ascii_digit());
    if text.is_empty() || !digits || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    text.parse().ok()
}

/// the column runs for one entry of a map that has `COLUMNS`
///
/// an entry `COLUMNS` has nothing for is a file every column of which is the
/// transpiler's, which is how the runner reads one: no runs on any line, and
/// so is a line past the last one the entry has
///
/// what the runner relies on is checked rather than assumed: a run covers at
/// least one column, a line's runs are in order and cover each column once, and
/// there are no more lines than `SOURCEMAP` has for the file. runs that break
/// any of that describe no file the runner relined
fn columns_for(
    all: &[(Literal, Literal)],
    key: &Literal,
    generated: &std::path::Path,
    lines_mapped: usize,
) -> Result<Vec<Vec<super::Run>>, ParseError> {
    let Some((_, value)) = all.iter().find(|(candidate, _)| candidate == key) else {
        return Ok(Vec::new());
    };
    let malformed = |found: String| ParseError::MalformedEntry {
        name: "COLUMNS",
        key: generated.display().to_string(),
        found,
        expected: "an entry is one string of lines each ended by `;`, their runs \
                   `start end by_line by_column` or `start end` joined by `,`",
    };
    let read = match value {
        Literal::Str(packed) => packed_columns(packed, &malformed)?,
        Literal::List(_) => {
            return Err(ParseError::OlderBy {
                name: "COLUMNS",
                key: generated.display().to_string(),
            });
        }
        other => return Err(malformed(other.describe().to_owned())),
    };
    checked_columns(read, lines_mapped, &malformed)
}

/// columns as they were read, held to what the runner relies on — see
/// [`columns_for`]
fn checked_columns(
    read: Vec<Vec<super::Run>>,
    lines_mapped: usize,
    malformed: &impl Fn(String) -> ParseError,
) -> Result<Vec<Vec<super::Run>>, ParseError> {
    if read.len() > lines_mapped {
        return Err(malformed(format!(
            "{} lines, where `SOURCEMAP` maps {lines_mapped}",
            read.len()
        )));
    }
    for runs in &read {
        for (index, run) in runs.iter().enumerate() {
            if run.start >= run.end {
                return Err(malformed(format!(
                    "a run from column {} to {}",
                    run.start, run.end
                )));
            }
            if let Some(before) = index.checked_sub(1).map(|before| &runs[before])
                && run.start < before.end
            {
                return Err(malformed(format!(
                    "a run from column {} after one that ends at {}",
                    run.start, before.end
                )));
            }
        }
    }
    Ok(read)
}

/// columns packed into one string: `";0 4 1 0,4 13;"`, each line's runs joined
/// by `,` and ended by `;`, a run `start end by_line by_column`, or `start end`
/// for a qualifier
///
/// a line is ended rather than separated so that a line with no runs is still
/// a line, and a string whose last line is not ended was not written whole
fn packed_columns(
    packed: &str,
    malformed: &impl Fn(String) -> ParseError,
) -> Result<Vec<Vec<super::Run>>, ParseError> {
    if packed.is_empty() {
        return Ok(Vec::new());
    }
    let Some(lines) = packed.strip_suffix(';') else {
        return Err(malformed(
            "packed columns whose last line is not ended by `;`".to_owned(),
        ));
    };
    lines
        .split(';')
        .map(|line| {
            if line.is_empty() {
                return Ok(Vec::new());
            }
            line.split(',')
                .map(|run| {
                    let numbers: Option<Vec<u32>> = run.split(' ').map(packed_number).collect();
                    match numbers.as_deref() {
                        Some(&[start, end, line, column]) => Ok(super::Run {
                            start,
                            end,
                            from: Some((line, column)),
                        }),
                        Some(&[start, end]) => Ok(super::Run {
                            start,
                            end,
                            from: None,
                        }),
                        _ => Err(malformed(format!("a packed run `{run}`"))),
                    }
                })
                .collect()
        })
        .collect()
}

/// the digests for one entry, when `DIGESTS` carries one for it
///
/// the two tables are keyed identically by the emitter — by the generated path,
/// spelled exactly the same way — so this is a lookup over the spelling rather
/// than over a canonicalised path. canonicalising here would let two entries
/// that point at one file collide, and which file a path names is the caller's
/// question rather than the parser's
fn digest_for(
    digests: &[(Literal, Literal)],
    key: &Literal,
) -> Result<Option<Digests>, ParseError> {
    let Some((_, value)) = digests.iter().find(|(candidate, _)| candidate == key) else {
        return Ok(None);
    };
    let expected = "an entry is `{\"by\": \"<digest>\", \"py\": \"<digest>\"}`";
    let named = |key: &str| -> Option<&String> {
        let Literal::Dict(fields) = value else {
            return None;
        };
        fields
            .iter()
            .find_map(|(name, digest)| match (name, digest) {
                (Literal::Str(name), Literal::Str(digest)) if name == key => Some(digest),
                _ => None,
            })
    };
    let malformed = |found: String| ParseError::MalformedEntry {
        name: "DIGESTS",
        key: path_key("DIGESTS", key)
            .map_or_else(|_| format!("{key:?}"), |path| path.display().to_string()),
        found,
        expected,
    };
    let (Some(source), Some(generated)) = (named("by"), named("py")) else {
        return Err(malformed(value.describe().to_owned()));
    };
    Ok(Some(Digests {
        source: source.clone(),
        generated: generated.clone(),
    }))
}

/// the value of a top-level assignment, as a dict
fn table<'a>(
    assignments: &'a [(String, Literal)],
    name: &'static str,
) -> Result<&'a [(Literal, Literal)], ParseError> {
    let value = assignments
        .iter()
        .find(|(assigned, _)| assigned == name)
        .map(|(_, value)| value)
        .ok_or(ParseError::MissingTable { name })?;
    match value {
        Literal::Dict(entries) => Ok(entries),
        other => Err(ParseError::NotATable {
            name,
            found: other.describe(),
        }),
    }
}

/// a table key, which is always a path written as a string
fn path_key(name: &'static str, key: &Literal) -> Result<PathBuf, ParseError> {
    match key {
        Literal::Str(path) => Ok(PathBuf::from(path)),
        other => Err(ParseError::MalformedEntry {
            name,
            key: format!("{other:?}"),
            found: other.describe().to_owned(),
            expected: "a table is keyed by the generated python path, as a string",
        }),
    }
}

/// a reader over the literal subset
struct Reader<'a> {
    text: &'a str,
    /// byte offset into `text`
    at: usize,
}

impl<'a> Reader<'a> {
    const fn new(text: &'a str) -> Self {
        Self { text, at: 0 }
    }

    /// every top-level `NAME = <literal>` in the file, in order
    ///
    /// anything else at the top level is refused. a map file is generated and
    /// holds nothing but comments and these two assignments, so meeting a third
    /// thing means this is not the file the reader thinks it is
    fn module(&mut self) -> Result<Vec<(String, Literal)>, ParseError> {
        let mut assignments = Vec::new();
        loop {
            self.skip_trivia();
            if self.at >= self.text.len() {
                return Ok(assignments);
            }
            let name = self.name()?;
            self.skip_trivia();
            self.expect('=', "`=` after the name of an assignment")?;
            let value = self.value()?;
            assignments.push((name, value));
        }
    }

    /// an identifier
    fn name(&mut self) -> Result<String, ParseError> {
        let start = self.at;
        while let Some(character) = self.peek() {
            if character.is_alphanumeric() || character == '_' {
                self.at += character.len_utf8();
            } else {
                break;
            }
        }
        if start == self.at {
            return Err(self.unexpected("the name of a top-level assignment"));
        }
        Ok(self.text[start..self.at].to_owned())
    }

    /// one literal
    fn value(&mut self) -> Result<Literal, ParseError> {
        self.skip_trivia();
        match self.peek() {
            Some('"') => self.string(),
            Some('[') => Ok(Literal::List(self.sequence('[', ']', "a list")?)),
            Some('(') => Ok(Literal::Tuple(self.sequence('(', ')', "a tuple")?)),
            Some('{') => self.dict(),
            Some(character) if character.is_ascii_digit() => self.integer(),
            Some('N') if self.text[self.at..].starts_with("None") => {
                self.at += "None".len();
                Ok(Literal::None)
            }
            _ => Err(self.unexpected("a string, an integer, `None`, a list, a tuple or a dict")),
        }
    }

    /// a double-quoted string, with the escapes the emitter writes
    ///
    /// exactly the four `py_str_literal` produces upstream, and no others. a
    /// `\t` or a `\x41` is not something it can write, so meeting one means the
    /// file was not written by the emitter this reader is for — and inventing a
    /// meaning for it is how a path silently becomes a different path
    fn string(&mut self) -> Result<Literal, ParseError> {
        self.expect('"', "a string")?;
        let mut out = String::new();
        loop {
            let Some(character) = self.peek() else {
                return Err(ParseError::Truncated { inside: "a string" });
            };
            self.at += character.len_utf8();
            match character {
                '"' => return Ok(Literal::Str(out)),
                '\\' => {
                    let Some(escaped) = self.peek() else {
                        return Err(ParseError::Truncated { inside: "a string" });
                    };
                    self.at += escaped.len_utf8();
                    match escaped {
                        '\\' => out.push('\\'),
                        '"' => out.push('"'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        _ => {
                            return Err(ParseError::Unexpected {
                                line: self.line(),
                                column: self.column(),
                                expected: "one of the escapes a source map is written with — \
                                           `\\\\`, `\\\"`, `\\n` or `\\r`",
                                found: format!("`\\{escaped}`"),
                            });
                        }
                    }
                }
                '\n' => {
                    return Err(ParseError::Unexpected {
                        line: self.line(),
                        column: self.column(),
                        expected: "the end of a string",
                        found: "a newline".to_owned(),
                    });
                }
                _ => out.push(character),
            }
        }
    }

    /// a non-negative integer
    fn integer(&mut self) -> Result<Literal, ParseError> {
        let start = self.at;
        while self
            .peek()
            .is_some_and(|character| character.is_ascii_digit())
        {
            self.at += 1;
        }
        self.text[start..self.at]
            .parse()
            .map(Literal::Int)
            .map_err(|_| ParseError::Unexpected {
                line: self.line(),
                column: self.column(),
                expected: "a line number that fits in 32 bits",
                found: self.text[start..self.at].to_owned(),
            })
    }

    /// a comma-separated sequence between two brackets, trailing comma allowed
    fn sequence(
        &mut self,
        open: char,
        close: char,
        what: &'static str,
    ) -> Result<Vec<Literal>, ParseError> {
        self.expect(open, what)?;
        let mut values = Vec::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                None => return Err(ParseError::Truncated { inside: what }),
                Some(character) if character == close => {
                    self.at += character.len_utf8();
                    return Ok(values);
                }
                Some(_) => {}
            }
            values.push(self.value()?);
            self.skip_trivia();
            match self.peek() {
                Some(',') => self.at += 1,
                Some(character) if character == close => {}
                None => return Err(ParseError::Truncated { inside: what }),
                Some(_) => return Err(self.unexpected("`,` or the end of the sequence")),
            }
        }
    }

    /// `{key: value, ...}`
    fn dict(&mut self) -> Result<Literal, ParseError> {
        self.expect('{', "a dict")?;
        let mut entries = Vec::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                None => return Err(ParseError::Truncated { inside: "a dict" }),
                Some('}') => {
                    self.at += 1;
                    return Ok(Literal::Dict(entries));
                }
                Some(_) => {}
            }
            let key = self.value()?;
            self.skip_trivia();
            self.expect(':', "`:` between a dict's key and its value")?;
            let value = self.value()?;
            entries.push((key, value));
            self.skip_trivia();
            match self.peek() {
                Some(',') => self.at += 1,
                Some('}') => {}
                None => return Err(ParseError::Truncated { inside: "a dict" }),
                Some(_) => return Err(self.unexpected("`,` or the end of the dict")),
            }
        }
    }

    /// whitespace and comments, which carry nothing
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(character) if character.is_whitespace() => self.at += character.len_utf8(),
                Some('#') => {
                    while let Some(character) = self.peek() {
                        self.at += character.len_utf8();
                        if character == '\n' {
                            break;
                        }
                    }
                }
                _ => return,
            }
        }
    }

    fn peek(&self) -> Option<char> {
        self.text[self.at..].chars().next()
    }

    fn expect(&mut self, character: char, what: &'static str) -> Result<(), ParseError> {
        self.skip_trivia();
        if self.peek() == Some(character) {
            self.at += character.len_utf8();
            return Ok(());
        }
        Err(self.unexpected(what))
    }

    /// what is at the cursor, for a message about it not belonging there
    fn unexpected(&self, expected: &'static str) -> ParseError {
        let found = match self.peek() {
            None => "the end of the file".to_owned(),
            Some(character) => format!("`{character}`"),
        };
        ParseError::Unexpected {
            line: self.line(),
            column: self.column(),
            expected,
            found,
        }
    }

    /// the line the cursor is on, counting from one
    fn line(&self) -> u32 {
        let lines = self.text[..self.at]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        u32::try_from(lines + 1).unwrap_or(u32::MAX)
    }

    /// the column the cursor is at, counting from one
    fn column(&self) -> u32 {
        let start = self.text[..self.at]
            .rfind('\n')
            .map_or(0, |newline| newline + 1);
        u32::try_from(self.text[start..self.at].chars().count() + 1).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the tables of a map's text, read the way [`read`] reads them
    fn tables(text: &str) -> Result<Vec<Entry>, ParseError> {
        read(text).map(|(entries, _)| entries)
    }

    /// what a map's text names of `by`'s own, read the way [`read`] reads it
    fn own(text: &str) -> Result<Own, ParseError> {
        read(text).map(|(_, own)| own)
    }

    /// a map with one entry, so a test can change one thing about it
    fn map(sourcemap: &str, digests: &str, columns: &str) -> String {
        format!("SOURCEMAP = {{{sourcemap}}}\nDIGESTS = {{{digests}}}\nCOLUMNS = {{{columns}}}\n")
    }

    const DIGESTS: &str = "\"/b/demo.py\": {\"by\": \"sha256:aa\", \"py\": \"sha256:bb\"},";

    #[test]
    fn the_packed_tables_are_read_as_by_writes_them() {
        let read = tables(&map(
            "\"/b/demo.py\": (\"/a/demo.by\", \"-,0,1\"),",
            DIGESTS,
            "\"/b/demo.py\": \";0 4 1 0,4 13,13 20 1 4;;\",",
        ))
        .expect("a well formed map");
        assert_eq!(read[0].lines, vec![None, Some(0), Some(1)]);
        let run = |start, end, from| super::super::Run { start, end, from };
        assert_eq!(
            read[0].columns,
            vec![
                Vec::new(),
                vec![
                    run(0, 4, Some((1, 0))),
                    run(4, 13, None),
                    run(13, 20, Some((1, 4)))
                ],
                Vec::new(),
            ]
        );

        // the empty string is a file of no lines, in either table
        let empty = tables(&map(
            "\"/b/demo.py\": (\"/a/demo.by\", \"\"),",
            DIGESTS,
            "\"/b/demo.py\": \"\",",
        ))
        .expect("a well formed map");
        assert_eq!(empty[0].lines, Vec::new());
        assert_eq!(empty[0].columns, Vec::<Vec<super::super::Run>>::new());

        // and a file `COLUMNS` has nothing for has no runs on any line: every
        // column of it is the transpiler's, which is how the runner reads it
        let absent = tables(&map(
            "\"/b/demo.py\": (\"/a/demo.by\", \"0\"),",
            DIGESTS,
            "",
        ))
        .expect("a well formed map");
        assert_eq!(absent[0].columns, Vec::<Vec<super::super::Run>>::new());
    }

    #[test]
    fn a_map_an_older_by_wrote_is_refused_naming_the_by_that_wrote_it() {
        // a `by` before 422bbe56 wrote its tables as lists. formats move freely
        // until basedpython 0.0.1, so this bpd reads only the packed strings,
        // and says which `by` it would need rather than reading a list as some
        // other table
        for (sourcemap, columns, table) in [
            (
                "\"/b/demo.py\": (\"/a/demo.by\", [None, 0]),",
                "\"/b/demo.py\": \";;\",",
                "SOURCEMAP",
            ),
            (
                "\"/b/demo.py\": (\"/a/demo.by\", \"-,0\"),",
                "\"/b/demo.py\": [[], []],",
                "COLUMNS",
            ),
        ] {
            let refused = tables(&map(sourcemap, DIGESTS, columns)).expect_err("a list is refused");
            assert_eq!(
                refused,
                ParseError::OlderBy {
                    name: table,
                    key: "/b/demo.py".to_owned()
                }
            );
            let said = refused.to_string();
            assert!(
                said.contains("422bbe56") && said.contains("newer `by`"),
                "{said}"
            );
        }
    }

    #[test]
    fn a_packed_string_by_cannot_write_is_refused() {
        // a packed string is read as exactly what `by` writes there and nothing
        // else: a sign, a space, an empty number or a missing end is not a table
        // any `by` wrote, and a guess at what it meant is a guess at a location
        for (lines, why) in [
            ("0,,1", "an empty element"),
            ("+1,0", "a sign"),
            ("007", "a leading zero"),
            ("0, 1", "a space"),
            ("None", "a word"),
            ("-1", "a negative line"),
            ("4294967296", "a line past 32 bits"),
        ] {
            let written = map(
                &format!("\"/b/demo.py\": (\"/a/demo.by\", \"{lines}\"),"),
                DIGESTS,
                "",
            );
            assert!(
                matches!(
                    tables(&written),
                    Err(ParseError::MalformedEntry {
                        name: "SOURCEMAP",
                        ..
                    })
                ),
                "{why} was accepted"
            );
        }
        let sourcemap = "\"/b/demo.py\": (\"/a/demo.by\", \"-,0,1\"),";
        for (columns, why) in [
            ("0 4 1 0", "a last line that is not ended"),
            ("0 4 1;", "a run of three numbers"),
            ("0 4 1 0 2;", "a run of five numbers"),
            ("0  4;", "an empty number"),
            ("00 4;", "a leading zero"),
            ("0 x;", "a word"),
            ("4 2;", "a run that ends before it starts"),
            ("0 0;", "a run that covers nothing"),
            ("4 8 0 4,0 4 0 0;", "runs out of order"),
            ("0 6 0 0,4 8 0 4;", "runs over one another"),
            (";;;;", "more lines than the file has"),
        ] {
            let written = map(
                sourcemap,
                DIGESTS,
                &format!("\"/b/demo.py\": \"{columns}\""),
            );
            assert!(
                matches!(
                    tables(&written),
                    Err(ParseError::MalformedEntry {
                        name: "COLUMNS",
                        ..
                    })
                ),
                "{why} was accepted"
            );
        }
    }

    /// the unpacking the runner `by` bb1fdd2a writes carries, cut out of it
    fn runner_unpacking() -> &'static str {
        let runner = bpd_test::basedpython::RUNNER;
        let start = runner
            .find("import re\n")
            .expect("the runner's unpacking starts with its import");
        let length = runner[start..]
            .find("\n\n\n# ")
            .expect("the runner goes on after its unpacking");
        &runner[start..start + length]
    }

    /// what bpd reads packed line tables and packed columns as, each in the
    /// json the runner's unpacking is printed in below, or `"refused"`.
    /// columns are read against a line table of three lines
    fn as_bpd_reads(lines: &[&str], columns: &[&str]) -> Vec<serde_json::Value> {
        let malformed = |found: String| ParseError::MalformedEntry {
            name: "COLUMNS",
            key: String::new(),
            found,
            expected: "",
        };
        let refused = |_| serde_json::json!("refused");
        let run = |run: &super::super::Run| match run.from {
            Some((line, column)) => serde_json::json!([run.start, run.end, line, column]),
            None => serde_json::json!([run.start, run.end, null, null]),
        };
        let read_lines = lines.iter().map(|packed| {
            packed_lines(packed, &malformed).map_or_else(refused, |read| serde_json::json!(read))
        });
        let read_columns = columns.iter().map(|packed| {
            packed_columns(packed, &malformed)
                .and_then(|read| checked_columns(read, 3, &malformed))
                .map_or_else(refused, |read| {
                    read.iter()
                        .map(|runs| runs.iter().map(run).collect::<serde_json::Value>())
                        .collect()
                })
        });
        read_lines.chain(read_columns).collect()
    }

    /// what the runner's unpacking answers for the same strings, read the same
    /// way
    fn as_the_runner_unpacks(lines: &[&str], columns: &[&str]) -> Vec<serde_json::Value> {
        let input = serde_json::json!([lines, columns]).to_string();
        let script = format!(
            "{unpacking}\nimport json\n\
             def answer(unpack, packed):\n    \
                 try:\n        return unpack(packed)\n    except ValueError:\n        return 'refused'\n\
             lines, columns = json.loads({input})\n\
             print(json.dumps([answer(_unpack_lines, packed) for packed in lines]\n    \
                 + [answer(lambda packed: _unpack_columns(packed, 3), packed) for packed in columns]))\n",
            unpacking = runner_unpacking(),
            input = serde_json::Value::String(input),
        );
        let interpreter = bpd_test::discovered().require()[0];
        serde_json::from_str(&bpd_test::eval(interpreter, &script))
            .expect("the runner's answers are json")
    }

    #[test]
    fn a_packed_string_is_read_exactly_as_the_runner_unpacks_it() {
        // two readers of one map: the runner, which compiles a module at the
        // lines and columns its entry says, and bpd, which proves a frame's code
        // by compiling it the same way. a string one reads and the other refuses,
        // or reads otherwise, is a frame bpd proves against a different table
        // from the one it ran with. so each string goes through both, valid and
        // invalid alike, and the answers have to be the same
        let lines = [
            "",
            "-",
            "0",
            "-,0,1,12",
            "4294967295",
            " 7 ",
            "007",
            "-1",
            "+1",
            "1_0",
            "٤",
            "0,,1",
            "0, 1",
            "None",
            "4294967296",
            ",",
            "0,",
            "--",
        ];
        let columns = [
            "",
            ";",
            ";;;",
            ";0 4 1 0,4 13,13 20 1 4;",
            "0 4294967295 4294967295 4294967295;",
            "0 4 1 0",
            "0 4 1;",
            "0 4 1 0 2;",
            "0 x;",
            "0 4 -1 0;",
            "0 4 +1 0;",
            "0 4_0;",
            "4 2;",
            "0 0;",
            "0 ٤;",
            "00 4;",
            "0  4;",
            " 0 4;",
            "4 8 0 4,0 4 0 0;",
            "0 4,;",
            "0 4294967296;",
            ";;;;",
        ];
        let bpd = as_bpd_reads(&lines, &columns);
        let runner = as_the_runner_unpacks(&lines, &columns);
        assert_eq!(bpd.len(), runner.len());
        for ((packed, bpd), runner) in lines.iter().chain(&columns).zip(&bpd).zip(&runner) {
            assert_eq!(
                bpd, runner,
                "bpd and the runner read {packed:?} differently"
            );
        }
    }

    #[test]
    fn what_by_names_of_its_own_is_read_as_by_f566374c_writes_it() {
        // the tail of a `_by_sourcemap.py` a `by run` tree carries, as `by`
        // f566374c wrote it, comment and all
        let written = format!(
            "{}\
             LAUNCHER = \"/var/folders/x/T/.tmp1/_by_runner.py\"\n\n\
             # every file of by's own python in this tree, none of which is the program's code:\n\
             # the launcher, the runtime's helpers, this map\n\
             BY_CODE = [\n    \"/var/folders/x/T/.tmp1/_by_runner.py\",\n    \
             \"/var/folders/x/T/.tmp1/_by_runtime.py\",\n]\n",
            map("", "", "")
        );
        assert_eq!(
            own(&written).expect("a well formed map"),
            Own {
                launcher: Some(PathBuf::from("/var/folders/x/T/.tmp1/_by_runner.py")),
                by_code: vec![
                    PathBuf::from("/var/folders/x/T/.tmp1/_by_runner.py"),
                    PathBuf::from("/var/folders/x/T/.tmp1/_by_runtime.py"),
                ],
            }
        );

        // `by build` names no launcher, and a map that names neither says
        // nothing of either
        let built = format!("{}LAUNCHER = None\nBY_CODE = []\n", map("", "", ""));
        assert_eq!(own(&built).expect("a well formed map"), Own::default());
        assert_eq!(
            own(&map("", "", "")).expect("a well formed map"),
            Own::default()
        );

        // and a shape it is not written in is refused by name
        let wrong = format!("{}LAUNCHER = [\"/a\"]\n", map("", "", ""));
        assert!(matches!(
            own(&wrong),
            Err(ParseError::MalformedName {
                name: "LAUNCHER",
                ..
            })
        ));
    }

    #[test]
    fn the_two_tables_are_read_and_paired_by_the_generated_path() {
        let entries = tables(&map(
            "\"/b/demo.py\": (\"/a/demo.by\", \"-,0,1\"),",
            DIGESTS,
            "",
        ))
        .expect("a well formed map");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].generated, PathBuf::from("/b/demo.py"));
        assert_eq!(entries[0].source, PathBuf::from("/a/demo.by"));
        assert_eq!(entries[0].lines, vec![None, Some(0), Some(1)]);
        let digests = entries[0].digests.as_ref().expect("both digests are there");
        assert_eq!(digests.source, "sha256:aa");
        assert_eq!(digests.generated, "sha256:bb");
    }

    #[test]
    fn comments_and_whitespace_carry_nothing() {
        let entries = tables(
            "# a comment\n\nSOURCEMAP = {\n    # another\n    \"/b.py\": (\"/a.by\", \"0\"),\n}\n\
             \nDIGESTS = {\n    \"/b.py\": {\"by\": \"sha256:aa\", \"py\": \"sha256:bb\"},\n}\n\
             \n# and another\nCOLUMNS = {}\n",
        )
        .expect("comments are trivia");
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn an_escape_the_emitter_writes_is_read_and_one_it_cannot_is_refused() {
        let entries = tables(&map(
            "\"/b/say \\\"hi\\\".py\": (\"/a\\\\b.by\", \"0\"),",
            "\"/b/say \\\"hi\\\".py\": {\"by\": \"sha256:aa\", \"py\": \"sha256:bb\"},",
            "",
        ))
        .expect("both escapes are ones `py_str_literal` writes");
        assert_eq!(entries[0].generated, PathBuf::from("/b/say \"hi\".py"));
        assert_eq!(entries[0].source, PathBuf::from("/a\\b.by"));

        let refused = tables(&map("\"/b\\x41.py\": (\"/a.by\", \"0\"),", "", "")).expect_err(
            "the emitter cannot write `\\x41`, so a file holding one is not one it wrote",
        );
        assert!(
            matches!(refused, ParseError::Unexpected { .. }),
            "{refused:?}"
        );
    }

    #[test]
    fn anything_that_is_not_a_literal_is_refused_with_where_it_is() {
        // the rule this parser exists for. a map file is generated, so a name,
        // a call or an operator in one means it is not the file this reads —
        // and reading past it would be reading a map nobody wrote
        for source in [
            "SOURCEMAP = {\"/b.py\": (\"/a.by\", [os.getpid()])}\nDIGESTS = {}\n",
            "SOURCEMAP = {\"/b.py\": (\"/a.by\", [0 + 1])}\nDIGESTS = {}\n",
            "SOURCEMAP = {\"/b.py\": (f\"/a.by\", [0])}\nDIGESTS = {}\n",
            "import os\nSOURCEMAP = {}\nDIGESTS = {}\n",
        ] {
            let refused = tables(source).expect_err("not a literal map: {source}");
            let said = refused.to_string();
            assert!(
                said.contains("line ") || said.contains("expected"),
                "a refusal has to say where it is: {said}"
            );
        }
    }

    #[test]
    fn a_map_missing_any_table_says_which() {
        for (source, missing) in [
            ("DIGESTS = {}\nCOLUMNS = {}\n", "SOURCEMAP"),
            ("SOURCEMAP = {}\nCOLUMNS = {}\n", "DIGESTS"),
            ("SOURCEMAP = {}\nDIGESTS = {}\n", "COLUMNS"),
        ] {
            let refused = tables(source).expect_err("one table is not a map");
            assert_eq!(refused, ParseError::MissingTable { name: missing });
        }
    }

    #[test]
    fn an_entry_that_is_not_the_shape_of_an_entry_names_the_file_it_is_under() {
        let refused =
            tables(&map("\"/b.py\": \"/a.by\",", "", "")).expect_err("not a `(path, lines)`");
        let ParseError::MalformedEntry { key, .. } = &refused else {
            panic!("expected a malformed entry, got {refused:?}")
        };
        assert_eq!(key, "/b.py");
    }

    #[test]
    fn a_digest_entry_without_both_sides_is_refused() {
        let refused = tables(&map(
            "\"/b.py\": (\"/a.by\", \"0\"),",
            "\"/b.py\": {\"by\": \"sha256:aa\"},",
            "",
        ))
        .expect_err("a pair with one digest cannot be checked on both sides");
        assert!(
            matches!(
                refused,
                ParseError::MalformedEntry {
                    name: "DIGESTS",
                    ..
                }
            ),
            "{refused:?}"
        );
    }

    #[test]
    fn a_file_that_ends_in_the_middle_of_something_says_what_was_open() {
        for (source, inside) in [
            ("SOURCEMAP = {\"/b.py\": (\"/a.by\", \"0\"),", "a dict"),
            ("SOURCEMAP = {\"/b.py\": (\"/a.by\", [0", "a list"),
            ("SOURCEMAP = {\"/b.py", "a string"),
        ] {
            assert_eq!(
                tables(source).expect_err("truncated"),
                ParseError::Truncated { inside }
            );
        }
    }
}
