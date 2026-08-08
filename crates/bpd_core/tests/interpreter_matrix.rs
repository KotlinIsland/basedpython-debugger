//! the capability model, checked against the interpreters actually installed
//!
//! every assertion here obtains its ground truth by a **different route** than
//! the capability probe — `python -V` rather than the json report,
//! `sys._is_gil_enabled()` rather than a config var. a probe that agrees with
//! itself proves nothing
//!
//! nothing in this file skips. when there is no supported interpreter,
//! `require` fails and says how to install one

use bpd_core::python::{
    Capabilities, Implementation, MINIMUM_ATTACH, MINIMUM_SUPPORTED, PythonVersion, RemoteDebug,
};

/// `sys.monitoring` arrived in 3.12, one release before `bpd`'s minimum. the
/// probe reports its presence rather than inferring it from the version, so
/// this is a real check rather than a restatement
const MONITORING_FROM: PythonVersion = PythonVersion::new(3, 12, 0);

#[test]
fn the_reported_version_matches_what_the_interpreter_says_about_itself() {
    for capabilities in bpd_test::discovered().require() {
        // `python -V` is a different surface than the json probe: no import of
        // `sysconfig`, no json, just the interpreter naming itself
        let banner = bpd_test::eval(capabilities, "import sys; print(sys.version)");
        let reported = capabilities.version.to_string();

        assert!(
            banner.starts_with(&reported),
            "`{}` reports version {reported}, but its own banner starts {banner:?}",
            capabilities.interpreter.display()
        );
    }
}

#[test]
fn the_executable_is_the_real_binary_behind_the_name() {
    for capabilities in bpd_test::discovered().all() {
        assert!(
            capabilities.executable.is_absolute(),
            "`{}` reported a relative sys.executable: {}",
            capabilities.interpreter.display(),
            capabilities.executable.display()
        );
        assert!(
            capabilities.executable.exists(),
            "`{}` reported a sys.executable that does not exist: {}",
            capabilities.interpreter.display(),
            capabilities.executable.display()
        );
    }
}

#[test]
fn monitoring_is_present_exactly_from_the_release_that_introduced_it() {
    for capabilities in bpd_test::discovered().all() {
        assert_eq!(
            capabilities.monitoring,
            capabilities.version >= MONITORING_FROM,
            "`{}` is python {} and reports `sys.monitoring` present = {}",
            capabilities.interpreter.display(),
            capabilities.version,
            capabilities.monitoring
        );
    }
}

#[test]
fn remote_debug_is_available_exactly_from_the_release_that_introduced_it() {
    // the probe reports what a debuggee launched from *this* environment would
    // inherit, so a suite run with the kill switch set asserts the other branch
    let disabled_here = std::env::var_os("PYTHON_DISABLE_REMOTE_DEBUG").is_some();

    for capabilities in bpd_test::discovered().all() {
        let expected = match (capabilities.version >= MINIMUM_ATTACH, disabled_here) {
            (false, _) => RemoteDebug::MissingApi,
            (true, true) => RemoteDebug::DisabledByEnvironment,
            (true, false) => RemoteDebug::Available,
        };

        assert_eq!(
            capabilities.remote_debug,
            expected,
            "`{}` is python {}",
            capabilities.interpreter.display(),
            capabilities.version
        );
    }
}

#[test]
fn a_gil_build_is_never_reported_as_free_threaded() {
    for capabilities in bpd_test::discovered().require() {
        // `sys._is_gil_enabled` is a different question than the build flag —
        // a free-threaded build re-enables the gil when an unprepared extension
        // is imported. so it only settles one direction, and that is the
        // direction that would matter: claiming free-threading falsely
        let gil_enabled = bpd_test::eval(capabilities, "import sys; print(sys._is_gil_enabled())");

        if gil_enabled == "False" {
            assert!(
                capabilities.free_threaded,
                "`{}` is running without the gil but was not reported as a \
                 free-threaded build",
                capabilities.interpreter.display()
            );
        }
    }
}

#[test]
fn an_extension_suffix_is_reported_for_every_supported_interpreter() {
    for capabilities in bpd_test::discovered().require() {
        // this is the key that selects an agent build. without it there is no
        // way to know which extension the interpreter could load
        let Some(suffix) = capabilities.ext_suffix.as_deref() else {
            panic!(
                "`{}` reported no EXT_SUFFIX, so no agent build could be \
                 selected for it",
                capabilities.executable.display()
            );
        };

        let ground_truth = bpd_test::eval(
            capabilities,
            "import sysconfig; print(sysconfig.get_config_var('EXT_SUFFIX'))",
        );
        assert_eq!(suffix, ground_truth);
    }
}

#[test]
fn the_refusal_agrees_with_the_reported_version_and_implementation() {
    for capabilities in bpd_test::discovered().all() {
        let debuggable = capabilities.require_debuggable().is_ok();
        let should_be = capabilities.implementation == Implementation::CPython
            && capabilities.version >= MINIMUM_SUPPORTED
            && capabilities.monitoring;

        assert_eq!(
            debuggable,
            should_be,
            "`{}` is {} {} and require_debuggable said {debuggable}",
            capabilities.interpreter.display(),
            capabilities.implementation,
            capabilities.version
        );
    }
}

#[test]
fn probing_something_that_is_not_an_interpreter_fails_rather_than_guessing() {
    let error = Capabilities::probe(std::path::Path::new("/nonexistent/python"))
        .expect_err("there is no interpreter at that path");

    let message = error.to_string();
    assert!(
        message.contains("/nonexistent/python"),
        "the refusal must name the thing it could not run, got {message:?}"
    );
}
