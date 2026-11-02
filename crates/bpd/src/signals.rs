//! the signals that ask bpd to end are passed on to the program it debugs
//!
//! a debugged program is bpd's child, and nothing else names it: `by run` starts
//! `bpd dap --listen` in the program's place, an editor's Stop signals the
//! process it started, and `docker stop` signals the first process of its
//! container. left to the defaults, a `SIGTERM` ended bpd and left the program
//! running with no parent — measured, through `bpd dap --listen`: bpd ended by
//! the signal, the program alive, its `SIGTERM` handler never run
//!
//! so on unix bpd handles the four signals that ask a process to end and passes
//! each on to every program it started that is still running, the contract `by
//! run` keeps for its own program:
//!
//! - **`SIGTERM` is always passed on, and at most once to a program** — a
//!   client's `terminate` asks with the same signal, and whichever came first
//!   is the one the program gets. see `bpd_engine::ending`
//! - **`SIGINT`, `SIGHUP` and `SIGQUIT` only when bpd is outside its
//!   terminal's foreground process group.** they are what a terminal sends —
//!   Ctrl+C, a hangup, Ctrl+\ — to every process of that group, and the
//!   program is in bpd's. sending one again would interrupt whatever the
//!   program does about the first. under an editor's pipes, as a container's
//!   first process or in a shell's background nothing has sent the program
//!   anything, and all four are passed on
//! - **to the program only**, and then bpd carries on as it does when a program
//!   ends: it reports how, and exits as the program did
//! - **a signal bpd was started ignoring is left alone.** `nohup`, and a shell
//!   starting a background job with no job control, start their command with
//!   the signal ignored, and that is inherited across `exec` where a handled
//!   signal goes back to its default. handling it would take the protection
//!   away from the program, and then send it the signal
//!
//! with no program running there is nothing to pass a signal on to, whoever
//! sent it, and bpd exits with `128` and its number, the way it would have ended
//! without a handler. after a program has ended that waits a moment, for what
//! bpd is about to say about how it ended. a program held at a stop handles a
//! signal when it is let go — a client's `terminate` is what lets it go
//!
//! on windows, Ctrl+C and Ctrl+Break reach every process attached to the
//! console, the program among them, so there is nothing to pass on: bpd leaves
//! them to the program and carries on until it has ended, rather than ending
//! first and leaving it running

pub(crate) use platform::install;

#[cfg(unix)]
mod platform {
    use std::io;
    use std::time::Duration;

    use bpd_engine::{Ending, Passed};
    use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
    use signal_hook::iterator::Signals;

    /// how long bpd lets its report of a program's end get out, when a signal
    /// comes after that end
    const LAST_WORD: Duration = Duration::from_secs(1);

    /// handle the signals that ask bpd to end, from now until it exits
    pub(crate) fn install() -> io::Result<()> {
        let mut handled = Vec::new();
        for signal in [SIGTERM, SIGINT, SIGHUP, SIGQUIT] {
            if !started_ignoring(signal)? {
                handled.push(signal);
            }
        }
        let mut signals = Signals::new(handled)?;
        let had_a_terminal = terminal_foreground().is_some();
        std::thread::Builder::new()
            .name("bpd-signals".to_string())
            .spawn(move || {
                for signal in signals.forever() {
                    let ending = match signal {
                        SIGTERM => Ending::Terminate,
                        SIGINT => Ending::Interrupt,
                        SIGHUP => Ending::Hangup,
                        SIGQUIT => Ending::Quit,
                        other => unreachable!("only the four were registered, and {other} came"),
                    };
                    let send =
                        ending == Ending::Terminate || !terminal_sent(ending, had_a_terminal);
                    match bpd_engine::pass_on(ending, send) {
                        Passed::Running => {}
                        Passed::NothingStarted => end_on(signal),
                        Passed::Over => {
                            std::thread::sleep(LAST_WORD);
                            end_on(signal);
                        }
                    }
                }
            })?;
        Ok(())
    }

    /// the signal's default, which the handler replaced: with no program, bpd
    /// ends on it as it would have without one
    fn end_on(signal: i32) -> ! {
        #[expect(
            clippy::exit,
            reason = "bpd is ending on a signal, on a thread that owns nothing \
                      the main thread would have to give back"
        )]
        std::process::exit(128 + signal)
    }

    /// whether this process was started with `signal` ignored
    #[expect(
        unsafe_code,
        reason = "`sigaction` is the only way to read a signal's disposition, and \
                  neither `signal-hook` nor `rustix` has a safe way to ask"
    )]
    fn started_ignoring(signal: i32) -> io::Result<bool> {
        // SAFETY: a zeroed `sigaction` is a valid value of that plain struct,
        // and with a null new action the call only writes through the pointer
        // it is given, which is to a live local
        let (read, current) = unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            let read = libc::sigaction(signal, std::ptr::null(), &raw mut current);
            (read, current)
        };
        if read != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(current.sa_sigaction == libc::SIG_IGN)
    }

    /// whether the terminal has already sent the program this signal, because
    /// bpd and the program are in the group it sends to
    ///
    /// which process sent a signal does not settle it: macos names the process
    /// that wrote the Ctrl+C to the terminal as its sender, just as it names a
    /// `kill`'s
    fn terminal_sent(ending: Ending, had_a_terminal: bool) -> bool {
        let hangup = ending == Ending::Hangup;
        // a hung up terminal signals its controlling process alone, on linux and
        // macos both, and its foreground group only hears once that process has
        // gone. so when that process is bpd nothing else has been sent anything.
        // asked first, because linux has already taken the terminal from the
        // whole session by the time the signal arrives, and `/dev/tty` says
        // nothing more
        let controlling =
            rustix::process::getsid(None).is_ok_and(|session| session == rustix::process::getpid());
        if hangup && controlling {
            return false;
        }
        let Some(foreground) = terminal_foreground() else {
            // a hangup takes the terminal with it, so it can no longer say who
            // was in its foreground. what is sent then, after the controlling
            // process has gone, goes to the group, the program's as well as bpd's
            return hangup && had_a_terminal;
        };
        foreground == rustix::process::getpgrp()
    }

    /// the foreground process group of bpd's controlling terminal, the group a
    /// terminal sends Ctrl+C, Ctrl+\ and a hangup to
    ///
    /// a process with no controlling terminal has none, and opening `/dev/tty`
    /// is how it finds out
    fn terminal_foreground() -> Option<rustix::process::Pid> {
        use rustix::fs::{Mode, OFlags, open};
        let terminal = open("/dev/tty", OFlags::RDONLY | OFlags::NOCTTY, Mode::empty()).ok()?;
        rustix::termios::tcgetpgrp(&terminal).ok()
    }
}

#[cfg(windows)]
mod platform {
    /// leave Ctrl+C and Ctrl+Break to the program, which the console sent them
    /// to as well
    pub(crate) fn install() -> std::io::Result<()> {
        ctrlc::set_handler(|| {}).map_err(std::io::Error::other)
    }
}
