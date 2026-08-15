//! what is provable about a frame's names, against a real interpreter
//!
//! the two claims here cannot be checked against a mocked frame, because both
//! of them are claims about cpython:
//!
//! - **the stability judgement is read off the type.** whether it is right
//!   depends on `Py_TPFLAGS_HEAPTYPE` and `tp_dictoffset` meaning what this
//!   believes they mean, which is a question about the interpreter
//! - **nothing runs the program.** the fixture is built so that a debugger that
//!   reached for `__bool__`, `__len__` or a property would *say so in the
//!   program's own output*. asserting on the answer alone would prove nothing —
//!   a wrong answer and a right one look the same until the side effect lands

use std::path::Path;

use bpd_core::python::Capabilities;
use bpd_core::{
    Binding, Class, Fact, Facts, FrameId, Limit, Mutation, Observed, Resolved, Running, Silence,
    SourceBreakpoint, Stability, StopReason,
};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// every shape the prover has to tell apart, in one frame
///
/// `Loud` is the trap: its `__len__`, `__bool__` and its `mode` property all
/// append to `TOUCHED`, and the program prints what is in it. so a run where the
/// debugger stayed out of the program's code is a run whose output is empty, and
/// that is an assertion rather than a belief
const PROVABLE: &str = r#"import enum
import pathlib

HERE = pathlib.Path(__file__).parent
TOUCHED = []


class Colour(enum.Enum):
    RED = 1
    BLUE = 2


class Plain:
    def __init__(self):
        self.limit = 7


class Loud:
    @property
    def mode(self):
        TOUCHED.append("mode")
        return "fast"

    def __len__(self):
        TOUCHED.append("len")
        return 3

    def __bool__(self):
        TOUCHED.append("bool")
        return True


class Counted(list):
    pass


def inspect_me():
    small = 5
    words = "hello"
    nothing = None
    truth = True
    growing = [1, 2, 3]
    frozen = (1, 2, 3)
    empty = {}
    colour = Colour.RED
    plain = Plain()
    loud = Loud()
    subclassed = Counted([1, 2])
    marker = 1
    return marker


inspect_me()
(HERE / "touched").write_text(",".join(TOUCHED))
"#;

fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

fn launch(fixture: &Fixture) -> Debuggee {
    match bpd_engine::launch(
        interpreter(),
        &bpd_engine::Program::Script(fixture.path()),
        &[],
    ) {
        Ok(Launched::Stopped(debuggee)) => debuggee,
        Ok(Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

fn bound(resolved: &[Resolved]) {
    for resolution in resolved {
        match &resolution.binding {
            Binding::Bound { .. }
            | Binding::BoundInTemplate { .. }
            | Binding::BoundInSource { .. } => {}
            Binding::Unbound { reason } => {
                panic!("breakpoint {} did not bind: {reason}", resolution.id)
            }
        }
    }
}

fn stop_at(debuggee: &mut Debuggee, file: &Path, line: u32) {
    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, file, line)])
        .expect("the breakpoint request was answered");
    bound(&resolved);

    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => {
            assert!(
                matches!(stop.reason, StopReason::Breakpoint { .. }),
                "expected a breakpoint stop, got {:?}",
                stop.reason
            );
        }
        other => panic!("the debuggee did not stop: {other:?}"),
    }
}

/// clear everything and let the program run to its own end
fn to_exit(debuggee: &mut Debuggee) {
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Exited { status, .. } => assert!(status.success(), "it exited with {status}"),
        other => panic!("nothing is set, and the program did not run to the end: {other:?}"),
    }
}

/// the stop the breakpoint makes, whose frame zero is `inspect_me`
const fn top() -> FrameId {
    FrameId { stop: 2, depth: 0 }
}

/// launch the fixture and stop where every name is bound
fn stopped(fixture: &Fixture) -> Debuggee {
    let mut debuggee = launch(fixture);
    stop_at(
        &mut debuggee,
        &fixture.path(),
        line_of(PROVABLE, "marker = 1"),
    );
    debuggee
}

/// everything proved about a set of names
fn prove(debuggee: &mut Debuggee, names: &[&str]) -> Facts {
    debuggee
        .facts(top(), names, Limit::default())
        .expect("the fact request was answered")
}

/// the facts about one name
fn about<'a>(facts: &'a Facts, name: &str) -> Vec<&'a Fact> {
    let about: Vec<&Fact> = facts
        .proved
        .iter()
        .filter(|fact| fact.name == name)
        .collect();
    assert!(
        !about.is_empty(),
        "nothing was proved about `{name}`; the silences were {:?}",
        facts.silent
    );
    about
}

/// require that one observation was made about a name, and answer its stability
fn observed(facts: &Facts, name: &str, wanted: &Observed) -> Stability {
    about(facts, name)
        .into_iter()
        .find(|fact| &fact.observed == wanted)
        .unwrap_or_else(|| {
            panic!(
                "expected {wanted:?} about `{name}`, and what was proved was {:?}",
                about(facts, name)
                    .iter()
                    .map(|fact| &fact.observed)
                    .collect::<Vec<_>>()
            )
        })
        .stability
        .clone()
}

/// why nothing was proved about a name
fn silence<'a>(facts: &'a Facts, name: &str) -> &'a Silence {
    &facts
        .silent
        .iter()
        .find(|silent| silent.name == name)
        .unwrap_or_else(|| panic!("`{name}` was not reported silent"))
        .why
}

#[test]
fn a_value_type_is_proved_permanently_and_a_container_only_until_it_changes() {
    let fixture = Fixture::new("provable", PROVABLE);
    let mut debuggee = stopped(&fixture);
    let facts = prove(
        &mut debuggee,
        &[
            "small", "words", "nothing", "truth", "growing", "frozen", "empty",
        ],
    );

    // an `int` cannot change and neither can the reading of it
    assert_eq!(
        observed(
            &facts,
            "small",
            &Observed::IsInt {
                text: "5".to_string()
            }
        ),
        Stability::Permanent
    );
    assert_eq!(
        observed(
            &facts,
            "words",
            &Observed::IsStr {
                text: "hello".to_string()
            }
        ),
        Stability::Permanent
    );
    assert_eq!(
        observed(&facts, "nothing", &Observed::IsNone),
        Stability::Permanent
    );
    assert_eq!(
        observed(&facts, "truth", &Observed::IsBool { value: true }),
        Stability::Permanent
    );

    // a `tuple`'s length is fixed and a `list`'s is not, and that is the whole
    // distinction the client cannot make for itself
    assert_eq!(
        observed(&facts, "frozen", &Observed::HasLength { length: 3 }),
        Stability::Permanent
    );
    assert_eq!(
        observed(&facts, "growing", &Observed::HasLength { length: 3 }),
        Stability::Until {
            mutation: Mutation::Contents
        }
    );

    // truthiness follows the length rather than being a second reading that
    // could disagree with it
    assert_eq!(
        observed(&facts, "empty", &Observed::IsTruthy { truthy: false }),
        Stability::Until {
            mutation: Mutation::Contents
        }
    );
    assert_eq!(
        observed(&facts, "growing", &Observed::IsTruthy { truthy: true }),
        Stability::Until {
            mutation: Mutation::Contents
        }
    );
}

#[test]
fn a_builtins_class_is_permanent_and_a_class_statements_is_not() {
    let fixture = Fixture::new("provable", PROVABLE);
    let mut debuggee = stopped(&fixture);
    let facts = prove(&mut debuggee, &["small", "plain"]);

    // cpython refuses `__class__` assignment for a static type, so this cannot
    // stop being true
    assert_eq!(
        observed(
            &facts,
            "small",
            &Observed::IsExactly {
                class: Class {
                    module: "builtins".to_string(),
                    qualname: "int".to_string(),
                }
            }
        ),
        Stability::Permanent
    );

    // a heap type's instance can be given another class, and saying otherwise
    // would be the debugger promising something cpython does not
    assert_eq!(
        observed(
            &facts,
            "plain",
            &Observed::IsExactly {
                class: Class {
                    module: "__main__".to_string(),
                    qualname: "Plain".to_string(),
                }
            }
        ),
        Stability::Until {
            mutation: Mutation::Class
        }
    );
}

#[test]
fn an_enum_member_is_named_by_the_class_and_member_a_source_can_resolve() {
    let fixture = Fixture::new("provable", PROVABLE);
    let mut debuggee = stopped(&fixture);
    let facts = prove(&mut debuggee, &["colour"]);

    // `Colour.RED` is what the source spells, so both halves have to be here —
    // a member name on its own resolves to nothing
    observed(
        &facts,
        "colour",
        &Observed::IsEnumMember {
            class: Class {
                module: "__main__".to_string(),
                qualname: "Colour".to_string(),
            },
            member: "RED".to_string(),
        },
    );
}

#[test]
fn a_dotted_path_is_followed_through_storage_and_refused_at_a_property() {
    let fixture = Fixture::new("provable", PROVABLE);
    let mut debuggee = stopped(&fixture);
    let facts = prove(
        &mut debuggee,
        &["plain.limit", "loud.mode", "plain.missing"],
    );

    // an ordinary attribute is in the instance dictionary, and reading it runs
    // nothing
    assert_eq!(
        observed(
            &facts,
            "plain.limit",
            &Observed::IsInt {
                text: "7".to_string()
            }
        ),
        Stability::Permanent
    );

    // a property is a data descriptor, so it wins over the instance dictionary
    // and reading it is calling the program
    let Silence::WouldRun { member, owner } = silence(&facts, "loud.mode") else {
        panic!(
            "expected a property to be refused, got {:?}",
            silence(&facts, "loud.mode")
        );
    };
    assert_eq!(member, "mode");
    assert_eq!(owner.qualname, "Loud");

    assert!(
        matches!(silence(&facts, "plain.missing"), Silence::Missing { segment } if segment == "missing"),
        "an attribute that is not stored is named rather than reached for: {:?}",
        silence(&facts, "plain.missing")
    );
}

#[test]
fn a_subclass_keeps_its_class_facts_and_is_given_no_length() {
    let fixture = Fixture::new("provable", PROVABLE);
    let mut debuggee = stopped(&fixture);
    let facts = prove(&mut debuggee, &["subclassed"]);

    observed(
        &facts,
        "subclassed",
        &Observed::IsExactly {
            class: Class {
                module: "__main__".to_string(),
                qualname: "Counted".to_string(),
            },
        },
    );

    // `Counted` does not override `__len__`, and proving that means judging what
    // the MRO holds. an absent fact is not a claim, and a wrong one would be
    assert!(
        !about(&facts, "subclassed")
            .iter()
            .any(|fact| matches!(fact.observed, Observed::HasLength { .. })),
        "a subclass was given a length, which is a reading nothing checked: {:?}",
        about(&facts, "subclassed")
    );
}

#[test]
fn a_name_that_is_not_bound_is_named_rather_than_left_out() {
    let fixture = Fixture::new("provable", PROVABLE);
    let mut debuggee = stopped(&fixture);
    let facts = prove(&mut debuggee, &["never_assigned"]);

    assert!(
        matches!(silence(&facts, "never_assigned"), Silence::Unbound),
        "an unbound name is reported: {:?}",
        silence(&facts, "never_assigned")
    );
    assert!(
        facts.proved.is_empty(),
        "nothing was asked about that could be proved: {:?}",
        facts.proved
    );
}

#[test]
fn a_path_deeper_than_the_request_allowed_is_refused_rather_than_cut() {
    let fixture = Fixture::new("provable", PROVABLE);
    let mut debuggee = stopped(&fixture);
    let facts = debuggee
        .facts(
            top(),
            &["plain.limit"],
            Limit {
                text: 1024,
                depth: 1,
            },
        )
        .expect("the fact request was answered");

    assert!(
        matches!(
            silence(&facts, "plain.limit"),
            Silence::TooDeep {
                segments: 2,
                limit: 1
            }
        ),
        "answering about `plain` would be a fact about a different thing: {:?}",
        silence(&facts, "plain.limit")
    );
}

#[test]
fn proving_facts_runs_none_of_the_programs_own_code() {
    let fixture = Fixture::new("provable", PROVABLE);
    let mut debuggee = stopped(&fixture);

    // every one of these would call into `Loud` if the prover reached for the
    // abstract protocol: `loud` is where `__len__` and `__bool__` live, and
    // `loud.mode` is a property
    prove(
        &mut debuggee,
        &["loud", "loud.mode", "growing", "plain.limit"],
    );

    to_exit(&mut debuggee);

    // the program's own record, not the debugger's. an answer that looked right
    // and was taken by running `Loud.__len__` fails here and only here
    let touched = std::fs::read_to_string(fixture.directory().join("touched"))
        .expect("the program wrote what the debugger touched");
    assert_eq!(
        touched, "",
        "reading facts ran the program's own code, which is the one thing this \
         request promises it does not do"
    );
}

/// the wire shape a client parses, pinned against a real interpreter's facts
///
/// `basedpython-pycharm` reads this json field by field — it has no rust types to share — so every
/// tag and key below is a contract with a program in another language that cannot fail loudly when
/// it changes. a rename here makes that plugin quietly find nothing, which is the failure this
/// exists to turn into a red test
///
/// it asserts the *strings*, not a snapshot, because what the other side hard-codes is strings
#[test]
fn the_json_a_client_parses_is_the_json_this_produces() {
    let fixture = Fixture::new("provable", PROVABLE);
    let mut debuggee = stopped(&fixture);
    let facts = prove(
        &mut debuggee,
        &["small", "growing", "plain", "colour", "never_assigned"],
    );

    let json = serde_json::to_value(&facts).expect("facts serialise");

    // the two lists a client reads, by the names it reads them by
    let proved = json["proved"].as_array().expect("`proved` is an array");
    let silent = json["silent"].as_array().expect("`silent` is an array");
    assert!(!proved.is_empty(), "nothing was proved: {json}");
    assert_eq!(
        silent.len(),
        1,
        "`never_assigned` is the only name with nothing to say: {json}"
    );
    assert_eq!(silent[0]["name"], "never_assigned");
    assert_eq!(silent[0]["why"]["silence"], "unbound");

    // one fact's whole shape, spelled out. a client reads `name`, then `observed.observed` to
    // learn which kind it is, then that kind's own fields
    let small = proved
        .iter()
        .find(|fact| fact["name"] == "small" && fact["observed"]["observed"] == "is_int")
        .unwrap_or_else(|| panic!("no `is_int` fact for `small`: {json}"));
    assert_eq!(small["observed"]["text"], "5");
    assert_eq!(small["scope"], "local");
    assert_eq!(small["stability"]["stability"], "permanent");

    // the tag every kind goes across as, which is what a client matches on
    let kinds: Vec<&str> = proved
        .iter()
        .filter_map(|fact| fact["observed"]["observed"].as_str())
        .collect();
    for expected in [
        "is_int",
        "is_exactly",
        "has_length",
        "is_truthy",
        "is_enum_member",
    ] {
        assert!(
            kinds.contains(&expected),
            "`{expected}` is a tag a client matches on and it is not in {kinds:?}"
        );
    }

    // a class is two fields, because a client resolves it against source it is reading
    let class = proved
        .iter()
        .find(|fact| fact["name"] == "plain" && fact["observed"]["observed"] == "is_exactly")
        .unwrap_or_else(|| panic!("no class fact for `plain`: {json}"));
    assert_eq!(class["observed"]["class"]["module"], "__main__");
    assert_eq!(class["observed"]["class"]["qualname"], "Plain");
    // a heap type's instance can be given another class, and a client that carried this reading
    // past a `__class__` assignment would be wrong. the mutation is named so it can decide
    assert_eq!(class["stability"]["stability"], "until");
    assert_eq!(class["stability"]["mutation"], "class");

    // the one a client must *drop*: true now, false after the next `append`
    let length = proved
        .iter()
        .find(|fact| fact["name"] == "growing" && fact["observed"]["observed"] == "has_length")
        .unwrap_or_else(|| panic!("no length fact for `growing`: {json}"));
    assert_eq!(length["stability"]["mutation"], "contents");

    // an enum member carries both halves, because `Colour.RED` is what the source spells
    let member = proved
        .iter()
        .find(|fact| fact["observed"]["observed"] == "is_enum_member")
        .unwrap_or_else(|| panic!("no enum fact: {json}"));
    assert_eq!(member["observed"]["class"]["qualname"], "Colour");
    assert_eq!(member["observed"]["member"], "RED");
}
