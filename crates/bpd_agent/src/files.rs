//! deciding whether the path a user typed and a `co_filename` are the same file
//!
//! this is where breakpoints quietly fail to bind. the user gives a path, the
//! interpreter has whatever string was handed to `compile`, and comparing the
//! two as text is wrong in every direction: a symlinked editable install, a
//! `/var` that is really `/private/var`, a case-insensitive filesystem where
//! `Widget.py` and `widget.py` are one file
//!
//! so text is not compared at all. the identity used is the **filesystem's
//! own**, which is the only answer that is right for all of those at once. a
//! path that has no such identity — a pseudo-filename like `<string>`, a
//! location inside a zip archive, a frozen module — has no identity at all, and
//! a breakpoint against it is refused rather than matched by resemblance

use std::path::Path;

#[cfg(not(any(unix, windows)))]
compile_error!(
    "bpd has no file identity for this platform. binding a breakpoint by \
     comparing path text would silently bind the wrong file, so the build \
     refuses rather than guessing"
);

/// what the filesystem calls this file, whatever path was used to reach it
///
/// two paths denote the same file exactly when their identities are equal
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FileId {
    device: u64,
    inode: u64,
}

/// what the filesystem calls this file, whatever path was used to reach it
///
/// two paths denote the same file exactly when their identities are equal
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FileId {
    canonical: std::path::PathBuf,
}

/// the identity of the file at `path`, or what the filesystem said instead
///
/// follows symlinks, because a breakpoint set through an editable install's
/// symlink and one set on the real file are the same breakpoint
///
/// `(device, inode)` is the unix filesystem's own answer to "same file?", and
/// it is right where comparing canonical paths is wrong: macos `realpath` does
/// **not** normalise case, so on a case-insensitive volume `Case.py` and
/// `case.py` canonicalise to two different strings and are one file
#[cfg(unix)]
pub(crate) fn identify(path: &Path) -> Result<FileId, String> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = require_file(path)?;
    Ok(FileId {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// the identity of the file at `path`, or what the filesystem said instead
///
/// windows has no stable device/inode pair on `Metadata`, and does not need
/// one: `canonicalize` there goes through `GetFinalPathNameByHandle`, which
/// resolves symlinks and junctions **and** returns the casing as it is stored
/// on disk. that makes the canonical path exactly the identity that
/// `(device, inode)` gives on unix
#[cfg(windows)]
pub(crate) fn identify(path: &Path) -> Result<FileId, String> {
    require_file(path)?;
    let canonical = std::fs::canonicalize(path).map_err(|error| error.to_string())?;
    Ok(FileId { canonical })
}

/// the metadata of a real file, or why `path` is not one
///
/// a directory is not somewhere a breakpoint can live, and refusing it here
/// gives the client a reason instead of a binding that quietly never fires
fn require_file(path: &Path) -> Result<std::fs::Metadata, String> {
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if metadata.is_file() {
        Ok(metadata)
    } else {
        Err("is not a regular file".to_string())
    }
}
