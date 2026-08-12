//! a stop holds one thread, and the rest of the program keeps running
//!
//! this is the model, and it is easy to claim and easy to get wrong. a debugger
//! that froze the whole process by accident would look identical from the
//! outside to one that meant to, so nothing here takes the agent's word for
//! anything: the proof that another thread is running is a **file the program
//! wrote while a thread was held**, and the proof that a held thread is held is
//! a file that did not appear
//!
//! the threads coordinate through files rather than through timing, so a slow
//! machine makes a test slower and never makes it flaky. the one place a
//! deadline appears is where a test has to show something did *not* happen, and
//! there the thing it waits on first is a side effect of the thread that was
//! supposed to be running

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bpd_core::python::Capabilities;
use bpd_core::{Holding, Mode, Progress, Running, SourceBreakpoint, Stop};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// how long a test waits for a side effect it expects to happen
///
/// generous, because it is only ever reached when something is wrong: a program
/// coordinating through files is as fast as the filesystem, and every wait here
/// is for something another thread does immediately
const PATIENCE: Duration = Duration::from_secs(30);

/// how long the census waits between its two samples
const SETTLE: Duration = Duration::from_millis(50);

/// what every threaded fixture in this file is built on
///
/// coordination through files, and every thread announcing the identity the
/// interpreter gave it, so a test can say which stop belongs to which thread
/// without guessing from the order they arrived in
const PRELUDE: &str = r#"import pathlib, threading, time

HERE = pathlib.Path(__file__).parent


def touch(name):
    (HERE / name).write_text("x")


def announce(name):
    (HERE / ("ident_" + name)).write_text(str(threading.get_ident()))


def wait_for(name):
    path = HERE / name
    deadline = time.monotonic() + 120
    while not path.exists():
        if time.monotonic() > deadline:
            raise SystemExit("the test never created " + name)
        time.sleep(0.002)

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

/// set one breakpoint and let the program go
fn arm(debuggee: &mut Debuggee, file: &Path, line: u32) {
    let resolved = debuggee
        .set_breakpoints(vec![SourceBreakpoint::at(1, file, line)])
        .expect("the breakpoint request was answered");
    assert_eq!(resolved.len(), 1);
}

/// wait for the next stop, and require that it is one
fn next_stop(debuggee: &mut Debuggee) -> Stop {
    match debuggee
        .wait(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was waited on")
    {
        Running::Stopped { stop, .. } => stop,
        Running::Exited { status, .. } => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        // bpd launched this program and holds its child, so it is bpd that
        // reads the exit
        Running::Ended { .. } => unreachable!(
            "the program bpd launched ended without an exit status, and bpd \
             holds its child"
        ),
        Running::Finishing { threads, .. } => {
            panic!("the debuggee ended holding {threads:?} instead of stopping")
        }
    }
}

/// resume everything and require that the program finishes
fn to_exit(debuggee: &mut Debuggee) {
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Exited { status, .. } => {
            assert!(status.success(), "the program exited with {status}");
        }
        Running::Stopped { stop, .. } => panic!("it stopped again for {stop:?}"),
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        // bpd launched this program and holds its child, so it is bpd that
        // reads the exit
        Running::Ended { .. } => unreachable!(
            "the program bpd launched ended without an exit status, and bpd \
             holds its child"
        ),
        Running::Finishing { threads, .. } => {
            panic!("the debuggee ended holding {threads:?}")
        }
    }
}

/// the file a thread of the program is waiting on
fn tell(fixture: &Fixture, name: &str) {
    std::fs::write(fixture.directory().join(name), "x")
        .unwrap_or_else(|error| panic!("could not write {name}: {error}"));
}

/// wait until the program produces a side effect, or say what never happened
fn expect(fixture: &Fixture, name: &str) {
    let path = fixture.directory().join(name);
    let deadline = Instant::now() + PATIENCE;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "the program never wrote `{name}`, so whatever was supposed to \
             produce it was not running"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// the identity a thread of the program announced for itself
fn ident(fixture: &Fixture, name: &str) -> u64 {
    let path = fixture.directory().join(format!("ident_{name}"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("`{}` was never written: {error}", path.display()))
        .trim()
        .parse()
        .expect("a thread identity is a number")
}

fn missing(fixture: &Fixture, name: &str) -> PathBuf {
    let path = fixture.directory().join(name);
    assert!(
        !path.exists(),
        "`{name}` exists already, so this proves nothing"
    );
    path
}

const PROGRESS: &str = r#"def worker():
    announce("worker")
    touch("worker_started")
    wait_for("go")
    touch("worker_ran")


announce("main")
thread = threading.Thread(target=worker)
thread.start()
wait_for("worker_started")
held = 1
thread.join()
touch("finished")
"#;

/// wait until `thread` has stopped making progress, or say that it never did
///
/// for a thread that is about to block in a C call this is the only sound way
/// to know it has got there: it announces itself *before* the blocking call,
/// so the announcement is not the event worth waiting for
fn settled(debuggee: &mut Debuggee, thread: u64) {
    for _ in 0..100 {
        let census = debuggee
            .threads(SETTLE)
            .expect("the threads were reported on");
        let state = census
            .get(thread)
            .unwrap_or_else(|| panic!("the census left out thread {thread}"));
        if state.progress == Progress::Still {
            return;
        }
    }
    panic!("thread {thread} never stopped making progress, so it never parked");
}

#[test]
fn another_thread_goes_on_running_while_one_thread_is_held() {
    let source = format!("{PRELUDE}{PROGRESS}");
    let fixture = Fixture::new("progress", &source);
    let at_line = line_of(&source, "held = 1");

    let mut debuggee = launch(&fixture);
    arm(&mut debuggee, &fixture.path(), at_line);
    let stop = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected the main thread to stop, got {other:?}"),
    };
    assert_eq!(stop.thread, ident(&fixture, "main"));

    // the whole claim, and nothing about it comes from the agent: the worker
    // thread is told to go while the main thread is held, and the only way the
    // file appears is a thread of the debuggee running python to write it
    let ran = missing(&fixture, "worker_ran");
    tell(&fixture, "go");
    expect(&fixture, "worker_ran");
    assert!(ran.exists());

    // and the held thread really is held: the line after it has not run
    assert!(
        !fixture.directory().join("finished").exists(),
        "the held thread ran on past its breakpoint"
    );

    to_exit(&mut debuggee);
}

const TWO_WORKERS: &str = r#"def worker(name):
    announce(name)
    touch(name + "_before")
    marker = name
    touch(name + "_after")


threads = [threading.Thread(target=worker, args=(name,)) for name in ("a", "b")]
for thread in threads:
    thread.start()
for thread in threads:
    thread.join()
touch("finished")
"#;

/// hold both workers of [`TWO_WORKERS`] and say which stop belongs to which
///
/// the order two threads reach a line in is the operating system's business, so
/// the stops are matched to the workers by the identity each one announced
fn hold_both(fixture: &Fixture, debuggee: &mut Debuggee) -> (Stop, Stop) {
    let first = next_stop(debuggee);
    let second = next_stop(debuggee);
    assert_eq!(
        debuggee.held().len(),
        2,
        "both threads are held, and neither has been resumed"
    );

    let a = ident(fixture, "a");
    if first.thread == a {
        (first, second)
    } else {
        (second, first)
    }
}

#[test]
fn a_second_thread_reaching_a_breakpoint_reports_its_own_stop_without_waiting() {
    let source = format!("{PRELUDE}{TWO_WORKERS}");
    let fixture = Fixture::new("two_workers", &source);
    let at_line = line_of(&source, "marker = name");

    let mut debuggee = launch(&fixture);
    arm(&mut debuggee, &fixture.path(), at_line);
    debuggee.resume_all().expect("the entry stop was resumed");

    // the point: nothing is resumed between the two. a second thread that had
    // to wait for the connection would be a thread that is not running, and
    // there would be no second stop until the first was let go
    let (a, b) = hold_both(&fixture, &mut debuggee);
    assert_ne!(a.thread, b.thread);
    assert_eq!(b.thread, ident(&fixture, "b"));
    assert_ne!(a.stop, b.stop, "each stop is a stop of its own");

    to_exit(&mut debuggee);
}

#[test]
fn resuming_one_held_thread_leaves_the_other_held() {
    let source = format!("{PRELUDE}{TWO_WORKERS}");
    let fixture = Fixture::new("two_workers", &source);
    let at_line = line_of(&source, "marker = name");

    let mut debuggee = launch(&fixture);
    arm(&mut debuggee, &fixture.path(), at_line);
    debuggee.resume_all().expect("the entry stop was resumed");
    let (a, b) = hold_both(&fixture, &mut debuggee);

    let b_after = missing(&fixture, "b_after");
    assert_eq!(
        debuggee
            .resume(&[a.thread])
            .expect("one thread was resumed"),
        [a.thread]
    );

    // `a` ran on, which means the program had every chance to run `b` too
    expect(&fixture, "a_after");
    assert!(
        !b_after.exists(),
        "the other thread ran on although it was never resumed"
    );

    let census = debuggee
        .threads(SETTLE)
        .expect("the threads were reported on");
    let state = census
        .get(b.thread)
        .unwrap_or_else(|| panic!("the census left out thread {}", b.thread));
    assert_eq!(state.held, Some(b.stop));
    assert_eq!(state.progress, Progress::Held);
    assert_eq!(debuggee.held().len(), 1, "only `b` is still held");

    to_exit(&mut debuggee);
}

#[test]
fn resuming_a_thread_that_is_not_held_is_refused_rather_than_ignored() {
    let source = format!("{PRELUDE}{TWO_WORKERS}");
    let fixture = Fixture::new("two_workers", &source);
    let at_line = line_of(&source, "marker = name");

    let mut debuggee = launch(&fixture);
    arm(&mut debuggee, &fixture.path(), at_line);
    debuggee.resume_all().expect("the entry stop was resumed");
    let (a, b) = hold_both(&fixture, &mut debuggee);

    let invented = a.thread + b.thread;
    let error = debuggee
        .resume(&[a.thread, invented])
        .expect_err("that thread is not held");
    let said = error.to_string();
    assert!(
        said.contains(&format!("thread {invented} is not held")),
        "the refusal has to name the thread, and said {said}"
    );

    // all or nothing: the thread that *was* named is still held, because a
    // resume that half happened would leave the client's idea of what is
    // running different from the agent's with nothing saying which is right
    assert_eq!(debuggee.held().len(), 2, "nothing was resumed");
    let census = debuggee
        .threads(Duration::ZERO)
        .expect("the threads were reported on");
    assert_eq!(
        census.get(a.thread).map(|state| state.progress),
        Some(Progress::Held)
    );

    to_exit(&mut debuggee);
}

const LOCKED: &str = r#"gate = threading.Lock()


def holder():
    announce("holder")
    with gate:
        touch("holding")
        inside = 1
    touch("released")


def waiter():
    announce("waiter")
    wait_for("holding")
    touch("waiter_started")
    with gate:
        touch("waiter_got_it")


threads = [threading.Thread(target=holder), threading.Thread(target=waiter)]
for thread in threads:
    thread.start()
for thread in threads:
    thread.join()
touch("finished")
"#;

#[test]
fn a_thread_piled_up_behind_a_lock_the_held_thread_took_is_reported_as_getting_nowhere() {
    let source = format!("{PRELUDE}{LOCKED}");
    let fixture = Fixture::new("locked", &source);
    let at_line = line_of(&source, "inside = 1");

    let mut debuggee = launch(&fixture);
    arm(&mut debuggee, &fixture.path(), at_line);
    let stop = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected the holder thread to stop, got {other:?}"),
    };
    assert_eq!(stop.thread, ident(&fixture, "holder"));

    // cpython records no owner for a `threading.Lock` and keeps no registry of
    // them, so a lock this thread took is **not** knowable and is not claimed.
    // this asserts the limit rather than working around it
    assert!(
        stop.holding.is_empty(),
        "a plain lock is not knowable, and the stop claimed {:?}",
        stop.holding
    );

    // what is knowable is the other end of it. the waiter announced itself and
    // then asked for the lock, and it is getting nowhere
    expect(&fixture, "waiter_started");
    let waiter = ident(&fixture, "waiter");

    // `waiter_started` is written on the line **before** `with gate:`, so the
    // marker proves the thread reached the lock and not that it is behind it —
    // a census taken the moment it appears can still catch the thread executing
    // the python that writes the marker. so the wait is on the **state**, which
    // is what the earlier flake in `stopping_the_world_...` was fixed to do:
    // waiting for time makes a test that fails under load, waiting for a
    // condition makes one that fails only when the condition never comes
    let deadline = Instant::now() + PATIENCE;
    let (census, state) = loop {
        let census = debuggee
            .threads(SETTLE)
            .expect("the threads were reported on");
        let state = census
            .get(waiter)
            .unwrap_or_else(|| panic!("the census left out thread {waiter}"))
            .clone();
        let arrived = state.progress == Progress::Still
            && state.at.as_ref().is_some_and(|at| at.function == "waiter");
        if arrived {
            break (census, state);
        }
        assert!(
            Instant::now() < deadline,
            "the waiter never settled behind the lock the held thread took. it \
             is blocked on `gate` and nothing can release it while the holder \
             is held, so this is the debugger failing to see a thread that is \
             getting nowhere. the last sample was progress {:?} at {:?}",
            state.progress,
            state.at
        );
    };

    assert_eq!(state.held, None, "bpd is not holding the waiter");
    let at = state.at.as_ref().expect("the waiter has a python frame");
    assert_eq!(
        Path::new(&at.file),
        fixture.path(),
        "the sample says where, and it is the program's own file"
    );
    assert_eq!(
        at.function, "waiter",
        "the sample names the function it got stuck in, and said {at}"
    );
    assert_eq!(census.settle, SETTLE, "the answer says what still means");

    to_exit(&mut debuggee);
}

#[test]
fn the_import_machinery_runs_in_frames_named_after_itself() {
    // the one lock cpython makes knowable, and the only reason it is knowable
    // is that the machinery is python with a filename bpd can recognise. there
    // is no api that says "this thread is importing", so this is measured in a
    // **bare** interpreter, with no agent near it: if cpython ever renames the
    // frozen modules the detection stops working, and this fails rather than
    // the detection quietly turning itself off
    let seen = bpd_test::eval(
        interpreter(),
        "import json, pathlib, sys, tempfile\n\
         directory = tempfile.mkdtemp()\n\
         probe = 'import sys\\n\\\n\
         frames = []\\n\\\n\
         frame = sys._getframe()\\n\\\n\
         while frame is not None:\\n\\\n\
         \x20   frames.append([frame.f_code.co_filename, frame.f_code.co_qualname])\\n\\\n\
         \x20   frame = frame.f_back\\n\\\n\
         import json\\n\\\n\
         print(json.dumps(frames))\\n'\n\
         pathlib.Path(directory, 'probe_module.py').write_text(probe)\n\
         sys.path.insert(0, directory)\n\
         import probe_module\n",
    );
    let seen: Vec<(String, String)> =
        serde_json::from_str(&seen).expect("the probe prints a json list of what it saw");

    let machinery: Vec<&(String, String)> = seen
        .iter()
        .filter(|(file, _)| file.starts_with("<frozen importlib._bootstrap"))
        .collect();
    assert!(
        !machinery.is_empty(),
        "a module executing during an import has the machinery's frames under \
         it, and the walk saw {seen:?}"
    );
    assert!(
        machinery
            .iter()
            .any(|(file, _)| file == "<frozen importlib._bootstrap>"),
        "the frame that knows which module is being imported lives in \
         `<frozen importlib._bootstrap>`, and the walk saw {seen:?}"
    );
    assert!(
        machinery
            .iter()
            .any(|(_, qualname)| qualname == "_find_and_load"),
        "`_find_and_load` is the frame carrying the module name, and the walk \
         saw {seen:?}"
    );
}

const IMPORTS: &str = r#"def importer(name):
    announce(name)
    touch(name + "_started")
    import late

    touch(name + "_imported")


first = threading.Thread(target=importer, args=("first",))
first.start()
wait_for("importing")
second = threading.Thread(target=importer, args=("second",))
second.start()
first.join()
second.join()
touch("finished")
"#;

const LATE: &str = r#"import pathlib

HERE = pathlib.Path(__file__).parent
(HERE / "importing").write_text("x")
value = 1
"#;

#[test]
fn a_thread_held_inside_the_import_system_says_which_module_it_is_holding() {
    let source = format!("{PRELUDE}{IMPORTS}");
    let fixture = Fixture::new("imports", &source);
    let late = fixture.sibling("late", LATE);
    let at_line = line_of(LATE, "value = 1");

    let mut debuggee = launch(&fixture);
    arm(&mut debuggee, &late, at_line);
    let stop = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected the importing thread to stop, got {other:?}"),
    };

    // the one lock cpython makes knowable, because its machinery runs in python
    // frames. a thread importing the same module blocks until this one is
    // resumed, and the user is told that rather than left to discover it
    assert_eq!(
        stop.holding,
        vec![Holding::ImportSystem {
            module: Some("late".to_string())
        }],
        "a stop inside an import has to say so"
    );
    let said = stop.holding[0].to_string();
    assert!(
        said.contains("`late`") && said.contains("blocks until this one is resumed"),
        "the report has to say what it blocks, and said {said}"
    );

    // and the thread behind it is where the report says it would be
    expect(&fixture, "second_started");
    let second = ident(&fixture, "second");
    let census = debuggee
        .threads(SETTLE)
        .expect("the threads were reported on");
    assert_eq!(
        census.get(second).map(|state| state.progress),
        Some(Progress::Still),
        "the second importer is behind the module lock"
    );

    to_exit(&mut debuggee);
}

const WORLD: &str = r#"gate = threading.Lock()
gate.acquire()
go_on = False


def spinner():
    announce("spinner")
    touch("spinner_started")
    while not go_on:
        pass
    touch("spinner_done")


def flipper():
    global go_on
    announce("flipper")
    touch("flipper_started")
    wait_for("release")
    go_on = True
    gate.release()
    touch("flipper_done")


def sleeper():
    announce("sleeper")
    touch("sleeper_started")
    gate.acquire()
    touch("sleeper_done")


announce("main")
threads = [
    threading.Thread(target=spinner),
    threading.Thread(target=flipper),
    threading.Thread(target=sleeper),
]
for thread in threads:
    thread.start()
wait_for("spinner_started")
wait_for("flipper_started")
wait_for("sleeper_started")
everyone = 1
for thread in threads:
    thread.join()
touch("finished")
"#;

#[test]
fn stopping_the_world_holds_what_it_can_and_never_counts_a_native_thread_as_held() {
    let source = format!("{PRELUDE}{WORLD}");
    let fixture = Fixture::new("world", &source);
    let at_line = line_of(&source, "everyone = 1");

    let mut debuggee = launch(&fixture);
    debuggee
        .set_breakpoints(vec![
            SourceBreakpoint::at(1, fixture.path(), at_line),
            // never fires, and that is the point. it arms `LINE` on the
            // spinner's code object, so the two lines of its loop are offered
            // once and then tell the interpreter never to offer them again —
            // and that loop calls nothing, so those two lines are the only
            // events that thread can ever produce. arming `LINE` for the whole
            // program does **not** undo a `DISABLE`, measured in a bare
            // interpreter by `arming_an_event_globally_does_not_undo_a_disable`,
            // so without `restart_events()` this thread is never caught and a
            // thread running python is reported as running in native code
            SourceBreakpoint::at(
                2,
                fixture.path(),
                line_of(&source, "touch(\"spinner_started\")"),
            )
            .when("False"),
        ])
        .expect("the breakpoint request was answered");
    let stop = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected the main thread to stop, got {other:?}"),
    };
    assert_eq!(stop.thread, ident(&fixture, "main"));

    // in non-stop mode the other threads are running, and the only reason a
    // read says otherwise later is that the world was stopped in between
    let before = debuggee
        .the_stack(None)
        .expect("the stack was answered")
        .mode;
    assert_eq!(before, Mode::NonStop);

    let spinner = ident(&fixture, "spinner");
    let flipper = ident(&fixture, "flipper");
    let sleeper = ident(&fixture, "sleeper");

    // `sleeper` touches its file and only *then* calls `gate.acquire()`, so
    // `wait_for("sleeper_started")` returning does not mean it is parked yet —
    // for a few bytecodes it is still executing python and stopping the world
    // would legitimately hold it. that window made this test fail about one run
    // in six
    //
    // the census is what closes it: a thread inside `gate.acquire()` has
    // released the GIL and its frame cannot advance, so it settles on `Still`
    // and stays there, while a thread in the window is `Moved`. this waits for
    // the precondition rather than assuming it
    settled(&mut debuggee, sleeper);

    let stopped = debuggee
        .stop_the_world(stop.stop, Duration::from_secs(10))
        .expect("the world was stopped");

    for (name, thread) in [
        ("main", stop.thread),
        ("spinner", spinner),
        ("flipper", flipper),
    ] {
        assert!(
            stopped.held.contains(&thread),
            "`{name}` was executing python and had to be held, and got {stopped:?}"
        );
    }
    // the hazard the mode exists around: `gate.acquire()` released the GIL and
    // executes no python, so nothing available here can stop it. counting it as
    // held would be claiming a whole-program snapshot that was never taken
    assert_eq!(
        stopped.native,
        vec![sleeper],
        "a thread parked in a C call is running, and is reported as running"
    );
    assert!(!stopped.held.contains(&sleeper));

    let during = debuggee
        .the_stack(None)
        .expect("the stack was answered")
        .mode;
    assert_eq!(
        during,
        Mode::StopTheWorld {
            native: vec![sleeper]
        },
        "every read says which mode it was taken in, and what was still moving"
    );

    // the world is released when the stop that asked for it is, and the threads
    // pick up where they left off
    let flipped = missing(&fixture, "flipper_done");
    tell(&fixture, "release");
    assert!(
        !flipped.exists(),
        "the flipper is parked and cannot have seen the file"
    );
    to_exit(&mut debuggee);
    expect(&fixture, "finished");
}

#[test]
fn arming_an_event_globally_does_not_undo_a_disable() {
    // why stopping the world costs a `restart_events()`. a location that
    // returned `DISABLE` stays disabled when the same event is armed for the
    // whole program, so a thread looping inside one is reachable by nothing
    // short of the process-wide restart. measured in a **bare** interpreter,
    // because it is a statement about PEP 669 rather than about bpd
    let seen = bpd_test::eval(
        interpreter(),
        "import json, sys\n\
         mon = sys.monitoring\n\
         mon.use_tool_id(0, 'probe')\n\
         seen = []\n\
         def spin(n):\n\
         \x20   total = 0\n\
         \x20   for i in range(n):\n\
         \x20       total += i\n\
         \x20   return total\n\
         def on_line(code, line):\n\
         \x20   if code is spin.__code__:\n\
         \x20       seen.append(line)\n\
         \x20   return mon.DISABLE\n\
         mon.register_callback(0, mon.events.LINE, on_line)\n\
         mon.set_local_events(0, spin.__code__, mon.events.LINE)\n\
         spin(3)\n\
         seen.clear()\n\
         mon.set_events(0, mon.events.LINE)\n\
         spin(3)\n\
         armed = list(seen)\n\
         seen.clear()\n\
         mon.restart_events()\n\
         spin(3)\n\
         restarted = list(seen)\n\
         print(json.dumps([armed, restarted]))\n",
    );
    let seen: Vec<Vec<u32>> =
        serde_json::from_str(&seen).expect("the probe prints two json lists of what it saw");
    let (armed, restarted) = (&seen[0], &seen[1]);

    assert!(
        armed.is_empty(),
        "arming the event for the whole program offered a disabled location \
         again, and saw {armed:?}"
    );
    assert!(
        !restarted.is_empty(),
        "`restart_events()` is the only thing that undoes a `DISABLE`, and it \
         offered nothing"
    );
}

const OUTLIVES: &str = r#"def worker():
    announce("worker")
    wait_for("go")
    held = 1
    touch("worker_done")


threading.Thread(target=worker).start()
wait_for("main_may_finish")
touch("main_finished")
"#;

#[test]
fn a_program_that_ends_with_a_thread_still_held_says_so_rather_than_looking_like_a_hang() {
    let source = format!("{PRELUDE}{OUTLIVES}");
    let fixture = Fixture::new("outlives", &source);
    let at_line = line_of(&source, "held = 1");

    let mut debuggee = launch(&fixture);
    arm(&mut debuggee, &fixture.path(), at_line);
    // written before the program is let go, so the worker reaches the
    // breakpoint without the test having to race it
    tell(&fixture, "go");

    let stop = match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, .. } => stop,
        other => panic!("expected the worker to stop, got {other:?}"),
    };
    assert_eq!(stop.thread, ident(&fixture, "worker"));

    // the main thread now runs off the end of the program. it cannot exit: the
    // interpreter finalizes by joining non-daemon threads, and the worker is
    // held. that is a fact the client is told rather than a process that sits
    // there with nothing having said why
    tell(&fixture, "main_may_finish");
    expect(&fixture, "main_finished");

    match debuggee
        .wait(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was waited on")
    {
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        // bpd launched this program and holds its child, so it is bpd that
        // reads the exit
        Running::Ended { .. } => unreachable!(
            "the program bpd launched ended without an exit status, and bpd \
             holds its child"
        ),
        Running::Finishing { threads, .. } => assert_eq!(threads, [stop.thread]),
        other => panic!("expected the program to report what it still holds, got {other:?}"),
    }

    to_exit(&mut debuggee);
    expect(&fixture, "worker_done");
}
