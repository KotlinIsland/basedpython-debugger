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
        "there is no `{name}` in it. a source map carries `SOURCEMAP` and \
         `DIGESTS`, and one written without both cannot be checked against the \
         files it describes"
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
}

/// the two digests one entry of `DIGESTS` carries, algorithm prefix and all
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Digests {
    /// of the `.by` bytes the transpiler read
    pub(super) source: String,
    /// of the python bytes it wrote
    pub(super) generated: String,
}

/// the entries `SOURCEMAP` and `DIGESTS` describe between them, in file order
///
/// an entry of `SOURCEMAP` with no `DIGESTS` beside it keeps a `None` digest
/// rather than being dropped: which entry could not be verified is what the
/// caller refuses by name, and one that quietly went missing would look like a
/// file the map was never about
pub(super) fn tables(text: &str) -> Result<Vec<Entry>, ParseError> {
    let assignments = Reader::new(text).module()?;
    let sourcemap = table(&assignments, "SOURCEMAP")?;
    let all_digests = table(&assignments, "DIGESTS")?;

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
        let [Literal::Str(source), Literal::List(lines)] = &parts[..] else {
            return Err(ParseError::MalformedEntry {
                name: "SOURCEMAP",
                key: generated.display().to_string(),
                found: format!("a tuple of {} values", parts.len()),
                expected: "an entry is `(path, [lines])`",
            });
        };
        let mut table = Vec::with_capacity(lines.len());
        for line in lines {
            match line {
                Literal::None => table.push(None),
                Literal::Int(line) => table.push(Some(*line)),
                other => {
                    return Err(ParseError::MalformedEntry {
                        name: "SOURCEMAP",
                        key: generated.display().to_string(),
                        found: other.describe().to_owned(),
                        expected: "every element of the line table is an integer or `None`",
                    });
                }
            }
        }
        let digests = digest_for(all_digests, key)?;
        entries.push(Entry {
            generated,
            source: PathBuf::from(source),
            lines: table,
            digests,
        });
    }
    Ok(entries)
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

    /// a map with one entry, so a test can change one thing about it
    fn map(sourcemap: &str, digests: &str) -> String {
        format!("SOURCEMAP = {{{sourcemap}}}\nDIGESTS = {{{digests}}}\n")
    }

    #[test]
    fn the_two_tables_are_read_and_paired_by_the_generated_path() {
        let entries = tables(&map(
            "\"/b/demo.py\": (\"/a/demo.by\", [None, 0, 1]),",
            "\"/b/demo.py\": {\"by\": \"sha256:aa\", \"py\": \"sha256:bb\"},",
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
            "# a comment\n\nSOURCEMAP = {\n    # another\n    \"/b.py\": (\"/a.by\", [0]),\n}\n\
             \nDIGESTS = {\n    \"/b.py\": {\"by\": \"sha256:aa\", \"py\": \"sha256:bb\"},\n}\n",
        )
        .expect("comments are trivia");
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn an_escape_the_emitter_writes_is_read_and_one_it_cannot_is_refused() {
        let entries = tables(&map(
            "\"/b/say \\\"hi\\\".py\": (\"/a\\\\b.by\", [0]),",
            "\"/b/say \\\"hi\\\".py\": {\"by\": \"sha256:aa\", \"py\": \"sha256:bb\"},",
        ))
        .expect("both escapes are ones `py_str_literal` writes");
        assert_eq!(entries[0].generated, PathBuf::from("/b/say \"hi\".py"));
        assert_eq!(entries[0].source, PathBuf::from("/a\\b.by"));

        let refused = tables(&map("\"/b\\x41.py\": (\"/a.by\", [0]),", "")).expect_err(
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
    fn a_map_missing_either_table_cannot_be_verified_and_says_so() {
        for (source, missing) in [
            ("SOURCEMAP = {}\n", "DIGESTS"),
            ("DIGESTS = {}\n", "SOURCEMAP"),
        ] {
            let refused = tables(source).expect_err("one table is not a map");
            assert_eq!(refused, ParseError::MissingTable { name: missing });
        }
    }

    #[test]
    fn an_entry_that_is_not_the_shape_of_an_entry_names_the_file_it_is_under() {
        let refused = tables(&map("\"/b.py\": \"/a.by\",", "")).expect_err("not a `(path, lines)`");
        let ParseError::MalformedEntry { key, .. } = &refused else {
            panic!("expected a malformed entry, got {refused:?}")
        };
        assert_eq!(key, "/b.py");
    }

    #[test]
    fn a_digest_entry_without_both_sides_is_refused() {
        let refused = tables(&map(
            "\"/b.py\": (\"/a.by\", [0]),",
            "\"/b.py\": {\"by\": \"sha256:aa\"},",
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
            ("SOURCEMAP = {\"/b.py\": (\"/a.by\", [0]),", "a dict"),
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
