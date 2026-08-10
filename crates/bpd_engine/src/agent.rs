//! finding the agent build and putting it where an interpreter can import it
//!
//! cargo names the artifact `libbpd_agent.dylib`, `libbpd_agent.so` or
//! `bpd_agent.dll` depending on the platform, and an interpreter only imports a
//! file named after the module. staging is that rename, into a directory that
//! goes on the debuggee's `PYTHONPATH`
//!
//! resolution today is "next to the running executable", which is what a cargo
//! build produces and what an installed layout would also produce. publishing a
//! build per interpreter tag, and choosing between them by `EXT_SUFFIX`, is
//! still ahead — until then an agent built for the wrong interpreter is caught
//! by the agent itself at import, which is the check that actually decides
//!
//! # why the directory is a cache and not a temporary
//!
//! it used to be a fresh `tempfile::tempdir()` per launch, and that cost 119 ms
//! of a 150 ms attach: on macOS the first load of a shared object the system has
//! never seen is validated, and a copy written a moment ago is never a file the
//! system has seen. staging to a stable path makes every launch after the first
//! import a file that has already been through it. the measurement is in
//! `docs/development/overhead.md`
//!
//! the entry is named after the **sha-256 of the artifact's bytes**, which is
//! what makes reuse safe: a rebuilt agent has different bytes, so it has a
//! different path, so no launch can be served a stale copy of an agent that has
//! since been rebuilt. that failure — running against code that is not the code
//! in front of you — is the one this project can least afford, and a key derived
//! from an mtime or a version string would leave it open
//!
//! # why the directory is checked before it is used
//!
//! what is cached is a shared object that gets loaded into the user's own
//! processes. a cache directory another user can write to is another user
//! choosing what runs inside the debuggee, so the directory is required to be
//! one nobody else can write to, and `bpd` refuses when it is not rather than
//! staging somewhere else. a fallback would turn a broken cache into a
//! performance regression nobody notices

use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// the module name the interpreter imports, and so the file's stem
const MODULE: &str = "bpd_agent";

/// the agent, renamed into a directory an interpreter can import from
#[derive(Debug)]
pub struct Staged {
    python_path: PathBuf,
}

impl Staged {
    /// the directory to put on the debuggee's `PYTHONPATH`
    pub fn python_path(&self) -> &Path {
        &self.python_path
    }
}

/// put the built agent where an interpreter can import it, and say where
///
/// the per-user cache is used, and created if it is not there yet
pub fn stage() -> Result<Staged> {
    stage_into(&default_cache()?)
}

/// the same, into a cache directory of the caller's choosing
///
/// the directory is held to the same rules as the default one: it has to be a
/// real directory that nobody but its owner can write to, or staging refuses
pub fn stage_into(cache: &Path) -> Result<Staged> {
    stage_artifact(cache, &built_artifact()?)
}

/// the cache entry holding exactly the bytes of `artifact`, made if absent
fn stage_artifact(cache: &Path, artifact: &Path) -> Result<Staged> {
    let bytes = std::fs::read(artifact).map_err(failed(artifact))?;

    trust(cache)?;
    let entry = cache.join(digest(&bytes));
    let module = entry.join(format!("{MODULE}{}", import_suffix()));

    // the bytes are compared rather than assumed from the path, so an entry
    // that was truncated by a full disk is republished instead of imported.
    // the read costs a fraction of a millisecond against the 119 ms the cache
    // is here to save
    if !holds(&module, &bytes)? {
        publish(cache, &entry, &module, &bytes)?;
    }

    // canonicalised because the debuggee reports a resolved `sys.path`, and a
    // home directory sits under a symlink on macos
    let python_path = entry.canonicalize().map_err(failed(&entry))?;

    Ok(Staged { python_path })
}

/// whether the staged file is already exactly these bytes
fn holds(module: &Path, bytes: &[u8]) -> Result<bool> {
    match std::fs::read(module) {
        Ok(found) => Ok(found == bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::StageAgent {
            path: module.to_path_buf(),
            source,
        }),
    }
}

/// write the entry, so that no interpreter ever sees a partial one
///
/// the file is written under a temporary name in the cache — the same
/// filesystem, so the rename is atomic — and renamed into place. two `bpd`
/// processes launching at once therefore either see no file or see the whole of
/// it, and since the name is the content, whichever of them wins wrote the same
/// bytes
fn publish(cache: &Path, entry: &Path, module: &Path, bytes: &[u8]) -> Result<()> {
    private_dir(entry).map_err(failed(entry))?;

    // `tempfile` opens this `0600` on unix, which is where the staged file's
    // mode comes from — `a_cache_this_bpd_made_is_reachable_by_nobody_else`
    // fails if that ever stops being true
    let mut staging = tempfile::Builder::new()
        .prefix(".staging")
        .tempfile_in(cache)
        .map_err(failed(cache))?;
    staging.write_all(bytes).map_err(failed(staging.path()))?;
    // the content is the name, so a file that reached the rename without
    // reaching the disk would be an entry whose name lies after a crash
    staging
        .as_file()
        .sync_all()
        .map_err(failed(staging.path()))?;

    match staging.persist(module) {
        Ok(_) => Ok(()),
        // windows refuses to replace a file another process has open, which is
        // what a debuggee that has this very agent loaded looks like. the
        // request is satisfied either way if the entry holds what was going to
        // be written, and that is checked rather than assumed
        Err(error) => {
            if holds(module, bytes)? {
                Ok(())
            } else {
                Err(Error::StageAgent {
                    path: module.to_path_buf(),
                    source: error.error,
                })
            }
        }
    }
}

/// refuse a cache directory that somebody other than its owner could write to
///
/// creating it first is deliberate: the common case is that it does not exist,
/// and a directory this makes is a directory with the right mode. what is
/// checked afterwards is what is actually on disk, which is the only thing that
/// says anything about a directory somebody else made first
fn trust(cache: &Path) -> Result<()> {
    match private_dir(cache) {
        Ok(()) => {}
        // a path that is already something else is described by the checks
        // below, which say what it is, rather than by `EEXIST`
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(failed(cache)(error)),
    }

    let metadata = std::fs::symlink_metadata(cache).map_err(failed(cache))?;

    // a link is refused on every platform, and on windows it is the whole of
    // the check: reading an ACL needs a security descriptor walk that is not
    // written here, so what stands between a windows user and somebody else's
    // agent is `%LOCALAPPDATA%` being per-user and this refusing a junction
    // pointing out of it
    if metadata.file_type().is_symlink() {
        return Err(untrusted(
            cache,
            "it is a link, and bpd will not follow one to the code it loads into \
             your own processes. replace it with a real directory",
        ));
    }
    if !metadata.is_dir() {
        return Err(untrusted(
            cache,
            "it is not a directory. move it aside, and bpd will make the \
             directory it needs",
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let owner = metadata.uid();
        let us = rustix::process::geteuid().as_raw();
        if owner != us {
            return Err(untrusted(
                cache,
                &format!(
                    "it belongs to uid {owner} and you are uid {us}. bpd loads \
                     what it caches there into your own processes, so it will \
                     not take one from a directory somebody else owns"
                ),
            ));
        }

        // write, not read. the agent is not a secret, and a rule that refused a
        // directory merely because somebody else can list it would refuse the
        // `0755` an ordinary umask produces. what cannot be allowed is another
        // user *putting a file there*, because the file is loaded into this
        // user's processes
        let mode = metadata.mode() & 0o7777;
        if mode & 0o022 != 0 {
            return Err(untrusted(
                cache,
                &format!(
                    "its mode is {mode:04o}, so users other than you can write \
                     into it — and what bpd caches there is loaded into your own \
                     processes. take that away with `chmod go-w {}`",
                    cache.display()
                ),
            ));
        }
    }

    Ok(())
}

fn untrusted(cache: &Path, reason: &str) -> Error {
    Error::UntrustedAgentCache {
        path: cache.to_path_buf(),
        reason: reason.to_owned(),
    }
}

/// make a directory, and its parents, that only its owner can reach
///
/// the mode is what a directory `bpd` made is checked against later, and umask
/// cannot widen it: `0700 & !umask` is never more than `0700`
fn private_dir(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }

    builder.create(path)
}

/// the sha-256 of the artifact, as the name of its cache entry
fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        write!(hex, "{byte:02x}").expect("writing to a string cannot fail");
    }
    hex
}

/// where a user's agent cache lives
fn default_cache() -> Result<PathBuf> {
    Ok(cache_home()?.join("bpd").join("agents"))
}

/// the base directory a user's caches go under
///
/// `~/.cache` on macos as well as linux, rather than `~/Library/Caches` on one
/// and the XDG location on the other, because one rule is one thing to check
/// and the directory is `bpd`'s own either way
#[cfg(not(windows))]
fn cache_home() -> Result<PathBuf> {
    // the XDG base directory specification says a relative value is invalid and
    // is to be ignored, which is why this is a fallback rather than a refusal
    if let Some(configured) = std::env::var_os("XDG_CACHE_HOME") {
        let configured = PathBuf::from(configured);
        if configured.is_absolute() {
            return Ok(configured);
        }
    }

    match std::env::home_dir() {
        Some(home) if home.is_absolute() => Ok(home.join(".cache")),
        _ => Err(Error::NoAgentCache {
            reason: "neither `XDG_CACHE_HOME` nor `HOME` names an absolute \
                     directory, so there is nowhere a per-user cache could go"
                .to_owned(),
        }),
    }
}

/// the same on windows, where the per-user location is `%LOCALAPPDATA%`
#[cfg(windows)]
fn cache_home() -> Result<PathBuf> {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        if local.is_absolute() {
            return Ok(local);
        }
    }

    match std::env::home_dir() {
        Some(home) if home.is_absolute() => Ok(home.join("AppData").join("Local")),
        _ => Err(Error::NoAgentCache {
            reason: "neither `LOCALAPPDATA` nor a user profile directory names \
                     an absolute path, so there is nowhere a per-user cache \
                     could go"
                .to_owned(),
        }),
    }
}

/// name the file an io failure was about, since a bare `io::Error` names nothing
fn failed(path: &Path) -> impl FnOnce(io::Error) -> Error + use<> {
    let path = path.to_path_buf();
    move |source| Error::StageAgent { path, source }
}

/// what an importable extension module is called on this platform
const fn import_suffix() -> &'static str {
    if cfg!(windows) { ".pyd" } else { ".so" }
}

/// where the agent build lives, relative to whatever is running
///
/// a test binary sits in `<target>/<profile>/deps`, and `bpd` itself in
/// `<target>/<profile>`. both are checked, so the same resolution works from a
/// test and from the installed binary
fn built_artifact() -> Result<PathBuf> {
    let running = std::env::current_exe().map_err(|source| Error::LocateAgent {
        reason: format!("the running executable has no path: {source}"),
    })?;
    let directory = running.parent().ok_or_else(|| Error::LocateAgent {
        reason: format!("`{}` has no parent directory", running.display()),
    })?;

    let name = cargo_artifact_name();
    let candidates = [directory.join(&name), directory.join("..").join(&name)];

    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }

    Err(Error::LocateAgent {
        reason: format!(
            "no `{name}` next to `{}`. build it for a supported interpreter:\n    \
             PYO3_PYTHON=python3.14 cargo build -p bpd_agent",
            running.display()
        ),
    })
}

fn cargo_artifact_name() -> String {
    if cfg!(windows) {
        format!("{MODULE}.dll")
    } else if cfg!(target_vendor = "apple") {
        format!("lib{MODULE}.dylib")
    } else {
        format!("lib{MODULE}.so")
    }
}

#[cfg(test)]
mod tests {
    use super::{MODULE, Staged, import_suffix, stage_artifact};
    use crate::Error;

    /// a cache in a directory of its own, and an agent whose bytes are ours to
    /// change. the real artifact would prove nothing about a rebuild, because a
    /// test cannot rebuild it
    struct Cache {
        directory: tempfile::TempDir,
    }

    impl Cache {
        fn new() -> Self {
            Self {
                directory: tempfile::tempdir().expect("a temporary directory can be made"),
            }
        }

        fn root(&self) -> std::path::PathBuf {
            self.directory.path().join("cache")
        }

        fn artifact(&self, bytes: &str) -> std::path::PathBuf {
            let path = self.directory.path().join("libbpd_agent.build");
            std::fs::write(&path, bytes).expect("the artifact can be written");
            path
        }

        fn stage(&self, bytes: &str) -> Staged {
            stage_artifact(&self.root(), &self.artifact(bytes))
                .unwrap_or_else(|error| panic!("staging failed: {error}"))
        }
    }

    fn module_in(staged: &Staged) -> std::path::PathBuf {
        staged
            .python_path()
            .join(format!("{MODULE}{}", import_suffix()))
    }

    fn staged_bytes(staged: &Staged) -> Vec<u8> {
        std::fs::read(module_in(staged)).expect("the staged module can be read")
    }

    #[test]
    fn the_same_agent_is_staged_to_the_same_place() {
        let cache = Cache::new();
        let first = cache.stage("an agent");
        let again = cache.stage("an agent");

        assert_eq!(first.python_path(), again.python_path());
        assert_eq!(staged_bytes(&again), b"an agent");
    }

    #[test]
    fn a_rebuilt_agent_is_never_served_from_the_cache() {
        let cache = Cache::new();
        let before = cache.stage("the agent as it was");
        let after = cache.stage("the agent, rebuilt");

        assert_ne!(
            before.python_path(),
            after.python_path(),
            "different bytes have to mean a different entry"
        );
        assert_eq!(staged_bytes(&after), b"the agent, rebuilt");
        assert_eq!(
            staged_bytes(&before),
            b"the agent as it was",
            "the old entry is left alone rather than overwritten"
        );
    }

    /// the mode is the proof, so this is a unix test: a second staging that
    /// succeeds against a cache nothing can be written into is a second staging
    /// that wrote nothing. a timestamp would only say it probably did not
    #[cfg(unix)]
    #[test]
    fn an_entry_that_is_already_right_is_not_written_again() {
        let cache = Cache::new();
        let first = cache.stage("an agent");

        let root = cache.root();
        set_mode(&root, 0o500);
        let again = stage_artifact(&root, &cache.artifact("an agent"));
        set_mode(&root, 0o700);

        let again = again.unwrap_or_else(|error| panic!("staging failed: {error}"));
        assert_eq!(first.python_path(), again.python_path());
    }

    /// two launches at once is the ordinary case on a machine with a test suite
    /// on it, and none of them may fail because another got there first. it is
    /// the *rename* that makes a partial entry impossible, and a test cannot
    /// prove that by racing — what this proves is that eight publishers into
    /// one cold cache all succeed, all agree on the entry, and all end up
    /// looking at the whole agent
    #[test]
    fn launches_that_race_each_other_all_get_the_whole_agent() {
        const AGENT: &str = "an agent, longer than a write is atomic for";

        let cache = Cache::new();
        let root = cache.root();
        let artifact = cache.artifact(AGENT);

        let staged: Vec<_> = std::thread::scope(|scope| {
            let racing: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| stage_artifact(&root, &artifact)))
                .collect();
            racing
                .into_iter()
                .map(|thread| {
                    thread
                        .join()
                        .expect("a staging thread does not panic")
                        .unwrap_or_else(|error| panic!("staging failed: {error}"))
                })
                .collect()
        });

        for one in &staged {
            assert_eq!(one.python_path(), staged[0].python_path());
            assert_eq!(staged_bytes(one), AGENT.as_bytes());
        }
    }

    #[test]
    fn an_entry_that_does_not_hold_the_agent_is_replaced() {
        let cache = Cache::new();
        let staged = cache.stage("an agent");
        std::fs::write(module_in(&staged), "half an ag").expect("the entry can be truncated");

        let again = cache.stage("an agent");

        assert_eq!(staged.python_path(), again.python_path());
        assert_eq!(
            staged_bytes(&again),
            b"an agent",
            "an entry whose bytes are not its name is republished, never imported"
        );
    }

    #[test]
    fn a_cache_that_is_not_a_directory_is_refused() {
        let cache = Cache::new();
        let root = cache.root();
        std::fs::write(&root, "not a directory").expect("the file can be written");

        let refused = stage_artifact(&root, &cache.artifact("an agent"));

        let Err(Error::UntrustedAgentCache { path, reason }) = refused else {
            panic!("a cache that is a file has to be refused, not used");
        };
        assert_eq!(path, root);
        assert!(reason.contains("not a directory"), "{reason}");
    }

    #[cfg(unix)]
    #[test]
    fn a_cache_anyone_could_write_to_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let cache = Cache::new();
        let root = cache.root();
        std::fs::create_dir_all(&root).expect("the cache directory can be made");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777))
            .expect("the mode can be set");

        let refused = stage_artifact(&root, &cache.artifact("an agent"));

        let Err(Error::UntrustedAgentCache { path, reason }) = refused else {
            panic!("a world writable cache has to be refused, not used");
        };
        assert_eq!(path, root);
        assert!(reason.contains("0777"), "{reason}");
        assert!(reason.contains("chmod go-w"), "{reason}");
    }

    /// the rule is about writing, and this is the line it draws. `0755` is what
    /// an ordinary umask produces and what `tempfile::tempdir()` makes, and
    /// somebody else being able to *read* the agent is not a way for them to
    /// choose what runs in this user's processes
    #[cfg(unix)]
    #[test]
    fn a_cache_others_can_only_read_is_used() {
        let cache = Cache::new();
        let root = cache.root();
        std::fs::create_dir_all(&root).expect("the cache directory can be made");
        set_mode(&root, 0o755);

        let staged = stage_artifact(&root, &cache.artifact("an agent"))
            .unwrap_or_else(|error| panic!("staging failed: {error}"));

        assert_eq!(staged_bytes(&staged), b"an agent");
    }

    #[cfg(unix)]
    #[test]
    fn a_cache_a_group_could_write_to_is_refused() {
        let cache = Cache::new();
        let root = cache.root();
        std::fs::create_dir_all(&root).expect("the cache directory can be made");
        set_mode(&root, 0o770);

        let refused = stage_artifact(&root, &cache.artifact("an agent"));

        let Err(Error::UntrustedAgentCache { reason, .. }) = refused else {
            panic!("a group writable cache has to be refused, not used");
        };
        assert!(reason.contains("0770"), "{reason}");
    }

    /// the only directory a test can count on somebody else owning is the root
    /// one, so that is what this uses. a test cannot make a directory belong to
    /// another user without being root, and a test that is root owns `/`
    #[cfg(unix)]
    #[test]
    fn a_cache_somebody_else_owns_is_refused() {
        if rustix::process::geteuid().is_root() {
            // `/` belongs to this user, so there is nothing here for the
            // ownership check to refuse. the assertion is skipped rather than
            // made against a state it was not written about
            return;
        }

        let cache = Cache::new();
        let refused = stage_artifact(std::path::Path::new("/"), &cache.artifact("an agent"));

        let Err(Error::UntrustedAgentCache { reason, .. }) = refused else {
            panic!("a cache somebody else owns has to be refused, not used");
        };
        assert!(reason.contains("uid 0"), "{reason}");
    }

    #[cfg(unix)]
    #[test]
    fn a_cache_reached_through_a_link_is_refused() {
        let cache = Cache::new();
        let real = cache.directory.path().join("elsewhere");
        std::fs::create_dir_all(&real).expect("the directory can be made");
        let root = cache.root();
        std::os::unix::fs::symlink(&real, &root).expect("the link can be made");

        let refused = stage_artifact(&root, &cache.artifact("an agent"));

        let Err(Error::UntrustedAgentCache { path, reason }) = refused else {
            panic!("a cache reached through a link has to be refused, not used");
        };
        assert_eq!(path, root);
        assert!(reason.contains("link"), "{reason}");
    }

    #[cfg(unix)]
    #[test]
    fn a_cache_this_bpd_made_is_reachable_by_nobody_else() {
        let cache = Cache::new();
        let staged = cache.stage("an agent");

        for made in [
            cache.root(),
            staged.python_path().to_path_buf(),
            module_in(&staged),
        ] {
            let mode = mode_of(&made);
            assert_eq!(mode & 0o077, 0, "`{}` is {mode:04o}", made.display());
        }
    }

    #[cfg(unix)]
    fn mode_of(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::MetadataExt as _;

        std::fs::metadata(path).expect("the path is there").mode() & 0o7777
    }

    #[cfg(unix)]
    fn set_mode(directory: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(mode))
            .expect("the mode can be set");
    }
}
