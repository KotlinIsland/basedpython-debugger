//! the agent, imported by a real interpreter
//!
//! nothing here links the agent — it is a `cdylib`, and the only thing that can
//! load it is cpython. so every assertion drives a real interpreter with the
//! built artifact staged on `PYTHONPATH`, which is also how it will reach a
//! debuggee

use bpd_test::agent::matching_interpreter;
use bpd_test::debuggee::Run;

fn run(code: &str) -> Run {
    bpd_test::agent::run(matching_interpreter(), code)
}

fn succeeds(code: &str) -> String {
    let run = run(code);
    assert!(
        run.success,
        "the snippet failed:\n{code}\nstdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );
    run.stdout.trim().to_string()
}

#[test]
fn the_agent_is_built_for_the_interpreter_that_imports_it() {
    let built_for = succeeds("import bpd_agent; print(bpd_agent.built_for())");
    // the `t` matters: a free-threaded build reports the same `version_info`
    // as the gil build of the same release and is a different abi, so a stamp
    // of the version alone names two interpreters
    let running = bpd_test::eval(matching_interpreter(), THE_EXPENSIVE_WAY);

    assert_eq!(built_for, running);
}

/// what a separate process says the running interpreter is, the expensive way
///
/// `sysconfig.get_config_var("Py_GIL_DISABLED")` is the answer everything else
/// uses and is exactly what the agent must **not** ask for inside a debuggee —
/// importing `sysconfig` and calling it pulls twenty-nine modules into
/// `sys.modules` that a bare run does not have. so it is the right ground truth
/// here: a different route to the same fact
const THE_EXPENSIVE_WAY: &str = "import sys, sysconfig\n\
     suffix = 't' if sysconfig.get_config_var('Py_GIL_DISABLED') else ''\n\
     print('%d.%d%s' % (sys.version_info[0], sys.version_info[1], suffix))";

#[test]
fn the_interpreter_the_agent_is_running_on_is_the_one_sysconfig_reports() {
    // the agent reads free-threadedness off the extension suffix, because
    // `sysconfig` costs a fingerprint. that is only sound if it agrees with
    // `sysconfig`, and the two have to be asked separately for the agreement to
    // mean anything
    let running = succeeds("import bpd_agent; print(bpd_agent.running_on())");
    let expensive = bpd_test::eval(matching_interpreter(), THE_EXPENSIVE_WAY);

    assert_eq!(running, expensive);
}

#[test]
fn an_interpreter_it_was_not_built_for_never_proceeds_silently() {
    // the agent is not abi3. loading it into a different release either fails
    // outright, or — as cpython 3.13 does with a 3.14 build — imports happily
    // and then runs against a layout it was not compiled for, which is far
    // worse. the property that matters is that neither of those ends in
    // "carried on"
    let built_for = succeeds("import bpd_agent; print(bpd_agent.built_for())");

    for interpreter in bpd_test::discovered().all() {
        let running = format!(
            "{}.{}{}",
            interpreter.version.major,
            interpreter.version.minor,
            if interpreter.free_threaded { "t" } else { "" }
        );
        if running == built_for {
            continue;
        }

        let run = bpd_test::agent::run(
            interpreter,
            "import bpd_agent; bpd_agent.verify_interpreter()",
        );
        assert!(
            !run.success,
            "python {running} imported an agent built for {built_for} and \
             carried on\nstdout:\n{}\nstderr:\n{}",
            run.stdout, run.stderr
        );
    }
}

#[test]
fn the_agent_claims_the_debugger_tool_id() {
    let claimed = succeeds(
        "import bpd_agent, sys\n\
         bpd_agent.claim()\n\
         print(sys.monitoring.get_tool(bpd_agent.debugger_tool_id()))",
    );

    assert_eq!(claimed, "bpd");
}

#[test]
fn the_tool_id_is_the_one_reserved_for_debuggers() {
    // claiming a different id would hand a client that asked for a debugger
    // another tool's event semantics
    let reserved = succeeds(
        "import bpd_agent, sys; \
         print(bpd_agent.debugger_tool_id() == sys.monitoring.DEBUGGER_ID)",
    );

    assert_eq!(reserved, "True");
}

#[test]
fn a_tool_id_already_in_use_is_refused_by_the_name_of_its_holder() {
    let run = run("import bpd_agent, sys\n\
         sys.monitoring.use_tool_id(sys.monitoring.DEBUGGER_ID, 'some other debugger')\n\
         bpd_agent.claim()\n");

    assert!(!run.success, "claiming a held tool id must fail");
    assert!(
        run.stderr.contains("some other debugger"),
        "the refusal must name the holder, stderr was:\n{}",
        run.stderr
    );
}

#[test]
fn releasing_the_tool_id_makes_it_claimable_again() {
    let sequence = succeeds(
        "import bpd_agent, sys\n\
         bpd_agent.claim()\n\
         bpd_agent.release()\n\
         print(bpd_agent.holder())\n\
         bpd_agent.claim()\n\
         print(bpd_agent.holder())\n",
    );

    assert_eq!(sequence.lines().collect::<Vec<_>>(), vec!["None", "bpd"]);
}

#[test]
fn the_agent_does_not_re_enable_the_gil_on_a_free_threaded_build() {
    // an extension that does not declare free-threading support makes cpython
    // turn the gil back on at import. every free-threading test would then pass
    // for the wrong reason, and the agent's own locking would never be exercised
    let interpreter = matching_interpreter();
    let enabled = succeeds("import bpd_agent, sys; print(sys._is_gil_enabled())");

    // no skip on a gil build — the expected answer is simply the other one
    // there, and a test that returns early is a test that reports success while
    // proving nothing
    let expected = if interpreter.free_threaded {
        "False"
    } else {
        "True"
    };
    assert_eq!(
        enabled,
        expected,
        "the gil is {enabled} after importing the agent into a {} build",
        if interpreter.free_threaded {
            "free-threaded"
        } else {
            "gil"
        }
    );
}

#[test]
fn importing_the_agent_warns_about_nothing() {
    // `-W error` turns cpython's "this module made me re-enable the gil"
    // RuntimeWarning into a failure. it is the same property as the test above,
    // caught at the moment of import and on every build configuration
    let run = bpd_test::agent::run(
        matching_interpreter(),
        "import warnings; warnings.simplefilter('error'); import bpd_agent",
    );

    assert!(
        run.success,
        "importing the agent raised a warning:\nstderr:\n{}",
        run.stderr
    );
}
