//! running a program on a **real terminal**, for the comparison a pipe cannot
//! make
//!
//! `crates/bpd/tests/launch_parity.rs` runs everything twice and compares, and
//! until this existed it ran both halves through pipes — `Command::output`
//! gives a pipe to whatever it starts. so anything that differs only between a
//! terminal and a pipe was invisible to it, and `isatty()` is exactly that:
//! program-observable, and what `rich`, `click`, `pytest` and `colorama` check
//! to decide colour, progress bars and formatting. so is the buffering that
//! follows from it — cpython line-buffers a terminal and block-buffers a pipe
//!
//! what is opened here is a pseudo-terminal, which is a terminal as far as
//! every question a program can ask about one goes. it is deliberately **not**
//! made the child's controlling terminal: that needs `setsid` between the fork
//! and the exec, `isatty` does not depend on it, and this crate stays free of
//! `unsafe`

use std::io::Read as _;
use std::process::Command;

use rustix::fs::{Mode, OFlags};
use rustix::pty::OpenptFlags;

/// what a program did on a terminal
///
/// **one** stream, and that is the point rather than a simplification: a
/// terminal has one, so a program's stdout and stderr arrive on it interleaved
/// in the order they were written, and there is nothing left that could tell
/// them apart afterwards
#[derive(Debug, Clone)]
pub struct OnATerminal {
    /// the process exit code, or `None` when a signal ended it
    pub exit_code: Option<i32>,
    /// whether the process exited successfully
    pub success: bool,
    /// everything the program wrote to the terminal, in the order it arrived
    ///
    /// with the line discipline's own translation still in it — a terminal
    /// turns `\n` into `\r\n` on the way out. it is left alone because both
    /// halves of a comparison go through the same discipline, and stripping it
    /// would be the harness editing what the program is being compared on
    pub written: String,
}

/// run one command with a fresh pseudo-terminal for all three of its standard
/// streams
///
/// the terminal is read **while the program runs** rather than after it exits.
/// a terminal's buffer is a few kilobytes, and a program that writes more than
/// that would block for ever on a reader that only started once it had finished
///
/// the command is taken **by value** on purpose, and that is load bearing —
/// see the drop below
///
/// # panics
///
/// if a terminal cannot be opened, if the command cannot be spawned, or if the
/// program writes bytes that are not utf-8. every one of them means the
/// comparison this was called for never happened, and a test that carried on
/// would be asserting against nothing
pub fn through_a_terminal(mut command: Command) -> OnATerminal {
    let controller = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)
        .expect("a pseudo-terminal can be opened");
    rustix::pty::grantpt(&controller).expect("the terminal device can be granted");
    rustix::pty::unlockpt(&controller).expect("the terminal device can be unlocked");
    let device =
        rustix::pty::ptsname(&controller, Vec::new()).expect("the terminal device is named");

    // `NOCTTY` on every one of them. this process may have no controlling
    // terminal — a test runner started from a service does not — and opening a
    // terminal device without it would quietly acquire one for the whole test
    // binary
    let opened = || {
        let device = rustix::fs::open(&device, OFlags::RDWR | OFlags::NOCTTY, Mode::empty())
            .expect("the terminal device this process just created can be opened");
        std::fs::File::from(device)
    };

    let program = command.get_program().to_owned();
    command.stdin(opened()).stdout(opened()).stderr(opened());
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("could not run {}: {error}", program.display()));

    // `spawn` **duplicates** the configured handles into the child rather than
    // giving them away, so until the command is dropped this process is still
    // a writer on the terminal and the read below would never see a hangup —
    // it hangs for exactly as long as the test is allowed to run
    drop(command);

    let controller = std::fs::File::from(controller);
    let reading = std::thread::Builder::new()
        .name("bpd-test-terminal".to_string())
        .spawn(move || read_until_hangup(controller))
        .expect("a reader thread can be started");

    let status = child.wait().expect("the program can be waited for");
    let written = reading.join().expect("nothing panics reading a terminal");

    OnATerminal {
        exit_code: status.code(),
        success: status.success(),
        written: String::from_utf8(written).expect("the program writes utf8"),
    }
}

/// everything written to the terminal, until the last thing holding the device
/// open lets go
///
/// the two kernels disagree about what that looks like. macos returns end of
/// file; linux returns `EIO`, which is the same event with a name that reads
/// like a failure. both are the hangup and neither is an error to report
fn read_until_hangup(mut controller: std::fs::File) -> Vec<u8> {
    let mut written = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match controller.read(&mut chunk) {
            Ok(0) => return written,
            Ok(read) => written.extend_from_slice(&chunk[..read]),
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) => {
                return written;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => panic!("the terminal could not be read: {error}"),
        }
    }
}
