//! the wheel a layout is delivered as
//!
//! nothing here needs an interpreter — the files are stand-ins, as in
//! `layout.rs`. what is under test is the **shape**: a wheel whose payload lands
//! somewhere agent resolution does not look installs cleanly and then cannot
//! debug anything, which is the failure this is written against
//!
//! the install itself is driven in CI, where a real `pip` puts a real wheel into
//! a real venv and a program is debugged through it. that is the half no
//! assertion about a zip can stand in for

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use bpd_core::python::InterpreterTag;
use bpd_release::wheel::DISTRIBUTION;
use bpd_release::{Refused, assemble, binary_name, wheel};

fn tag(text: &str) -> InterpreterTag {
    InterpreterTag::parse(text).unwrap_or_else(|| panic!("`{text}` is a tag"))
}

fn file(at: &Path, name: &str, contents: &str) -> PathBuf {
    let path = at.join(name);
    std::fs::write(&path, contents).expect("the temporary directory is writable");
    path
}

/// an assembled layout with a binary and two agents in it
fn layout(at: &Path) -> PathBuf {
    let binary = file(at, "bpd", "the debugger");
    let agents = BTreeMap::from([
        (tag("3.13"), file(at, "agent-3.13.so", "for 3.13")),
        (tag("3.14t"), file(at, "agent-3.14t.so", "for 3.14t")),
    ]);
    let out = at.join("layout");
    assemble(&binary, &agents, &out).expect("the layout was assembled");
    out
}

/// every entry of a wheel, by name
fn entries(path: &Path) -> Vec<String> {
    let file = std::fs::File::open(path).expect("the wheel was written");
    let mut zip = zip::ZipArchive::new(file).expect("a wheel is a zip");
    (0..zip.len())
        .map(|index| {
            zip.by_index(index)
                .expect("an entry the archive just counted")
                .name()
                .to_string()
        })
        .collect()
}

/// one entry's bytes
fn read(path: &Path, name: &str) -> Vec<u8> {
    let file = std::fs::File::open(path).expect("the wheel was written");
    let mut zip = zip::ZipArchive::new(file).expect("a wheel is a zip");
    let mut entry = zip
        .by_name(name)
        .unwrap_or_else(|_| panic!("`{name}` is in the wheel"));
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .expect("the entry is readable");
    bytes
}

#[test]
fn the_payload_lands_where_agent_resolution_already_looks() {
    // the whole design in one assertion. `bpd_engine::agent` looks for
    // `agents/<tag>/` in the directory holding the running binary and the one
    // above it, which for an installed binary is `<prefix>/bin` and `<prefix>`.
    // a wheel's `scripts` installs into the first and its `data` into the
    // second, so this layout is that layout — and no engine code knows a wheel
    // exists
    let held = tempfile::tempdir().expect("a temporary directory");
    let built = wheel(
        &layout(held.path()),
        DISTRIBUTION,
        "0.1.0",
        "macosx_11_0_arm64",
        &held.path().join("dist"),
    )
    .expect("the wheel was written");

    let names = entries(&built.path);
    assert!(
        names.contains(&format!(
            "basedpython_debugger-0.1.0.data/scripts/{}",
            binary_name()
        )),
        "the binary has to go to `scripts`, which is `<prefix>/bin`: {names:#?}"
    );
    // the filename comes from `agent_at`, which is what the layout and the
    // engine both use — the assertion here is the **directory** it sits in,
    // since that is what decides whether resolution ever finds it
    //
    // spelled with `/` rather than taken from `agent_at`, because a zip's entry
    // names are `/` by the format's own rule and taking the separator from the
    // platform is how a windows wheel came to carry `agents\3.13\…`. a test
    // that asked `agent_at` would have agreed with that
    for tag_of in ["3.13", "3.14t"] {
        let artifact = bpd_engine::agent::cargo_artifact_name();
        let at = format!("basedpython_debugger-0.1.0.data/data/agents/{tag_of}/{artifact}");
        assert!(
            names.contains(&at),
            "the agent for {tag_of} has to go to `data`, which is `<prefix>`, \
             the directory above `bin`: {names:#?}"
        );
        assert!(
            at.contains(&format!("/agents/{tag_of}/")),
            "and under `agents/<tag>/`, which is the shape resolution walks: {at}"
        );
    }
}

#[test]
fn the_filename_carries_the_escaped_name_and_the_metadata_carries_the_real_one() {
    // pip reads the distribution back out of the filename by splitting on `-`,
    // so a name with one in it is a name it reads as a different project at a
    // different version. the specs answer that by escaping the name for the
    // filename — and pypi decides which project an upload belongs to from
    // `METADATA`, so the two have to differ and both have to be right
    let held = tempfile::tempdir().expect("a temporary directory");
    let built = wheel(
        &layout(held.path()),
        DISTRIBUTION,
        "0.1.0",
        "macosx_11_0_arm64",
        &held.path().join("dist"),
    )
    .expect("the wheel was written");

    let filename = built
        .path
        .file_name()
        .expect("a wheel has a filename")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        filename, "basedpython_debugger-0.1.0-py3-none-macosx_11_0_arm64.whl",
        "the filename carries the escaped name, and exactly five dash-joined \
         fields with it"
    );
    assert_eq!(
        filename.matches('-').count(),
        4,
        "a wheel filename has five fields, and pip splits them on `-`: {filename}"
    );

    let metadata = String::from_utf8(read(
        &built.path,
        "basedpython_debugger-0.1.0.dist-info/METADATA",
    ))
    .expect("METADATA is utf8");
    assert!(
        metadata.contains(&format!("Name: {DISTRIBUTION}\n")),
        "the metadata carries the name pypi holds the project under, not the \
         escaped one: {metadata}"
    );
    assert!(
        metadata.contains(&format!(
            "Requires-Python: >={}\n",
            bpd_core::python::MINIMUM_SUPPORTED
        )),
        "and the floor the debugger itself holds: {metadata}"
    );
}

#[test]
fn the_binary_is_carried_under_the_name_the_platform_runs_it_by() {
    // windows runs a file by its extension. a wheel carrying `bpd` there
    // installs `Scripts/bpd`, which pip is perfectly happy with and windows
    // cannot execute — so the name comes from the same place the layout's does
    let held = tempfile::tempdir().expect("a temporary directory");
    let built = wheel(
        &layout(held.path()),
        DISTRIBUTION,
        "0.1.0",
        "win_amd64",
        &held.path().join("dist"),
    )
    .expect("the wheel was written");

    let at = format!("basedpython_debugger-0.1.0.data/scripts/{}", binary_name());
    assert!(
        entries(&built.path).contains(&at),
        "the script entry is the name this platform executes: {:#?}",
        entries(&built.path)
    );
    assert_eq!(
        std::env::consts::EXE_SUFFIX.is_empty(),
        !cfg!(windows),
        "and that name is the platform's, which is what makes the line above a \
         claim about windows rather than about this machine"
    );
}

#[test]
fn the_wheel_is_tagged_for_a_platform_and_not_for_an_interpreter() {
    // the decision this feature turns on. `bpd` links nothing of cpython and
    // drives interpreters it is handed, so a `cp313-cp313-…` wheel would ship a
    // copy of the same binary per python version and leave each install able to
    // debug exactly one of them
    let held = tempfile::tempdir().expect("a temporary directory");
    let built = wheel(
        &layout(held.path()),
        DISTRIBUTION,
        "0.1.0",
        "macosx_11_0_arm64",
        &held.path().join("dist"),
    )
    .expect("the wheel was written");

    assert_eq!(built.tag, "py3-none-macosx_11_0_arm64");
    assert!(
        built
            .path
            .file_name()
            .expect("a wheel has a filename")
            .to_string_lossy()
            .ends_with("-py3-none-macosx_11_0_arm64.whl"),
        "the filename carries the tag, and pip reads it from there: {}",
        built.path.display()
    );

    // and one wheel carries every agent the layout had, which is what makes it
    // python agnostic rather than merely tagged that way
    let names = entries(&built.path);
    let agents = names
        .iter()
        .filter(|name| name.contains("/agents/"))
        .count();
    assert_eq!(
        agents, 2,
        "a per-platform wheel carries every agent for that platform: {names:#?}"
    );

    let metadata = String::from_utf8(read(
        &built.path,
        "basedpython_debugger-0.1.0.dist-info/WHEEL",
    ))
    .expect("WHEEL is utf8");
    assert!(
        metadata.contains("Root-Is-Purelib: false"),
        "true puts the payload in `site-packages`, where nothing looks for an \
         agent: {metadata}"
    );
    assert!(
        metadata.contains("Tag: py3-none-macosx_11_0_arm64"),
        "said {metadata}"
    );
}

#[test]
fn record_names_every_file_with_a_digest_of_what_was_written() {
    // RECORD is what pip checks the install against, so a digest of anything
    // other than the bytes in the archive is a wheel that installs and then
    // fails a verification nobody ran until later
    let held = tempfile::tempdir().expect("a temporary directory");
    let built = wheel(
        &layout(held.path()),
        DISTRIBUTION,
        "0.1.0",
        "macosx_11_0_arm64",
        &held.path().join("dist"),
    )
    .expect("the wheel was written");

    let record = String::from_utf8(read(
        &built.path,
        "basedpython_debugger-0.1.0.dist-info/RECORD",
    ))
    .expect("RECORD is utf8");

    for line in record.lines() {
        let mut fields = line.split(',');
        let name = fields.next().expect("a RECORD line names a file");
        let digest = fields.next().expect("and carries a digest field");
        let length = fields.next().expect("and a length field");

        // RECORD cannot carry its own digest, and says so with two empty fields
        if name.ends_with("/RECORD") {
            assert_eq!(
                (digest, length),
                ("", ""),
                "RECORD lists itself with neither, and this says {line:?}"
            );
            continue;
        }

        let bytes = read(&built.path, name);
        assert_eq!(
            length,
            bytes.len().to_string(),
            "the length in RECORD is not what the archive holds for {name}"
        );
        let expected = format!("sha256={}", urlsafe(&bytes));
        assert_eq!(
            digest, expected,
            "the digest in RECORD is not of the bytes in the archive for {name}"
        );
    }

    // and everything in the archive is listed, so nothing arrives unrecorded
    for name in entries(&built.path) {
        assert!(
            record
                .lines()
                .any(|line| line.starts_with(&format!("{name},"))),
            "`{name}` is in the wheel and not in RECORD:\n{record}"
        );
    }
}

/// the same encoding RECORD uses, computed independently of the code under test
fn urlsafe(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let digest = Sha256::digest(bytes);
    let mut out = String::new();
    for chunk in digest.chunks(3) {
        let mut block = [0_u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let packed = (u32::from(block[0]) << 16) | (u32::from(block[1]) << 8) | u32::from(block[2]);
        for index in 0..=chunk.len() {
            out.push(char::from(
                ALPHABET[((packed >> (18 - index * 6)) & 0x3f) as usize],
            ));
        }
    }
    out
}

#[test]
fn a_layout_that_no_longer_matches_its_manifest_is_not_shipped() {
    // the last moment anything can check. a wheel built from a layout whose
    // digests have moved would ship exactly what the manifest exists to catch,
    // with pip's own RECORD then attesting to the wrong bytes
    let held = tempfile::tempdir().expect("a temporary directory");
    let at = layout(held.path());
    std::fs::write(at.join(binary_name()), "something else entirely")
        .expect("the layout is writable in a test");

    let refused = wheel(
        &at,
        DISTRIBUTION,
        "0.1.0",
        "macosx_11_0_arm64",
        &held.path().join("dist"),
    );
    assert!(
        matches!(refused, Err(Refused::Changed { .. })),
        "a layout that does not verify must not become a wheel, got {refused:?}"
    );
}
