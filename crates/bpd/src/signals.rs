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
//!
//! before any program has started there is nothing to pass a signal on to, and
//! bpd exits with `128` and its number, the way it would have ended without a
//! handler. a program held at a stop handles a signal when it is let go — a
//! client's `terminate` is what lets it go
//!
//! on windows, Ctrl+C and Ctrl+Break reach every process attached to the
//! console, the program among them, so there is nothing to pass on: bpd leaves
//! them to the program and carries on until it has ended, rather than ending
//! first and leaving it running

pub(crate) use platform::install;

#[cfg(unix)]
mod platform {
    use bpd_engine::{Ending, Passed};
    use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
    use signal_hook::iterator::Signals;

    /// handle the signals that ask bpd to end, from now until it exits
    pub(crate) fn install() -> std::io::Result<()> {
        let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP, SIGQUIT])?;
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
                    if ending != Ending::Terminate && in_terminal_foreground() {
                        continue;
                    }
                    if bpd_engine::pass_on(ending) == Passed::NothingStarted {
                        #[expect(
                            clippy::exit,
                            reason = "the signal's default, which this handler \
                                      replaced: with no program, bpd ends on it \
                                      as it would have without one"
                        )]
                        std::process::exit(128 + signal);
                    }
                }
            })?;
        Ok(())
    }

    /// whether bpd is in the foreground process group of its controlling
    /// terminal, the group a terminal sends Ctrl+C, Ctrl+\ and a hangup to
    ///
    /// a process with no controlling terminal is in nobody's foreground, and
    /// opening `/dev/tty` is how it finds out it has none. which process sent
    /// a signal does not settle it: macos names the process that wrote the
    /// Ctrl+C to the terminal as its sender, just as it names a `kill`'s
    fn in_terminal_foreground() -> bool {
        use rustix::fs::{Mode, OFlags, open};
        let Ok(terminal) = open("/dev/tty", OFlags::RDONLY | OFlags::NOCTTY, Mode::empty()) else {
            return false;
        };
        rustix::termios::tcgetpgrp(&terminal)
            .is_ok_and(|foreground| foreground == rustix::process::getpgrp())
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
