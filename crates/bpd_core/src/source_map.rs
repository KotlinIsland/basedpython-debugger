//! the `.by` a generated python line came from, and the proof it still is
//!
//! basedpython transpiles `.by` to `.py` and the interpreter runs the `.py`. so
//! every location the interpreter reports is a location in a file the user did
//! not write, and the layer that makes the two views agree is this one
//!
//! it is the single easiest place in a debugger to be quietly wrong, and
//! `docs/development/source-mapping.md` states the rule it holds to:
//! **a map either resolves a location or it errors**. there is
//! no identity fallback here and there will not be one. a fallback that returned
//! the raw line when the map had no entry would produce a location that points
//! at the wrong line of the wrong file and looks exactly like a correct one
//!
//! ## what is read
//!
//! `by run` transpiles into a build directory and writes [`MAP_FILENAME`] beside
//! the python it generated. that file carries two tables, both keyed by the
//! generated `.py` path:
//!
//! ```python
//! SOURCEMAP = {
//!     "/tmp/.tmpXXXX/demo.py": ("/abs/demo.by", [None, None, 0, 1, 2]),
//! }
//!
//! DIGESTS = {
//!     "/tmp/.tmpXXXX/demo.py": {"by": "sha256:…", "py": "sha256:…"},
//! }
//! ```
//!
//! the list is indexed by **generated** line, zero-based, and holds the
//! zero-based `.by` line it came from — or `None` where the transpiler emitted
//! prelude that no `.by` line is behind. that `None` is the provenance, and it
//! is why a generated line with no source is an answer this module can give
//! rather than a gap it has to guess across
//!
//! ## why the digests decide everything
//!
//! a map describes a **pair** of files and its lines are true only while both
//! are still the ones it was built from. an editor that saves the `.by` after
//! the transpile leaves the map describing a pair that no longer exists — and it
//! goes on resolving lines with total confidence. that is a wrong answer rather
//! than a missing one, which is the failure a source map exists to prevent
//!
//! so [`SourceMap::load`] recomputes both digests from the files on disk and
//! refuses the whole map when any of them has moved. the refusal names every
//! file that changed. it is the **whole** map rather than the entry, because a
//! build with a stale file in it is a stale build: the fix is to transpile
//! again, and a debugger that went on mapping the other forty-nine files would
//! be inviting exactly the confusion the digest was added to end
//!
//! nothing here runs the generated python or imports the map. bpd reads the
//! bytes, hashes the bytes, and parses the two tables itself — see [`literal`],
//! which accepts the literal subset the emitter writes and refuses everything
//! else by name

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

mod literal;

pub use literal::ParseError;

/// the file `by run` writes beside the python it generated
pub const MAP_FILENAME: &str = "_by_sourcemap.py";

/// the digest algorithm this reader can recompute
///
/// the algorithm is named in the value on purpose, so upstream can change it
/// without breaking a reader. a reader that met one it did not know and compared
/// the hex anyway would be comparing something it could never have produced, so
/// meeting one is a refusal — see [`MapError::UnknownDigest`]
const ALGORITHM: &str = "sha256";

/// a location, as a file and a **one-based** line
///
/// one-based because that is what every other location in `bpd` is, and what a
/// person counting lines in an editor means. the tables in [`MAP_FILENAME`] are
/// zero-based, and converting at the edge is why nothing downstream has to
/// remember which kind it is holding
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Located {
    /// the file the location is in
    pub file: PathBuf,
    /// the line of it, counting from one
    pub line: u32,
}

/// one `.by`/`.py` pair the map describes
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pair {
    /// the generated python, canonicalised
    generated: PathBuf,
    /// the `.by` it was transpiled from, canonicalised
    source: PathBuf,
    /// indexed by zero-based generated line, holding the zero-based `.by` line
    ///
    /// `None` is prelude the transpiler emitted that no `.by` line is behind
    lines: Vec<Option<u32>>,
}

impl Pair {
    /// the last `.by` line anything was generated for, if any
    fn last_source_line(&self) -> Option<u32> {
        self.lines.iter().flatten().copied().max()
    }
}

/// a verified map from generated python back to the `.by` it came from
///
/// there is no way to build one that has not been checked. [`SourceMap::load`]
/// is the only constructor and it verifies before it returns, so a value of this
/// type is itself the evidence that the pair of files it describes are still the
/// pair it was built from
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMap {
    /// keyed by the canonical generated path, so a lookup is one comparison
    pairs: BTreeMap<PathBuf, Pair>,
}

/// why a location has no counterpart, which is never answered with a guess
///
/// every variant names the file it is about, because the thing a person does
/// with one of these is go and look at that file. `#[non_exhaustive]` for the
/// reason [`crate::Unbound`] is: a reason added later must not silently join a
/// client's catch-all arm
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "unmapped", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Unmapped {
    /// no entry of the map is about this file at all
    ///
    /// asked of a `.py` this means the interpreter is running python the build
    /// did not generate — the standard library, a dependency, anything the
    /// transpiler never saw. asked of a `.by` it means the file is not part of
    /// this build
    NotInTheMap {
        /// the file as it was asked about
        file: PathBuf,
    },

    /// the generated file has no such line
    ///
    /// the map's table covers the file the transpiler wrote, so a line past its
    /// end is a line of a **different** file with the same name. it is reported
    /// rather than resolved, because the digests said the two agreed and this
    /// says they do not
    PastTheEnd {
        /// the generated file
        file: PathBuf,
        /// the line that was asked about
        line: u32,
        /// how many lines the map covers
        covered: u32,
    },

    /// the transpiler emitted this line and no `.by` line is behind it
    ///
    /// prelude, a lowering's own scaffolding, an import the source never wrote.
    /// the map says so itself — this is the `None` in its table — so it is a
    /// fact rather than a gap, and reporting it as a `.by` line would be the
    /// debugger inventing a line the user never wrote
    NoSourceLine {
        /// the generated file
        file: PathBuf,
        /// the generated line, counting from one
        line: u32,
        /// the `.by` the rest of that file came from
        source: PathBuf,
    },

    /// nothing was generated for that `.by` line or for any line after it
    ///
    /// the analogue of [`crate::Unbound::NoExecutableLine`] one level up: there
    /// is nowhere in the generated python for a location at the end of a `.by`
    /// file to go
    NoGeneratedLine {
        /// the `.by` file
        file: PathBuf,
        /// the line that was asked about
        requested: u32,
        /// the last `.by` line the transpiler generated anything for, if any
        last_mapped: Option<u32>,
    },
}

impl std::fmt::Display for Unmapped {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInTheMap { file } => write!(
                formatter,
                "the basedpython source map says nothing about `{}`. it covers \
                 the files this build transpiled and no others — if that file \
                 is part of the project, transpile again so the map takes it in",
                file.display()
            ),
            Self::PastTheEnd {
                file,
                line,
                covered,
            } => write!(
                formatter,
                "the source map covers {covered} lines of `{}` and line {line} \
                 is past the end of it. the file that was generated is not the \
                 file being read",
                file.display()
            ),
            Self::NoSourceLine { file, line, source } => write!(
                formatter,
                "line {line} of `{}` was emitted by the transpiler and no line \
                 of `{}` is behind it. bpd will not report a `.by` line it was \
                 not given one for",
                file.display(),
                source.display()
            ),
            Self::NoGeneratedLine {
                file,
                requested,
                last_mapped,
            } => {
                write!(
                    formatter,
                    "the transpiler generated nothing for line {requested} of \
                     `{}`, or for any line after it",
                    file.display()
                )?;
                match last_mapped {
                    Some(last) => write!(
                        formatter,
                        ". the last line it generated anything for is line {last}"
                    ),
                    None => formatter
                        .write_str(". it generated nothing for any line of that file at all"),
                }
            }
        }
    }
}

/// why a map could not be loaded, or could not be trusted once it was
///
/// none of these degrade into a map with a hole in it. a map that loaded with
/// something wrong about it is worse than no map, because everything downstream
/// was written to believe it
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MapError {
    /// the build directory has no map file in it
    #[error(
        "`{}` holds no `{MAP_FILENAME}`. that file is written by `by run`, \
         beside the python it generated — a directory without one is not a \
         basedpython build directory",
        directory.display()
    )]
    NoMapFile {
        /// the directory that was looked in
        directory: PathBuf,
    },

    /// the map file could not be read
    #[error("could not read `{}`", path.display())]
    Unreadable {
        /// the file that could not be read
        path: PathBuf,
        /// what the filesystem said
        #[source]
        source: std::io::Error,
    },

    /// the map file is not the shape a map file is
    #[error("`{}` is not a source map: {source}", path.display())]
    Malformed {
        /// the map file
        path: PathBuf,
        /// what was wrong with it, and where
        #[source]
        source: ParseError,
    },

    /// the two tables do not describe the same set of files
    ///
    /// `SOURCEMAP` and `DIGESTS` are keyed identically by the emitter. an entry
    /// in one and not the other is a map that cannot be verified, and an
    /// unverified entry is not one this module will resolve a line from
    #[error(
        "`{}` maps `{}` and carries no digest for it, so nothing can say \
         whether that file is still the one it describes",
        path.display(),
        generated.display()
    )]
    UndigestedEntry {
        /// the map file
        path: PathBuf,
        /// the generated file with no digest beside it
        generated: PathBuf,
    },

    /// a digest names an algorithm this reader cannot recompute
    ///
    /// deliberately fatal rather than skipped. a reader that met an unknown
    /// algorithm and mapped the entry anyway would be trusting a map it had not
    /// checked, which is the one thing the digest exists to stop
    #[error(
        "the digest `{digest}` in `{}` names an algorithm bpd cannot \
         recompute. it understands `{ALGORITHM}:` — this build was written by a \
         newer `by` than this bpd knows about, so update bpd",
        path.display()
    )]
    UnknownDigest {
        /// the map file
        path: PathBuf,
        /// the digest as it was written
        digest: String,
    },

    /// one of the files the map describes could not be read to check it
    #[error("could not read `{}` to check it against the source map", path.display())]
    Uncheckable {
        /// the file that could not be read
        path: PathBuf,
        /// what the filesystem said
        #[source]
        source: std::io::Error,
    },

    /// a file the map describes is not the file it was built from
    ///
    /// the whole map is refused rather than the entry. a build with a stale file
    /// in it is a stale build, the fix is to transpile again, and a map that
    /// went on answering about the files that had not moved would be one a
    /// person could keep using without ever finding out
    #[error("{}", stale(directory, changed))]
    Stale {
        /// the build directory the map is in
        directory: PathBuf,
        /// every file that is no longer what the map describes
        changed: Vec<PathBuf>,
    },
}

/// what a `write!` into a `String` cannot do, said once rather than at each of
/// them
const GROWS: &str = "a `String` grows to fit and has no other way to fail";

/// [`MapError::Stale`], which is the message this whole module exists for
fn stale(directory: &Path, changed: &[PathBuf]) -> String {
    let mut message = format!(
        "the basedpython build in `{}` is stale: ",
        directory.display()
    );
    match changed {
        [one] => {
            write!(message, "`{}` has changed", one.display()).expect(GROWS);
        }
        many => {
            write!(message, "{} files have changed —", many.len()).expect(GROWS);
            for file in many {
                write!(message, " `{}`", file.display()).expect(GROWS);
            }
        }
    }
    message.push_str(
        " since it was transpiled. every line the map reports about it would be \
         wrong with total confidence, so bpd will not report one. transpile \
         again and debug the build that comes out",
    );
    message
}

impl SourceMap {
    /// read the map out of a build directory and check it against disk
    ///
    /// the only constructor. it parses [`MAP_FILENAME`], recomputes both digests
    /// of **every** entry from the files on disk, and refuses if any of them
    /// has moved — so a `SourceMap` in hand is a map that was true a moment ago
    ///
    /// the paths inside are canonicalised, because a location arrives spelled
    /// however the interpreter or the user spelled it. the map's own keys are
    /// what the transpiler wrote, which on macos is a `/var` path that `/tmp`
    /// symlinks to — the runner shim upstream calls `os.path.realpath` for the
    /// same reason
    pub fn load(directory: &Path) -> Result<Self, MapError> {
        let path = directory.join(MAP_FILENAME);
        if !path.is_file() {
            return Err(MapError::NoMapFile {
                directory: directory.to_path_buf(),
            });
        }
        let text = std::fs::read_to_string(&path).map_err(|source| MapError::Unreadable {
            path: path.clone(),
            source,
        })?;
        let tables = literal::tables(&text).map_err(|source| MapError::Malformed {
            path: path.clone(),
            source,
        })?;

        let mut pairs = BTreeMap::new();
        let mut changed = Vec::new();
        for entry in tables {
            let Some(digests) = entry.digests else {
                return Err(MapError::UndigestedEntry {
                    path,
                    generated: entry.generated,
                });
            };
            for (file, digest) in [
                (&entry.generated, &digests.generated),
                (&entry.source, &digests.source),
            ] {
                let Some(hex) = digest
                    .strip_prefix(ALGORITHM)
                    .and_then(|rest| rest.strip_prefix(':'))
                else {
                    return Err(MapError::UnknownDigest {
                        path,
                        digest: digest.clone(),
                    });
                };
                let bytes = std::fs::read(file).map_err(|source| MapError::Uncheckable {
                    path: file.clone(),
                    source,
                })?;
                if digest_of(&bytes) != hex {
                    changed.push(file.clone());
                }
            }

            // canonicalising after the read, so a file that is not there is
            // reported as unreadable rather than as an unresolvable path
            let generated = canonical(&entry.generated)?;
            let source = canonical(&entry.source)?;
            pairs.insert(
                generated.clone(),
                Pair {
                    generated,
                    source,
                    lines: entry.lines,
                },
            );
        }

        if !changed.is_empty() {
            return Err(MapError::Stale {
                directory: directory.to_path_buf(),
                changed,
            });
        }

        Ok(Self { pairs })
    }

    /// the `.by` location a generated python location came from
    ///
    /// the direction a frame, a stop and a traceback go. every failure is an
    /// [`Unmapped`] naming the file, and none of them is a line
    pub fn to_source(&self, generated: &Path, line: u32) -> Result<Located, Unmapped> {
        let Some(pair) = self.pair_generating(generated) else {
            return Err(Unmapped::NotInTheMap {
                file: generated.to_path_buf(),
            });
        };
        let covered = u32::try_from(pair.lines.len()).unwrap_or(u32::MAX);
        // a line is one-based here and the table is zero-based. line 0 is not a
        // line any file has, and it is the caller's own confusion rather than a
        // property of the map, so it is the same answer as a line past the end
        let index = line.checked_sub(1).filter(|index| *index < covered);
        let Some(index) = index else {
            return Err(Unmapped::PastTheEnd {
                file: pair.generated.clone(),
                line,
                covered,
            });
        };
        let entry = pair.lines[index as usize];
        let Some(source_line) = entry else {
            return Err(Unmapped::NoSourceLine {
                file: pair.generated.clone(),
                line,
                source: pair.source.clone(),
            });
        };
        Ok(Located {
            file: pair.source.clone(),
            line: source_line + 1,
        })
    }

    /// the generated python location a `.by` location becomes
    ///
    /// the direction a breakpoint goes, and the map is forward-only, so this is
    /// a search rather than a lookup. the rule it applies, in two steps that are
    /// each worth stating:
    ///
    /// 1. the `.by` line asked for, or the **next one after it** that the
    ///    transpiler generated anything for. a blank line and a comment generate
    ///    nothing, and a breakpoint on one moves forward exactly as it does in
    ///    ordinary python
    /// 2. among the generated lines that `.by` line became — one source line can
    ///    become several — the **first**, because a stop anywhere but the first
    ///    would land in the middle of a statement
    ///
    /// what makes step 1 safe is that the caller maps the answer back. the
    /// interpreter may move a breakpoint on again to reach an executable line,
    /// and mapping that answer through [`Self::to_source`] is what makes the
    /// report of where it went a true one rather than the line that was asked
    /// for
    pub fn to_generated(&self, source: &Path, line: u32) -> Result<Located, Unmapped> {
        let Some(pair) = self.pair_from(source) else {
            return Err(Unmapped::NotInTheMap {
                file: source.to_path_buf(),
            });
        };
        let wanted = line.saturating_sub(1);
        let target = pair
            .lines
            .iter()
            .flatten()
            .copied()
            .filter(|candidate| *candidate >= wanted)
            .min();
        let Some(target) = target else {
            return Err(Unmapped::NoGeneratedLine {
                file: pair.source.clone(),
                requested: line,
                last_mapped: pair.last_source_line().map(|last| last + 1),
            });
        };
        let index = pair
            .lines
            .iter()
            .position(|entry| *entry == Some(target))
            .unwrap_or_else(|| {
                unreachable!("`target` was taken from the table, so the table holds it")
            });
        Ok(Located {
            file: pair.generated.clone(),
            line: u32::try_from(index + 1).unwrap_or(u32::MAX),
        })
    }

    /// the pair whose generated python is that file
    fn pair_generating(&self, file: &Path) -> Option<&Pair> {
        let file = file.canonicalize().ok()?;
        self.pairs.get(&file)
    }

    /// the pair whose `.by` source is that file
    fn pair_from(&self, file: &Path) -> Option<&Pair> {
        let file = file.canonicalize().ok()?;
        self.pairs.values().find(|pair| pair.source == file)
    }
}

/// a path as the filesystem really spells it
fn canonical(path: &Path) -> Result<PathBuf, MapError> {
    path.canonicalize().map_err(|source| MapError::Uncheckable {
        path: path.to_path_buf(),
        source,
    })
}

/// the sha-256 of some bytes, as lowercase hex
fn digest_of(bytes: &[u8]) -> String {
    use sha2::Digest as _;

    let mut out = String::with_capacity(64);
    for byte in sha2::Sha256::digest(bytes) {
        write!(out, "{byte:02x}").expect(GROWS);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a build directory holding a `.by`, the python it "generated" and a map
    ///
    /// the pair does not have to have come out of the transpiler for the map to
    /// be a valid one — a map is a claim about two files and a table, and this
    /// makes one whose digests are true. what it buys is a table this test can
    /// choose, so the cases that matter (a prelude with no source, a `.by` line
    /// nothing was generated for) are reachable without finding a `.by` program
    /// that happens to produce them
    struct Build {
        directory: tempfile::TempDir,
        source: PathBuf,
        generated: PathBuf,
    }

    impl Build {
        fn new(by: &str, py: &str, lines: &[Option<u32>]) -> Self {
            let directory = tempfile::tempdir().expect("a temporary directory");
            let source = directory.path().join("demo.by");
            let generated = directory.path().join("demo.py");
            std::fs::write(&source, by).expect("the .by is written");
            std::fs::write(&generated, py).expect("the .py is written");
            let build = Self {
                directory,
                source,
                generated,
            };
            build.write_map(lines, None, None);
            build
        }

        /// write the map, optionally lying about one of the two digests
        fn write_map(&self, lines: &[Option<u32>], by: Option<&str>, py: Option<&str>) {
            let table: Vec<String> = lines
                .iter()
                .map(|line| line.map_or_else(|| "None".to_owned(), |line| line.to_string()))
                .collect();
            let by = by.map_or_else(
                || format!("{ALGORITHM}:{}", digest_of(&read(&self.source))),
                ToOwned::to_owned,
            );
            let py = py.map_or_else(
                || format!("{ALGORITHM}:{}", digest_of(&read(&self.generated))),
                ToOwned::to_owned,
            );
            let generated = self.generated.display();
            let source = self.source.display();
            std::fs::write(
                self.directory.path().join(MAP_FILENAME),
                format!(
                    "# generated by `by run`\n\
                     SOURCEMAP = {{\n    \"{generated}\": (\"{source}\", [{}]),\n}}\n\n\
                     DIGESTS = {{\n    \"{generated}\": {{\"by\": \"{by}\", \"py\": \"{py}\"}},\n}}\n",
                    table.join(", ")
                ),
            )
            .expect("the map is written");
        }

        fn load(&self) -> Result<SourceMap, MapError> {
            SourceMap::load(self.directory.path())
        }

        fn loaded(&self) -> SourceMap {
            self.load().expect("the map this test wrote loads")
        }
    }

    fn read(path: &Path) -> Vec<u8> {
        std::fs::read(path).expect("a file this test just wrote")
    }

    /// a three-line prelude and then five `.by` lines, one of which became two
    fn ordinary() -> Build {
        Build::new(
            "a\nb\nc\nd\ne\n",
            "import x\nimport y\nimport z\nA\nB\nB2\nC\nD\n",
            &[
                None,
                None,
                None,
                Some(0),
                Some(1),
                Some(1),
                Some(2),
                Some(3),
            ],
        )
    }

    #[test]
    fn a_generated_line_resolves_to_the_by_line_it_came_from() {
        let build = ordinary();
        let map = build.loaded();

        let located = map
            .to_source(&build.generated, 4)
            .expect("generated line 4 came from the first `.by` line");
        assert_eq!(located.line, 1);
        assert_eq!(located.file, build.source.canonicalize().expect("on disk"));
    }

    #[test]
    fn a_generated_line_the_transpiler_invented_is_refused_rather_than_attributed() {
        // the whole rule in one test. lines 1 to 3 are prelude, and the nearest
        // `.by` line to them is line 1 — which is exactly the answer a fallback
        // would give and exactly the answer that would be a lie
        let build = ordinary();
        let map = build.loaded();

        for line in 1..=3 {
            let refusal = map
                .to_source(&build.generated, line)
                .expect_err("a prelude line has no `.by` behind it");
            assert!(
                matches!(refusal, Unmapped::NoSourceLine { .. }),
                "expected no source line for generated line {line}, got {refusal:?}"
            );
            let said = refusal.to_string();
            assert!(
                said.contains("demo.py"),
                "the refusal names the file: {said}"
            );
        }
    }

    #[test]
    fn a_line_past_the_end_of_what_the_map_covers_is_refused() {
        let build = ordinary();
        let map = build.loaded();

        let refusal = map
            .to_source(&build.generated, 9)
            .expect_err("the map covers eight lines");
        assert!(matches!(
            refusal,
            Unmapped::PastTheEnd {
                covered: 8,
                line: 9,
                ..
            }
        ));
    }

    #[test]
    fn a_by_line_resolves_to_the_first_generated_line_it_became() {
        let build = ordinary();
        let map = build.loaded();

        // `.by` line 2 became generated lines 5 and 6. a stop on the second
        // would land in the middle of the statement the first one starts
        let located = map
            .to_generated(&build.source, 2)
            .expect("the second `.by` line was generated");
        assert_eq!(located.line, 5);
        assert_eq!(
            located.file,
            build.generated.canonicalize().expect("on disk")
        );
    }

    #[test]
    fn a_by_line_nothing_was_generated_for_moves_to_the_next_one_that_was() {
        // a blank line and a comment generate nothing, and a breakpoint on one
        // moves forward exactly as it does in ordinary python. what makes that
        // honest is that the caller maps the answer back and says where it went
        let build = Build::new("a\n\n\nb\n", "A\nB\n", &[Some(0), Some(3)]);
        let map = build.loaded();

        let located = map
            .to_generated(&build.source, 2)
            .expect("line 4 is the next `.by` line anything was generated for");
        assert_eq!(located.line, 2);
        assert_eq!(map.to_source(&located.file, 2).expect("and back").line, 4);
    }

    #[test]
    fn a_by_line_past_everything_the_transpiler_generated_is_refused() {
        let build = ordinary();
        let map = build.loaded();

        let refusal = map
            .to_generated(&build.source, 5)
            .expect_err("nothing was generated for the fifth `.by` line");
        let Unmapped::NoGeneratedLine { last_mapped, .. } = &refusal else {
            panic!("expected no generated line, got {refusal:?}")
        };
        assert_eq!(*last_mapped, Some(4));
        assert!(refusal.to_string().contains("line 4"), "{refusal}");
    }

    #[test]
    fn a_file_the_map_says_nothing_about_is_refused_rather_than_passed_through() {
        let build = ordinary();
        let map = build.loaded();
        let stranger = build.directory.path().join("other.py");
        std::fs::write(&stranger, "x = 1\n").expect("a file to ask about");

        assert!(matches!(
            map.to_source(&stranger, 1),
            Err(Unmapped::NotInTheMap { .. })
        ));
        assert!(matches!(
            map.to_generated(&stranger, 1),
            Err(Unmapped::NotInTheMap { .. })
        ));
    }

    #[test]
    fn a_by_edited_since_the_transpile_refuses_the_whole_map() {
        let build = ordinary();
        std::fs::write(&build.source, "a\nb\nc\nd\ne\nf\n").expect("the user edits their file");

        let error = build.load().expect_err("the map no longer describes it");
        let MapError::Stale { changed, .. } = &error else {
            panic!("expected a stale map, got {error:?}")
        };
        assert_eq!(changed, std::slice::from_ref(&build.source));
        let said = error.to_string();
        assert!(said.contains("demo.by"), "{said}");
        assert!(said.contains("transpile again"), "{said}");
    }

    #[test]
    fn a_generated_python_that_is_not_what_was_written_refuses_the_whole_map() {
        let build = ordinary();
        std::fs::write(&build.generated, "print('something else')\n").expect("a stale build");

        let error = build.load().expect_err("the generated python moved");
        assert!(matches!(error, MapError::Stale { .. }), "{error:?}");
    }

    #[test]
    fn both_files_moving_are_both_named() {
        let build = ordinary();
        std::fs::write(&build.source, "changed\n").expect("an edit");
        std::fs::write(&build.generated, "changed\n").expect("another");

        let error = build.load().expect_err("both moved");
        let MapError::Stale { changed, .. } = &error else {
            panic!("expected a stale map, got {error:?}")
        };
        assert_eq!(changed.len(), 2, "{changed:?}");
        let said = error.to_string();
        assert!(said.contains("2 files have changed"), "{said}");
    }

    #[test]
    fn a_digest_algorithm_this_reader_cannot_recompute_is_refused_not_skipped() {
        let build = ordinary();
        build.write_map(
            &[
                None,
                None,
                None,
                Some(0),
                Some(1),
                Some(1),
                Some(2),
                Some(3),
            ],
            Some("blake3:00"),
            None,
        );

        let error = build.load().expect_err("bpd cannot recompute blake3");
        let MapError::UnknownDigest { digest, .. } = &error else {
            panic!("expected an unknown digest, got {error:?}")
        };
        assert_eq!(digest, "blake3:00");
        assert!(error.to_string().contains("update bpd"), "{error}");
    }

    #[test]
    fn a_directory_with_no_map_in_it_says_what_writes_one() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let error = SourceMap::load(directory.path()).expect_err("there is no map");
        let said = error.to_string();
        assert!(said.contains("by run"), "{said}");
        assert!(said.contains(MAP_FILENAME), "{said}");
    }

    #[test]
    fn an_entry_with_no_digest_beside_it_is_refused() {
        let build = ordinary();
        let generated = build.generated.display();
        let source = build.source.display();
        std::fs::write(
            build.directory.path().join(MAP_FILENAME),
            format!(
                "SOURCEMAP = {{\n    \"{generated}\": (\"{source}\", [None]),\n}}\n\
                 DIGESTS = {{}}\n"
            ),
        )
        .expect("a map with nothing to check it by");

        let error = build.load().expect_err("nothing can verify that entry");
        assert!(
            matches!(error, MapError::UndigestedEntry { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn the_map_a_real_by_run_writes_is_the_shape_this_reads() {
        // pinned against output captured from `by run` itself, so a change to
        // the emitter's format fails here rather than in front of a user. the
        // paths are rewritten to files this test makes, because the ones in a
        // real map are in a temporary directory that has been deleted — the
        // *shape* is what is being pinned, and the digests are checked against
        // disk by everything else in this file
        const REAL: &str = "\
# generated by `by run` — maps transpiled python frames to .by source
SOURCEMAP = {
    \"<gen>\": (\"<src>\", [None, None, None, None, None, None, None, None, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]),
}

# sha-256 of the two files each SOURCEMAP entry describes, over the bytes
# the transpiler read and wrote. recompute both from disk before trusting a
# mapped line: a mismatch means the file is no longer the one mapped
DIGESTS = {
    \"<gen>\": {\"by\": \"<byd>\", \"py\": \"<pyd>\"},
}
";
        let build = ordinary();
        std::fs::write(
            build.directory.path().join(MAP_FILENAME),
            REAL.replace("<gen>", &build.generated.display().to_string())
                .replace("<src>", &build.source.display().to_string())
                .replace(
                    "<byd>",
                    &format!("{ALGORITHM}:{}", digest_of(&read(&build.source))),
                )
                .replace(
                    "<pyd>",
                    &format!("{ALGORITHM}:{}", digest_of(&read(&build.generated))),
                ),
        )
        .expect("the captured map is written");

        let map = build.loaded();
        // generated line 9 is the first with a `.by` line behind it, and that
        // is the first line of the file — an eight line prelude, which is why
        // the offset is not something to assume
        assert_eq!(map.to_source(&build.generated, 9).expect("mapped").line, 1);
        assert!(matches!(
            map.to_source(&build.generated, 8),
            Err(Unmapped::NoSourceLine { .. })
        ));
    }
}
