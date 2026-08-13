//! the `launch.json` entry, as the client sends it
//!
//! every field here is read. a field that were parsed and ignored would be a
//! setting a user could put in their `launch.json`, see accepted, and never get
//! — which is the placeholder ban applied to configuration

use std::path::PathBuf;

use bpd_core::{Detail, Threads};

/// what a `launch` request carries
/// `Serialize` as well as `Deserialize`, and for one reason: a debugged fork is
/// a second session of the same program, and what tells a client how to start it
/// is a configuration this adapter writes. writing it out of the one it was
/// given is what makes the child's session carry the same settings as its
/// parent's — the same `stopTheWorld`, the same value bounds, and the same
/// `debugChildren`, so that a fork of a fork is debugged too
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "it is a wire schema: every field is one a client writes in its \
              `launch.json`, and each is an independent yes-or-no about the \
              session. folding them into a state machine would make the \
              configuration a client writes and the one this parses two \
              different shapes"
)]
pub struct Configuration {
    /// the program to run
    pub program: PathBuf,

    /// arguments for the program, exactly as it would receive them
    #[serde(default)]
    pub args: Vec<String>,

    /// the interpreter to run under, resolved on `PATH` like any other command
    #[serde(default = "Configuration::python")]
    pub python: PathBuf,

    /// stay stopped before the program's first statement
    ///
    /// bpd holds every program there — it is how a breakpoint gets bound before
    /// anything runs — so this decides whether the client is *told* about it,
    /// not whether it happens
    #[serde(default)]
    pub stop_on_entry: bool,

    /// hold every thread that can be held for the duration of each stop
    ///
    /// off by default, which is bpd's own default: a stop holds one thread and
    /// the rest of the program keeps running. with it on, every stop is
    /// reported to the client as a whole-program one — and only when it really
    /// was, because a thread parked in a C call cannot be held and is named
    /// instead
    #[serde(default)]
    pub stop_the_world: bool,

    /// how much of a value to read, and what the debugger may run to read it
    ///
    /// DAP has nowhere to carry these, and they are the difference between an
    /// object graph that opens and one that reports it ran out of budget. the
    /// omission always says which bound bit, so this is the setting to change
    /// when it does
    #[serde(default)]
    pub variables: Detail,

    /// how far apart the two samples a thread census compares are taken
    ///
    /// what [`bpd_core::Progress::Still`] means: in the same place, this far
    /// apart. the default is [`Threads::SETTLE`]
    #[serde(default = "Configuration::settle_ms")]
    pub thread_settle_ms: u32,

    /// debug a child the program **forks**
    ///
    /// off by default, and deliberately not debugpy's default of on: a debugged
    /// child **stops**, and a setting that produced stopped processes without
    /// being asked for would be a debugger that hangs programs by default
    ///
    /// it needs two things of the client, and both are refused up front rather
    /// than discovered when a child is already held: the client has to support
    /// the `startDebugging` reverse request, and this adapter has to be
    /// reachable by a second connection — `bpd dap --listen`. a fork on a
    /// platform that has none is refused by the agent
    ///
    /// it covers both ways a child comes into being. a **forked** child is held
    /// at the line that forked; a child that was **exec'd** — `subprocess`,
    /// `multiprocessing` with `spawn`, django's `runserver` reloader — is a
    /// fresh interpreter and is held at its own startup, before its program has
    /// been compiled
    ///
    /// it is also the one setting a program can notice: reaching an `exec`'d
    /// child means `PYTHONPATH`, appended. see
    /// [child processes](../../../docs/development/subprocesses.md)
    #[serde(default)]
    pub debug_children: bool,

    /// where the program runs, and therefore what its standard streams are
    ///
    /// the debug console by default, which is what this adapter has always
    /// given a debuggee: pipes, forwarded as `output` events, and `/dev/null`
    /// for stdin. either terminal is the `runInTerminal` reverse request — the
    /// client starts the program in a terminal it owns, so the program has a
    /// **real** one and `isatty()` is true
    ///
    /// it is refused at `launch` unless the client said it supports that
    /// request in `initialize`, because a client that cannot be asked would
    /// leave the session waiting for an agent nothing was going to start
    #[serde(default)]
    pub console: Console,

    /// run the program without debugging it
    ///
    /// refused rather than ignored. bpd has no path that launches a program
    /// without its agent, and a client that asked for one and got a debugged
    /// run would be measuring something it did not ask for
    #[serde(default)]
    pub no_debug: bool,
}

/// where a debuggee's standard streams come from
///
/// the names are the ones a `launch.json` already has for this, because a
/// person writing one has met them in every other python debugger. what the two
/// terminals differ in is the `kind` of the `runInTerminal` request, and that is
/// the client's own decision about where to put a terminal — bpd has nothing to
/// say about it beyond passing the word through
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Console {
    /// the client's debug console, which is not a terminal
    ///
    /// the program's stdout and stderr are pipes this adapter reads and
    /// forwards as `output` events, and its stdin is `/dev/null`. that is what
    /// `python program.py | client < /dev/null` gives, which is the bare run a
    /// debug console is the same shape as
    #[default]
    InternalConsole,

    /// a terminal inside the client
    IntegratedTerminal,

    /// a terminal the client opens outside itself
    ExternalTerminal,
}

impl Console {
    /// the `kind` of the `runInTerminal` request this asks for, if it asks
    ///
    /// `None` is the debug console, which is not a terminal and asks for
    /// nothing
    pub const fn kind(self) -> Option<&'static str> {
        match self {
            Self::InternalConsole => None,
            Self::IntegratedTerminal => Some("integrated"),
            Self::ExternalTerminal => Some("external"),
        }
    }
}

impl Configuration {
    /// the interpreter used when the configuration does not name one
    fn python() -> PathBuf {
        PathBuf::from("python3")
    }

    /// the settle interval used when the configuration does not name one
    fn settle_ms() -> u32 {
        u32::try_from(Threads::SETTLE.as_millis())
            .expect("the core's settle interval is milliseconds that fit a u32")
    }

    /// the settle interval this configuration asks for
    pub const fn settle(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.thread_settle_ms as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Configuration {
        serde_json::from_str(text).expect("the configuration is valid")
    }

    #[test]
    fn a_configuration_that_names_only_the_program_gets_the_documented_defaults() {
        let configuration = parse(r#"{"program": "/tmp/app.py"}"#);

        assert_eq!(configuration.program, PathBuf::from("/tmp/app.py"));
        assert_eq!(configuration.python, PathBuf::from("python3"));
        assert!(!configuration.stop_on_entry);
        assert!(!configuration.stop_the_world);
        assert!(!configuration.no_debug);
        // the debug console, which is what this adapter has always given a
        // debuggee. a default of either terminal would change what every
        // existing session's program *is*
        assert_eq!(configuration.console, Console::InternalConsole);
        assert_eq!(configuration.console.kind(), None);
        assert_eq!(configuration.variables, Detail::default());
        assert_eq!(configuration.settle(), Threads::SETTLE);
    }

    #[test]
    fn every_bound_on_a_value_can_be_set_from_the_launch_configuration() {
        // the omission a value carries says which bound bit and asks for a
        // larger one. a client with no way to give it a larger one would be
        // reading an instruction it cannot follow
        let configuration = parse(
            r#"{"program": "a.py",
                "variables": {"depth": 9, "children": 5, "text": 12,
                              "budget": 64, "attributes": false, "repr": true}}"#,
        );

        assert_eq!(
            configuration.variables,
            Detail {
                depth: 9,
                children: 5,
                text: 12,
                budget: 64,
                attributes: false,
                repr: true,
            }
        );
    }

    #[test]
    fn a_misspelled_bound_is_refused_rather_than_quietly_taking_its_default() {
        // the omission a value carries names a bound and asks for a larger one.
        // a `launch.json` that spells it wrong and is accepted anyway would go
        // on being cut in the same place, with the same advice, for ever
        let error = serde_json::from_str::<Configuration>(
            r#"{"program": "a.py", "variables": {"dept": 9}}"#,
        )
        .expect_err("`dept` is not a bound on a value");
        assert!(error.to_string().contains("dept"), "said {error}");
    }

    #[test]
    fn each_terminal_asks_for_the_kind_of_the_reverse_request_it_is() {
        // the word goes through to the client, which is what decides where a
        // terminal goes. a spelling the client does not know would be a
        // configuration accepted here and refused there
        assert_eq!(
            parse(r#"{"program": "a.py", "console": "integratedTerminal"}"#)
                .console
                .kind(),
            Some("integrated")
        );
        assert_eq!(
            parse(r#"{"program": "a.py", "console": "externalTerminal"}"#)
                .console
                .kind(),
            Some("external")
        );

        // and a spelling that is not one of the three is refused rather than
        // taking the default, which would be a program running somewhere the
        // user did not ask for
        let error = serde_json::from_str::<Configuration>(
            r#"{"program": "a.py", "console": "integrated"}"#,
        )
        .expect_err("`integrated` is not one of the three");
        assert!(
            error.to_string().contains("integratedTerminal"),
            "said {error}"
        );
    }

    #[test]
    fn a_configuration_with_no_program_is_refused_rather_than_defaulted() {
        let error = serde_json::from_str::<Configuration>(r#"{"args": []}"#)
            .expect_err("there is nothing to run");
        assert!(error.to_string().contains("program"), "said {error}");
    }
}
