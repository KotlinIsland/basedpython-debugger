//! what a program wrote, and the moment it is reported over
//!
//! a front end that pipes the debuggee reads its output on threads of its own,
//! and learns the program is over on a **different** descriptor — the control
//! connection, which closes when the process dies. nothing orders those two
//! against each other, so the engine waits for the first before reporting the
//! second
//!
//! that wait cannot be unbounded, and this is the file that says why in a form
//! that fails: a forked child inherits the write end of the pipe, so a program
//! that leaves one running never reaches end-of-file at all. waiting for it
//! would turn the exit of every program with a daemon behind it into a hang
//!
//! everything here drives a real interpreter, because what is under test is what
//! a real pipe and a real `fork` do

// **the whole file is unix.** its fixtures call `os.fork()`, which does not
// exist on windows — so without this the windows leg of the matrix compiles it,
// runs it, and fails on an `AttributeError` that has nothing to do with the
// debugger
#![cfg(unix)]

use std::io::BufRead as _;
use std::sync::{Arc, Mutex};

use bpd_core::python::Capabilities;
use bpd_core::{Forwarded, Running};
use bpd_engine::{Debuggee, Forwarders, Launched};
use bpd_test::debuggee::Fixture;
use bpd_test::reporting::Children;

/// the interpreter the built agent matches, or a failure saying how to get one
fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// everything the program wrote, as the front ends collect it
type Collected = Arc<Mutex<String>>;

/// launch with the debuggee's streams in pipes, read the way a front end reads
/// them
///
/// the threads are handed to the engine rather than detached, which is the whole
/// subject of this file: it is what lets the engine wait for them before it says
/// the program is over
fn piped(fixture: &Fixture) -> (Debuggee, Collected) {
    let collected: Collected = Arc::new(Mutex::new(String::new()));
    let writing = Arc::clone(&collected);

    let launched = bpd_engine::launch_piped(
        interpreter(),
        &bpd_engine::Program::Script(fixture.path()),
        &[],
        move |stdout, stderr| {
            Forwarders::on(
                [
                    Box::new(stdout) as Box<dyn std::io::Read + Send>,
                    Box::new(stderr) as Box<dyn std::io::Read + Send>,
                ]
                .into_iter()
                .map(|stream| {
                    let into = Arc::clone(&writing);
                    std::thread::spawn(move || {
                        // a line at a time, and published as each one arrives,
                        // because that is what the real front ends do and it is
                        // the difference that matters here: a reader that
                        // accumulated until end-of-file would have **nothing**
                        // to show for a stream a child is still holding open,
                        // which is the case below
                        let mut reader = std::io::BufReader::new(stream);
                        let mut line = String::new();
                        loop {
                            line.clear();
                            match reader.read_line(&mut line) {
                                Ok(0) | Err(_) => return,
                                Ok(_) => into
                                    .lock()
                                    .expect("nothing panics holding the output")
                                    .push_str(&line),
                            }
                        }
                    })
                })
                .collect(),
            )
        },
    );

    match launched {
        Ok(Launched::Stopped(debuggee)) => (debuggee, collected),
        Ok(Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

/// a program that says more than a pipe holds and then stops saying it
///
/// the size is the point. a pipe holds about 64 KiB, so a program writing this
/// much cannot have had it all read by the time it exits — the last of it is
/// still in the pipe, and whatever is reading has more reads to do after the
/// process is gone
const A_LOT_TO_SAY: &str = r#"for line in range(4000):
    print(f"line {line} of a program with plenty to say", flush=True)
print("the last thing this program said", flush=True)
"#;

#[test]
fn a_program_is_not_reported_over_until_what_it_wrote_has_been_carried() {
    let fixture = Fixture::new("talker", A_LOT_TO_SAY);
    let (mut debuggee, collected) = piped(&fixture);

    let output = match debuggee
        .run(&mut Children::default())
        .expect("the debuggee was resumed")
    {
        Running::Exited { status, output, .. } => {
            assert!(status.success(), "the program exited with {status}");
            output
        }
        other => panic!("nothing was set, and the program answered with {other:?}"),
    };

    assert_eq!(
        output,
        Forwarded::Everything,
        "nothing outlived this program, so its pipe reached end-of-file the \
         moment it died and there was nothing left to wait for"
    );

    // read **after** the run answered, which is what makes this an assertion
    // about order. the engine has said the program is over, and everything it
    // wrote has to already be here — a debugger that reported the exit first
    // would be one whose client stops listening before the last line arrives
    let said = collected
        .lock()
        .expect("nothing panics holding the output")
        .clone();
    assert!(
        said.contains("the last thing this program said"),
        "the program was reported over while the last thing it printed was \
         still in a pipe. {} bytes had been carried at that point",
        said.len()
    );
    for line in [0, 1_000, 2_500, 3_999] {
        let expected = format!("line {line} of a program with plenty to say");
        assert!(
            said.contains(&expected),
            "the engine reported the program over without {expected:?}, so what \
             it wrote and what was carried are not the same thing"
        );
    }
}

/// a program whose child outlives it, holding the stream the parent wrote to
///
/// `os.fork` rather than `subprocess`, because a fork inherits the write end of
/// the pipe **by construction** — there is no argument to get wrong. the child
/// outlives the parent by longer than the engine will wait, which is what makes
/// the outcome a certainty rather than a race
const A_CHILD_THAT_OUTLIVES_IT: &str = r#"import os
import sys
import time

if os.fork() == 0:
    # longer than the engine will wait, and no longer: this child is orphaned
    # when the test ends, and a stray that outlives the suite is a stray
    # somebody has to notice
    time.sleep(6)
    os._exit(0)

print("the parent is done", flush=True)
sys.exit(0)
"#;

#[test]
fn a_program_whose_child_holds_its_stream_open_is_said_to_be_still_writing() {
    // the bound, and why it has to exist. the child holds the write end of the
    // parent's stdout, so the pipe never reaches end-of-file while it lives — a
    // debugger that waited for one here would hang at the exit of every program
    // that leaves something running behind it
    //
    // so the wait gives up, and what it reports is that it gave up. that is the
    // difference between a claim bpd cannot make and a claim it makes anyway
    let fixture = Fixture::new("parent", A_CHILD_THAT_OUTLIVES_IT);
    let (mut debuggee, collected) = piped(&fixture);

    let output = match debuggee
        .run(&mut Children::default())
        .expect("the debuggee was resumed")
    {
        Running::Exited { status, output, .. } => {
            assert!(status.success(), "the parent exited with {status}");
            output
        }
        other => panic!("nothing was set, and the program answered with {other:?}"),
    };

    assert_eq!(
        output,
        Forwarded::StillHeldOpen,
        "the child inherited the write end and is still running, so end-of-file \
         cannot have arrived — reporting that everything was carried would be \
         bpd claiming a thing it waited for and did not get"
    );

    // and what the parent itself wrote is here regardless: the bound stops the
    // wait, it does not stop the reading
    let said = collected
        .lock()
        .expect("nothing panics holding the output")
        .clone();
    assert!(
        said.contains("the parent is done"),
        "the parent's own output was lost rather than merely unfinished: {said:?}"
    );
}
