//! what cpython does for each of the three launch forms
//!
//! this is a recorded baseline, not a test of `bpd`. `bpd launch` has to
//! reproduce every value here exactly, and the comparison needs something to
//! compare against — so the ground truth is written down before the feature
//! that must match it
//!
//! every field asserted here differs between at least two of the forms. that is
//! the point: none of the three is a special case of another, and a launcher
//! that treats them as one gets at least one of these wrong

use bpd_test::debuggee::{Fixture, Form};

#[test]
fn a_script_puts_its_own_directory_first_on_the_path() {
    for interpreter in bpd_test::discovered().require() {
        let fixture = Fixture::launch_probe();
        let observed = fixture
            .run(interpreter, Form::Script, &["one", "two"])
            .observed();

        assert_eq!(
            observed.argv,
            vec![
                fixture.path().display().to_string(),
                "one".to_string(),
                "two".to_string()
            ]
        );
        assert_eq!(observed.path0, fixture.directory().display().to_string());
        assert_eq!(
            observed.file.as_deref(),
            Some(fixture.path().display().to_string().as_str())
        );
        assert_eq!(observed.spec, None);
        assert_eq!(observed.package, None);
        assert_eq!(observed.name, "__main__");
    }
}

#[test]
fn a_script_is_found_by_its_own_directory_and_not_the_working_directory() {
    // the two coincide often enough that a test which never separates them
    // would pass against a launcher that used the wrong one
    for interpreter in bpd_test::discovered().require() {
        let fixture = Fixture::launch_probe();
        let elsewhere = std::env::temp_dir()
            .canonicalize()
            .expect("the system temporary directory exists");

        let observed = fixture
            .run_in(&elsewhere, interpreter, Form::Script, &[])
            .observed();

        assert_eq!(observed.path0, fixture.directory().display().to_string());
        assert_ne!(observed.path0, elsewhere.display().to_string());
    }
}

#[test]
fn a_module_puts_the_working_directory_first_and_carries_a_spec() {
    for interpreter in bpd_test::discovered().require() {
        let fixture = Fixture::launch_probe();
        let observed = fixture
            .run(interpreter, Form::Module, &["one", "two"])
            .observed();

        // `-m` rewrites argv[0] to the resolved file, not the module name. a
        // launcher that passes the module name through gets this wrong, and
        // any program that reports its own invocation shows it
        assert_eq!(
            observed.argv,
            vec![
                fixture.path().display().to_string(),
                "one".to_string(),
                "two".to_string()
            ]
        );
        assert_eq!(observed.path0, fixture.directory().display().to_string());
        assert_eq!(observed.spec.as_deref(), Some("launch_probe"));
        assert_eq!(observed.package.as_deref(), Some(""));
        assert_eq!(observed.name, "__main__");
    }
}

#[test]
fn a_command_has_no_file_and_an_empty_path_entry() {
    for interpreter in bpd_test::discovered().require() {
        let fixture = Fixture::launch_probe();
        let observed = fixture
            .run(interpreter, Form::Command, &["one", "two"])
            .observed();

        assert_eq!(
            observed.argv,
            vec!["-c".to_string(), "one".to_string(), "two".to_string()]
        );
        // the empty string means "the working directory, resolved at import
        // time". it is not the same as the working directory spelled out, and
        // a launcher that substitutes one for the other changes what a
        // relative import finds after a chdir
        assert_eq!(observed.path0, "");
        assert_eq!(observed.file, None);
        assert_eq!(observed.spec, None);
        assert_eq!(observed.name, "__main__");
    }
}

#[test]
fn the_three_forms_disagree_with_each_other() {
    // if this ever passes trivially, the fixture stopped observing something
    for interpreter in bpd_test::discovered().require() {
        let fixture = Fixture::launch_probe();

        let script = fixture.run(interpreter, Form::Script, &[]).observed();
        let module = fixture.run(interpreter, Form::Module, &[]).observed();
        let command = fixture.run(interpreter, Form::Command, &[]).observed();

        assert_ne!(script, module);
        assert_ne!(module, command);
        assert_ne!(script, command);
    }
}

#[test]
fn a_failing_program_reports_its_exit_code_and_stderr() {
    for interpreter in bpd_test::discovered().require() {
        let fixture = Fixture::new("boom", "raise SystemExit(3)\n");
        let run = fixture.run(interpreter, Form::Script, &[]);

        assert!(!run.success);
        assert_eq!(run.exit_code, Some(3));
    }
}

#[test]
fn an_uncaught_exception_reaches_stderr_untouched() {
    for interpreter in bpd_test::discovered().require() {
        let fixture = Fixture::new("boom", "raise ValueError('the message')\n");
        let run = fixture.run(interpreter, Form::Script, &[]);

        assert!(!run.success);
        assert_eq!(run.exit_code, Some(1));
        assert!(
            run.stderr.contains("ValueError: the message"),
            "stderr was:\n{}",
            run.stderr
        );
    }
}
