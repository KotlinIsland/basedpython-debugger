//! a `SIGTERM` to `bpd launch` is passed on to its program, which exits as it
//! chooses, and `bpd` exits as it did
//!
//! the DAP half — `bpd dap`, which `by run` starts as its program, and the
//! client's `terminate` beside the signal — is in `dap.rs`. this is the command
//! line's, where there is no client at all and the only thing that can end the
//! program is what is sent to bpd
#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bpd_test::debuggee::Fixture;

/// the binary this test run built
const BPD: &str = env!("CARGO_BIN_EXE_bpd");

/// how long the test waits for the program, and then for bpd
const PATIENCE: Duration = Duration::from_mins(1);

/// a program that writes that it handled `SIGTERM` and exits 7
const ENDS_ON_SIGTERM: &str = r#"import os, pathlib, signal, sys, time

HERE = pathlib.Path(__file__).parent


def on_term(signum, frame):
    with (HERE / "handled").open("a") as handled:
        handled.write(f"{signum}\n")
    sys.exit(7)


signal.signal(signal.SIGTERM, on_term)
(HERE / "ready").write_text(str(os.getpid()))
while True:
    time.sleep(0.01)
"#;

#[test]
fn a_sigterm_to_bpd_launch_is_handled_once_by_the_program_and_bpd_exits_as_it_did() {
    // measured before this through `bpd dap --listen`: bpd ended on the
    // signal, 143, and the program ran on with no parent and its handler never
    // run. `bpd launch` had the same default
    let fixture = Fixture::new("ends_on_sigterm", ENDS_ON_SIGTERM);
    let mut bpd = Command::new(BPD)
        .arg("launch")
        .arg("--python")
        .arg(&bpd_test::agent::matching_interpreter().executable)
        .arg(fixture.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("the binary was built by the same cargo invocation as this test");

    let deadline = Instant::now() + PATIENCE;
    while !fixture.directory().join("ready").exists() {
        assert!(
            Instant::now() < deadline,
            "the program never said it was ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let program: i32 = std::fs::read_to_string(fixture.directory().join("ready"))
        .expect("the program wrote its pid")
        .parse()
        .expect("a pid is a number");

    let sent = Command::new("kill")
        .args(["-TERM", &bpd.id().to_string()])
        .status()
        .expect("`kill` runs");
    assert!(sent.success());

    let status = loop {
        if let Some(status) = bpd.try_wait().expect("bpd is a real child") {
            break status;
        }
        assert!(Instant::now() < deadline, "bpd outlived its program");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        status.code(),
        Some(7),
        "bpd exits as its program did: {status}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.directory().join("handled"))
            .expect("the program's handler ran"),
        "15\n",
        "handled once"
    );
    let alive = Command::new("kill")
        .args(["-0", &program.to_string()])
        .stderr(Stdio::null())
        .status()
        .expect("`kill` runs");
    assert!(!alive.success(), "the program is not left running");
}

/// run a command on a new controlling terminal, wait until it is ready, do one
/// thing to the terminal, and print how the command ended
///
/// written in python because the standard library's `pty.fork` is the safe way
/// to give a process a controlling terminal, which is what makes it the
/// foreground of one. `ready` is a file to wait for, or empty to wait for the
/// command's first output
const ON_A_TERMINAL: &str = r#"import os, pty, select, signal, sys, time

action, ready, command = sys.argv[1], sys.argv[2], sys.argv[3:]
pid, terminal = pty.fork()
if pid == 0:
    os.execv(command[0], command)


def read_some(wait):
    if terminal is not None and select.select([terminal], [], [], wait)[0]:
        try:
            return os.read(terminal, 4096)
        except OSError:
            return b""
    time.sleep(wait)
    return b""


deadline = time.monotonic() + 30
said = b""
while not (os.path.exists(ready) if ready else said):
    if time.monotonic() > deadline:
        print("never ready", said)
        os.kill(pid, signal.SIGKILL)
        sys.exit(1)
    said += read_some(0.01)

if action == "ctrl-c":
    os.write(terminal, b"\x03")
elif action == "hang-up":
    os.close(terminal)
    terminal = None

while time.monotonic() < deadline:
    ended, status = os.waitpid(pid, os.WNOHANG)
    if ended:
        print("exit", os.waitstatus_to_exitcode(status))
        sys.exit(0)
    read_some(0.01)
print("still running")
os.kill(pid, signal.SIGKILL)
"#;

/// run `command` under [`ON_A_TERMINAL`] and say what it printed
fn on_a_terminal(action: &str, ready: &std::path::Path, command: &[&std::ffi::OsStr]) -> String {
    let driver = Fixture::new("on_a_terminal", ON_A_TERMINAL);
    let ran = Command::new(&bpd_test::agent::matching_interpreter().executable)
        .arg(driver.path())
        .arg(action)
        .arg(ready)
        .args(command)
        .stdin(Stdio::null())
        .output()
        .expect("the interpreter runs");
    assert!(
        ran.status.success(),
        "{}{}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
    String::from_utf8_lossy(&ran.stdout).trim().to_string()
}

/// `bpd launch` of `fixture`, as what [`on_a_terminal`] takes
fn launching(fixture: &Fixture) -> (std::path::PathBuf, std::path::PathBuf) {
    (
        bpd_test::agent::matching_interpreter().executable.clone(),
        fixture.path(),
    )
}

#[test]
fn ctrl_c_at_a_terminal_ends_a_bpd_with_no_program_to_leave_it_to() {
    // a terminal's Ctrl+C reaches the program as well as bpd, which is why bpd
    // does not send it again. with no program there is nothing else for it to
    // end, and swallowing it left `bpd dap --listen` with no way to interrupt it
    let said = on_a_terminal(
        "ctrl-c",
        std::path::Path::new(""),
        &[
            std::ffi::OsStr::new(BPD),
            std::ffi::OsStr::new("dap"),
            std::ffi::OsStr::new("--listen"),
            std::ffi::OsStr::new("0"),
        ],
    );
    assert_eq!(said, "exit 130");
}

#[test]
fn ctrl_c_at_a_terminal_reaches_the_program_once() {
    const ON_INTERRUPT: &str = r#"import pathlib, signal, sys, time

HERE = pathlib.Path(__file__).parent


def on_interrupt(signum, frame):
    with (HERE / "handled").open("a") as handled:
        handled.write(f"{signum}\n")
    time.sleep(0.5)
    sys.exit(7)


signal.signal(signal.SIGINT, on_interrupt)
(HERE / "ready").write_text("")
while True:
    time.sleep(0.01)
"#;
    let fixture = Fixture::new("on_interrupt", ON_INTERRUPT);
    let (python, program) = launching(&fixture);
    let said = on_a_terminal(
        "ctrl-c",
        &fixture.directory().join("ready"),
        &[
            std::ffi::OsStr::new(BPD),
            std::ffi::OsStr::new("launch"),
            std::ffi::OsStr::new("--python"),
            python.as_os_str(),
            program.as_os_str(),
        ],
    );
    assert_eq!(said, "exit 7");
    assert_eq!(
        std::fs::read_to_string(fixture.directory().join("handled"))
            .expect("the program's handler ran"),
        "2\n",
        "the terminal sent it, and bpd did not send it again"
    );
}

/// a program that writes a line for every `SIGHUP` it handles, and takes long
/// enough over the first for a second one to interrupt it
const ON_HANGUP: &str = r#"import pathlib, signal, sys, time

HERE = pathlib.Path(__file__).parent


def on_hangup(signum, frame):
    with (HERE / "handled").open("a") as handled:
        handled.write(f"{signum}\n")
    time.sleep(0.5)
    sys.exit(7)


signal.signal(signal.SIGHUP, on_hangup)
(HERE / "ready").write_text("")
while True:
    time.sleep(0.01)
"#;

#[test]
fn a_terminal_hanging_up_on_bpd_as_its_session_leader_reaches_the_program_once() {
    // bpd is the terminal's controlling process here, as the first process of a
    // container with a terminal is. a hangup signals it alone, on linux and
    // macos both, so the program has been sent nothing until bpd passes it on
    let fixture = Fixture::new("on_hangup", ON_HANGUP);
    let (python, program) = launching(&fixture);
    let said = on_a_terminal(
        "hang-up",
        &fixture.directory().join("ready"),
        &[
            std::ffi::OsStr::new(BPD),
            std::ffi::OsStr::new("launch"),
            std::ffi::OsStr::new("--python"),
            python.as_os_str(),
            program.as_os_str(),
        ],
    );
    assert_ne!(said, "still running");
    assert_eq!(
        std::fs::read_to_string(fixture.directory().join("handled"))
            .expect("the program's handler ran"),
        "1\n",
        "one hangup, handled once"
    );
}

#[test]
fn a_terminal_hanging_up_on_a_shell_above_bpd_reaches_the_program_once() {
    // the usual shape: a shell is the controlling process and bpd is in its
    // foreground group. the group is sent the hangup when the shell goes, and by
    // then `/dev/tty` no longer says who was in the foreground, so bpd read that
    // as nothing having sent the program anything and interrupted its handler.
    // the `true` stops the shell from `exec`ing bpd in its own place
    let fixture = Fixture::new("on_hangup", ON_HANGUP);
    let (python, program) = launching(&fixture);
    let script = format!(
        "\"{BPD}\" launch --python \"{}\" \"{}\"; true",
        python.display(),
        program.display()
    );
    on_a_terminal(
        "hang-up",
        &fixture.directory().join("ready"),
        &[
            std::ffi::OsStr::new("/bin/sh"),
            std::ffi::OsStr::new("-c"),
            std::ffi::OsStr::new(&script),
        ],
    );

    // what ends here is the shell, and the handler takes its time after it
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        std::fs::read_to_string(fixture.directory().join("handled"))
            .expect("the program's handler ran"),
        "1\n",
        "one hangup, handled once"
    );
}

#[test]
fn a_signal_bpd_was_started_ignoring_stays_ignored_by_it_and_its_program() {
    // `nohup` starts its command with `SIGHUP` ignored and that is inherited
    // across `exec`, but a handled signal goes back to its default there. so a
    // handler in bpd took away what `nohup` is for, and then bpd passed the
    // hangup on to a program with nothing protecting it
    const SAYS_WHAT_IT_INHERITED: &str = r#"import pathlib, signal, time

HERE = pathlib.Path(__file__).parent
(HERE / "ready").write_text(signal.getsignal(signal.SIGHUP).name)
while not (HERE / "go").exists():
    time.sleep(0.01)
"#;
    let fixture = Fixture::new("says_what_it_inherited", SAYS_WHAT_IT_INHERITED);
    let mut bpd = Command::new("nohup")
        .arg(BPD)
        .arg("launch")
        .arg("--python")
        .arg(&bpd_test::agent::matching_interpreter().executable)
        .arg(fixture.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("`nohup` runs");

    let deadline = Instant::now() + PATIENCE;
    while !fixture.directory().join("ready").exists() {
        assert!(
            Instant::now() < deadline,
            "the program never said it was ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // written and then read: the file exists a moment before it has its text
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        std::fs::read_to_string(fixture.directory().join("ready")).expect("the program wrote it"),
        "SIG_IGN"
    );

    let sent = Command::new("kill")
        .args(["-HUP", &bpd.id().to_string()])
        .status()
        .expect("`kill` runs");
    assert!(sent.success());
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        bpd.try_wait().expect("bpd is a real child").is_none(),
        "bpd ended on a signal it was started ignoring"
    );

    std::fs::write(fixture.directory().join("go"), "").expect("the fixture's directory is there");
    let status = bpd.wait().expect("bpd is a real child");
    assert!(status.success(), "{status}");
}

#[test]
fn a_program_that_closed_its_connection_and_runs_on_still_gets_the_signal() {
    // the connection closing is what sends the engine to wait for the process,
    // and a process can run on after that. the engine sat in a blocking `wait`
    // holding the lock every signal goes through, so a `SIGTERM` to bpd never
    // reached the program and nothing could end it
    //
    // the program `exec`s a shell, which closes the connection with the rest of
    // its close-on-exec descriptors and leaves no agent behind. closing them by
    // hand instead leaves the agent's threads watching descriptors the program
    // took away, and one that notices ends the program, as it does whenever it
    // loses its debugger
    const CLOSES_ITS_CONNECTION: &str = r#"import os, pathlib

HERE = pathlib.Path(__file__).parent
ON_TERM = """
trap 'echo 15 > handled; exit 7' TERM
echo $$ > pid
: > ready
while :; do sleep 0.01; done
"""
os.chdir(HERE)
os.execv("/bin/sh", ["sh", "-c", ON_TERM])
"#;
    let fixture = Fixture::new("closes_its_connection", CLOSES_ITS_CONNECTION);
    let mut bpd = Command::new(BPD)
        .arg("launch")
        .arg("--python")
        .arg(&bpd_test::agent::matching_interpreter().executable)
        .arg(fixture.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("the binary was built by the same cargo invocation as this test");

    let deadline = Instant::now() + PATIENCE;
    while !fixture.directory().join("ready").exists() {
        assert!(
            Instant::now() < deadline,
            "the program never said it was ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // long enough for the engine to see the connection close
    std::thread::sleep(Duration::from_millis(500));

    let sent = Command::new("kill")
        .args(["-TERM", &bpd.id().to_string()])
        .status()
        .expect("`kill` runs");
    assert!(sent.success());

    let handled = fixture.directory().join("handled");
    let reached = Instant::now() + Duration::from_secs(10);
    while !handled.exists() && Instant::now() < reached {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !handled.exists() {
        let killed = bpd.kill();
        drop(killed);
        let reaped = bpd.wait();
        drop(reaped);
        // the program outlives bpd, holding this test's stderr open
        let program = std::fs::read_to_string(fixture.directory().join("pid"))
            .expect("the program wrote its pid before it said it was ready");
        let killed = Command::new("kill")
            .args(["-KILL", program.trim()])
            .status();
        drop(killed);
        panic!("the signal never reached the program");
    }
    let status = bpd.wait().expect("bpd is a real child");
    assert_eq!(status.code(), Some(7), "{status}");
}
