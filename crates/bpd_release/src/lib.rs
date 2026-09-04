//! the layout a released `bpd` is shipped as, assembled and checked
//!
//! `bpd` carries **an agent per interpreter tag** and chooses one at launch by
//! what the interpreter says it is. that is what an installed `bpd` does and no
//! development build does it: a checkout has one agent, built against whichever
//! interpreter `PYO3_PYTHON` last named. so the layout is a thing that only
//! exists at release time, and something has to build it
//!
//! nothing here uploads, tags, signs or contacts anything. it takes files that
//! already exist and produces a directory and a manifest, and
//! [`verify`] reads that directory back and says whether it is still what the
//! manifest claims. the release workflow is what carries one to pypi, by
//! running these same commands on every platform and uploading the wheels —
//! `docs/development/releasing.md` is where that is written down
//!
//! ## why it refuses rather than does its best
//!
//! a release that carries the wrong agent for a tag is not a release that fails
//! — it is one that **works** until somebody runs the interpreter it lied
//! about, and then refuses at import with a message about the wrong python. the
//! same discipline the rest of the project holds applies to the thing that
//! builds it: every input is checked, and an assembly that cannot be completed
//! produces nothing at all rather than a directory somebody might ship
//!
//! a tag is parsed with [`InterpreterTag::parse`] — the same parser that reads
//! one back at launch — rather than by a rule written here that could come to
//! disagree with it

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use bpd_core::python::InterpreterTag;
use sha2::{Digest as _, Sha256};

/// the file a layout carries its own description in
///
/// beside the binary rather than inside it, because the thing it describes is
/// the directory: an executable cannot carry the digest of itself
pub const MANIFEST: &str = "MANIFEST";

pub mod wheel;

pub use wheel::{Wheel, wheel};

/// what `bpd` is called in a layout
///
/// **not a constant**, because windows runs a file by its extension. a layout
/// carrying the binary as `bpd` installs to `Scripts/bpd`, which windows cannot
/// execute and pip will not complain about — the same class of failure as an
/// agent under a name nothing looks for, and invisible in exactly the same way
///
/// it is the shape [`bpd_engine::agent::cargo_artifact_name`] has and for the
/// same reason: the name belongs to the platform the layout is **for**, and a
/// layout is assembled natively on it
#[must_use]
pub fn binary_name() -> String {
    format!("bpd{}", std::env::consts::EXE_SUFFIX)
}

/// where an agent for one tag lives, under the layout root
///
/// **the name is not the one the file was built under.** `bpd_engine::agent`
/// scans `agents/<tag>/` and joins exactly `cargo_artifact_name()` onto it, so
/// an agent copied in under whatever cargo left it called — or under a name a
/// build script chose — assembles cleanly, verifies cleanly, and is then
/// invisible at every launch. measured: a layout built that way answers
/// `bpd carries no agent build at all`
///
/// so the name comes from the engine rather than from the input, and from the
/// same function the scan uses
#[must_use]
pub fn agent_at(tag: InterpreterTag) -> PathBuf {
    Path::new("agents")
        .join(tag.to_string())
        .join(bpd_engine::agent::cargo_artifact_name())
}

/// why a release could not be assembled, or is not what it says
///
/// deliberately closed and deliberately specific. "packaging failed" is a
/// message somebody has to reproduce to act on; every variant here names the
/// file and what was wrong with it
#[derive(Debug, thiserror::Error)]
pub enum Refused {
    /// the platform tag is not one pip could read
    ///
    /// a wheel's filename joins its fields with dashes, so a dash inside one
    /// makes a name pip parses as different fields entirely — and what installs
    /// is some other version of something else
    #[error(
        "`{tag}` is not a platform tag a wheel filename can carry. it joins its \
         fields with `-`, so a tag with one in it becomes two fields — write it \
         the way pip does, `macosx_11_0_arm64` rather than `macosx-11-0-arm64`"
    )]
    PlatformTag {
        /// the tag as it was given
        tag: String,
    },

    /// the version is not one a wheel filename can carry
    ///
    /// the same rule as the platform tag, and the one the cargo version of this
    /// workspace fails: `0.0.1-a1` written into a filename makes a field pip
    /// reads as a build tag on the version `0.0.1`
    #[error(
        "`{given}` is not a version a wheel filename can carry. it joins its \
         fields with `-`, so a version with one in it becomes two fields and \
         installs as some other version — write it the way pep 440 does, \
         `0.0.1a1` rather than `0.0.1-a1`"
    )]
    Version {
        /// the version as it was given
        given: String,
    },

    /// the distribution is not a name
    ///
    /// it is refused rather than escaped into something acceptable, because
    /// what escaping produces is a perfectly good name for a different project
    #[error(
        "`{given}` is not a distribution name. it is alphanumeric at both ends \
         with `-`, `_` and `.` the only punctuation between — anything else is \
         a name pip reads back out of the filename as some other distribution"
    )]
    DistributionName {
        /// the name as it was given
        given: String,
    },

    /// the zip underneath the wheel refused something
    #[error("the wheel could not be written: {what}: {said}")]
    Zip {
        /// what was being done
        what: &'static str,
        /// what the zip writer said
        said: String,
    },

    /// the binary to ship is not there
    #[error(
        "the binary to ship is `{path}`, and there is no file there. a release \
         is assembled from artifacts that already exist — this builds nothing"
    )]
    NoBinary {
        /// where it was looked for
        path: PathBuf,
    },

    /// no agent was given
    #[error(
        "a release carries an agent per interpreter tag and none was given. one \
         with no agents debugs nothing at all: every launch refuses with the \
         tag it wanted and the tags that are carried, and that list is empty"
    )]
    NoAgents,

    /// a tag was given that is not one
    #[error(
        "`{given}` is not an interpreter tag. a tag is what an interpreter says \
         it is — `3.13`, `3.14`, `3.14t` — and it is parsed here by the same \
         parser that reads one back at launch, so a spelling this rejects is \
         one no interpreter would ever match"
    )]
    NotATag {
        /// the text that was given
        given: String,
    },

    /// the same tag was given twice
    #[error(
        "the tag `{tag}` was given twice, as `{first}` and `{second}`. which of \
         them a release carried would be whichever was copied last, and a \
         release whose contents depend on argument order is one nobody can \
         reproduce"
    )]
    TagTwice {
        /// the tag
        tag: InterpreterTag,
        /// the first file given for it
        first: PathBuf,
        /// the second
        second: PathBuf,
    },

    /// an agent file is not there
    #[error(
        "the agent for `{tag}` is `{path}`, and there is no file there. it is \
         built by `PYO3_PYTHON=python{tag} cargo build -p bpd_agent --release`"
    )]
    NoAgent {
        /// the tag it was given for
        tag: InterpreterTag,
        /// where it was looked for
        path: PathBuf,
    },

    /// the output directory is already holding something
    #[error(
        "`{path}` already holds something. a release is assembled into an empty \
         directory and never over the top of one: a layout with a leftover \
         agent in it carries a tag nothing built, and would ship"
    )]
    NotEmpty {
        /// the directory
        path: PathBuf,
    },

    /// a file could not be read or written
    #[error("{what} `{path}`: {source}")]
    File {
        /// what was being done
        what: &'static str,
        /// the file it was being done to
        path: PathBuf,
        /// the reason the filesystem gave
        source: std::io::Error,
    },

    /// the manifest is not there, or does not read as one
    #[error(
        "`{path}` is not a manifest bpd wrote: {why}. a layout without one \
         cannot be checked at all, and an unchecked layout is one whose \
         contents are somebody's word"
    )]
    NotAManifest {
        /// where it was looked for
        path: PathBuf,
        /// what was wrong with it
        why: String,
    },

    /// a file in the layout is not what the manifest says
    #[error(
        "`{file}` is not what the manifest says it is. the manifest has \
         {expected} and the file on disk is {found} — so this layout has been \
         changed since it was assembled, and what else changed is not knowable \
         from here"
    )]
    Changed {
        /// the file, relative to the layout root
        file: String,
        /// the digest the manifest carries
        expected: String,
        /// the digest the file has now
        found: String,
    },

    /// the manifest names a file the layout does not hold
    #[error(
        "the manifest names `{file}` and the layout does not hold it. a release \
         missing a file it says it carries refuses at whichever launch reaches \
         for it, which is a machine other than this one"
    )]
    Missing {
        /// the file, relative to the layout root
        file: String,
    },
}

/// what a layout holds, and the digest of every file in it
///
/// the evidence rather than a description: [`verify`] reads it back and
/// compares, so a layout that has been changed since it was assembled says so
/// instead of being trusted
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// every file, relative to the layout root, in a fixed order
    ///
    /// sorted, so that assembling the same inputs twice produces the same
    /// manifest byte for byte. a release nobody can reproduce is one whose
    /// contents are an assertion
    pub files: BTreeMap<String, String>,

    /// the interpreter tags this layout carries an agent for
    pub tags: Vec<InterpreterTag>,
}

impl fmt::Display for Manifest {
    /// the manifest as it is written, and as it is read back
    ///
    /// one file a line, digest first, two spaces, path — the shape `sha256sum`
    /// writes, so that a person who does not have `bpd` can check a release with
    /// a tool they already have
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (file, digest) in &self.files {
            writeln!(out, "{digest}  {file}")?;
        }
        Ok(())
    }
}

/// the sha-256 of a file, in the form the manifest carries
fn digest_of(path: &Path) -> Result<String, Refused> {
    let bytes = std::fs::read(path).map_err(|source| Refused::File {
        what: "reading",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(format!("sha256:{:x}", Sha256::digest(&bytes)))
}

/// read the tags and files a release was asked to carry
///
/// each is `<tag>=<path>`, which is the shape a shell loop over interpreters
/// produces without quoting anything
///
/// # errors
///
/// when a tag is not one, or the same tag is given twice
pub fn agents(given: &[String]) -> Result<BTreeMap<InterpreterTag, PathBuf>, Refused> {
    let mut agents: BTreeMap<InterpreterTag, PathBuf> = BTreeMap::new();
    for entry in given {
        let (tag, path) = entry.split_once('=').ok_or_else(|| Refused::NotATag {
            given: entry.clone(),
        })?;
        let tag = InterpreterTag::parse(tag).ok_or_else(|| Refused::NotATag {
            given: tag.to_string(),
        })?;
        if let Some(first) = agents.get(&tag) {
            return Err(Refused::TagTwice {
                tag,
                first: first.clone(),
                second: PathBuf::from(path),
            });
        }
        agents.insert(tag, PathBuf::from(path));
    }
    Ok(agents)
}

/// build the layout a released `bpd` is shipped as
///
/// every input is checked before anything is written, so a refusal leaves no
/// directory behind — the same "never applied partially" rule the debugger
/// holds itself to, applied to the thing that builds it
///
/// # errors
///
/// when an input is missing, a tag is not one, or the output is not empty
pub fn assemble(
    binary: &Path,
    agents: &BTreeMap<InterpreterTag, PathBuf>,
    out: &Path,
) -> Result<Manifest, Refused> {
    if !binary.is_file() {
        return Err(Refused::NoBinary {
            path: binary.to_path_buf(),
        });
    }
    if agents.is_empty() {
        return Err(Refused::NoAgents);
    }
    for (tag, path) in agents {
        if !path.is_file() {
            return Err(Refused::NoAgent {
                tag: *tag,
                path: path.clone(),
            });
        }
    }
    // checked before the first write, and it is the reason the checks above are
    // all up here: a layout half assembled and then refused is a directory
    // somebody has to know not to ship
    let occupied = match std::fs::read_dir(out) {
        Ok(mut entries) => entries.next().is_some(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(source) => {
            return Err(Refused::File {
                what: "reading",
                path: out.to_path_buf(),
                source,
            });
        }
    };
    if occupied {
        return Err(Refused::NotEmpty {
            path: out.to_path_buf(),
        });
    }

    let mut files = BTreeMap::new();
    let name = binary_name();
    copy(binary, &out.join(&name))?;
    files.insert(name, digest_of(binary)?);

    for (tag, path) in agents {
        let at = agent_at(*tag);
        copy(path, &out.join(&at))?;
        files.insert(at.to_string_lossy().into_owned(), digest_of(path)?);
    }

    let manifest = Manifest {
        files,
        tags: agents.keys().copied().collect(),
    };
    write(&out.join(MANIFEST), manifest.to_string().as_bytes())?;
    Ok(manifest)
}

/// copy one file, making the directory above it
fn copy(from: &Path, to: &Path) -> Result<(), Refused> {
    if let Some(directory) = to.parent() {
        std::fs::create_dir_all(directory).map_err(|source| Refused::File {
            what: "creating",
            path: directory.to_path_buf(),
            source,
        })?;
    }
    std::fs::copy(from, to)
        .map(|_written| ())
        .map_err(|source| Refused::File {
            what: "copying to",
            path: to.to_path_buf(),
            source,
        })
}

/// write one file
fn write(path: &Path, bytes: &[u8]) -> Result<(), Refused> {
    std::fs::write(path, bytes).map_err(|source| Refused::File {
        what: "writing",
        path: path.to_path_buf(),
        source,
    })
}

/// read a layout back and say whether it is still what its manifest claims
///
/// what makes the manifest evidence rather than decoration. it is the same
/// discipline `bpd_core::SourceMap` holds: a digest that is written and never
/// checked says nothing at all
///
/// # errors
///
/// when the manifest is missing or unreadable, a file it names is gone, or a
/// file's contents have changed since it was assembled
pub fn verify(layout: &Path) -> Result<Manifest, Refused> {
    let path = layout.join(MANIFEST);
    let text = std::fs::read_to_string(&path).map_err(|source| Refused::File {
        what: "reading",
        path: path.clone(),
        source,
    })?;

    let mut files = BTreeMap::new();
    let mut tags = Vec::new();
    for (at, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let (digest, file) = line.split_once("  ").ok_or_else(|| Refused::NotAManifest {
            path: path.clone(),
            why: format!(
                "line {} is `{line}`, and a line is a digest, two spaces, a path",
                at + 1
            ),
        })?;
        if !digest.starts_with("sha256:") {
            return Err(Refused::NotAManifest {
                path: path.clone(),
                why: format!("line {} carries `{digest}`, which is not a sha-256", at + 1),
            });
        }

        let whole = layout.join(file);
        if !whole.is_file() {
            return Err(Refused::Missing {
                file: file.to_string(),
            });
        }
        let found = digest_of(&whole)?;
        if found != digest {
            return Err(Refused::Changed {
                file: file.to_string(),
                expected: digest.to_string(),
                found,
            });
        }

        // the tags are read back out of the paths rather than carried
        // separately, so a manifest cannot come to name a tag whose agent is
        // not in the list of files it checked
        if let Some(tag) = file
            .strip_prefix("agents/")
            .and_then(|rest| rest.split('/').next())
            .and_then(InterpreterTag::parse)
        {
            tags.push(tag);
        }
        files.insert(file.to_string(), digest.to_string());
    }

    if files.is_empty() {
        return Err(Refused::NotAManifest {
            path,
            why: "it is empty, and an empty manifest checks nothing".to_string(),
        });
    }
    tags.sort_unstable();
    tags.dedup();
    Ok(Manifest { files, tags })
}
