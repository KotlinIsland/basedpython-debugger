//! a debug script drives a real interpreter, and its transcript is true
//!
//! a transcript is a claim about fifty things instead of one, so nothing here
//! takes it at its word. the fixture writes a marker file from inside every
//! call, and where the program **really** got to is read off the filesystem
//! rather than off the record that says so — which is the only way to catch a
//! script that reports a location the program was not at
//!
//! the required shapes, each with a test: a conditional that takes each branch,
//! a loop that hits its bound and says so, a budget exhausted mid-script
//! returning a transcript labelled partial, a step that raises halting the
//! script, a `run_to` that runs out of clock and leaves nothing armed, and the
//! same script over the same run producing the same transcript

use std::ffi::OsString;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::Path;

use bpd_core::python::Capabilities;
use bpd_core::{
    Answered, Bound, Budget, Content, Did, Disarmed, Evaluated, Halted, Landed, Outcome, Predicate,
    Record, Running, Script, Step, StopReason, Transcript,
};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// a program that charges five amounts, three of them negative, and writes a
/// marker from inside each call
///
/// the markers are what a test reads to find out where the program really got
/// to. every one of them is written *after* the line a breakpoint sits on, so a
/// program held at the third negative amount has written the first four and not
/// the fifth
const CHARGES: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent


def note(name):
    (HERE / name).write_text("x")


def charge(amount):
    seen = amount
    note("charged_" + str(amount))
    return seen


def main():
    total = 0
    for amount in (5, -1, 7, -2, -3):
        total += charge(amount)
    note("main_finished")
    return total


main()
"#;

/// a program that goes round a loop for a long time before it can be stopped
///
/// what a `run_to` runs out of clock against. it does reach lines, so a pause
/// armed to take the script's own breakpoint back off lands
const SPINNING: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent
(HERE / "running").write_text("x")
total = 0
for step in range(4_000_000):
    total += step
(HERE / "finished").write_text("x")
never = 1
"#;

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

fn launch(fixture: &Fixture) -> Debuggee {
    match bpd_engine::launch(
        interpreter(),
        &bpd_engine::Program::Script(fixture.path()),
        &[] as &[OsString],
    ) {
        Ok(Launched::Stopped(debuggee)) => debuggee,
        Ok(Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

fn budget(steps: u32, wall_ms: u64, bytes: u32) -> Budget {
    Budget {
        steps: NonZeroU32::new(steps).expect("a test asks for at least one step"),
        wall_ms: NonZeroU64::new(wall_ms).expect("a test allows at least a millisecond"),
        bytes: NonZeroU32::new(bytes).expect("a test allows at least a byte"),
    }
}

/// a budget nothing in a passing test is expected to hit
fn generous() -> Budget {
    budget(200, 60_000, 1 << 20)
}

fn predicate(expression: &str) -> Predicate {
    Predicate {
        expression: expression.to_string(),
        frame: 0,
    }
}

/// whether the program has written a marker
fn wrote(fixture: &Fixture, name: &str) -> bool {
    fixture.directory().join(name).exists()
}

/// what a record says the step did, as a short name
fn did(record: &Record) -> &'static str {
    match record.did {
        Did::Stepped { .. } => "stepped",
        Did::Continued { .. } => "continued",
        Did::RanTo { .. } => "ran_to",
        Did::Evaluated { .. } => "evaluated",
        Did::Walked { .. } => "walked",
        Did::Logged { .. } => "logged",
        Did::Branched { .. } => "branched",
        Did::Tested { .. } => "tested",
        Did::Bounded { .. } => "bounded",
        Did::Finished { .. } => "finished",
        Did::Refused { .. } => "refused",
        _ => "something this test does not know about",
    }
}

/// every note a script's `log` steps recorded, in order
fn notes(transcript: &Transcript) -> Vec<&str> {
    transcript
        .records
        .iter()
        .filter_map(|record| match &record.did {
            Did::Logged { note } => Some(note.as_str()),
            _ => None,
        })
        .collect()
}

/// run to the line `seen = amount` in `charge`, for the `wanted`th negative
/// amount
fn to_the_negative_charge(file: &Path, wanted: u32) -> Step {
    Step::RunTo {
        file: file.to_path_buf(),
        line: line_of(CHARGES, "seen = amount"),
        condition: Some("amount < 0".to_string()),
        hits: Some(bpd_core::HitCondition::Exactly {
            count: NonZeroU32::new(wanted).expect("a test asks for at least the first"),
        }),
    }
}

#[test]
fn a_script_runs_to_the_third_negative_amount_and_the_program_really_is_there() {
    // the motivating investigation, end to end: *run to the third call with a
    // negative amount and show me the stack*. one call, and the answer says
    // where the program got to at every step
    let fixture = Fixture::new("charges", CHARGES);
    let mut debuggee = launch(&fixture);

    let transcript = debuggee
        .the_script(Script {
            steps: vec![
                to_the_negative_charge(&fixture.path(), 3),
                Step::Eval {
                    expression: "amount".to_string(),
                    frame: 0,
                    detail: bpd_core::Detail::default(),
                },
                Step::Stack { top: Some(3) },
            ],
            budget: generous(),
        })
        .expect("the script was run");

    assert_eq!(
        transcript.outcome,
        Outcome::Ran,
        "the script did not finish: {transcript:#?}"
    );

    // the program's own markers, not the transcript's word for it. the fourth
    // charge wrote its marker and the fifth is the one it is held before
    assert!(wrote(&fixture, "charged_-2"), "it never reached the fourth");
    assert!(
        !wrote(&fixture, "charged_-3"),
        "it ran past the line the script said it stopped on"
    );

    let Did::RanTo {
        armed_as,
        landed: Some(Landed::Stopped { to }),
        disarmed,
        ..
    } = &transcript.records[0].did
    else {
        panic!("the run_to did not land: {:#?}", transcript.records[0])
    };
    assert_eq!(
        to.place
            .as_ref()
            .expect("a breakpoint stop has a line")
            .line,
        line_of(CHARGES, "seen = amount")
    );
    assert_eq!(
        *disarmed,
        Disarmed::Removed,
        "a run_to takes its own breakpoint back off"
    );
    assert!(
        *armed_as > 0,
        "the script's own breakpoint has an id of its own, so a stop that names \
         it is unambiguous"
    );

    let Did::Evaluated {
        result: Evaluated::Value { value },
        ..
    } = &transcript.records[1].did
    else {
        panic!(
            "the eval did not produce a value: {:#?}",
            transcript.records[1]
        )
    };
    assert_eq!(
        value.content,
        Content::Int {
            text: "-3".to_string(),
            omitted: None,
        },
        "the third negative amount is -3"
    );

    let Did::Walked { frames, .. } = &transcript.records[2].did else {
        panic!("the stack was not walked: {:#?}", transcript.records[2])
    };
    assert_eq!(
        frames.iter().map(bpd_core::Frame::name).collect::<Vec<_>>(),
        vec!["charge", "main", "<module>"],
        "no frame of bpd's own is in the stack"
    );
}

#[test]
fn a_conditional_takes_each_branch_and_the_transcript_says_which() {
    let fixture = Fixture::new("branching", CHARGES);
    let mut debuggee = launch(&fixture);

    let branch = Step::If {
        predicate: predicate("amount < 0"),
        then: vec![Step::Log {
            note: "negative".to_string(),
        }],
        otherwise: vec![Step::Log {
            note: "positive".to_string(),
        }],
    };

    // run to the first negative amount, where the predicate is true
    let taken = debuggee
        .the_script(Script {
            steps: vec![to_the_negative_charge(&fixture.path(), 1), branch.clone()],
            budget: generous(),
        })
        .expect("the script was run");
    assert_eq!(notes(&taken), vec!["negative"], "{taken:#?}");
    assert_eq!(
        taken.records[1].step, "2",
        "a record says which step of the tree it came from"
    );
    assert_eq!(
        taken.records[2].step, "2.then.1",
        "and a step inside a branch says which branch it was in"
    );

    let Did::Branched {
        answered: Answered::Value { value },
        ..
    } = &taken.records[1].did
    else {
        panic!("the branch did not answer: {:#?}", taken.records[1])
    };
    assert!(*value, "`amount < 0` at the first negative charge");

    // and the other way: run on to a positive amount, where the same tree takes
    // `otherwise`
    let untaken = debuggee
        .the_script(Script {
            steps: vec![
                Step::RunTo {
                    file: fixture.path(),
                    line: line_of(CHARGES, "seen = amount"),
                    condition: Some("amount > 0".to_string()),
                    hits: None,
                },
                branch,
            ],
            budget: generous(),
        })
        .expect("the script was run");
    assert_eq!(notes(&untaken), vec!["positive"], "{untaken:#?}");
    assert_eq!(
        untaken.records[2].step, "2.otherwise.1",
        "the other branch names itself too"
    );
}

#[test]
fn a_loop_that_hits_its_bound_says_so_and_stops_the_script() {
    let fixture = Fixture::new("bounded", CHARGES);
    let mut debuggee = launch(&fixture);

    // `True` never goes false, so this is a loop that only its bound stops. two
    // passes of one step over, and then the record that says the bound bit
    let transcript = debuggee
        .the_script(Script {
            steps: vec![
                to_the_negative_charge(&fixture.path(), 1),
                Step::While {
                    predicate: predicate("1 == 1"),
                    limit: NonZeroU32::new(2).expect("2 is not zero"),
                    body: vec![Step::StepOver],
                },
                Step::Log {
                    note: "this never runs".to_string(),
                },
            ],
            budget: generous(),
        })
        .expect("the script was run");

    let Outcome::Halted {
        at,
        why: Halted::Bounded { limit },
    } = &transcript.outcome
    else {
        panic!("the loop's bound did not stop it: {transcript:#?}")
    };
    assert_eq!(at, "2");
    assert_eq!(limit.get(), 2);
    assert!(
        transcript.outcome.to_string().contains("larger `limit`"),
        "it has to say what to do about it: {}",
        transcript.outcome
    );

    assert_eq!(
        transcript.records.iter().map(did).collect::<Vec<_>>(),
        vec![
            "ran_to", "tested", "stepped", "tested", "stepped", "bounded"
        ],
        "one test per pass, and the record that says the bound bit"
    );
    assert!(
        notes(&transcript).is_empty(),
        "the step after the loop must not run: {transcript:#?}"
    );
}

#[test]
fn a_budget_exhausted_mid_script_returns_a_transcript_labelled_partial() {
    let fixture = Fixture::new("budgeted", CHARGES);
    let mut debuggee = launch(&fixture);

    let steps = vec![
        to_the_negative_charge(&fixture.path(), 1),
        Step::Log {
            note: "first".to_string(),
        },
        Step::Log {
            note: "second".to_string(),
        },
        Step::Log {
            note: "third".to_string(),
        },
    ];

    let transcript = debuggee
        .the_script(Script {
            steps,
            budget: budget(3, 60_000, 1 << 20),
        })
        .expect("the script was run");

    assert!(
        transcript.partial(),
        "a transcript that ran out of budget is partial: {transcript:#?}"
    );
    let Outcome::Exhausted {
        at,
        bound: Bound::Steps { limit },
        made,
    } = &transcript.outcome
    else {
        panic!("the step budget did not bite: {transcript:#?}")
    };
    assert_eq!(limit.get(), 3);
    assert_eq!(
        *made, 3,
        "three records were made and the fourth step did not run"
    );
    assert_eq!(at, "4", "the step it stopped at is named");
    assert_eq!(notes(&transcript), vec!["first", "second"]);

    // and the byte budget, which is the one that bites first in practice
    let cramped = debuggee
        .the_script(Script {
            steps: vec![
                Step::Eval {
                    expression: "amount".to_string(),
                    frame: 0,
                    detail: bpd_core::Detail::default(),
                },
                Step::Log {
                    note: "this never runs".to_string(),
                },
            ],
            budget: budget(200, 60_000, 32),
        })
        .expect("the script was run");
    assert!(cramped.partial(), "{cramped:#?}");
    let Outcome::Exhausted {
        bound: Bound::Bytes { limit, recorded },
        ..
    } = &cramped.outcome
    else {
        panic!("the byte budget did not bite: {cramped:#?}")
    };
    assert_eq!(limit.get(), 32);
    assert!(
        *recorded > 32,
        "one record can carry the total past the bound, and what was really \
         recorded is what is reported: {cramped:#?}"
    );
    assert_eq!(
        cramped.bytes, *recorded,
        "the transcript says how many bytes it made either way"
    );
}

#[test]
fn a_step_that_raises_halts_the_script_and_the_program_does_not_move() {
    let fixture = Fixture::new("raising", CHARGES);
    let mut debuggee = launch(&fixture);

    let transcript = debuggee
        .the_script(Script {
            steps: vec![
                to_the_negative_charge(&fixture.path(), 1),
                Step::Eval {
                    expression: "amount // 0".to_string(),
                    frame: 0,
                    detail: bpd_core::Detail::default(),
                },
                Step::Continue,
                Step::Log {
                    note: "this never runs".to_string(),
                },
            ],
            budget: generous(),
        })
        .expect("the script was run");

    let Outcome::Halted {
        at,
        why: Halted::Raised { expression, error },
    } = &transcript.outcome
    else {
        panic!("the raise did not halt it: {transcript:#?}")
    };
    assert_eq!(at, "2");
    assert_eq!(expression, "amount // 0");
    assert_eq!(error.kind, "ZeroDivisionError");

    // the record carries the exception rather than a value bpd made up
    let Did::Evaluated {
        result: Evaluated::Raised { .. },
        ..
    } = &transcript.records[1].did
    else {
        panic!(
            "the record did not carry the exception: {:#?}",
            transcript.records[1]
        )
    };

    // and the `continue` after it did not run, which the program itself says:
    // it is still held before the second charge's marker
    assert!(!wrote(&fixture, "charged_-1"), "the program ran on");
    assert_eq!(transcript.records.len(), 2, "{transcript:#?}");
    assert_eq!(
        debuggee.held().len(),
        1,
        "the thread the script was driving is still held"
    );
}

#[test]
fn a_run_to_that_runs_out_of_clock_leaves_nothing_armed() {
    let fixture = Fixture::new("spinning", SPINNING);
    let mut debuggee = launch(&fixture);

    // a line the program reaches only after millions of loop passes, and a
    // clock far too short to get there. the script's own breakpoint cannot be
    // taken off a program that is running, so the engine holds a thread to do it
    let transcript = debuggee
        .the_script(Script {
            steps: vec![
                Step::RunTo {
                    file: fixture.path(),
                    line: line_of(SPINNING, "never = 1"),
                    condition: None,
                    hits: None,
                },
                Step::Log {
                    note: "this never runs".to_string(),
                },
            ],
            budget: budget(200, 50, 1 << 20),
        })
        .expect("the script was run");

    assert!(transcript.partial(), "{transcript:#?}");
    let Outcome::Exhausted {
        bound: Bound::Wall { .. },
        ..
    } = &transcript.outcome
    else {
        panic!("the wall clock budget did not bite: {transcript:#?}")
    };

    let Did::RanTo {
        landed: Some(Landed::StillRunning),
        disarmed,
        ..
    } = &transcript.records[0].did
    else {
        panic!(
            "the run_to should still have been running: {:#?}",
            transcript.records[0]
        )
    };
    let Disarmed::PausedToRemove { at } = disarmed else {
        panic!("the script's own breakpoint was left armed: {disarmed}")
    };
    assert!(
        matches!(at.why, StopReason::Paused { .. }),
        "the thread it took the breakpoint off on is one it paused: {at:?}"
    );

    // the proof that nothing is armed: let the program go, and it runs to its
    // end without stopping at the line the script was running to
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the program was resumed")
    {
        Running::Exited { status, .. } => assert!(status.success(), "it exited with {status}"),
        other => panic!("the script left something armed and the program stopped at it: {other:?}"),
    }
    assert!(
        wrote(&fixture, "finished"),
        "the program ran to its end, which it cannot have done through a \
         breakpoint nobody took off"
    );
}

#[test]
fn the_same_script_over_the_same_run_produces_the_same_transcript() {
    // an agent re-runs a script to confirm a reading rather than trusting its
    // memory of it, so two runs of the same script over the same program have
    // to be equal — including the stop numbers, which are minted per session
    // from the same place
    let fixture = Fixture::new("repeatable", CHARGES);

    let script = || Script {
        steps: vec![
            to_the_negative_charge(&fixture.path(), 2),
            Step::If {
                predicate: predicate("amount < 0"),
                then: vec![
                    Step::Eval {
                        expression: "amount".to_string(),
                        frame: 0,
                        detail: bpd_core::Detail::default(),
                    },
                    Step::StepOver,
                    Step::Stack { top: Some(2) },
                ],
                otherwise: vec![Step::Log {
                    note: "unreachable".to_string(),
                }],
            },
        ],
        budget: generous(),
    };

    let first = launch(&fixture)
        .the_script(script())
        .expect("the script was run");
    // a second launch of the same program from the same place. nothing in a
    // transcript may be a wall clock reading, or these would never be equal
    let second = launch(&fixture)
        .the_script(script())
        .expect("the script was run again");

    assert_eq!(
        without_the_thread(&first),
        without_the_thread(&second),
        "two runs of the same script over the same program disagree"
    );
    assert_eq!(first.outcome, Outcome::Ran, "{first:#?}");

    // and within one run there is nothing to normalise: every record of a
    // script names the one thread it drives
    let threads: Vec<u64> = first
        .records
        .iter()
        .map(|record| record.at.thread)
        .collect();
    assert!(
        threads.windows(2).all(|pair| pair[0] == pair[1]),
        "a script drives one thread and its records name {threads:?}"
    );
}

/// a transcript with the interpreter's thread identity taken out of it
///
/// two launches are two **processes**, and what `threading.get_ident` reports
/// is the operating system's — it repeats between processes on some platforms
/// and is not something bpd may claim. everything else has to be equal, which is
/// what determinism means here: nothing in a transcript is a measurement, so the
/// same script over the same program state produces the same record of it
fn without_the_thread(transcript: &Transcript) -> String {
    let written =
        serde_json::to_string(transcript).expect("a transcript's serde is derived and cannot fail");
    let mut normalised = String::with_capacity(written.len());
    let mut rest = written.as_str();
    while let Some(at) = rest.find("\"thread\":") {
        let (before, after) = rest.split_at(at + "\"thread\":".len());
        normalised.push_str(before);
        normalised.push_str("<the operating system's>");
        rest = after.trim_start_matches(|character: char| character.is_ascii_digit());
    }
    normalised.push_str(rest);
    normalised
}

#[test]
fn a_predicate_that_is_not_a_bool_halts_rather_than_being_guessed_at() {
    let fixture = Fixture::new("truthiness", CHARGES);
    let mut debuggee = launch(&fixture);

    let transcript = debuggee
        .the_script(Script {
            steps: vec![
                to_the_negative_charge(&fixture.path(), 1),
                Step::If {
                    // truthy in python, and not a `bool`. deciding a branch on
                    // it would mean running the program's own `__bool__`
                    predicate: predicate("[amount]"),
                    then: vec![Step::Log {
                        note: "taken".to_string(),
                    }],
                    otherwise: Vec::new(),
                },
            ],
            budget: generous(),
        })
        .expect("the script was run");

    let Outcome::Halted {
        why: Halted::NotABool { kind, .. },
        ..
    } = &transcript.outcome
    else {
        panic!("a non-bool predicate was answered anyway: {transcript:#?}")
    };
    assert_eq!(kind, "list");
    assert!(
        transcript.outcome.to_string().contains("len(x) > 0"),
        "it has to say what to write instead: {}",
        transcript.outcome
    );
    assert!(notes(&transcript).is_empty(), "the branch was taken anyway");
}

#[test]
fn a_run_to_that_cannot_bind_says_so_rather_than_running_to_nothing() {
    let fixture = Fixture::new("unbindable", CHARGES);
    let mut debuggee = launch(&fixture);

    let transcript = debuggee
        .the_script(Script {
            steps: vec![Step::RunTo {
                file: fixture.directory().join("no_such_file.py"),
                line: 1,
                condition: None,
                hits: None,
            }],
            budget: generous(),
        })
        .expect("the script was run");

    let Outcome::Halted {
        why: Halted::Unbound { reason },
        ..
    } = &transcript.outcome
    else {
        panic!("an unbindable run_to ran anyway: {transcript:#?}")
    };
    assert!(
        reason.to_string().contains("no_such_file.py"),
        "the refusal names the file: {reason}"
    );

    // and the program has not moved: running to a breakpoint that binds nothing
    // would otherwise have spent the whole clock arriving nowhere
    assert!(!wrote(&fixture, "charged_5"));
}

#[test]
fn a_step_the_session_refuses_still_gets_a_record_and_stops_the_script() {
    let fixture = Fixture::new("refusing", CHARGES);
    let mut debuggee = launch(&fixture);

    let transcript = debuggee
        .the_script(Script {
            steps: vec![
                to_the_negative_charge(&fixture.path(), 1),
                Step::Eval {
                    // the stack is three deep at that stop, so there is no
                    // frame 99 and answering about one would be answering about
                    // a frame that does not exist
                    expression: "amount".to_string(),
                    frame: 99,
                    detail: bpd_core::Detail::default(),
                },
                Step::Log {
                    note: "this never runs".to_string(),
                },
            ],
            budget: generous(),
        })
        .expect("the script was run");

    let Outcome::Halted {
        why: Halted::Refused { reason },
        at,
    } = &transcript.outcome
    else {
        panic!("a refused step did not stop the script: {transcript:#?}")
    };
    assert_eq!(at, "2");
    assert!(reason.contains("frame 99"), "said {reason}");

    // a step missing from a transcript is a step a reader would assume ran, so
    // even one that did nothing has a record — named by the step it was
    let Did::Refused { doing, .. } = &transcript.records[1].did else {
        panic!("the refused step has no record: {transcript:#?}")
    };
    assert_eq!(doing, "eval");
    assert!(notes(&transcript).is_empty(), "{transcript:#?}");
}

#[test]
fn a_finish_step_ends_the_script_and_says_why() {
    let fixture = Fixture::new("finishing", CHARGES);
    let mut debuggee = launch(&fixture);

    let transcript = debuggee
        .the_script(Script {
            steps: vec![
                Step::If {
                    predicate: predicate("1 == 1"),
                    then: vec![Step::Finish {
                        because: "nothing to look at here".to_string(),
                    }],
                    otherwise: Vec::new(),
                },
                Step::Log {
                    note: "this never runs".to_string(),
                },
            ],
            budget: generous(),
        })
        .expect("the script was run");

    assert_eq!(
        transcript.outcome,
        Outcome::Finished {
            at: "1.then.1".to_string(),
            because: "nothing to look at here".to_string(),
        },
        "{transcript:#?}"
    );
    assert!(!transcript.partial(), "a script that ended itself is whole");
    assert!(notes(&transcript).is_empty(), "{transcript:#?}");
    // the bound is computed before the script runs, and an early end is under it
    assert!(
        transcript.at_most >= u64::try_from(transcript.records.len()).expect("a small number"),
        "a script ran more steps than it could: {transcript:#?}"
    );
}

#[test]
fn a_script_that_names_a_stop_nothing_holds_is_refused_before_it_runs() {
    let fixture = Fixture::new("nostop", CHARGES);
    let mut debuggee = launch(&fixture);

    let refused = debuggee
        .run_script(
            9_001,
            Script {
                steps: vec![Step::StepOver],
                budget: generous(),
            },
        )
        .expect_err("stop 9001 is not held");
    let said = refused.to_string();
    assert!(said.contains("9001"), "said {said}");

    // nothing ran, which the program itself says
    assert!(!wrote(&fixture, "charged_5"));
}
