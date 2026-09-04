//! refuse at build time to build an agent that could never work
//!
//! the agent is not `abi3`: it reads `sys.monitoring` and interpreter internals
//! that change between releases, so a build is only loadable by the interpreter
//! it was compiled against. that interpreter is chosen by `PYO3_PYTHON`, or by
//! whatever `python3` is on PATH, and neither is guaranteed to be one `bpd`
//! supports
//!
//! building against an unsupported interpreter would produce an artifact that
//! cannot possibly do its job, and the failure would surface much later as a
//! missing attribute on `sys`. it is caught here instead

fn main() {
    let config = pyo3_build_config::get();
    let version = config.version();

    // the minimum in `bpd_core::python::MINIMUM_SUPPORTED`, repeated because a
    // build script cannot depend on a workspace crate. the test
    // `the_agent_build_minimum_matches_the_support_policy` keeps the two honest
    let (minimum_major, minimum_minor) = (3, 13);

    if (version.major, version.minor) < (minimum_major, minimum_minor) {
        println!(
            "cargo::error=bpd_agent is being built against python {}.{}, and it \
             needs {minimum_major}.{minimum_minor} or newer — PEP 669 does not \
             exist before then. set PYO3_PYTHON to a supported interpreter, for \
             example `PYO3_PYTHON=python3.14 cargo build`",
            version.major, version.minor
        );
        return;
    }

    // a free-threaded build is a **different abi**, not a variant of the same
    // one: different struct layouts, a different `EXT_SUFFIX`, different
    // reference counting. `sys.version_info` reports 3.14 for both, so a stamp
    // of the version alone cannot tell them apart and the agent would wave
    // through a load it must refuse. the `t` follows cpython's own suffix
    let free_threaded = config
        .build_flags()
        .0
        .contains(&pyo3_build_config::BuildFlag::Py_GIL_DISABLED);

    // recorded so `the_agent_build_minimum_matches_the_support_policy` can put
    // the number above beside `bpd_core::python::MINIMUM_SUPPORTED`. a build
    // script cannot depend on a workspace crate, so the two are written twice
    // and a test is the only thing that can keep them the same
    println!("cargo::rustc-env=BPD_AGENT_MINIMUM={minimum_major}.{minimum_minor}");

    // recorded so the agent can refuse at import time when it is loaded by an
    // interpreter other than the one it was compiled for
    println!(
        "cargo::rustc-env=BPD_AGENT_PYTHON={}.{}{}",
        version.major,
        version.minor,
        if free_threaded { "t" } else { "" }
    );
    println!("cargo::rerun-if-env-changed=PYO3_PYTHON");

    // **and on the interpreter itself**, so that upgrading the python at that
    // path rebuilds the agent that was compiled against the old one. the
    // variable holding the same text is not the same interpreter, and pyo3
    // emits only `rerun-if-env-changed` triggers
    //
    // what this catches is the interpreter's own file changing. what it cannot
    // catch is the **name** resolving somewhere else — a symlink repointed, or
    // `PYO3_PYTHON=python` meaning 3.13 in one ci job and 3.15 in the next —
    // because cargo compares the mtime of whatever the path leads to now, and
    // another interpreter that was installed last week is not newer. measured:
    // building against 3.13 through a symlink, repointing it at 3.14 and
    // building again leaves the artifact stamped `3.13`
    //
    // `PYO3_ENVIRONMENT_SIGNATURE` is what covers that, and it is the
    // interpreter's tag in `ci.yaml`. the agent refuses at import either way,
    // which is the design working — but a build that produces an artifact
    // saying one thing and being another has already wasted somebody's day
    if let Some(executable) = config.executable() {
        println!("cargo::rerun-if-changed={executable}");
    }

    declare_the_audit_hook(config);
}

/// write the declaration of `PySys_AddAuditHook` this build needs
///
/// cpython exports it and pyo3-ffi does not declare it, so the agent declares
/// it itself — and on windows that declaration has to say **which library** the
/// symbol comes from. a bare `extern "C"` block names no library, which unix
/// does not care about (the interpreter's symbols are already in the process)
/// and windows will not link at all: `unresolved external symbol
/// PySys_AddAuditHook`, which is what every windows job in ci said
///
/// the name is `python313`, `python314t`, and so on — one per interpreter, and
/// therefore not something that can be written down here. it is taken from the
/// same `pyo3_build_config` that pyo3-ffi takes its own from, for whichever
/// interpreter this agent is being built against, which is the property the
/// whole agent-per-tag design already has
fn declare_the_audit_hook(config: &pyo3_build_config::InterpreterConfig) {
    let out = std::path::PathBuf::from(
        std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR for a build script"),
    );

    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let attribute = if windows {
        let Some(dll) = config.lib_name() else {
            println!(
                "cargo::error=this interpreter's configuration carries no \
                 library name, and on windows the agent cannot declare \
                 PySys_AddAuditHook without one — a python installed without \
                 its development files is the usual cause"
            );
            return;
        };

        // `raw-dylib` imports straight from the dll by name and needs no import
        // library on disk, which is what pyo3-ffi does for its own symbols. on
        // x86 the imported names are undecorated, and everywhere else they are
        // not — the same split pyo3-ffi makes, for the same reason
        let undecorated = if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86") {
            ", import_name_type = \"undecorated\""
        } else {
            ""
        };
        format!("#[link(name = \"{dll}\", kind = \"raw-dylib\"{undecorated})]\n")
    } else {
        String::new()
    };

    let declaration = format!(
        "// generated by build.rs — see `declare_the_audit_hook`\n\
         #[expect(\n\
         \x20   unsafe_code,\n\
         \x20   reason = \"the only interface cpython offers for a native audit hook is \\\n\
         \x20             a C one\"\n\
         )]\n\
         {attribute}\
         unsafe extern \"C\" {{\n\
         \x20   fn PySys_AddAuditHook(hook: AuditHook, user: *mut c_void) -> c_int;\n\
         }}\n"
    );

    let at = out.join("audit_hook.rs");
    std::fs::write(&at, declaration)
        .unwrap_or_else(|error| panic!("could not write {}: {error}", at.display()));
}
