//! a basedpython build directory, written rather than transpiled
//!
//! basedpython transpiles `.by` to `.py` and the interpreter runs the `.py`, so
//! every test about a `.by` location needs three files that agree: the source, the
//! generated python, and the `_by_sourcemap.py` between them whose digests are
//! true of both.
//!
//! ## why the pairs are written rather than transpiled
//!
//! a source map is a claim about two files and a line table between them, and the
//! digests are what make the claim checkable. nothing about `bpd` reading one
//! depends on the transpiler having produced it — the same file written by hand
//! with true digests **is** a valid map, and the format it has to be in is pinned
//! separately against output captured from `by run` itself, in
//! `bpd_core::source_map`.
//!
//! what writing them buys is the line table. the cases that decide whether the
//! source mapping rule holds — a generated line the transpiler invented, a `.by`
//! line nothing was generated for, a `.by` edited since the build — are each one
//! entry in that table, and reaching them through a real `by` would mean finding a
//! basedpython program that happens to produce each one and then depending on it
//! going on producing it. the tests depend on the *rule* instead.
//!
//! it also means they run wherever `cargo test` runs. the `by` binary is a sibling
//! repository rather than a package this suite installs, and a test that is
//! skipped when it is missing is a test that reports success while proving
//! nothing.
//!
//! ## the runner is captured rather than written
//!
//! what the interpreter runs is not the generated python as python's loader would
//! compile it. `by run` starts [`RUNNER`], its `_by_runner.py`, and that compiles
//! each staged module **as its `.by`** — named after the `.by` path, every line the
//! `.by` line it came from — so the locations a debugger reads out of a build are
//! the runner's doing. a runner written here would be a claim about that
//! construction; the one `by run` writes is the construction. it is kept byte for
//! byte as `by run` wrote it, and a `by` that changes it is a `by` whose builds
//! this suite no longer describes until it is captured again.
//!
//! ## it is here rather than beside one test
//!
//! two suites ask about `.by` locations from the two ends: `bpd_engine` asks the
//! engine directly, and `bpd`'s DAP acceptance drives the adapter as a process
//! over the wire. a build they each wrote for themselves would be two fixtures
//! that could disagree about what `by run` leaves on disk, which is the one thing
//! neither of them is testing.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use bpd_core::source_map::MAP_FILENAME;

/// `_by_runner.py`, as `by run` writes it into every build it starts
///
/// captured from `by` `df2a124c2087`, where it is `BY_RUNNER_SRC` in
/// `crates/ty/src/by_commands.rs`
pub const RUNNER: &str = include_str!("by_runner.py");

/// the `.by` a person wrote
///
/// what is in it barely matters — the interpreter never reads it — but it is
/// real basedpython, and `line_of` is what names a line of it rather than a
/// number that goes stale
pub const SOURCE: &str = "\
def add(a: int, b: int) -> int:
    total = a + b
    return total


# a comment, which the transpile does not keep
def main() -> None:
    answer = add(2, 3)
    print(answer)


main()  # entry
";

/// the python `by` would have transpiled [`SOURCE`] to
///
/// the six line prelude is the point of it, and so is the comment that is not
/// here: the offset between the two files is not a constant, and a debugger that
/// assumed one would be reporting the wrong line of the wrong file with total
/// confidence
///
/// it writes its answer to `sys.argv[1]` rather than printing it, so that a test
/// can prove the program ran past a stop from outside the protocol — see
/// [`Build::marks`]
pub const GENERATED: &str = "\
from __future__ import annotations
import os
from pathlib import Path
import sys


def add(a: int, b: int) -> int:
    total = a + b
    return total


def main() -> None:
    answer = add(2, 3)
    Path(sys.argv[1]).write_text(str(answer) + os.linesep)


main()
";

/// which `.by` line each generated line of [`GENERATED`] came from, zero-based
/// on both sides
///
/// `None` is prelude. the two `import` lines of [`SOURCE`] have no counterpart
/// here on purpose: the transpiler emits its own imports and drops the source's,
/// which is why a line table is not an offset
#[must_use]
pub fn line_table() -> Vec<Option<u32>> {
    vec![
        // the six line prelude, which no `.by` line is behind
        None,
        None,
        None,
        None,
        None,
        None,
        Some(0), // def add
        Some(1), // total = a + b
        Some(2), // return total
        Some(3),
        Some(4),
        // the `.by`'s comment is line 5 and became nothing, so the next
        // generated line skips it — this is why a line table is not an offset
        Some(6), // def main
        Some(7), // answer = add(2, 3)
        Some(8), // print(answer), which became a write_text
        Some(9),
        Some(10),
        Some(11), // main()
    ]
}

/// a basedpython build directory: the `.by`, the python, and the map
///
/// the fields are public because the tests assert about the paths themselves —
/// that a frame names the `.by` and not the `.py` is the whole point of most of
/// them — and an accessor per field would be ceremony over a fixture.
pub struct Build {
    directory: tempfile::TempDir,
    /// the `.by` a person wrote
    pub source: PathBuf,
    /// the python `by` transpiled it to, which is what the interpreter runs
    pub generated: PathBuf,
    /// where the program writes its answer, so a stop is provable
    pub marks: PathBuf,
}

/// a path as a source map carries one
///
/// the map's strings take `\\` and `\"`, and **a windows path is full of the
/// first**: written raw, `C:\Users\…` reaches bpd's parser as `\C` and the
/// whole build is refused — by name, with a line and a column, which is the
/// parser doing its job about a map this file wrote badly
fn in_a_map(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', r"\\")
        .replace('"', "\\\"")
}

impl Build {
    /// the build of [`SOURCE`], [`GENERATED`] and [`line_table`]
    #[must_use]
    pub fn demo() -> Self {
        Self::pair(SOURCE, GENERATED, &line_table())
    }

    /// a build of `by` and `py`, mapped by `lines`
    ///
    /// `lines` is which `.by` line each generated line came from, zero-based on
    /// both sides, `None` for a generated line no source line is behind.
    #[must_use]
    pub fn pair(by: &str, py: &str, lines: &[Option<u32>]) -> Self {
        let directory = tempfile::tempdir().expect("a temporary directory");
        // canonicalised for the reason every fixture in this suite is: a
        // temporary directory is under `/var` on macos and `/tmp` names it
        // through a symlink, and the map's own paths would then be a third
        // spelling of the same file
        let root = directory
            .path()
            .canonicalize()
            .expect("the directory was just made");
        let source = root.join("demo.by");
        let generated = root.join("demo.py");
        std::fs::write(&source, by).expect("the `.by` is written");
        std::fs::write(&generated, py).expect("the generated python is written");
        let build = Self {
            directory,
            source,
            generated,
            marks: root.join("answer.txt"),
        };
        build.write_map(lines);
        build
    }

    /// write `_by_sourcemap.py` with digests that are true of what is on disk
    pub fn write_map(&self, lines: &[Option<u32>]) {
        let table: Vec<String> = lines
            .iter()
            .map(|line| line.map_or_else(|| "None".to_owned(), |line| line.to_string()))
            .collect();
        std::fs::write(
            self.root().join(MAP_FILENAME),
            format!(
                "# generated by `by run` — maps transpiled python frames to .by source\n\
                 SOURCEMAP = {{\n    \"{generated}\": (\"{source}\", [{table}]),\n}}\n\n\
                 DIGESTS = {{\n    \"{generated}\": {{\"by\": \"{by}\", \"py\": \"{py}\"}},\n}}\n",
                generated = in_a_map(&self.generated),
                source = in_a_map(&self.source),
                table = table.join(", "),
                by = digest(&self.source),
                py = digest(&self.generated),
            ),
        )
        .expect("the map is written");
    }

    /// `by run`'s runner, written into the build — the program `by run` starts
    ///
    /// launched with [`Self::arguments`], it runs [`Self::module`] as
    /// `__main__` exactly as `by run` would, compiled as its `.by`. it is also
    /// what puts frames **under** the build on the stack: the runner itself and
    /// the import machinery it goes through, none of which is basedpython
    #[must_use]
    pub fn runner(&self) -> PathBuf {
        let path = self.root().join("_by_runner.py");
        std::fs::write(&path, RUNNER).expect("the runner is written");
        path
    }

    /// the module the build's `.by` is, which is what `by run` is asked to run
    #[must_use]
    pub fn module(&self) -> String {
        self.generated
            .file_stem()
            .expect("the generated python has a name")
            .to_string_lossy()
            .into_owned()
    }

    /// what [`Self::runner`] is started with to run `module`: its name, and the
    /// file the program writes its answer to
    #[must_use]
    pub fn arguments_running(&self, module: &str) -> Vec<OsString> {
        vec![module.into(), self.marks.clone().into_os_string()]
    }

    /// [`Self::arguments_running`] the build's own module
    #[must_use]
    pub fn arguments(&self) -> Vec<OsString> {
        self.arguments_running(&self.module())
    }

    /// the build directory, canonicalised — the directory `bpd` finds the map in
    #[must_use]
    pub fn root(&self) -> PathBuf {
        self.directory
            .path()
            .canonicalize()
            .expect("the directory is there for the life of the build")
    }

    /// what the program wrote, which is empty until it has run past the stop
    #[must_use]
    pub fn answer(&self) -> String {
        std::fs::read_to_string(&self.marks).unwrap_or_default()
    }
}

/// the sha-256 of a file, as `_by_sourcemap.py` writes one
fn digest(path: &Path) -> String {
    use sha2::Digest as _;
    use std::fmt::Write as _;

    let bytes = std::fs::read(path).expect("a file this fixture just wrote");
    let mut out = String::from("sha256:");
    for byte in sha2::Sha256::digest(&bytes) {
        write!(out, "{byte:02x}").expect("a `String` grows to fit");
    }
    out
}
