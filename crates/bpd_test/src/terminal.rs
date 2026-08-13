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

use std::ffi::{CStr, CString};
use std::io::{Read as _, Write as _};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
pub fn through_a_terminal(command: Command) -> OnATerminal {
    let (controller, device) = open_a_terminal();
    let (mut child, controller) = started_on(command, controller, &device);
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

/// a terminal a program can be started on and then **talked to**
///
/// [`through_a_terminal`] runs one command and hands back everything it wrote,
/// which is what a comparison of two runs needs. this is the other half: a
/// terminal that outlives the command, so that a test can read what the program
/// has said so far and type a line back at it while it is still running
///
/// what needs that is `runInTerminal`. the client is asked to start the program
/// and answers when it has, the program's output goes to the terminal rather
/// than to the debugger, and the thing under test is that a debuggee on one
/// really has it — `isatty()`, and a line the program reads with `input()`
#[derive(Debug)]
pub struct Terminal {
    /// the controlling end, for typing at the program
    controller: std::fs::File,
    /// the device the program's own three streams are opened on
    device: CString,
    /// everything the program has written, as it arrives
    written: Arc<Mutex<Vec<u8>>>,
}

impl Terminal {
    /// open one, with a thread reading it from the moment it exists
    ///
    /// read from the start rather than at the end, for the reason
    /// [`through_a_terminal`] reads while the program runs: a terminal's buffer
    /// is a few kilobytes and a program that writes more than that would block
    ///
    /// # panics
    ///
    /// if a terminal cannot be opened, or if a reader thread cannot be started
    pub fn open() -> Self {
        let (controller, device) = open_a_terminal();
        let controller = std::fs::File::from(controller);
        let reading = controller
            .try_clone()
            .expect("a file this process just opened can be cloned");
        let written = Arc::new(Mutex::new(Vec::new()));

        std::thread::Builder::new()
            .name("bpd-test-terminal".to_string())
            .spawn({
                let written = Arc::clone(&written);
                move || collect_until_hangup(reading, &written)
            })
            .expect("a reader thread can be started");

        Self {
            controller,
            device,
            written,
        }
    }

    /// start a command with this terminal as all three of its standard streams
    ///
    /// # panics
    ///
    /// if the command cannot be spawned
    pub fn run(&self, command: Command) -> std::process::Child {
        let (child, controller) = started_on(command, self.controller_fd(), &self.device);
        // the second handle on the controlling end is not needed here: this
        // terminal already has one, and a reader started at `open`
        drop(controller);
        child
    }

    /// type a line at the program, exactly as a person at the terminal would
    ///
    /// # panics
    ///
    /// if the terminal cannot be written to
    pub fn type_line(&self, line: &str) {
        let mut writing = &self.controller;
        writeln!(writing, "{line}").expect("the terminal can be written to");
        writing.flush().expect("the terminal can be written to");
    }

    /// everything the program has written so far
    ///
    /// with the line discipline's own translation still in it, for the reason
    /// [`OnATerminal::written`] keeps it
    ///
    /// # panics
    ///
    /// if the program wrote bytes that are not utf-8
    pub fn written(&self) -> String {
        let written = self
            .written
            .lock()
            .expect("nothing panics holding the text");
        String::from_utf8(written.clone()).expect("the program writes utf8")
    }

    /// wait until the program has written something, and answer with all of it
    ///
    /// # panics
    ///
    /// if `patience` runs out first, quoting everything that did arrive. a test
    /// that carried on would be asserting against a program that had not
    /// reached the line it is about
    pub fn wait_for(&self, wanted: &str, patience: Duration) -> String {
        let deadline = Instant::now() + patience;
        loop {
            let written = self.written();
            if written.contains(wanted) {
                return written;
            }
            assert!(
                Instant::now() < deadline,
                "the program never wrote `{wanted}` on its terminal. what it did \
                 write:\n{written}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// a handle on the controlling end for a fresh start
    fn controller_fd(&self) -> rustix::fd::OwnedFd {
        self.controller
            .try_clone()
            .expect("a file this process opened can be cloned")
            .into()
    }
}

/// open a pseudo-terminal, and name the device its program's streams go on
fn open_a_terminal() -> (rustix::fd::OwnedFd, CString) {
    let controller = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)
        .expect("a pseudo-terminal can be opened");
    rustix::pty::grantpt(&controller).expect("the terminal device can be granted");
    rustix::pty::unlockpt(&controller).expect("the terminal device can be unlocked");
    let device =
        rustix::pty::ptsname(&controller, Vec::new()).expect("the terminal device is named");
    (controller, device)
}

/// spawn a command with a terminal as all three of its standard streams
///
/// the command is taken **by value** on purpose. `spawn` *duplicates* the
/// configured handles into the child rather than giving them away, so until the
/// command is dropped this process is still a writer on the terminal and a read
/// of it would never see a hangup — it would hang for exactly as long as the
/// test is allowed to run
fn started_on(
    mut command: Command,
    controller: rustix::fd::OwnedFd,
    device: &CStr,
) -> (std::process::Child, std::fs::File) {
    // `NOCTTY` on every one of them. this process may have no controlling
    // terminal — a test runner started from a service does not — and opening a
    // terminal device without it would quietly acquire one for the whole test
    // binary
    let opened = || {
        let device = rustix::fs::open(device, OFlags::RDWR | OFlags::NOCTTY, Mode::empty())
            .expect("the terminal device this process just created can be opened");
        std::fs::File::from(device)
    };

    let program = command.get_program().to_owned();
    command.stdin(opened()).stdout(opened()).stderr(opened());
    let child = command
        .spawn()
        .unwrap_or_else(|error| panic!("could not run {}: {error}", program.display()));
    drop(command);

    (child, std::fs::File::from(controller))
}

/// everything written to the terminal, until the last thing holding the device
/// open lets go
///
/// the two kernels disagree about what that looks like. macos returns end of
/// file; linux returns `EIO`, which is the same event with a name that reads
/// like a failure. both are the hangup and neither is an error to report
fn read_until_hangup(controller: std::fs::File) -> Vec<u8> {
    let written = Arc::new(Mutex::new(Vec::new()));
    collect_until_hangup(controller, &written);
    let taken = written.lock().expect("nothing panics holding the text");
    taken.clone()
}

/// the same read, into a buffer somebody else can look at while it fills
fn collect_until_hangup(mut controller: std::fs::File, into: &Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0_u8; 4096];
    loop {
        let read = match controller.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => read,
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) => {
                return;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("the terminal could not be read: {error}"),
        };
        into.lock()
            .expect("nothing panics holding the text")
            .extend_from_slice(&chunk[..read]);
    }
}
