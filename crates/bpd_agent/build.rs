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

    // recorded so the agent can refuse at import time when it is loaded by an
    // interpreter other than the one it was compiled for
    println!(
        "cargo::rustc-env=BPD_AGENT_PYTHON={}.{}{}",
        version.major,
        version.minor,
        if free_threaded { "t" } else { "" }
    );
    println!("cargo::rerun-if-env-changed=PYO3_PYTHON");
}
