//! running a real program under a real interpreter, and observing what it saw
//!
//! this is the half of the harness that does not need a debugger yet. it lays
//! out a fixture, runs it the three ways cpython can be entered, and returns a
//! structured record of what the program observed about its own launch
//!
//! that record is the thing `bpd launch` has to reproduce exactly. running
//! under a debugger must be indistinguishable from running directly, and the
//! fields here are the ones that give it away — a program that sees a different
//! `sys.path[0]` imports different modules, which is how "it only fails under
//! the debugger" reports happen
//!
//! the tests over this module pin **cpython's** behaviour rather than `bpd`'s.
//! that is deliberate: the parity comparison needs a recorded baseline to
//! compare against, and writing it down before the feature exists is the same
//! discipline as starting a bug fix with a failing test

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use bpd_core::python::Capabilities;

/// the program that reports what it can see about its own launch
///
/// on disk rather than in a string literal so it stays real, lintable python
pub const LAUNCH_PROBE: &str = include_str!("../resources/launch_probe.py");

/// how the interpreter is entered
///
/// the three forms differ in ways user code can observe, and none of them is a
/// special case of another
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// `python <path to the file> <args>`
    Script,
    /// `python -m <module> <args>`
    Module,
    /// `python -c <the file's source> <args>`
    Command,
}

/// a program on disk, in a directory of its own
#[derive(Debug)]
pub struct Fixture {
    /// held so the directory outlives the fixture, not read directly
    _directory: tempfile::TempDir,
    canonical: PathBuf,
    module: String,
    source: String,
}

impl Fixture {
    /// write `source` as `<module>.py` in a fresh directory
    ///
    /// # panics
    ///
    /// if the directory or the file cannot be created. a fixture that does not
    /// exist would make every test over it assert against nothing
    pub fn new(module: &str, source: &str) -> Self {
        let directory = tempfile::tempdir().expect("a temporary directory is available");
        let path = directory.path().join(format!("{module}.py"));
        let mut file = std::fs::File::create(&path)
            .unwrap_or_else(|error| panic!("could not create {}: {error}", path.display()));
        file.write_all(source.as_bytes())
            .unwrap_or_else(|error| panic!("could not write {}: {error}", path.display()));

        // canonicalised because cpython reports a resolved `sys.path[0]`, and
        // the temporary directory sits under a symlink on macos
        // (`/var` -> `/private/var`). comparing the two spellings would fail
        // for a reason that has nothing to do with launch parity
        let canonical = directory
            .path()
            .canonicalize()
            .expect("the directory was just created");

        Self {
            _directory: directory,
            canonical,
            module: module.to_string(),
            source: source.to_string(),
        }
    }

    /// a fixture holding [`LAUNCH_PROBE`]
    pub fn launch_probe() -> Self {
        Self::new("launch_probe", LAUNCH_PROBE)
    }

    /// write another module beside this one and return where it landed
    ///
    /// the fixture's directory is `sys.path[0]` for the script form, so a
    /// sibling is importable by name. that is what makes it possible to test a
    /// breakpoint in a module the program has not imported *yet*
    ///
    /// # panics
    ///
    /// if the file cannot be written. a fixture that does not exist would make
    /// every test over it assert against nothing
    pub fn sibling(&self, module: &str, source: &str) -> PathBuf {
        let path = self.directory().join(format!("{module}.py"));
        std::fs::write(&path, source)
            .unwrap_or_else(|error| panic!("could not write {}: {error}", path.display()));
        path
    }

    /// the directory the program lives in, resolved
    pub fn directory(&self) -> &Path {
        &self.canonical
    }

    /// the program's path on disk
    pub fn path(&self) -> PathBuf {
        self.directory().join(format!("{}.py", self.module))
    }

    /// run the fixture with its own directory as the working directory
    pub fn run(&self, interpreter: &Capabilities, form: Form, args: &[&str]) -> Run {
        self.run_in(self.directory(), interpreter, form, args)
    }

    /// run the fixture from a chosen working directory
    ///
    /// the working directory is what distinguishes "the script's directory"
    /// from "the current directory" in `sys.path[0]`, and the two coincide
    /// often enough that a test which never separates them proves nothing
    ///
    /// # panics
    ///
    /// if the interpreter cannot be spawned, or writes non-utf8
    pub fn run_in(
        &self,
        working_directory: &Path,
        interpreter: &Capabilities,
        form: Form,
        args: &[&str],
    ) -> Run {
        let mut command = Command::new(&interpreter.executable);
        command.current_dir(working_directory);

        // never `-I` or `-E` here. isolated mode drops the script's directory
        // from `sys.path`, which is one of the exact values under test
        match form {
            Form::Script => {
                command.arg(self.path());
            }
            Form::Module => {
                command.args(["-m", &self.module]);
            }
            Form::Command => {
                command.args(["-c", &self.source]);
            }
        }
        command.args(args);

        let output = command.output().unwrap_or_else(|error| {
            panic!(
                "could not run `{}`: {error}",
                interpreter.executable.display()
            )
        });

        Run {
            exit_code: output.status.code(),
            success: output.status.success(),
            stdout: String::from_utf8(output.stdout).expect("the fixture writes utf8"),
            stderr: String::from_utf8(output.stderr).expect("cpython writes utf8 to stderr"),
        }
    }
}

/// what came back from running a fixture
#[derive(Debug, Clone)]
pub struct Run {
    /// the process exit code, or `None` when a signal ended it
    pub exit_code: Option<i32>,
    /// whether the process exited successfully
    pub success: bool,
    /// everything the program wrote to stdout
    pub stdout: String,
    /// everything the program wrote to stderr
    pub stderr: String,
}

impl Run {
    /// parse the launch probe's report
    ///
    /// # panics
    ///
    /// if the program failed or did not print the expected report. either means
    /// the ground truth was never established, and asserting against it would
    /// be asserting against nothing
    pub fn observed(&self) -> Observed {
        assert!(
            self.success,
            "the fixture exited with {:?}\nstdout:\n{}\nstderr:\n{}",
            self.exit_code, self.stdout, self.stderr
        );

        serde_json::from_str(&self.stdout).unwrap_or_else(|error| {
            panic!(
                "the fixture did not print a launch report: {error}\nstdout:\n{}\nstderr:\n{}",
                self.stdout, self.stderr
            )
        })
    }
}

/// what a program could see about how it was launched
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Observed {
    /// `sys.argv`
    pub argv: Vec<String>,
    /// `sys.path[0]`, the entry that decides what a bare import finds first
    pub path0: String,
    /// `__name__`, which is `__main__` for all three forms
    pub name: String,
    /// `__main__.__file__`, absent under `-c`
    pub file: Option<String>,
    /// `__main__.__package__`
    pub package: Option<String>,
    /// `__main__.__spec__.name`, set only under `-m`
    pub spec: Option<String>,
    /// `sys.executable`, which a re-exec would change
    pub executable: String,
}
