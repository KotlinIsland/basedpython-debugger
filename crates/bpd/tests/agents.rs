//! one `bpd`, several agents, and each interpreter getting its own
//!
//! the agent is a cpython extension and is not abi3, so an installed `bpd`
//! carries one build per interpreter tag and has to choose between them. this
//! is that choice, driven the way a user meets it: a `bpd` binary in a
//! directory of its own, an `agents/<tag>/` layout beside it, and a real launch
//! of a real interpreter through it. nothing here asserts on a path string —
//! what is asserted is which file the interpreter **loaded**
//!
//! the layout is written through
//! [`bpd_engine::agent::published_at`](bpd_engine::agent::published_at), which
//! is the same answer the resolution reads back. spelling it out a second time
//! here would be a test of one restatement against another
//!
//! only one of the tags can hold a real agent: `cargo test` builds the agent
//! against one interpreter, and a test cannot build three more. the others hold
//! bytes that are not an agent — which is what makes the negative half exact,
//! since the entry a launch stages into is named after the sha-256 of what it
//! staged. an interpreter that reached the file under another tag's directory
//! would name another entry

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use bpd_core::python::{Capabilities, InterpreterTag};

/// the binary this test run built, which is what gets installed into a fixture
const BPD: &str = env!("CARGO_BIN_EXE_bpd");

/// what the program prints if it ever gets to run
///
/// nothing else in the output can produce it, so its absence is evidence rather
/// than an assumption about what an empty stdout means
const ANNOUNCED: &str = "the-program-ran";

/// a tag no interpreter has, and never will
///
/// the refusal is about the **list** of tags carried. a fixture built out of the
/// tags this machine happens to have would mean something different on a
/// machine with different interpreters, and would say nothing at all on one
/// with a single interpreter
const NEVER: [&str; 2] = ["3.99", "3.99t"];

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// a `bpd` installed in a directory of its own, carrying what the test says
///
/// the binary goes in `bin/` and the agents beside it, which is the pair the
/// resolution looks in — an installed `<prefix>/bin/bpd` and `<prefix>`
struct Install {
    directory: tempfile::TempDir,
}

impl Install {
    fn new() -> Self {
        let install = Self {
            directory: tempfile::tempdir().expect("a temporary directory can be made"),
        };
        std::fs::create_dir_all(install.directory.path().join("bin"))
            .expect("the directory can be made");
        std::fs::copy(BPD, install.bpd()).expect("the binary this test run built can be copied");
        install
    }

    fn bpd(&self) -> PathBuf {
        self.directory
            .path()
            .join("bin")
            .join(if cfg!(windows) { "bpd.exe" } else { "bpd" })
    }

    /// where the cache this install stages into lands
    ///
    /// its own, so a launch here neither reads nor writes the user's — and so
    /// the entries named below are entries this test made
    fn cache_home(&self) -> PathBuf {
        self.directory.path().join("cache")
    }

    /// carry these bytes as the agent for a tag
    fn carries(&self, tag: &str, bytes: &[u8]) {
        let tag = InterpreterTag::parse(tag).expect("the test names a real tag");
        let artifact = bpd_engine::agent::published_at(self.directory.path(), tag);
        std::fs::create_dir_all(
            artifact
                .parent()
                .expect("a published agent is a file in a directory"),
        )
        .expect("the directory can be made");
        std::fs::write(&artifact, bytes).expect("the agent can be written");
    }

    /// run a program under this install, on the interpreter named
    fn launch(&self, interpreter: &Capabilities, program: &Path) -> Output {
        Command::new(self.bpd())
            .arg("launch")
            .arg("--python")
            .arg(&interpreter.executable)
            .arg(program)
            // unix reads the first and windows the second. what a launch stages
            // goes here rather than into the user's own cache
            .env("XDG_CACHE_HOME", self.cache_home())
            .env("LOCALAPPDATA", self.cache_home())
            .output()
            .expect("the binary was built by the same cargo invocation as this test")
    }
}

/// the agent this test run built, as bytes
///
/// read back out of a staged entry rather than found in the build tree: staging
/// is public, and what it holds is exactly the artifact
fn built_agent() -> Vec<u8> {
    let staged = bpd_test::agent::staged_for(interpreter());
    let module = staged.python_path().join(if cfg!(windows) {
        "bpd_agent.pyd"
    } else {
        "bpd_agent.so"
    });
    std::fs::read(&module).expect("the staged agent can be read")
}

/// bytes that are not an agent, distinct for every tag
///
/// distinct because the entry they stage into is named after them: two tags
/// holding the same bytes would stage into one entry, and the failure could not
/// say which directory it came from
fn not_an_agent(tag: InterpreterTag) -> Vec<u8> {
    format!("this is not an agent, and it is the one under `{tag}`").into_bytes()
}

/// the cache entry bytes stage into, which is the sha-256 of them
fn entry_of(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        write!(hex, "{byte:02x}").expect("writing to a string cannot fail");
    }
    hex
}

fn program() -> bpd_test::debuggee::Fixture {
    bpd_test::debuggee::Fixture::new("announces", &format!("print('{ANNOUNCED}')\n"))
}

#[test]
fn each_interpreter_is_launched_with_the_agent_carried_for_its_own_tag() {
    let install = Install::new();
    let matching = interpreter().tag();
    install.carries(&matching.to_string(), &built_agent());

    // every other interpreter on this machine gets a directory of its own,
    // holding something that is not an agent. a `bpd` that picked by anything
    // other than the tag would reach one of these
    let others: Vec<&Capabilities> = bpd_test::discovered()
        .supported()
        .into_iter()
        .filter(|candidate| candidate.tag() != matching)
        .collect();
    for other in &others {
        install.carries(&other.tag().to_string(), &not_an_agent(other.tag()));
    }

    let fixture = program();
    let ran = install.launch(interpreter(), &fixture.path());
    assert!(
        ran.status.success(),
        "python {matching} was not launched with the agent carried for it\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
    assert!(
        String::from_utf8_lossy(&ran.stdout).contains(ANNOUNCED),
        "the program did not run\nstdout:\n{}",
        String::from_utf8_lossy(&ran.stdout)
    );

    // and the other half: each of the others reached the file under **its own**
    // tag, which is what the entry named in the failure says. on a machine with
    // one interpreter there is nothing here to iterate, and the assertion above
    // is the whole of what this run proves
    for other in &others {
        let failed = install.launch(other, &fixture.path());
        let said = String::from_utf8_lossy(&failed.stderr);

        assert!(
            !failed.status.success(),
            "python {} was launched with something that is not an agent\nstderr:\n{said}",
            other.tag()
        );
        assert!(
            !String::from_utf8_lossy(&failed.stdout).contains(ANNOUNCED),
            "the program ran under an agent that never loaded"
        );
        assert!(
            said.contains(&entry_of(&not_an_agent(other.tag()))),
            "python {} had to be given the file under `{}`, and the entry it \
             staged says otherwise\nstderr:\n{said}",
            other.tag(),
            other.tag()
        );
    }
}

#[test]
fn an_interpreter_no_agent_is_carried_for_is_refused_naming_the_tags_that_are() {
    let install = Install::new();
    for tag in NEVER {
        install.carries(tag, b"an agent for an interpreter nobody has");
    }

    let fixture = program();
    let refused = install.launch(interpreter(), &fixture.path());
    let said = String::from_utf8_lossy(&refused.stderr);

    assert!(
        !refused.status.success(),
        "an interpreter with no agent for it has to be refused\nstderr:\n{said}"
    );
    assert!(
        !String::from_utf8_lossy(&refused.stdout).contains(ANNOUNCED),
        "the program ran before the refusal, which is the one thing a refusal \
         must not allow"
    );
    assert!(
        said.contains(&interpreter().interpreter.display().to_string()),
        "the refusal has to name the interpreter it is about\nstderr:\n{said}"
    );
    assert!(
        said.contains(&format!("`{}`", interpreter().tag())),
        "and the tag that interpreter needs\nstderr:\n{said}"
    );
    for present in NEVER {
        assert!(
            said.contains(&format!("`{present}`")),
            "and the tags that are carried, of which `{present}` is \
             one\nstderr:\n{said}"
        );
    }
    assert!(
        said.contains("cargo build -p bpd_agent"),
        "and what to do about it\nstderr:\n{said}"
    );
}

#[test]
fn a_bpd_carrying_no_agent_at_all_says_so_and_says_where_it_looked() {
    let install = Install::new();

    let fixture = program();
    let refused = install.launch(interpreter(), &fixture.path());
    let said = String::from_utf8_lossy(&refused.stderr);

    assert!(
        !refused.status.success(),
        "a bpd with no agent in it cannot launch anything\nstderr:\n{said}"
    );
    assert!(
        !String::from_utf8_lossy(&refused.stdout).contains(ANNOUNCED),
        "the program ran before the refusal"
    );
    assert!(said.contains("no agent build at all"), "stderr:\n{said}");
    assert!(
        said.contains(&install.directory.path().join("bin").display().to_string()),
        "the refusal has to name where it looked\nstderr:\n{said}"
    );
}
