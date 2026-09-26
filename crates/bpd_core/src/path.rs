//! a path resolved the way an interpreter spells one
//!
//! **windows `canonicalize` returns the verbatim form** — `\\?\C:\Users\…` —
//! and no interpreter ever says that about itself: `co_filename`, `os.getcwd()`
//! and `sys.path[0]` are `C:\Users\…`, and so is every path an editor sends. a
//! location reported in the first form names the right file and still matches
//! nothing a client holds, so an editor opens a second tab for it, or none
//!
//! only the drive form is unwrapped. `\\?\UNC\server\share` is a different path
//! from `\\server\share` to some apis, and a path that is only reachable in its
//! verbatim form is left in it rather than shortened into one that is not

use std::io;
use std::path::{Path, PathBuf};

/// `path` with its links resolved, spelled the way an interpreter spells it
///
/// # errors
///
/// whatever `canonicalize` said: the file is not there, or a component of it
/// cannot be read
pub fn resolved(path: &Path) -> io::Result<PathBuf> {
    path.canonicalize().map(plainly)
}

/// a canonical path with the verbatim drive prefix taken off, where it has one
#[must_use]
pub fn plainly(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};

        if let Some(Component::Prefix(prefix)) = path.components().next()
            && let Prefix::VerbatimDisk(letter) = prefix.kind()
        {
            let spelled = path.to_string_lossy();
            let rest = spelled
                .strip_prefix(r"\\?\")
                .unwrap_or_else(|| unreachable!("a verbatim disk prefix is spelled `\\\\?\\`"));
            assert!(
                rest.starts_with(&format!("{}:", letter as char)),
                "the verbatim prefix is all that came off {spelled}"
            );
            return PathBuf::from(rest);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resolved_path_is_the_file_it_resolves() {
        let directory = tempfile::tempdir().expect("a temporary directory is available");
        let file = directory.path().join("app.by");
        std::fs::write(&file, "").expect("the file is written");

        let resolved = resolved(&file).expect("the file is there");
        assert!(resolved.is_absolute(), "{}", resolved.display());
        assert_eq!(
            std::fs::read(&resolved).expect("the resolved path opens"),
            b"",
            "{} is the file that was written",
            resolved.display()
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_resolved_path_has_no_verbatim_prefix() {
        let directory = tempfile::tempdir().expect("a temporary directory is available");
        let resolved = resolved(directory.path()).expect("the directory is there");
        let spelled = resolved.display().to_string();
        assert!(
            !spelled.starts_with(r"\\?\"),
            "{spelled} is not how an interpreter spells a path"
        );
        assert!(
            directory
                .path()
                .canonicalize()
                .expect("the directory is there")
                .display()
                .to_string()
                .starts_with(r"\\?\"),
            "`canonicalize` no longer returns the verbatim form, so this module \
             has nothing left to take off and can go"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_verbatim_unc_path_is_left_as_it_is() {
        let unc = PathBuf::from(r"\\?\UNC\server\share\app.by");
        assert_eq!(plainly(unc.clone()), unc);
    }

    #[test]
    fn a_plain_path_is_left_as_it_is() {
        let plain = std::env::temp_dir();
        assert_eq!(plainly(plain.clone()), plain);
    }
}
