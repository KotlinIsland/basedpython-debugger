//! finding the agent build and putting it where an interpreter can import it
//!
//! cargo names the artifact `libbpd_agent.dylib`, `libbpd_agent.so` or
//! `bpd_agent.dll` depending on the platform, and an interpreter only imports a
//! file named after the module. staging is that rename, into a directory that
//! goes on the debuggee's `PYTHONPATH`
//!
//! one `bpd` carries an agent per interpreter tag, in `agents/<tag>/` beside
//! the binary, and picks between them by what the interpreter said about itself
//! when it was probed — never by what a path claims. the agent's own
//! `verify_interpreter` is unchanged and still runs: selection picking the
//! right file and the agent checking it was compiled for the interpreter that
//! imported it are two different guarantees, and the second is what catches a
//! wrong first
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

use std::collections::BTreeMap;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use bpd_core::python::{Capabilities, InterpreterTag};

use crate::{Error, Result};

/// the module name the interpreter imports, and so the file's stem
const MODULE: &str = "bpd_agent";

/// where a `bpd` keeps the agents it carries, beside the binary
///
/// one directory per interpreter tag, each holding the artifact under **cargo's
/// own name**:
///
/// ```text
/// bpd
/// agents/3.13/libbpd_agent.so
/// agents/3.14/libbpd_agent.so
/// agents/3.14t/libbpd_agent.so
/// ```
///
/// a directory per tag rather than a tag in the file name, for two reasons.
/// the artifact keeps the name cargo gave it, so whatever assembles the layout
/// copies and renames nothing — there is no step in which a name could be
/// invented that disagrees with the bytes beside it. and the tags a `bpd`
/// carries are then **read off the filesystem** rather than recovered by
/// parsing file names, which is what lets a refusal say what is really there
///
/// the cache under `~/.cache/bpd/agents/` is untouched by any of it. it is
/// keyed on the sha-256 of the bytes, so several agents are simply several
/// entries, and an entry still holds one file named for the platform's import
/// suffix and nothing else — which is the rule `bpd cache` reads a directory by
const AGENTS: &str = "agents";

/// what an importable extension module is called, on unix and on windows
const UNIX_SUFFIX: &str = ".so";
const WINDOWS_SUFFIX: &str = ".pyd";

/// what a child that was `exec`'d is entered through
///
/// four lines that find the agent and one that calls it. everything a `.py`
/// file in a debuggee's path could get wrong is a decision that belongs in the
/// agent, where it is rust and is tested — so this holds no decisions at all:
/// it reads three variables, imports one module, and calls one function
///
/// it is **not** basedpython under `python/`, and the architecture invariant is
/// what says so: a python layer goes there when it is more than about a dozen
/// lines, and this is eleven. it is also the one file in the tree that has to
/// be readable by an interpreter bpd did not build for — a child could be any
/// python, and the message it prints when the agent will not import into one is
/// the whole of what a user has to act on
const CHILD_HOOK: &str = include_str!("../resources/sitecustomize.py");

/// what the file is called, which is the whole of how an interpreter finds it
pub(crate) const CHILD_MODULE: &str = "sitecustomize.py";

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

/// put the agent for this interpreter where it can import it, and say where
///
/// the per-user cache is used, and created if it is not there yet
pub fn stage_for(interpreter: &Capabilities) -> Result<Staged> {
    stage_for_into(&default_cache()?, interpreter)
}

/// the same, into a cache directory of the caller's choosing
///
/// the directory is held to the same rules as the default one: it has to be a
/// real directory that nobody but its owner can write to, or staging refuses
pub fn stage_for_into(cache: &Path, interpreter: &Capabilities) -> Result<Staged> {
    stage_artifact(cache, &artifact_for(interpreter)?)
}

/// the cache entry holding exactly the bytes of `artifact`, made if absent
pub(crate) fn stage_artifact(cache: &Path, artifact: &Path) -> Result<Staged> {
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

/// put the `sitecustomize` a child is entered through where an interpreter can
/// import it, and say where
///
/// **a directory of its own, holding nothing else.** it goes on the end of a
/// debuggee's `PYTHONPATH` when child debugging is asked for, which puts it on
/// every descendant's `sys.path` — so anything else in it would be a module bpd
/// had added to programs it is not even debugging
///
/// it is a cache of its own rather than an entry of the agent's, because the
/// two hold different things: an agent entry holds one extension module, and
/// this one holds a source file **and** the `__pycache__` cpython writes beside
/// it the first time a child imports it. `bpd cache` knows both shapes and says
/// which directory it is describing, rather than one entry having two
pub fn stage_child_hook() -> Result<Staged> {
    stage_child_hook_into(&child_cache()?)
}

/// the same, into a cache directory of the caller's choosing
///
/// the directory is held to the same rules as the default one, by the same
/// check that holds the agent's
pub fn stage_child_hook_into(cache: &Path) -> Result<Staged> {
    let bytes = CHILD_HOOK.as_bytes();

    trust(cache)?;
    let entry = cache.join(digest(bytes));
    let module = entry.join(CHILD_MODULE);

    if !holds(&module, bytes)? {
        publish(cache, &entry, &module, bytes)?;
    }

    // canonicalised for the reason the agent's directory is: what lands on the
    // debuggee's `sys.path` is compared against what a bare run has, and a home
    // directory sits under a symlink on macos
    let python_path = entry.canonicalize().map_err(failed(&entry))?;
    Ok(Staged { python_path })
}

/// where the `sitecustomize` a child is entered through is cached
pub(crate) fn child_cache() -> Result<PathBuf> {
    Ok(cache_home()?.join("bpd").join("children"))
}

/// the entry the `sitecustomize` this `bpd` carries stages into
///
/// unlike an agent build there is nothing to go and find: the file is compiled
/// into the binary, so this is the same bytes [`stage_child_hook`] writes,
/// hashed the same way, and there is no state in which it cannot be named
pub(crate) fn child_hook_digest() -> String {
    digest(CHILD_HOOK.as_bytes())
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
    trusted(cache, &metadata)
}

/// the checks themselves, over metadata the caller has already taken
///
/// split out because `bpd cache` is held to exactly this rule and must **not**
/// create the directory to be told about it: a report that made the thing it
/// was asked to describe would answer a question nobody asked
pub(crate) fn trusted(cache: &Path, metadata: &std::fs::Metadata) -> Result<()> {
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
    Error::UntrustedCache {
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
pub(crate) fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        write!(hex, "{byte:02x}").expect("writing to a string cannot fail");
    }
    hex
}

/// where a user's agent cache lives
pub(crate) fn default_cache() -> Result<PathBuf> {
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
        _ => Err(Error::NoCacheHome {
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
        _ => Err(Error::NoCacheHome {
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
    if cfg!(windows) {
        WINDOWS_SUFFIX
    } else {
        UNIX_SUFFIX
    }
}

/// every name a cache entry's module can have, on any platform
///
/// staging writes the running platform's one, and the other is here because
/// reading an entry is not the same act as writing one: a home directory shared
/// with another machine can hold an entry this platform would never have
/// written, and an entry is what it is regardless of which machine made it
pub(crate) fn module_names() -> [String; 2] {
    [
        format!("{MODULE}{UNIX_SUFFIX}"),
        format!("{MODULE}{WINDOWS_SUFFIX}"),
    ]
}

/// the agent builds this `bpd` carries, and where it looked for them
#[derive(Debug)]
struct Carried {
    /// the published agents, by the tag of the interpreter each one is for
    tagged: BTreeMap<InterpreterTag, PathBuf>,
    /// the single untagged artifact `cargo build -p bpd_agent` leaves behind
    ///
    /// its path says nothing about which interpreter it is for, and it does not
    /// have to: a checkout has exactly one, and the agent's own
    /// `verify_interpreter` is what decides whether it fits. that check is not
    /// a fallback for this — it runs against a published agent too
    development: Option<PathBuf>,
    /// the directories that were looked in, so a refusal can say where
    looked_in: Vec<PathBuf>,
}

/// where an agent is looked for, relative to whatever is running
///
/// a test binary sits in `<target>/<profile>/deps`, and `bpd` itself in
/// `<target>/<profile>`. both are looked in, so the same resolution works from
/// a test and from an installed binary — where the pair is `<prefix>/bin` and
/// `<prefix>`
fn roots() -> Result<Vec<PathBuf>> {
    let running = std::env::current_exe().map_err(|source| Error::LocateAgent {
        reason: format!("the running executable has no path: {source}"),
    })?;
    let directory = running.parent().ok_or_else(|| Error::LocateAgent {
        reason: format!("`{}` has no parent directory", running.display()),
    })?;

    let mut roots = vec![directory.to_path_buf()];
    roots.extend(directory.parent().map(Path::to_path_buf));
    Ok(roots)
}

/// everything this `bpd` could stage, read off the filesystem
fn carried() -> Result<Carried> {
    carried_in(&roots()?)
}

/// the same, over roots the caller names
fn carried_in(roots: &[PathBuf]) -> Result<Carried> {
    let name = cargo_artifact_name();
    let mut carried = Carried {
        tagged: BTreeMap::new(),
        development: None,
        looked_in: roots.to_vec(),
    };

    for root in roots {
        let published = root.join(AGENTS);
        match std::fs::read_dir(&published) {
            Ok(found) => {
                for child in found {
                    let child = child.map_err(unreadable(&published))?;
                    // a directory whose name is not exactly a tag is not an
                    // agent directory. reading one loosely would hand a release
                    // an agent built for another, which is the load that
                    // imports and then reads the wrong offsets
                    let Some(tag) = child.file_name().to_str().and_then(InterpreterTag::parse)
                    else {
                        continue;
                    };
                    let artifact = child.path().join(&name);
                    if artifact.is_file() {
                        // the nearer root wins, which is the order a single
                        // artifact is looked for in too
                        carried.tagged.entry(tag).or_insert(artifact);
                    }
                }
            }
            // a `bpd` that carries nothing published is the ordinary state of a
            // checkout, not a failure
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(unreadable(&published)(source)),
        }

        let development = root.join(&name);
        if carried.development.is_none() && development.is_file() {
            carried.development = Some(development);
        }
    }

    Ok(carried)
}

/// the agent build for an interpreter, or a refusal naming what is carried
///
/// keyed on [`Capabilities::tag`] — the release and the build configuration the
/// interpreter itself reported — rather than on `EXT_SUFFIX`, which also
/// carries a platform that a file on this machine cannot disagree about. the
/// tag is the vocabulary the agent's own check is written in, so the thing
/// selection asks for is the thing verification compares
pub(crate) fn artifact_for(interpreter: &Capabilities) -> Result<PathBuf> {
    select(interpreter, &carried()?)
}

/// where an agent built for `tag` goes, under a directory `bpd` looks in
///
/// the layout is `bpd`'s to define, so whatever assembles one asks here instead
/// of repeating it — the resolution reads back exactly the path this names
pub fn published_at(root: &Path, tag: InterpreterTag) -> PathBuf {
    root.join(AGENTS)
        .join(tag.to_string())
        .join(cargo_artifact_name())
}

/// the choice itself, over an already-read layout
fn select(interpreter: &Capabilities, carried: &Carried) -> Result<PathBuf> {
    let tag = interpreter.tag();

    if let Some(artifact) = carried.tagged.get(&tag) {
        return Ok(artifact.clone());
    }
    // the development build is the weaker claim, because nothing about it names
    // an interpreter — so it is taken only where nothing was published for this
    // one, and the agent settles it at import
    if let Some(artifact) = &carried.development {
        return Ok(artifact.clone());
    }

    Err(Error::LocateAgent {
        reason: no_agent_for(interpreter, tag, carried),
    })
}

/// every agent build this `bpd` carries, in tag order and the untagged one last
///
/// a launch takes one of these. `bpd cache` has to know all of them, because
/// each stages into an entry of its own
pub(crate) fn built_artifacts() -> Result<Vec<(Option<InterpreterTag>, PathBuf)>> {
    let carried = carried()?;

    let mut all: Vec<_> = carried
        .tagged
        .iter()
        .map(|(tag, artifact)| (Some(*tag), artifact.clone()))
        .collect();
    all.extend(carried.development.clone().map(|artifact| (None, artifact)));

    if all.is_empty() {
        return Err(Error::LocateAgent {
            reason: carries_nothing(&carried),
        });
    }
    Ok(all)
}

/// why this interpreter cannot be launched, and what would change that
///
/// it names all three of the things a reader needs: the interpreter and the tag
/// it needs, the tags that are here instead, and what to do. "no agent for
/// python 3.13" is not enough when the answer is "this build carries 3.14 and
/// 3.15"
fn no_agent_for(interpreter: &Capabilities, tag: InterpreterTag, carried: &Carried) -> String {
    let named = interpreter.interpreter.display();

    if carried.tagged.is_empty() {
        return format!(
            "bpd carries no agent build at all, and python {} (`{named}`) needs \
             the one tagged `{tag}`. nothing was found in {}. build it for this \
             interpreter:\n    PYO3_PYTHON={named} cargo build -p bpd_agent",
            interpreter.version,
            looked_in(carried),
        );
    }

    format!(
        "bpd carries no agent for python {} (`{named}`), which needs the build \
         tagged `{tag}`. it carries {}. the agent is a cpython extension and is \
         not abi3 — it reads interpreter state whose layout changes between \
         releases — so one build loads into one release and one build \
         configuration and no other. debug with an interpreter this bpd carries, \
         or build the agent for this one:\n    \
         PYO3_PYTHON={named} cargo build -p bpd_agent",
        interpreter.version,
        tags(carried),
    )
}

/// the same, asked of a `bpd` rather than about an interpreter
fn carries_nothing(carried: &Carried) -> String {
    format!(
        "bpd carries no agent build at all. nothing was found in {}. an \
         installed bpd keeps one per interpreter tag in `{AGENTS}/<tag>/` beside \
         the binary, and a checkout has the one cargo built:\n    \
         PYO3_PYTHON=python3.14 cargo build -p bpd_agent",
        looked_in(carried),
    )
}

/// the tags carried, with the directory each was found in
fn tags(carried: &Carried) -> String {
    carried
        .tagged
        .iter()
        .map(|(tag, artifact)| {
            let directory = artifact.parent().unwrap_or_else(|| {
                unreachable!("`{}` is a file in a tag directory", artifact.display())
            });
            format!("`{tag}` in `{}`", directory.display())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// the directories that were looked in, named so the reader can look too
fn looked_in(carried: &Carried) -> String {
    carried
        .looked_in
        .iter()
        .map(|root| format!("`{}`", root.display()))
        .collect::<Vec<_>>()
        .join(" or ")
}

/// name the directory an agent layout could not be read out of
fn unreadable(path: &Path) -> impl FnOnce(io::Error) -> Error + use<> {
    let path = path.to_path_buf();
    move |source| Error::LocateAgent {
        reason: format!(
            "`{}` is where bpd keeps the agents it carries, and it could not be \
             read: {source}. bpd will not report an interpreter unsupported \
             while it cannot see what it holds",
            path.display()
        ),
    }
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
    use std::path::{Path, PathBuf};

    use bpd_core::python::{
        Capabilities, Implementation, InterpreterTag, PythonVersion, RemoteDebug,
    };

    use super::{
        MODULE, Staged, cargo_artifact_name, carried_in, import_suffix, published_at, select,
        stage_artifact,
    };
    use crate::Error;

    /// an install laid out the way `bpd` will be installed, in a directory of
    /// its own
    ///
    /// the agents are written through [`published_at`], which is the same
    /// answer the resolution reads back — a test that spelled the layout out a
    /// second time would be a test of one restatement against another
    struct Install {
        directory: tempfile::TempDir,
    }

    impl Install {
        fn new() -> Self {
            Self {
                directory: tempfile::tempdir().expect("a temporary directory can be made"),
            }
        }

        /// where the binary sits, and the directory above it
        fn roots(&self) -> Vec<PathBuf> {
            vec![self.beside(), self.prefix()]
        }

        fn prefix(&self) -> PathBuf {
            self.directory.path().to_path_buf()
        }

        fn beside(&self) -> PathBuf {
            self.directory.path().join("bin")
        }

        /// carry an agent for a tag, under the root of the caller's choosing
        fn carries(root: &Path, tag: &str, bytes: &str) -> PathBuf {
            let tag = InterpreterTag::parse(tag).expect("the test names a real tag");
            let artifact = published_at(root, tag);
            write(&artifact, bytes);
            artifact
        }

        /// leave the artifact a `cargo build -p bpd_agent` leaves behind
        fn development_build(&self, bytes: &str) -> PathBuf {
            let artifact = self.beside().join(cargo_artifact_name());
            write(&artifact, bytes);
            artifact
        }
    }

    fn write(path: &Path, bytes: &str) {
        std::fs::create_dir_all(
            path.parent()
                .expect("an artifact path names a file in a directory"),
        )
        .expect("the directory can be made");
        std::fs::write(path, bytes).expect("the artifact can be written");
    }

    /// an interpreter that reported itself as this release and build
    fn interpreter(tag: &str) -> Capabilities {
        let tag = InterpreterTag::parse(tag).expect("the test names a real tag");
        let (version, free_threaded) = (tag.to_string(), tag.to_string().ends_with('t'));
        let version = version.trim_end_matches('t');
        let (major, minor) = version
            .split_once('.')
            .expect("a tag is a major and a minor");

        Capabilities {
            interpreter: PathBuf::from(format!("python{version}")),
            executable: PathBuf::from(format!("/usr/bin/python{version}")),
            version: PythonVersion::new(
                major.parse().expect("a tag's major is a number"),
                minor.parse().expect("a tag's minor is a number"),
                0,
            ),
            implementation: Implementation::CPython,
            free_threaded,
            debug_build: false,
            ext_suffix: Some(format!(".cpython-{major}{minor}-darwin.so")),
            monitoring: true,
            remote_debug: RemoteDebug::Available,
        }
    }

    fn chosen_for(install: &Install, tag: &str) -> Result<PathBuf, Error> {
        let carried = carried_in(&install.roots()).expect("the layout can be read");
        select(&interpreter(tag), &carried)
    }

    fn refusal_for(install: &Install, tag: &str) -> String {
        match chosen_for(install, tag) {
            Ok(artifact) => panic!(
                "python {tag} was given `{}`, and nothing here is for it",
                artifact.display()
            ),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn every_interpreter_is_given_the_agent_carried_for_its_own_tag() {
        let install = Install::new();
        let root = install.prefix();
        for tag in ["3.13", "3.14", "3.14t", "3.15"] {
            Install::carries(&root, tag, tag);
        }

        for tag in ["3.13", "3.14", "3.14t", "3.15"] {
            let chosen = chosen_for(&install, tag)
                .unwrap_or_else(|error| panic!("python {tag} was refused: {error}"));
            assert_eq!(
                std::fs::read_to_string(&chosen).expect("the chosen artifact can be read"),
                tag,
                "python {tag} was given `{}`",
                chosen.display()
            );
        }
    }

    /// the free-threaded build is a different abi and its agent is a different
    /// file. a rule that read the version and stopped would hand each of these
    /// the other's agent, and cpython would import it and read the wrong offsets
    #[test]
    fn a_free_threaded_interpreter_is_never_given_the_gil_builds_agent() {
        let install = Install::new();
        let root = install.prefix();
        Install::carries(&root, "3.14", "the gil build");
        Install::carries(&root, "3.14t", "the free-threaded build");

        assert_eq!(
            std::fs::read_to_string(chosen_for(&install, "3.14t").expect("3.14t is carried"))
                .expect("the chosen artifact can be read"),
            "the free-threaded build"
        );
    }

    #[test]
    fn the_development_build_is_used_when_nothing_is_published() {
        let install = Install::new();
        let artifact = install.development_build("the agent cargo built");

        assert_eq!(
            chosen_for(&install, "3.14").expect("the development build is there"),
            artifact,
            "a checkout has one artifact and no layout, and that has to keep working"
        );
    }

    /// a published agent names the interpreter it is for and the development
    /// build names nothing, so the specific one wins where there is one. the
    /// development build is still reachable for every other interpreter, which
    /// is what a checkout that has also assembled a layout looks like
    #[test]
    fn a_published_agent_is_preferred_to_the_untagged_one() {
        let install = Install::new();
        Install::carries(&install.prefix(), "3.14", "the published agent");
        install.development_build("the agent cargo built");

        assert_eq!(
            std::fs::read_to_string(chosen_for(&install, "3.14").expect("3.14 is carried"))
                .expect("the chosen artifact can be read"),
            "the published agent"
        );
        assert_eq!(
            std::fs::read_to_string(chosen_for(&install, "3.13").expect("the development build"))
                .expect("the chosen artifact can be read"),
            "the agent cargo built"
        );
    }

    /// the directory beside the binary comes first, the one above it second —
    /// the same order a single artifact is looked for in
    #[test]
    fn the_nearer_root_wins() {
        let install = Install::new();
        Install::carries(&install.prefix(), "3.14", "the one further away");
        Install::carries(&install.beside(), "3.14", "the one beside the binary");

        assert_eq!(
            std::fs::read_to_string(chosen_for(&install, "3.14").expect("3.14 is carried"))
                .expect("the chosen artifact can be read"),
            "the one beside the binary"
        );
    }

    /// a directory whose name is nearly a tag is not one, and a tag directory
    /// with no agent in it carries nothing. either read loosely would end in an
    /// interpreter being handed a file that was never for it
    #[test]
    fn a_directory_that_is_not_an_agent_for_a_tag_carries_nothing() {
        let install = Install::new();
        let agents = install.prefix().join("agents");
        for name in ["3.014", "3.14.0", "python3.14", "cpython-314-darwin"] {
            write(&agents.join(name).join(cargo_artifact_name()), "not a tag");
        }
        std::fs::create_dir_all(agents.join("3.14")).expect("the directory can be made");
        std::fs::write(agents.join("3.14").join("notes.txt"), "not an agent")
            .expect("the file can be written");

        let refused = refusal_for(&install, "3.14");
        assert!(
            refused.contains("no agent build at all"),
            "a tag directory with no agent in it carries nothing: {refused}"
        );
    }

    #[test]
    fn an_interpreter_nothing_is_carried_for_is_told_which_tags_are() {
        let install = Install::new();
        let root = install.prefix();
        Install::carries(&root, "3.14", "an agent");
        Install::carries(&root, "3.15", "an agent");

        let refused = refusal_for(&install, "3.13");

        assert!(refused.contains("python 3.13.0"), "{refused}");
        assert!(
            refused.contains("`3.13`"),
            "the refusal has to name the tag that is needed: {refused}"
        );
        for present in ["`3.14`", "`3.15`"] {
            assert!(
                refused.contains(present),
                "the refusal has to name what is carried, and {present} is: {refused}"
            );
        }
        assert!(
            refused.contains("cargo build -p bpd_agent"),
            "and what to do about it: {refused}"
        );
    }

    #[test]
    fn a_bpd_carrying_nothing_says_so_and_names_where_it_looked() {
        let install = Install::new();

        let refused = refusal_for(&install, "3.14");

        assert!(refused.contains("no agent build at all"), "{refused}");
        for root in install.roots() {
            assert!(
                refused.contains(&root.display().to_string()),
                "the refusal has to name where it looked, and `{}` is one: {refused}",
                root.display()
            );
        }
        assert!(refused.contains("cargo build -p bpd_agent"), "{refused}");
    }

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

        fn root(&self) -> PathBuf {
            self.directory.path().join("cache")
        }

        fn artifact(&self, bytes: &str) -> PathBuf {
            let path = self.directory.path().join("libbpd_agent.build");
            std::fs::write(&path, bytes).expect("the artifact can be written");
            path
        }

        fn stage(&self, bytes: &str) -> Staged {
            stage_artifact(&self.root(), &self.artifact(bytes))
                .unwrap_or_else(|error| panic!("staging failed: {error}"))
        }
    }

    fn module_in(staged: &Staged) -> PathBuf {
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

        let Err(Error::UntrustedCache { path, reason }) = refused else {
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

        let Err(Error::UntrustedCache { path, reason }) = refused else {
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

        let Err(Error::UntrustedCache { reason, .. }) = refused else {
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
        let refused = stage_artifact(Path::new("/"), &cache.artifact("an agent"));

        let Err(Error::UntrustedCache { reason, .. }) = refused else {
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

        let Err(Error::UntrustedCache { path, reason }) = refused else {
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
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::MetadataExt as _;

        std::fs::metadata(path).expect("the path is there").mode() & 0o7777
    }

    #[cfg(unix)]
    fn set_mode(directory: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(mode))
            .expect("the mode can be set");
    }
}
