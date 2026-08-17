//! what the values half of a recording costs, decomposed
//!
//! the first measurement of it was one number — the location and a copy of the
//! locals against the location alone — and one number does not say *where* the
//! cost is. a `LINE` event carries a code object and a line and **no frame**, so
//! three separate things happen before a value is stored: a frame is reached,
//! `f_locals` is materialised, and each name is rendered
//!
//! so [`Depth`] is cumulative, and each row this prints differs from the one
//! above it by exactly one of those. the instrument is the deliverable; the
//! numbers are whatever it says on the machine it is run on
//!
//! ## what it found, and what it did not
//!
//! **rendering dominates.** across every run of this, reaching the frame and
//! materialising `f_locals` together added a fraction of what rendering added on
//! its own. so the values half is expensive per *name* rather than per line,
//! which makes "which names" the lever — and that is the shape logpoints already
//! have
//!
//! **no multiple is published here.** four attempts produced tables that
//! disagreed, and the reason turned out to be the machine rather than the
//! design: they were taken at a load average near 100, with another build and an
//! IDE competing. a ratio is also a fraction whose denominator is the program —
//! a bare iteration of [`LOOP`] is nanoseconds, so any fixed per-event cost
//! divides into an enormous number, which is why [`HEAVIER`] is here to be run
//! beside it
//!
//! so this is run on a **quiet machine** when a figure is wanted, and what it
//! prints beside every row is the spread, because a number whose noise is larger
//! than the effect is not a measurement. that is also why nothing in the docs
//! quotes one from here yet
//!
//! ## what is not concluded
//!
//! whether the per-event floor can come down. the callback checks several
//! subsystems and takes a lock per line, and none of that was isolated here

use bpd_core::{Depth, Running, SourceBreakpoint};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

fn launch(fixture: &Fixture) -> Debuggee {
    match bpd_engine::launch(
        bpd_test::agent::matching_interpreter(),
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

/// a heavier body, so the ratio can be seen to move
///
/// the multiple a recording costs is a fraction whose denominator is the
/// program. a bare iteration of [`LOOP`] is 13 ns, so any fixed per-event cost
/// divides into an enormous number — and quoting that number alone, as this
/// project did, reads as a property of bpd when half of it is a property of the
/// loop. this body does real work per line and the same recording costs a much
/// smaller multiple of it
const HEAVIER: &str = r"import time


def work(rounds):
    total = 0
    label = 'counting'
    for index in range(rounds):
        total = total + len(str(index)) + sum(range(index % 8))
        label = ('counting', index % 4)
    return total


samples = []
for _ in range(7):
    started = time.perf_counter()
    work(50000)
    samples.append(time.perf_counter() - started)

elapsed = min(samples)
spread = max(samples) - min(samples)
done = 1              # the breakpoint
";

/// the loop the original measurement used: three lines, many times
///
/// the locals are deliberately ordinary — two ints and a str — because a frame
/// full of exotic objects would measure the renderer rather than the recording
const LOOP: &str = r"import time


def work(rounds):
    total = 0
    label = 'counting'
    for index in range(rounds):
        total = total + index
        label = 'counting'
    return total


# many samples inside one process, and the fastest kept. one sample per launch
# was what the first two attempts did, and a fresh debuggee costs hundreds of
# milliseconds of its own — the spread between launches was larger than the
# effect, and it put the rows out of the order the depths make impossible
samples = []
for _ in range(7):
    started = time.perf_counter()
    work(50000)
    samples.append(time.perf_counter() - started)

elapsed = min(samples)
spread = max(samples) - min(samples)
done = 1              # the breakpoint
";

/// run the loop once at one depth, and say how long **the program** took
///
/// timed by the program itself, with `time.perf_counter()` around the loop and
/// the answer read out of the stopped frame. the first version of this timed the
/// engine call instead, and measured launch: a fresh debuggee costs hundreds of
/// milliseconds and the loop costs a few, so what came back was noise with the
/// signal buried in it — `f_locals` appeared to be *faster* than reaching the
/// frame, which the depths being cumulative makes impossible
fn timed(program: &str, depth: Option<Depth>) -> (f64, f64) {
    let fixture = Fixture::new("loop", program);
    let done = line_of(program, "done = 1");
    let mut debuggee = launch(&fixture);

    if let Some(depth) = depth {
        debuggee
            .record(true, depth)
            .expect("recording was answered");
    }
    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), done)])
        .expect("the breakpoint was answered");

    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        other => panic!("the breakpoint never stopped it: {other:?}"),
    }

    let frame = debuggee
        .the_stack(Some(1))
        .expect("the stack was answered")
        .frames[0]
        .id;
    let read = |debuggee: &mut Debuggee, name: &str| match debuggee
        .evaluate(frame, name, bpd_core::Detail::default())
        .expect("the evaluation was answered")
    {
        bpd_core::Evaluated::Value { value } => match value.content {
            bpd_core::Content::Float { text } => text
                .parse::<f64>()
                .expect("`perf_counter` differences are floats"),
            other => panic!("`{name}` is a float, and this is {other:?}"),
        },
        bpd_core::Evaluated::Raised { error } => panic!("`{name}` raised {error:?}"),
    };
    (
        read(&mut debuggee, "elapsed"),
        read(&mut debuggee, "spread"),
    )
}

#[test]
#[ignore = "a measurement rather than an assertion — run it with --ignored"]
#[expect(
    clippy::print_stdout,
    reason = "the table it prints is the whole point of it: this is a \
              measurement, and one nobody can read is not one"
)]
fn what_each_step_of_the_values_half_costs() {
    // the depths are cumulative, so each row is the one above it plus one thing
    let rows = [
        ("not recording", None),
        ("the location", Some(Depth::Where)),
        ("and the frame", Some(Depth::Frame)),
        ("and `f_locals`", Some(Depth::Locals)),
        ("and rendering them", Some(Depth::Values)),
    ];

    for (workload, program) in [("a 13 ns body", LOOP), ("a heavier body", HEAVIER)] {
        let measured: Vec<(&str, f64, f64)> = rows
            .into_iter()
            .map(|(what, depth)| {
                let (took, spread) = timed(program, depth);
                (what, took, spread)
            })
            .collect();

        let bare = measured[0].1;
        println!(
            "\n{workload}\n{:<22} {:>10}  {:>8}  {:>10}",
            "what is kept", "fastest", "vs bare", "spread"
        );
        for (what, took, spread) in &measured {
            println!(
                "{:<22} {:>7.1} ms  {:>7.1}×  {:>7.1} ms",
                what,
                took * 1000.0,
                took / bare,
                spread * 1000.0
            );
        }
    }
    println!(
        "\nseven samples each, inside one process, fastest kept. the spread is \
         printed\nbecause a number whose noise is larger than the effect is not \
         a measurement\n"
    );
}

#[test]
fn a_recording_at_depth_says_what_the_frame_held() {
    // the experiment is only worth measuring if it answers the question it is
    // for. this is that check: a step carries the names the frame had and what
    // they were, rendered
    let fixture = Fixture::new("loop", LOOP);
    let done = line_of(LOOP, "done = 1");
    let mut debuggee = launch(&fixture);

    debuggee
        .record(true, Depth::Values)
        .expect("recording was answered");
    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), done)])
        .expect("the breakpoint was answered");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        other => panic!("the breakpoint never stopped it: {other:?}"),
    }

    let went = debuggee.trail().expect("the trail was answered");
    let inside: Vec<&bpd_core::Visited> = went
        .went
        .iter()
        .filter(|step| step.function == "work")
        .collect();
    assert!(
        !inside.is_empty(),
        "the recording covered the loop: {:#?}",
        went.went.len()
    );

    // the names the loop binds, at some step of it
    let named: Vec<&str> = inside
        .iter()
        .flat_map(|step| step.held.kept.iter().map(|(name, _)| name.as_str()))
        .collect();
    for wanted in ["total", "label", "index"] {
        assert!(
            named.contains(&wanted),
            "`{wanted}` is a local of that loop and no step recorded it: {:#?}",
            inside.first().map(|step| &step.held.kept)
        );
    }

    // and a str is rendered as itself rather than as its type, which is what
    // makes this worth having over the location alone
    let labels: Vec<&str> = inside
        .iter()
        .flat_map(|step| step.held.kept.iter())
        .filter(|(name, _)| name == "label")
        .map(|(_, text)| text.as_str())
        .collect();
    assert!(
        labels.iter().any(|text| text.contains("counting")),
        "a str renders as itself: {labels:?}"
    );
}

#[test]
fn the_shipped_depth_keeps_nothing_of_the_frame() {
    // the committed trail is `Where`, and this is what says the experiment has
    // not quietly changed it. a recording at the shipped depth costs what it
    // always did because it does none of the work above
    let fixture = Fixture::new("loop", LOOP);
    let done = line_of(LOOP, "done = 1");
    let mut debuggee = launch(&fixture);

    debuggee
        .record(true, Depth::Where)
        .expect("recording was answered");
    debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, fixture.path(), done)])
        .expect("the breakpoint was answered");
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { .. } => {}
        other => panic!("the breakpoint never stopped it: {other:?}"),
    }

    let went = debuggee.trail().expect("the trail was answered");
    assert!(
        went.went.iter().all(|step| step.held.kept.is_empty()),
        "the shipped depth records where and nothing else"
    );
}

#[test]
fn a_depth_nobody_named_is_the_cheap_one() {
    // the default is what a front end sends when its user said nothing, and it
    // has to be the depth that costs least — the alternative is a debugger
    // charging somebody for a mode they did not ask for
    assert_eq!(Depth::default(), Depth::Where);
}

#[test]
fn every_depth_is_named_the_way_both_front_ends_spell_it() {
    // MCP offers these as an enum in its schema and DAP parses one out of the
    // request, and both go through the same serde derive. a rename on one side
    // only would be a depth a client could ask for and never get
    for (depth, spelled) in [
        (Depth::Where, "where"),
        (Depth::Frame, "frame"),
        (Depth::Locals, "locals"),
        (Depth::Values, "values"),
    ] {
        assert_eq!(depth.to_string(), spelled);
        assert_eq!(
            serde_json::to_value(depth).expect("a depth serialises"),
            serde_json::Value::String(spelled.to_string()),
            "the wire spelling and the printed one have to agree"
        );
    }
}
