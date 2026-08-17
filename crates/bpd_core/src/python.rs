//! what `bpd` needs to know about an interpreter before it will drive it
//!
//! the support policy is in `docs/development/python-support.md` and it is not
//! a suggestion: cpython
//! 3.13 or newer, PEP 669 only, and PEP 768 for attach. this module answers
//! "can this interpreter be debugged" with a yes or a named reason, never with
//! a reduced feature set

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// the oldest interpreter `bpd` will drive
///
/// PEP 669 landed in 3.12, but 3.13 is the first release where
/// `sys.monitoring` reports generator and exception events consistently enough
/// to build stepping on. supporting 3.12 would mean carrying a second set of
/// stepping rules for one release, which is exactly the kind of compromise this
/// project does not make
pub const MINIMUM_SUPPORTED: PythonVersion = PythonVersion::new(3, 13, 0);

/// the oldest interpreter that can be attached to while it is already running
///
/// PEP 768 (`sys.remote_exec`) landed in 3.14. below this, `bpd attach` refuses
/// — there is no ptrace fallback and there will not be one
pub const MINIMUM_ATTACH: PythonVersion = PythonVersion::new(3, 14, 0);

/// `sys.version_info[:3]` of an interpreter
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PythonVersion {
    /// `sys.version_info.major`
    pub major: u8,
    /// `sys.version_info.minor`
    pub minor: u8,
    /// `sys.version_info.micro`
    pub micro: u8,
}

impl PythonVersion {
    /// a version from its three components
    pub const fn new(major: u8, minor: u8, micro: u8) -> Self {
        Self {
            major,
            minor,
            micro,
        }
    }
}

impl fmt::Display for PythonVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.micro)
    }
}

/// which agent build an interpreter can load
///
/// the agent is a cpython extension and is **not** abi3 — it reads
/// `sys.monitoring` and frame internals whose layout changes between releases —
/// so one build belongs to one `major.minor`. a free-threaded interpreter is a
/// **different abi** rather than a variant of the same one: different struct
/// layouts, different reference counting, and the same `sys.version_info`. so
/// the tag carries both, spelled the way cpython spells its own extension
/// suffix — `3.14`, `3.14t`
///
/// it is the one vocabulary three things share: what `bpd` names an agent
/// directory with, what the agent stamps into itself at build time, and what
/// `bpd_agent.verify_interpreter()` compares at import. selection picking the
/// right file and the agent checking it was compiled for the interpreter that
/// imported it are two different guarantees, and they are only comparable
/// because they are said in the same words
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InterpreterTag {
    major: u8,
    minor: u8,
    free_threaded: bool,
}

impl InterpreterTag {
    /// the tag of a release and a build configuration
    pub const fn new(major: u8, minor: u8, free_threaded: bool) -> Self {
        Self {
            major,
            minor,
            free_threaded,
        }
    }

    /// the tag written back out of text — a directory name, or a build stamp
    ///
    /// `None` unless the text is exactly what [`Display`](fmt::Display) would
    /// have written. `3.014` and `3.14.0` name no interpreter, and a tag read
    /// loosely would hand one release's agent to another — which is the load
    /// that imports and then reads the wrong offsets
    pub fn parse(text: &str) -> Option<Self> {
        let (version, free_threaded) = match text.strip_suffix('t') {
            Some(version) => (version, true),
            None => (text, false),
        };
        let (major, minor) = version.split_once('.')?;
        let tag = Self::new(major.parse().ok()?, minor.parse().ok()?, free_threaded);
        (tag.to_string() == text).then_some(tag)
    }
}

impl fmt::Display for InterpreterTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)?;
        if self.free_threaded {
            f.write_str("t")?;
        }
        Ok(())
    }
}

/// which python implementation an interpreter is
///
/// only [`Implementation::CPython`] can be debugged. the variant for everything
/// else exists so the refusal can name what was actually found
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Implementation {
    /// cpython, the only supported implementation
    CPython,
    /// anything else, as reported by `sys.implementation.name`
    Other(Box<str>),
}

impl fmt::Display for Implementation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CPython => f.write_str("cpython"),
            Self::Other(name) => f.write_str(name),
        }
    }
}

/// whether PEP 768 attach is possible for a given interpreter
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteDebug {
    /// `sys.remote_exec` is present and nothing in the environment disables it
    ///
    /// a build configured with `--without-remote-debug` still fails, and can
    /// only be detected at attach time, so it is left for whatever attaches to
    /// report as its own failure rather than guessed at here
    ///
    /// this is a fact about the **interpreter**. nothing in `bpd` attaches yet,
    /// so it is not an offer — see M8 on the roadmap
    Available,
    /// the interpreter predates PEP 768
    MissingApi,
    /// `PYTHON_DISABLE_REMOTE_DEBUG` is set in the environment
    DisabledByEnvironment,
}

impl fmt::Display for RemoteDebug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Available => f.write_str("available"),
            Self::MissingApi => write!(f, "unavailable, needs python {MINIMUM_ATTACH} or newer"),
            Self::DisabledByEnvironment => {
                f.write_str("disabled by `PYTHON_DISABLE_REMOTE_DEBUG` in the environment")
            }
        }
    }
}

/// everything `bpd` learned about an interpreter by asking it
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// the interpreter as it was named on the command line
    pub interpreter: PathBuf,
    /// `sys.executable`, which is the real binary behind a venv shim
    pub executable: PathBuf,
    /// `sys.version_info[:3]`
    pub version: PythonVersion,
    /// `sys.implementation.name`
    pub implementation: Implementation,
    /// a `Py_GIL_DISABLED` build, where the agent must be thread safe without
    /// leaning on the GIL
    pub free_threaded: bool,
    /// a `--with-pydebug` build
    pub debug_build: bool,
    /// `sysconfig.get_config_var("EXT_SUFFIX")`, which names the agent
    /// extension this interpreter can load
    pub ext_suffix: Option<String>,
    /// whether `sys.monitoring` exists — the entire event backbone
    pub monitoring: bool,
    /// whether this interpreter can be attached to while already running
    pub remote_debug: RemoteDebug,
}

impl Capabilities {
    /// the agent build this interpreter can load
    ///
    /// taken from what the interpreter said about itself when it was probed,
    /// which is the only thing that can be right: a path or a file name only
    /// ever *claims* which interpreter it is for
    pub const fn tag(&self) -> InterpreterTag {
        InterpreterTag::new(self.version.major, self.version.minor, self.free_threaded)
    }

    /// ask an interpreter what it is
    pub fn probe(interpreter: &Path) -> Result<Self> {
        let output = Command::new(interpreter)
            .args(["-I", "-c", PROBE])
            .output()
            .map_err(|source| Error::InterpreterLaunch {
                path: interpreter.to_path_buf(),
                source,
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::InterpreterProbe {
                path: interpreter.to_path_buf(),
                reason: match stderr.lines().last() {
                    Some(line) => format!("exited with {}: {line}", output.status),
                    None => format!("exited with {}", output.status),
                },
            });
        }

        let report: ProbeReport =
            serde_json::from_slice(&output.stdout).map_err(|source| Error::InterpreterProbe {
                path: interpreter.to_path_buf(),
                reason: format!("its answer was not the expected json: {source}"),
            })?;

        Ok(report.into_capabilities(interpreter.to_path_buf()))
    }

    /// refuse, with a reason, if this interpreter cannot be debugged at all
    ///
    /// the checks are ordered so the most fundamental mismatch is the one
    /// reported: a python 3.9 pypy is a pypy problem, not a version problem
    pub fn require_debuggable(&self) -> Result<()> {
        if self.implementation != Implementation::CPython {
            return Err(Error::UnsupportedImplementation {
                path: self.interpreter.clone(),
                found: self.implementation.clone(),
            });
        }

        if self.version < MINIMUM_SUPPORTED {
            return Err(Error::UnsupportedVersion {
                path: self.interpreter.clone(),
                found: self.version,
                minimum: MINIMUM_SUPPORTED,
            });
        }

        if !self.monitoring {
            return Err(Error::MonitoringUnavailable {
                path: self.interpreter.clone(),
                found: self.version,
            });
        }

        Ok(())
    }
}

/// asked of every candidate interpreter, including ones far too old to debug —
/// it has to stay parseable by them so the refusal can name a real version
///
/// `-I` keeps site-packages and the environment out of the answer
const PROBE: &str = r#"
import json, os, sys, sysconfig
sys.stdout.write(json.dumps({
    "version": list(sys.version_info[:3]),
    "implementation": sys.implementation.name,
    "executable": sys.executable,
    "free_threaded": bool(sysconfig.get_config_var("Py_GIL_DISABLED")),
    "debug_build": hasattr(sys, "gettotalrefcount"),
    "ext_suffix": sysconfig.get_config_var("EXT_SUFFIX"),
    "monitoring": hasattr(sys, "monitoring"),
    "remote_exec": hasattr(sys, "remote_exec"),
    "remote_debug_disabled_by_env": "PYTHON_DISABLE_REMOTE_DEBUG" in os.environ,
}))
"#;

#[derive(serde::Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "this mirrors the probe's json one field for one field. collapsing \
              the flags into enums here would put the interpretation on the \
              parsing side, where a mistake is invisible"
)]
struct ProbeReport {
    version: [u8; 3],
    implementation: String,
    executable: PathBuf,
    free_threaded: bool,
    debug_build: bool,
    ext_suffix: Option<String>,
    monitoring: bool,
    remote_exec: bool,
    remote_debug_disabled_by_env: bool,
}

impl ProbeReport {
    fn into_capabilities(self, interpreter: PathBuf) -> Capabilities {
        let [major, minor, micro] = self.version;
        Capabilities {
            interpreter,
            executable: self.executable,
            version: PythonVersion::new(major, minor, micro),
            implementation: if self.implementation == "cpython" {
                Implementation::CPython
            } else {
                Implementation::Other(self.implementation.into_boxed_str())
            },
            free_threaded: self.free_threaded,
            debug_build: self.debug_build,
            ext_suffix: self.ext_suffix,
            monitoring: self.monitoring,
            remote_debug: match (self.remote_exec, self.remote_debug_disabled_by_env) {
                (false, _) => RemoteDebug::MissingApi,
                (true, true) => RemoteDebug::DisabledByEnvironment,
                (true, false) => RemoteDebug::Available,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> ProbeReport {
        ProbeReport {
            version: [3, 14, 0],
            implementation: "cpython".to_string(),
            executable: PathBuf::from("/usr/bin/python3.14"),
            free_threaded: false,
            debug_build: false,
            ext_suffix: Some(".cpython-314-darwin.so".to_string()),
            monitoring: true,
            remote_exec: true,
            remote_debug_disabled_by_env: false,
        }
    }

    fn capabilities(report: ProbeReport) -> Capabilities {
        report.into_capabilities(PathBuf::from("python3"))
    }

    #[test]
    fn versions_order_by_component() {
        assert!(PythonVersion::new(3, 9, 20) < PythonVersion::new(3, 13, 0));
        assert!(PythonVersion::new(3, 13, 0) < PythonVersion::new(3, 13, 1));
        assert!(PythonVersion::new(3, 13, 99) < PythonVersion::new(3, 14, 0));
        assert!(MINIMUM_SUPPORTED < MINIMUM_ATTACH);
    }

    #[test]
    fn a_current_cpython_is_debuggable() {
        capabilities(report())
            .require_debuggable()
            .expect("cpython 3.14 with sys.monitoring is the supported configuration");
    }

    #[test]
    fn an_old_cpython_is_refused_by_version() {
        let capabilities = capabilities(ProbeReport {
            version: [3, 12, 8],
            monitoring: true,
            ..report()
        });

        let error = capabilities
            .require_debuggable()
            .expect_err("3.12 is below the minimum");
        let Error::UnsupportedVersion { found, minimum, .. } = error else {
            panic!("expected a version refusal, got {error:?}");
        };
        assert_eq!(found, PythonVersion::new(3, 12, 8));
        assert_eq!(minimum, MINIMUM_SUPPORTED);
    }

    #[test]
    fn implementation_is_reported_before_version() {
        let capabilities = capabilities(ProbeReport {
            version: [3, 9, 18],
            implementation: "pypy".to_string(),
            monitoring: false,
            ..report()
        });

        let error = capabilities
            .require_debuggable()
            .expect_err("pypy is not supported");
        assert!(matches!(error, Error::UnsupportedImplementation { .. }));
    }

    #[test]
    fn a_supported_version_without_monitoring_is_refused() {
        let capabilities = capabilities(ProbeReport {
            monitoring: false,
            ..report()
        });

        let error = capabilities
            .require_debuggable()
            .expect_err("a stripped build cannot be debugged");
        assert!(matches!(error, Error::MonitoringUnavailable { .. }));
    }

    #[test]
    fn remote_debug_distinguishes_missing_from_disabled() {
        assert_eq!(
            capabilities(ProbeReport {
                remote_exec: false,
                ..report()
            })
            .remote_debug,
            RemoteDebug::MissingApi
        );
        assert_eq!(
            capabilities(ProbeReport {
                remote_debug_disabled_by_env: true,
                ..report()
            })
            .remote_debug,
            RemoteDebug::DisabledByEnvironment
        );
        assert_eq!(capabilities(report()).remote_debug, RemoteDebug::Available);
    }

    /// the free-threaded build is the half a version alone cannot say, and it
    /// is the half that decides between two artifacts on disk
    #[test]
    fn a_tag_is_the_release_and_the_build_configuration() {
        assert_eq!(capabilities(report()).tag().to_string(), "3.14");
        assert_eq!(
            capabilities(ProbeReport {
                free_threaded: true,
                ..report()
            })
            .tag()
            .to_string(),
            "3.14t"
        );
        assert_ne!(
            capabilities(report()).tag(),
            capabilities(ProbeReport {
                free_threaded: true,
                ..report()
            })
            .tag(),
            "a free-threaded interpreter is a different abi, not a variant of \
             the same one"
        );
    }

    #[test]
    fn a_tag_reads_back_from_exactly_the_spelling_it_writes() {
        for spelled in ["3.13", "3.14", "3.14t", "3.15", "4.0"] {
            let tag =
                InterpreterTag::parse(spelled).unwrap_or_else(|| panic!("`{spelled}` is a tag"));
            assert_eq!(tag.to_string(), spelled);
        }
    }

    /// a directory whose name is nearly a tag is not one. reading it loosely
    /// would resolve one release's agent to another, which is the load that
    /// imports and then reads the wrong offsets
    #[test]
    fn text_that_is_not_a_tag_is_not_read_as_one() {
        for spelled in [
            "",
            "3",
            "3.",
            ".14",
            "3.014",
            "03.14",
            "3.14.0",
            "3.14T",
            "3.14t ",
            "python3.14",
            "t",
            "3.-1",
            "3.256",
        ] {
            assert_eq!(
                InterpreterTag::parse(spelled),
                None,
                "`{spelled}` was read as a tag"
            );
        }
    }

    // the probe has to stay parseable by interpreters far older than the
    // minimum, so `bpd doctor` can name the version it is refusing rather than
    // dying on a syntax error. `python3` on PATH is whatever the machine has,
    // which is the point — this test is a failure if the probe only works on a
    // supported interpreter
    #[test]
    fn the_probe_answers_on_whatever_python3_is_on_path() {
        let capabilities = Capabilities::probe(Path::new("python3"))
            .expect("`python3` on PATH answered the capability probe");

        assert_eq!(capabilities.version.major, 3);
        assert!(capabilities.executable.is_absolute());
    }
}
