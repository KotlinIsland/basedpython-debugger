//! `bpd cache` driven as a real process, over a cache of the test's own
//!
//! the engine's tests cover what reading and clearing a cache do to a
//! directory. this covers what a user runs: the binary, what it prints, and —
//! the part that matters most — its **exit code**, since a clear that could not
//! remove an entry has to be distinguishable from one that did, by a script as
//! well as by a person
//!
//! the cache is redirected with the same environment variables the resolution
//! itself reads, rather than with a flag that would exist for the test's
//! benefit. both are set, because which one is read is the platform's business
//!
//! there are **two** caches under that home — the agents, and the hook a child
//! that was `exec`'d is entered through — and the command is about both. what is
//! asserted per root is what differs between them: an entry of the second gains
//! a `__pycache__` the first time an interpreter imports the hook, which is the
//! one thing in either cache that bpd did not write

use std::path::{Path, PathBuf};
use std::process::Command;

/// the binary this test run built, not whatever `bpd` is on PATH
const BPD: &str = env!("CARGO_BIN_EXE_bpd");

struct Run {
    success: bool,
    stdout: String,
    stderr: String,
}

/// a home for a cache, and the agent directory `bpd` will resolve inside it
struct Home {
    directory: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().expect("a temporary directory can be made"),
        }
    }

    /// where `bpd` resolves its agent cache to, given this as the cache home
    fn cache(&self) -> PathBuf {
        self.directory.path().join("bpd").join("agents")
    }

    /// and the other one, holding the hook an `exec`'d child is entered through
    fn children(&self) -> PathBuf {
        self.directory.path().join("bpd").join("children")
    }

    /// put the child hook in it, the way a launch does, and say where
    fn stage_hook(&self) -> PathBuf {
        let staged = bpd_engine::agent::stage_child_hook_into(&self.children())
            .unwrap_or_else(|error| panic!("could not stage the child hook: {error}"));
        self.children().join(digest_of(staged.python_path()))
    }

    /// put the built agent in it, the way a launch would, and say where
    ///
    /// under the cache as `bpd` will spell it. `Staged` canonicalises its
    /// answer, because a debuggee reports a resolved `sys.path` and on macos a
    /// temporary directory is under a `/var` that is a link to `/private/var` —
    /// the same entry, named the way the report will name it
    fn stage(&self) -> PathBuf {
        let staged = bpd_engine::agent::stage_for_into(
            &self.cache(),
            bpd_test::agent::matching_interpreter(),
        )
        .unwrap_or_else(|error| panic!("could not stage the agent: {error}"));
        self.cache().join(digest_of(staged.python_path()))
    }

    fn run(&self, arguments: &[&str]) -> Run {
        let output = Command::new(BPD)
            .arg("cache")
            .args(arguments)
            // unix reads the first and windows the second. setting both means
            // no test here has a platform it silently does not cover
            .env("XDG_CACHE_HOME", self.directory.path())
            .env("LOCALAPPDATA", self.directory.path())
            .output()
            .expect("the binary was built by the same cargo invocation as this test");

        Run {
            success: output.status.success(),
            stdout: String::from_utf8(output.stdout).expect("bpd writes utf8"),
            stderr: String::from_utf8(output.stderr).expect("bpd writes utf8"),
        }
    }
}

/// the entry directory's name, which is the digest the report has to print
fn digest_of(entry: &Path) -> String {
    entry
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .expect("a staged entry is named after its digest")
        .to_owned()
}

#[test]
fn a_cache_that_is_not_there_is_said_plainly_and_is_not_a_failure() {
    let home = Home::new();
    let run = home.run(&[]);

    assert!(
        run.success,
        "an empty cache is the ordinary state of a machine, not an error\n{}",
        run.stderr
    );
    assert!(run.stdout.contains("it is not there"), "{}", run.stdout);
    assert!(
        !home.cache().exists(),
        "asking about the cache must not create it"
    );
}

#[test]
fn the_report_names_the_path_the_entries_and_the_entry_the_built_agent_is_in() {
    let home = Home::new();
    let entry = home.stage();
    let run = home.run(&[]);

    assert!(run.success, "{}", run.stderr);
    assert!(
        run.stdout.contains(&home.cache().display().to_string()),
        "the report has to name the directory it is about\n{}",
        run.stdout
    );
    assert!(run.stdout.contains("entries      1"), "{}", run.stdout);
    assert!(
        run.stdout.contains(&digest_of(&entry)),
        "the report has to name the entry the built agent is in\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("staged,"),
        "and say that it is the one that is there\n{}",
        run.stdout
    );
}

#[test]
fn clearing_takes_the_entry_and_says_what_it_reclaimed() {
    let home = Home::new();
    let entry = home.stage();
    let held = std::fs::metadata(entry.join(if cfg!(windows) {
        "bpd_agent.pyd"
    } else {
        "bpd_agent.so"
    }))
    .expect("the staged module is there")
    .len();

    let run = home.run(&["clear"]);

    assert!(run.success, "{}", run.stderr);
    assert!(
        run.stdout.contains("removed      1 entry"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains(&format!("{held} bytes")),
        "the exact number of bytes reclaimed is the number `du` would have \
         shown\n{}",
        run.stdout
    );
    assert!(!entry.exists(), "the entry is really gone");
}

#[test]
fn keeping_the_current_entry_leaves_exactly_that_one() {
    let home = Home::new();
    let entry = home.stage();

    let run = home.run(&["clear", "--keep-current"]);

    assert!(run.success, "{}", run.stderr);
    assert!(
        run.stdout
            .contains(&format!("kept         {}", digest_of(&entry))),
        "{}",
        run.stdout
    );
    assert!(
        entry.is_dir(),
        "the entry a launch would import is still there"
    );
}

#[test]
fn something_bpd_never_wrote_stops_it_and_is_named() {
    let home = Home::new();
    let entry = home.stage();
    let intruder = home.cache().join("something_else");
    std::fs::write(&intruder, "not bpd's").expect("the file can be written");

    let reported = home.run(&[]);
    assert!(
        !reported.success,
        "a cache holding something unaccounted for is not a clean report\n{}",
        reported.stdout
    );
    assert!(
        reported.stdout.contains(&intruder.display().to_string()),
        "the report has to name what it found\n{}",
        reported.stdout
    );

    let refused = home.run(&["clear"]);
    assert!(!refused.success, "{}", refused.stdout);
    assert!(
        refused.stderr.contains(&intruder.display().to_string()),
        "the refusal has to name what stopped it\n{}",
        refused.stderr
    );
    assert!(intruder.is_file(), "the stray file is never removed");
    assert!(entry.is_dir(), "and nothing is removed around it either");
}

/// the hook a child is entered through is cached the same way and was reported
/// by nothing, which is the same unbounded growth by the side door
#[test]
fn the_report_names_both_caches_and_the_entry_this_bpd_stages_into_each() {
    let home = Home::new();
    let agent = home.stage();
    let hook = home.stage_hook();
    let run = home.run(&[]);

    assert!(run.success, "{}", run.stderr);
    for (root, entry) in [(home.cache(), &agent), (home.children(), &hook)] {
        assert!(
            run.stdout.contains(&root.display().to_string()),
            "the report has to name `{}`\n{}",
            root.display(),
            run.stdout
        );
        assert!(
            run.stdout.contains(&digest_of(entry)),
            "and the entry in it that this bpd stages into\n{}",
            run.stdout
        );
    }
    assert!(
        run.stdout.contains("sitecustomize"),
        "a person reading two directories has to be told what the second one \
         holds\n{}",
        run.stdout
    );
}

/// the entry an interpreter has imported the hook out of holds the bytecode it
/// compiled as well, and it is the removal that has to survive that: bpd never
/// wrote the `__pycache__`, and refusing to clear over one would leave this
/// cache growing on every machine that has ever debugged a child
#[test]
fn clearing_takes_the_child_hook_and_the_bytecode_an_interpreter_left_beside_it() {
    let home = Home::new();
    let hook = home.stage_hook();

    let interpreter = &bpd_test::agent::matching_interpreter().executable;
    let ran = Command::new(interpreter)
        .env("PYTHONPATH", &hook)
        .args(["-c", "pass"])
        .output()
        .unwrap_or_else(|error| panic!("could not run `{}`: {error}", interpreter.display()));
    assert!(
        ran.status.success(),
        "the staged hook did not import: {}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert!(
        hook.join("__pycache__").is_dir(),
        "cpython caches the bytecode of a module it imports beside the source, \
         and this test is about clearing over that"
    );

    let run = home.run(&["clear"]);

    assert!(run.success, "{}\n{}", run.stdout, run.stderr);
    assert!(!hook.exists(), "the entry is really gone, bytecode and all");
    assert!(
        home.children().is_dir(),
        "the cache directory itself is not an entry, and the next launch stages \
         into it"
    );
}

#[test]
fn keeping_the_current_entries_leaves_the_hook_as_well_as_the_agent() {
    let home = Home::new();
    let agent = home.stage();
    let hook = home.stage_hook();

    let run = home.run(&["clear", "--keep-current"]);

    assert!(run.success, "{}", run.stderr);
    for entry in [&agent, &hook] {
        assert!(
            run.stdout
                .contains(&format!("kept         {}", digest_of(entry))),
            "`{}` is what this bpd stages, and was not said to be kept\n{}",
            entry.display(),
            run.stdout
        );
        assert!(entry.is_dir(), "and it is still there");
    }
}

/// the refusal is about **a directory** — one with a surprise in it may not be
/// the one bpd thinks it is — so it stops that one and not the other. the exit
/// code still says the answer is not the whole answer
#[test]
fn something_bpd_never_wrote_in_one_cache_does_not_stop_the_other() {
    let home = Home::new();
    let agent = home.stage();
    let hook = home.stage_hook();
    let intruder = home.children().join("notes.txt");
    std::fs::write(&intruder, "not bpd's").expect("the file can be written");

    let run = home.run(&["clear"]);

    assert!(
        !run.success,
        "a cache that was not cleared is not a success\n{}",
        run.stdout
    );
    assert!(
        run.stderr.contains(&intruder.display().to_string()),
        "the refusal has to name what stopped it\n{}",
        run.stderr
    );
    assert!(intruder.is_file(), "the stray is never removed");
    assert!(hook.is_dir(), "and nothing is removed around it");
    assert!(
        !agent.exists(),
        "the other cache holds nothing unaccounted for, and one directory's \
         surprise is not a reason to leave every entry of another one where it \
         is"
    );
}

/// the failure this is really about is windows refusing to delete a shared
/// object a debuggee has loaded, which cannot be produced here. an entry
/// nothing can be removed from is the same shape, and what is asserted is the
/// part a script depends on: the entry is named, and the command does **not**
/// report success
#[cfg(unix)]
#[test]
fn an_entry_that_will_not_go_is_named_and_the_command_fails() {
    use std::os::unix::fs::PermissionsExt as _;

    let home = Home::new();
    let entry = home.stage();
    std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o500))
        .expect("the mode can be set");

    let run = home.run(&["clear"]);
    std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o700))
        .expect("the mode can be set");

    assert!(
        !run.success,
        "an entry that is still there must never be reported as cleared\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains(&entry.display().to_string()),
        "the entry that would not go has to be named\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("Permission denied"),
        "and so has the reason\n{}",
        run.stdout
    );
    assert!(
        entry.is_dir(),
        "it is still there, which is what was reported"
    );
}
