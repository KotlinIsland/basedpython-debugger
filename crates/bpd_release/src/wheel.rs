//! the same layout, delivered by pip
//!
//! ## why a wheel is only a delivery vehicle here
//!
//! the `bpd` binary is not a python extension. it links nothing of cpython —
//! `otool -L` on it names no libpython — and it drives interpreters it is handed
//! rather than the one it lives inside. only the **agent** is version specific,
//! and it is loaded by the debuggee, which is very often not the interpreter
//! anybody ran `pip install` in
//!
//! so the wheel is tagged `py3-none-<platform>`: one per platform, carrying
//! every agent for it. tagging it per interpreter — `cp313-cp313-…` — would ship
//! one copy of the same binary per python version and leave each install able to
//! debug exactly one of them, which is a tool made smaller by its packaging.
//! this is the shape `ruff` and `uv` ship in, for the same reason
//!
//! ## the layout is already the install layout
//!
//! a wheel's `.data` directory installs into the environment's own scheme
//! directories: `scripts` becomes `<prefix>/bin` and `data` becomes `<prefix>`.
//! `bpd_engine::agent` looks for `agents/<tag>/` in the directory holding the
//! running binary **and the one above it** — which for an installed binary is
//! `<prefix>/bin` and `<prefix>`
//!
//! so a wheel that puts the binary in `scripts/` and the agents under
//! `data/agents/` produces exactly the layout that resolution already expects.
//! nothing in the engine knows a wheel exists
//!
//! ## there is no sdist
//!
//! and there will not be one. building this from source needs cargo and **one
//! interpreter per agent**, so an sdist that pip could not actually build would
//! be a package that installs by appearing to and then failing at the first
//! launch. what ships is what was built and verified

use std::fmt::Write as _;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{Manifest, Refused, verify};

/// the metadata version every field below is written to
const METADATA_VERSION: &str = "2.1";

/// the interpreter floor, which is the debugger's own and not the wheel's
///
/// a `py3-none-…` wheel installs into any python, and this is what stops it
/// installing somewhere it could not debug. it is the same floor
/// [`bpd_core::python::MINIMUM_SUPPORTED`] holds, written the way pip reads it
fn requires_python() -> String {
    format!(">={}", bpd_core::python::MINIMUM_SUPPORTED)
}

/// what a wheel was written as, so a caller can say where it went
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wheel {
    /// where the file is
    pub path: PathBuf,
    /// its distribution name
    pub distribution: String,
    /// the version it was built for
    pub version: String,
    /// the full tag, as it appears in the filename
    pub tag: String,
    /// what went in, in the order it was written
    pub contents: Vec<String>,
}

/// write an assembled layout out as a wheel
///
/// the layout is **verified first**, by the same [`verify`] a release goes
/// through: a wheel built from a layout whose digests no longer match would ship
/// the one thing the manifest exists to prevent
///
/// `platform` is taken rather than detected, and that is deliberate. what
/// manylinux level a linux binary satisfies is a fact about the toolchain that
/// built it, which this program cannot see — guessing it produces a wheel pip
/// installs on a machine whose libc is too old, and the failure lands at the
/// first launch rather than at install time
///
/// # errors
///
/// when the layout does not verify, when the output cannot be written, or when
/// the platform tag is not one pip could parse
pub fn wheel(
    layout: &Path,
    distribution: &str,
    version: &str,
    platform: &str,
    out: &Path,
) -> Result<Wheel, Refused> {
    if platform.is_empty() || platform.contains('-') {
        return Err(Refused::PlatformTag {
            tag: platform.to_string(),
        });
    }

    // the layout is evidence only if it is checked, and this is the last moment
    // anything can check it
    let manifest = verify(layout)?;

    let tag = format!("py3-none-{platform}");
    let name = format!("{distribution}-{version}-{tag}.whl");
    std::fs::create_dir_all(out).map_err(|source| Refused::File {
        what: "creating",
        path: out.to_path_buf(),
        source,
    })?;
    let path = out.join(&name);

    let file = std::fs::File::create(&path).map_err(|source| Refused::File {
        what: "creating",
        path: path.clone(),
        source,
    })?;
    let contents = write_into(file, layout, &manifest, distribution, version, &tag)?;

    Ok(Wheel {
        path,
        distribution: distribution.to_string(),
        version: version.to_string(),
        tag,
        contents,
    })
}

/// what a wheel records about one file it carries
struct Recorded {
    at: String,
    digest: String,
    length: u64,
}

/// everything except opening the file, so a test can write into memory
fn write_into<W: Write + Seek>(
    into: W,
    layout: &Path,
    manifest: &Manifest,
    distribution: &str,
    version: &str,
    tag: &str,
) -> Result<Vec<String>, Refused> {
    let mut zip = zip::ZipWriter::new(into);
    let mut recorded: Vec<Recorded> = Vec::new();

    let data = format!("{distribution}-{version}.data");
    let dist_info = format!("{distribution}-{version}.dist-info");

    // the binary goes to `scripts`, which pip installs into `<prefix>/bin` — and
    // it has to keep its executable bit or what lands there cannot be run
    let binary = layout.join(crate::BINARY);
    recorded.push(add(
        &mut zip,
        &format!("{data}/scripts/{}", crate::BINARY),
        &std::fs::read(&binary).map_err(|source| Refused::File {
            what: "reading",
            path: binary.clone(),
            source,
        })?,
        0o755,
    )?);

    // and the agents to `data`, which pip installs into `<prefix>` — the
    // directory above `bin`, which is the second place agent resolution looks
    for tag_of in &manifest.tags {
        let at = crate::agent_at(*tag_of);
        let from = layout.join(&at);
        let bytes = std::fs::read(&from).map_err(|source| Refused::File {
            what: "reading",
            path: from.clone(),
            source,
        })?;
        recorded.push(add(
            &mut zip,
            &format!("{data}/data/{}", at.to_string_lossy()),
            &bytes,
            0o644,
        )?);
    }

    recorded.push(add(
        &mut zip,
        &format!("{dist_info}/METADATA"),
        metadata(distribution, version).as_bytes(),
        0o644,
    )?);
    recorded.push(add(
        &mut zip,
        &format!("{dist_info}/WHEEL"),
        wheel_metadata(tag).as_bytes(),
        0o644,
    )?);

    // RECORD is last and lists itself with neither a hash nor a length, because
    // it cannot contain its own digest
    let mut record = String::new();
    for one in &recorded {
        writeln!(record, "{},sha256={},{}", one.at, one.digest, one.length).map_err(|_| {
            Refused::Zip {
                what: "writing RECORD",
                said: "a string would not take more text".to_string(),
            }
        })?;
    }
    writeln!(record, "{dist_info}/RECORD,,").map_err(|_| Refused::Zip {
        what: "writing RECORD",
        said: "a string would not take more text".to_string(),
    })?;
    add(
        &mut zip,
        &format!("{dist_info}/RECORD"),
        record.as_bytes(),
        0o644,
    )?;

    zip.finish().map_err(|source| Refused::Zip {
        what: "finishing the wheel",
        said: source.to_string(),
    })?;

    let mut written: Vec<String> = recorded.into_iter().map(|one| one.at).collect();
    written.push(format!("{dist_info}/RECORD"));
    Ok(written)
}

/// one file into the zip, and what RECORD will say about it
fn add<W: Write + Seek>(
    zip: &mut zip::ZipWriter<W>,
    at: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<Recorded, Refused> {
    // stored rather than deflated. the payload is native binaries, which do not
    // compress to much, and it keeps this crate off a compression backend
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(mode);
    zip.start_file(at, options).map_err(|source| Refused::Zip {
        what: "starting a file in the wheel",
        said: source.to_string(),
    })?;
    zip.write_all(bytes).map_err(|source| Refused::File {
        what: "writing into the wheel",
        path: PathBuf::from(at),
        source,
    })?;

    Ok(Recorded {
        at: at.to_string(),
        digest: urlsafe(&Sha256::digest(bytes)),
        length: bytes.len() as u64,
    })
}

/// a digest as RECORD carries one: urlsafe base64, with the padding stripped
fn urlsafe(digest: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in digest.chunks(3) {
        let mut block = [0_u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let packed = (u32::from(block[0]) << 16) | (u32::from(block[1]) << 8) | u32::from(block[2]);
        // one output character per six bits, and one fewer group than the input
        // had bytes plus one — which is what dropping the padding means
        for index in 0..=chunk.len() {
            let shift = 18 - index * 6;
            out.push(char::from(ALPHABET[((packed >> shift) & 0x3f) as usize]));
        }
    }
    out
}

/// the `METADATA` a wheel carries
fn metadata(distribution: &str, version: &str) -> String {
    format!(
        "Metadata-Version: {METADATA_VERSION}\n\
         Name: {distribution}\n\
         Version: {version}\n\
         Summary: a debugger for python and basedpython\n\
         Requires-Python: {}\n\
         \n\
         `bpd` is a native binary. this wheel carries it and one agent per\n\
         interpreter it can debug, and pip is only how they arrive — nothing in\n\
         it is imported by the python that installed it.\n",
        requires_python()
    )
}

/// the `WHEEL` a wheel carries
///
/// `Root-Is-Purelib: false` is the one field here with teeth. true would have
/// pip put the payload in `site-packages`, and nothing looks for an agent there
fn wheel_metadata(tag: &str) -> String {
    format!(
        "Wheel-Version: 1.0\n\
         Generator: bpd-release\n\
         Root-Is-Purelib: false\n\
         Tag: {tag}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_is_written_the_way_record_carries_one() {
        // urlsafe base64 with no padding, which is what pip's own installer
        // compares against. the empty input's sha-256 is a fixed value, so this
        // is checked against something written down rather than against itself
        let empty = urlsafe(&Sha256::digest(b""));
        assert_eq!(empty, "47DEQpj8HBSa-_TImW-5JCeuQeRkm5NMpJWZG3hSuFU");
        assert!(
            !empty.contains('=') && !empty.contains('+') && !empty.contains('/'),
            "padding and the non-urlsafe alphabet both break RECORD: {empty}"
        );
    }

    #[test]
    fn a_platform_tag_with_a_dash_in_it_is_refused() {
        // the filename joins its fields with dashes, so a dash inside one makes
        // a name pip parses as different fields entirely — and the wheel
        // installs as some other version of something else
        let refused = wheel(
            Path::new("/tmp/nothing"),
            "basedpythondebugger",
            "0.1.0",
            "macosx-11-0-arm64",
            Path::new("/tmp/nowhere"),
        );
        assert!(
            matches!(refused, Err(Refused::PlatformTag { .. })),
            "a dash in the platform tag has to be refused, got {refused:?}"
        );
    }
}
