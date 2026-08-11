//! the agent staging cache, seen from outside a launch
//!
//! [`agent::stage`](crate::agent::stage) keeps one copy of the agent per build,
//! under `<cache>/<sha-256 of its bytes>/`, and never removes one. that is not
//! an oversight — the name being the content is what makes reuse safe — but it
//! does mean a rebuild leaves its predecessor behind for ever. on the machine
//! this was written on the cache had reached **89 entries and 448 MB**, against
//! one agent of 5.6 MB, and that number is what this module exists for
//!
//! # why nothing here happens on its own
//!
//! there is no pruning on launch, no age limit and no eviction, and the reason
//! is not caution about deleting things. it is that neither question a pruner
//! would have to answer can be answered from inside one `bpd`:
//!
//! - **what is still needed.** an entry is on the `PYTHONPATH` of every
//!   debuggee launched from it, including debuggees of another `bpd` on the
//!   same machine that this one cannot see
//! - **whether it can even be removed.** windows refuses to delete a shared
//!   object a process has loaded, which is exactly what an entry in use looks
//!   like — so a background pruner would fail there routinely, for a reason the
//!   user never asked about
//!
//! a person asking is the only thing that knows. so this reads the cache and
//! removes what it is told to remove, and does neither unless asked
//!
//! # what it will not do
//!
//! report a removal it did not make. an entry that could not be removed is
//! named, with the failure that stopped it, and the caller is left holding a
//! [`Cleared`] that says so — removing four of five entries and calling it
//! cleared is the quiet lie this project exists to not tell
//!
//! it also refuses to remove anything it does not recognise. an entry is a
//! 64 character hex directory holding the agent and nothing else, because that
//! is all staging ever writes; anything else in the cache is reported and the
//! whole operation stops, since a directory with a surprise in it is a
//! directory that may not be the one this thinks it is

use std::io;
use std::path::{Path, PathBuf};

use crate::agent;
use crate::{Error, Result};

/// how many characters a sha-256 is, written as hex
const DIGEST_LEN: usize = 64;

/// one cached agent build
#[derive(Debug, Clone)]
pub struct Entry {
    digest: String,
    path: PathBuf,
    size: u64,
}

impl Entry {
    /// the sha-256 of the agent this entry holds, which is also its name
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// the entry directory itself, which is what goes on a `PYTHONPATH`
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// how many bytes it holds
    pub fn size(&self) -> u64 {
        self.size
    }
}

/// something in the cache that staging never put there
///
/// carried rather than thrown, because a report of a cache is still a true
/// report of it when there is a stray file in the middle. what a stray stops is
/// [`Cache::clear`], which will not delete anything from a directory holding
/// something it cannot account for
#[derive(Debug, Clone)]
pub struct Stray {
    path: PathBuf,
    reason: String,
}

impl Stray {
    /// what was found
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// why it is not an entry
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// an entry that was asked to go and did not
#[derive(Debug)]
pub struct Failure {
    path: PathBuf,
    source: io::Error,
}

impl Failure {
    /// the file or directory that would not go
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// what the operating system said about it
    pub fn source(&self) -> &io::Error {
        &self.source
    }
}

/// what a [`Cache::clear`] really did
///
/// every field is a statement about what is now on disk rather than about what
/// was attempted. a caller that reports success without reading `failures`
/// reports something untrue, which is why they are here together
#[derive(Debug)]
#[must_use = "a clear that could not remove an entry says so here, and reporting it is the point"]
pub struct Cleared {
    removed: Vec<Entry>,
    kept: Option<Entry>,
    failures: Vec<Failure>,
}

impl Cleared {
    /// the entries that are gone
    pub fn removed(&self) -> &[Entry] {
        &self.removed
    }

    /// how many bytes they were holding
    pub fn reclaimed(&self) -> u64 {
        self.removed.iter().map(Entry::size).sum()
    }

    /// the entry that was asked for by digest and left alone, if it was there
    pub fn kept(&self) -> Option<&Entry> {
        self.kept.as_ref()
    }

    /// the entries that were asked to go and did not, and why
    pub fn failures(&self) -> &[Failure] {
        &self.failures
    }

    /// whether everything that was asked for actually happened
    pub fn succeeded(&self) -> bool {
        self.failures.is_empty()
    }
}

/// the agent cache as it is right now
#[derive(Debug)]
pub struct Cache {
    root: PathBuf,
    present: bool,
    entries: Vec<Entry>,
    strays: Vec<Stray>,
}

/// read the per-user cache, wherever a launch would stage into
///
/// a cache that is not there is not a failure and is not created — it is a
/// [`Cache`] whose [`present`](Cache::present) is false, holding nothing
pub fn open() -> Result<Cache> {
    open_at(&agent::default_cache()?)
}

/// the same, for a cache directory of the caller's choosing
///
/// held to the rule staging is held to: a directory that is a link, is not a
/// directory, belongs to another uid or is writable by anyone else is refused
/// here exactly as it is refused there, by the same check
pub fn open_at(root: &Path) -> Result<Cache> {
    let metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Cache {
                root: root.to_path_buf(),
                present: false,
                entries: Vec::new(),
                strays: Vec::new(),
            });
        }
        Err(source) => return Err(unreadable(root)(source)),
    };
    agent::trusted(root, &metadata)?;

    let mut entries = Vec::new();
    let mut strays = Vec::new();
    for child in std::fs::read_dir(root).map_err(unreadable(root))? {
        let child = child.map_err(unreadable(root))?.path();
        match classify(&child)? {
            Found::Entry(entry) => entries.push(entry),
            Found::Stray(stray) => strays.push(stray),
            Found::EntryWithStrays(found) => strays.extend(found),
        }
    }

    // the order a directory is read in is the filesystem's business, and a
    // report that changes between two runs over the same cache is a report
    // somebody has to read twice
    entries.sort_by(|one, other| one.digest.cmp(&other.digest));
    strays.sort_by(|one, other| one.path.cmp(&other.path));

    Ok(Cache {
        root: root.to_path_buf(),
        present: true,
        entries,
        strays,
    })
}

/// the digest of the agent build this `bpd` would stage right now
///
/// the same bytes, read the same way and hashed the same way as a launch hashes
/// them, so the answer is the entry a launch would use rather than a guess at it
pub fn current() -> Result<String> {
    let artifact = agent::built_artifact()?;
    let bytes = std::fs::read(&artifact).map_err(unreadable(&artifact))?;
    Ok(agent::digest(&bytes))
}

impl Cache {
    /// the directory itself
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// whether the directory is there at all
    ///
    /// false is the ordinary state of a machine that has never launched a
    /// debuggee, not a failure
    pub fn present(&self) -> bool {
        self.present
    }

    /// every cached agent build, by digest
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// everything in the cache that staging never put there
    pub fn strays(&self) -> &[Stray] {
        &self.strays
    }

    /// how many bytes the entries hold between them
    pub fn size(&self) -> u64 {
        self.entries.iter().map(Entry::size).sum()
    }

    /// the entry holding the agent with this digest, if the cache has it
    pub fn entry(&self, digest: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.digest == digest)
    }

    /// remove entries, leaving the one named by `keep` if it is there
    ///
    /// nothing is removed at all when the cache holds a [`Stray`]: it is one
    /// directory, and something in it that staging did not write is a reason to
    /// stop and say so rather than to delete around
    ///
    /// an entry that will not go is collected into
    /// [`Cleared::failures`] and the rest are still attempted, because "which
    /// ones" is the useful answer — one entry a debuggee has loaded on windows
    /// should not stand between a user and the other eighty-eight
    pub fn clear(&self, keep: Option<&str>) -> Result<Cleared> {
        if let Some(stray) = self.strays.first() {
            // one of them is named here, and `Cache::strays` has the rest. a
            // refusal is about the decision, and the decision is the same
            // whether there is one surprise in the directory or twenty
            let others = match self.strays.len() - 1 {
                0 => String::new(),
                more => format!(", along with {more} more it did not write"),
            };
            return Err(Error::UnexpectedInAgentCache {
                root: self.root.clone(),
                reason: format!(
                    "nothing has been removed. `{}` is in it, and {}{} — move \
                     aside whatever does not belong, and this will clear the \
                     entries",
                    stray.path.display(),
                    stray.reason,
                    others
                ),
            });
        }

        let mut cleared = Cleared {
            removed: Vec::new(),
            kept: None,
            failures: Vec::new(),
        };

        for entry in &self.entries {
            if keep == Some(entry.digest.as_str()) {
                cleared.kept = Some(entry.clone());
                continue;
            }
            match remove(entry) {
                Ok(()) => cleared.removed.push(entry.clone()),
                Err(failure) => cleared.failures.push(failure),
            }
        }

        Ok(cleared)
    }
}

/// take one entry away, without ever taking anything else
///
/// the known names and then the directory, rather than a recursive remove: what
/// is inside was read and accounted for when the cache was opened, and
/// `remove_dir` refuses a directory that has gained something since instead of
/// carrying it off. a race with a `bpd` publishing into this very entry
/// therefore ends as a named failure rather than as a deletion nobody described
fn remove(entry: &Entry) -> std::result::Result<(), Failure> {
    for name in agent::module_names() {
        let module = entry.path.join(name);
        match std::fs::remove_file(&module) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Failure {
                    path: module,
                    source,
                });
            }
        }
    }

    std::fs::remove_dir(&entry.path).map_err(|source| Failure {
        path: entry.path.clone(),
        source,
    })
}

/// what one thing in the cache directory turned out to be
enum Found {
    Entry(Entry),
    Stray(Stray),
    /// an entry shaped directory holding something staging never wrote
    ///
    /// it is not offered as an entry, because the only thing to do with an
    /// entry is remove it and the contents are not this cache's to remove
    EntryWithStrays(Vec<Stray>),
}

fn classify(path: &Path) -> Result<Found> {
    let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
        return Ok(Found::Stray(stray(
            path,
            "its name is not text, and an entry is named with the hex of a \
             sha-256",
        )));
    };
    if !is_digest(name) {
        return Ok(Found::Stray(stray(
            path,
            "an entry is named with the 64 hex characters of the agent's \
             sha-256, and that is not one",
        )));
    }

    let metadata = std::fs::symlink_metadata(path).map_err(unreadable(path))?;
    if metadata.file_type().is_symlink() {
        return Ok(Found::Stray(stray(
            path,
            "it is a link, and staging only ever makes real directories — bpd \
             will not follow one out of the cache to delete what is on the \
             other end",
        )));
    }
    if !metadata.is_dir() {
        return Ok(Found::Stray(stray(
            path,
            "an entry is a directory holding the agent, and that is not a \
             directory",
        )));
    }

    read_entry(path, name)
}

/// an entry directory, if everything in it is what staging writes
fn read_entry(path: &Path, digest: &str) -> Result<Found> {
    let names = agent::module_names();
    let mut size = 0;
    let mut strays = Vec::new();

    for child in std::fs::read_dir(path).map_err(unreadable(path))? {
        let child = child.map_err(unreadable(path))?.path();
        let held = child
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|name| names.iter().any(|module| module == name));

        let metadata = std::fs::symlink_metadata(&child).map_err(unreadable(&child))?;
        if !held || !metadata.is_file() {
            strays.push(stray(
                &child,
                "an entry holds the staged agent and nothing else, and that is \
                 not it",
            ));
            continue;
        }
        size += metadata.len();
    }

    if strays.is_empty() {
        // an entry with no module in it is still an entry, and still this
        // cache's to remove. it is what a removal that failed half way through
        // leaves behind, and refusing to finish that would be refusing to
        // clean up after the last refusal
        Ok(Found::Entry(Entry {
            digest: digest.to_owned(),
            path: path.to_path_buf(),
            size,
        }))
    } else {
        Ok(Found::EntryWithStrays(strays))
    }
}

fn is_digest(name: &str) -> bool {
    name.len() == DIGEST_LEN
        && name
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn stray(path: &Path, reason: &str) -> Stray {
    Stray {
        path: path.to_path_buf(),
        reason: reason.to_owned(),
    }
}

/// name the file an io failure was about, since a bare `io::Error` names nothing
fn unreadable(path: &Path) -> impl FnOnce(io::Error) -> Error + use<> {
    let path = path.to_path_buf();
    move |source| Error::ReadAgentCache { path, source }
}

#[cfg(test)]
mod tests {
    use super::{Cache, open_at};
    use crate::Error;
    use crate::agent::stage_artifact;

    /// a cache the test fills through the **real** staging code, with agents
    /// whose bytes are the test's to choose. writing entries by hand would be a
    /// test of this module against a restatement of the naming rule rather than
    /// against the rule
    struct Fixture {
        directory: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                directory: tempfile::tempdir().expect("a temporary directory can be made"),
            }
        }

        fn root(&self) -> std::path::PathBuf {
            self.directory.path().join("cache")
        }

        /// stage an agent of these bytes, and say which entry it went to
        ///
        /// under the root as this module would spell it. `Staged` canonicalises
        /// its answer, because a debuggee reports a resolved `sys.path` — and
        /// on macos a temporary directory is under a `/var` that is a link to
        /// `/private/var`. it is the same entry, named the way the cache was
        /// asked about
        fn stage(&self, bytes: &str) -> std::path::PathBuf {
            let artifact = self.directory.path().join("libbpd_agent.build");
            std::fs::write(&artifact, bytes).expect("the artifact can be written");
            let staged = stage_artifact(&self.root(), &artifact)
                .unwrap_or_else(|error| panic!("staging failed: {error}"));
            self.root().join(digest_of(staged.python_path()))
        }

        fn open(&self) -> Cache {
            open_at(&self.root())
                .unwrap_or_else(|error| panic!("the cache could not be read: {error}"))
        }
    }

    /// the digest is the entry directory's name, taken from what staging
    /// returned rather than computed here a second time
    fn digest_of(entry: &std::path::Path) -> String {
        entry
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .expect("a staged entry is named after its digest")
            .to_owned()
    }

    #[test]
    fn a_cache_that_is_not_there_is_reported_rather_than_made() {
        let fixture = Fixture::new();
        let cache = fixture.open();

        assert!(!cache.present());
        assert!(cache.entries().is_empty());
        assert_eq!(cache.size(), 0);
        assert!(
            !fixture.root().exists(),
            "reading a cache must not create the directory it was asked about"
        );
    }

    #[test]
    fn nothing_is_removed_from_a_cache_that_is_not_there() {
        let fixture = Fixture::new();
        let cleared = fixture
            .open()
            .clear(None)
            .unwrap_or_else(|error| panic!("clearing an absent cache failed: {error}"));

        assert!(cleared.succeeded());
        assert!(cleared.removed().is_empty());
        assert!(!fixture.root().exists());
    }

    #[test]
    fn every_staged_build_is_an_entry_with_the_size_it_holds() {
        let fixture = Fixture::new();
        let agents = ["one agent", "another agent", "a third one, longer"];
        let staged: Vec<_> = agents.iter().map(|bytes| fixture.stage(bytes)).collect();

        let cache = fixture.open();

        assert!(cache.present());
        assert_eq!(cache.entries().len(), 3);
        assert!(cache.strays().is_empty());
        for (entry, bytes) in staged.iter().zip(agents) {
            let found = cache
                .entry(&digest_of(entry))
                .unwrap_or_else(|| panic!("`{}` was staged and is not reported", entry.display()));
            assert_eq!(found.path(), entry);
            assert_eq!(found.size(), bytes.len() as u64);
        }
        assert_eq!(
            cache.size(),
            agents.iter().map(|bytes| bytes.len() as u64).sum::<u64>()
        );
    }

    #[test]
    fn clearing_takes_every_entry_and_reclaims_what_they_held() {
        let fixture = Fixture::new();
        for bytes in ["one agent", "another agent"] {
            fixture.stage(bytes);
        }
        let held = fixture.open().size();

        let cleared = fixture
            .open()
            .clear(None)
            .unwrap_or_else(|error| panic!("clearing failed: {error}"));

        assert!(cleared.succeeded());
        assert_eq!(cleared.removed().len(), 2);
        assert_eq!(cleared.reclaimed(), held);
        assert!(cleared.kept().is_none());

        let after = fixture.open();
        assert!(after.entries().is_empty());
        assert_eq!(after.size(), 0);
        assert!(
            fixture.root().is_dir(),
            "the cache directory itself is not an entry, and the next launch stages into it"
        );
    }

    #[test]
    fn the_entry_that_was_asked_for_is_the_one_left_behind() {
        let fixture = Fixture::new();
        let keep = fixture.stage("the agent to keep");
        let go = fixture.stage("the agent to lose");
        let digest = digest_of(&keep);

        let cleared = fixture
            .open()
            .clear(Some(&digest))
            .unwrap_or_else(|error| panic!("clearing failed: {error}"));

        assert!(cleared.succeeded());
        assert_eq!(
            cleared.kept().map(super::Entry::digest),
            Some(digest.as_str())
        );
        assert_eq!(cleared.removed().len(), 1);
        assert!(keep.is_dir(), "the entry that was kept is still there");
        assert!(!go.exists(), "every other entry is gone");
    }

    #[test]
    fn a_digest_that_is_not_in_the_cache_keeps_nothing_and_says_so() {
        let fixture = Fixture::new();
        fixture.stage("an agent");

        let cleared = fixture
            .open()
            .clear(Some(&"0".repeat(64)))
            .unwrap_or_else(|error| panic!("clearing failed: {error}"));

        assert!(cleared.succeeded());
        assert!(cleared.kept().is_none());
        assert_eq!(cleared.removed().len(), 1);
    }

    #[test]
    fn a_cache_that_is_not_a_directory_is_refused_by_the_same_check_staging_uses() {
        let fixture = Fixture::new();
        std::fs::write(fixture.root(), "not a directory").expect("the file can be written");

        let refused = open_at(&fixture.root());

        let Err(Error::UntrustedAgentCache { path, reason }) = refused else {
            panic!("a cache that is a file has to be refused, not read");
        };
        assert_eq!(path, fixture.root());
        assert!(reason.contains("not a directory"), "{reason}");
    }

    #[cfg(unix)]
    #[test]
    fn a_cache_anyone_could_write_to_is_refused_before_it_is_read() {
        let fixture = Fixture::new();
        fixture.stage("an agent");
        set_mode(&fixture.root(), 0o777);

        let refused = open_at(&fixture.root());
        set_mode(&fixture.root(), 0o700);

        let Err(Error::UntrustedAgentCache { reason, .. }) = refused else {
            panic!("a world writable cache has to be refused, not read");
        };
        assert!(reason.contains("0777"), "{reason}");
    }

    #[test]
    fn something_staging_never_wrote_stops_the_whole_clear() {
        let fixture = Fixture::new();
        let entry = fixture.stage("an agent");
        let intruder = fixture.root().join("notes.txt");
        std::fs::write(&intruder, "not mine").expect("the file can be written");

        let cache = fixture.open();
        assert_eq!(cache.strays().len(), 1);
        assert_eq!(cache.strays()[0].path(), intruder);
        assert_eq!(cache.entries().len(), 1);

        let Err(Error::UnexpectedInAgentCache { root, reason }) = cache.clear(None) else {
            panic!("a cache holding something unaccounted for has to be refused");
        };
        assert_eq!(root, fixture.root());
        assert!(reason.contains("notes.txt"), "{reason}");
        assert!(
            intruder.is_file(),
            "the stray file is reported, never removed"
        );
        assert!(entry.is_dir(), "and nothing else is removed either");
    }

    #[test]
    fn an_entry_holding_something_else_is_reported_rather_than_emptied() {
        let fixture = Fixture::new();
        let entry = fixture.stage("an agent");
        let extra = entry.join("something_else");
        std::fs::write(&extra, "not the agent").expect("the file can be written");

        let cache = fixture.open();
        assert!(
            cache.entries().is_empty(),
            "an entry with a surprise in it is not offered as one to remove"
        );
        assert_eq!(cache.strays().len(), 1);
        assert_eq!(cache.strays()[0].path(), extra);

        let refused = cache.clear(None);
        assert!(matches!(refused, Err(Error::UnexpectedInAgentCache { .. })));
        assert!(extra.is_file());
        assert!(entry.is_dir());
    }

    /// the one thing here that cannot be undone is deleting, so a link out of
    /// the cache is the case worth its own test: what is on the other end of it
    /// is not this cache's, at any depth
    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_cache_is_never_followed_to_what_it_points_at() {
        let fixture = Fixture::new();
        let outside = fixture.directory.path().join("somebody elses directory");
        std::fs::create_dir_all(&outside).expect("the directory can be made");
        let treasure = outside.join("a file that is not ours");
        std::fs::write(&treasure, "keep me").expect("the file can be written");

        // named exactly as an entry is named, which is the case a rule that
        // only looked at names would walk straight into
        fixture.stage("an agent");
        let link = fixture.root().join("a".repeat(64));
        std::os::unix::fs::symlink(&outside, &link).expect("the link can be made");

        let cache = fixture.open();
        assert_eq!(cache.strays().len(), 1);
        assert!(cache.strays()[0].reason().contains("link"));

        let refused = cache.clear(None);
        assert!(matches!(refused, Err(Error::UnexpectedInAgentCache { .. })));
        assert!(
            treasure.is_file(),
            "what a link points at is not the cache's"
        );
        assert!(
            link.is_symlink(),
            "and the link itself is reported, not removed"
        );
    }

    /// the failure this exists for is windows refusing to delete a loaded
    /// shared object, which cannot be produced here. an unwritable entry
    /// directory produces the same shape — a removal the operating system
    /// refuses — and that is what is asserted on: the entry is **named**, the
    /// other entries still go, and the clear does not call itself a success
    #[cfg(unix)]
    #[test]
    fn an_entry_that_will_not_go_is_named_rather_than_skipped() {
        let fixture = Fixture::new();
        let stuck = fixture.stage("the agent that stays");
        let ordinary = fixture.stage("the agent that goes");
        set_mode(&stuck, 0o500);

        let cleared = fixture
            .open()
            .clear(None)
            .unwrap_or_else(|error| panic!("clearing failed: {error}"));
        set_mode(&stuck, 0o700);

        assert!(
            !cleared.succeeded(),
            "an entry that is still there is not a success"
        );
        assert_eq!(cleared.failures().len(), 1);
        assert!(
            cleared.failures()[0].path().starts_with(&stuck),
            "the failure has to name the entry it was about, got `{}`",
            cleared.failures()[0].path().display()
        );
        assert_eq!(
            cleared.failures()[0].source().kind(),
            std::io::ErrorKind::PermissionDenied
        );

        assert_eq!(cleared.removed().len(), 1, "the rest still go");
        assert!(!ordinary.exists());
        assert!(
            stuck.is_dir() && stuck.join("bpd_agent.so").is_file(),
            "an entry that could not be removed is left whole, not half taken"
        );
    }

    #[cfg(unix)]
    fn set_mode(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .expect("the mode can be set");
    }
}
