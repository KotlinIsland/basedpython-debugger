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
