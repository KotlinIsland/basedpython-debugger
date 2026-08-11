//! a child process the debuggee started, and what `bpd` can tell about it
//!
//! the debuggee is one process, and a program that starts another one has moved
//! the work `bpd` was pointed at somewhere `bpd` is not. django's `runserver`
//! is the case that made this worth reporting: the reloader starts a child and
//! waits on its exit code, so the process holding the agent never renders a
//! template
//!
//! nothing here debugs the child. what it does is **say the child is there**,
//! rather than leaving somebody looking at a supervisor and wondering why a
//! breakpoint in a request handler never fires
//!
//! ## what is knowable, and what is not
//!
//! the whole of what the debuggee can see is the argument vector it is about to
//! hand the operating system. that is enough to recognise the interpreter it is
//! already running, and it is not enough to know what `/bin/sh -c "…"` will do
//! — so [`Verdict`] has three values rather than a boolean, and the third one
//! is `bpd` saying it cannot tell
//!
//! guessing would be the worse failure here. a debugger that announced a python
//! child on every `git` invocation would be a debugger nobody reads the output
//! of, and one that stayed silent about `uv run python` would be the silence
//! this exists to remove

use std::fmt;

/// a child process the debuggee started while it ran
///
/// only the ones that could be python. a child that is plainly something else
/// is not reported at all — see [`Verdict`]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Spawn {
    /// the audit event that saw it, which is what the program did
    ///
    /// carried as cpython's own event name rather than as a category of
    /// `bpd`'s, because it is the thing a person can look up. one child raises
    /// exactly one of them
    pub event: String,

    /// the file the child will execute, when the program named one
    ///
    /// absent for [`Made::Fork`], where the child runs this very process
    pub executable: Option<String>,

    /// the argument vector, as the program gave it
    ///
    /// empty for [`Made::Fork`], and for an event that carries no vector
    pub arguments: Vec<String>,

    /// what `bpd` can tell about the child being python, and on what evidence
    pub verdict: Verdict,
}

impl Spawn {
    /// the command, as one line, or `None` when the event carried no vector
    ///
    /// quoting is deliberately not attempted. this is a report of what the
    /// program asked for, and a line that could be pasted into a shell would be
    /// a different thing from the vector the operating system was handed
    pub fn command(&self) -> Option<String> {
        if self.arguments.is_empty() {
            return None;
        }
        Some(self.arguments.join(" "))
    }
}

/// what `bpd` can tell about a child being python
///
/// deliberately closed, and deliberately without a "no". a child that is
/// plainly not python is not reported at all, so there is no variant for one —
/// a value of this type is always a reason the child **was** worth mentioning
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// the child will run the interpreter this debuggee is running
    ///
    /// the executable is the debuggee's own `sys.executable`. this is as
    /// certain as it gets from an argument vector
    ThisInterpreter,

    /// the child is this process, copied — a `fork` with no `exec` after it
    ///
    /// it is python by construction, and it is more than that: it holds the
    /// agent's `sys.monitoring` state and the debugger's own control connection,
    /// neither of which `bpd` gave it
    ThisProcess,

    /// the child will run a python interpreter that is not this one
    ///
    /// the evidence is the **file name**, which is not proof: a file called
    /// `python` can be a wrapper script. it is reported as what it is
    AnotherInterpreter {
        /// the file name the verdict was read from
        named: String,
    },

    /// a python interpreter is named in the command, and `bpd` cannot tell
    /// whether the child will run it
    ///
    /// `env python …`, `uv run python …`, `sh -c "python …"`. the word is there
    /// and what the child does with it is the child's business — `bpd` sees the
    /// vector and no further
    Perhaps {
        /// the word the verdict was read from
        named: String,
    },
}

impl Verdict {
    /// whether the child is certainly python
    ///
    /// the two certain cases are the interpreter identified by path and the
    /// fork. a name and a word in a command line are evidence, not proof
    pub const fn certain(&self) -> bool {
        matches!(self, Self::ThisInterpreter | Self::ThisProcess)
    }
}

/// the sentence every front end says about a spawn
///
/// written once, here, because the CLI, the DAP adapter and the MCP server all
/// have to say it and three wordings of the same fact is three descriptions of
/// the same program
impl fmt::Display for Spawn {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.verdict {
            Verdict::ThisProcess => write!(
                out,
                "the program forked. the child is a copy of this process — it \
                 holds the agent's monitoring state and this session's control \
                 connection, and bpd is not debugging it as a session of its own"
            ),
            Verdict::ThisInterpreter => write!(
                out,
                "the program started a python child, running the interpreter \
                 this program is running, and bpd is not debugging it"
            ),
            Verdict::AnotherInterpreter { named } => write!(
                out,
                "the program started a child whose program is called `{named}`, \
                 which is a python interpreter's name and is not the one this \
                 program is running. bpd is not debugging it"
            ),
            Verdict::Perhaps { named } => write!(
                out,
                "the program started a child whose command names `{named}`, and \
                 bpd cannot tell from the command whether the child will run \
                 python. if it does, bpd is not debugging it"
            ),
        }?;

        write!(out, " ({}", self.event)?;
        if let Some(command) = self.command() {
            write!(out, ": {command}")?;
        }
        write!(out, ")")
    }
}

/// a way of starting a child that this interpreter raises no audit event for
///
/// `bpd` reports the children it can see, and the failure that would make that
/// worthless is **silence read as "there was no child"**. so where an
/// interpreter leaves `bpd` blind, `bpd` says which children it will not be
/// able to see, at the moment it becomes possible that there are some
///
/// deliberately closed and deliberately specific. "this might not catch
/// everything" is a disclaimer; naming the start method, the interpreter and
/// the release that fixes it is something a person can act on
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "blind_to", rename_all = "snake_case")]
pub enum Blindspot {
    /// `multiprocessing`'s `spawn` and `forkserver` start methods, below 3.14
    ///
    /// they reach a child through `multiprocessing.util.spawnv_passfds`, which
    /// calls `_posixsubprocess.fork_exec` — and that only became an audit event
    /// in 3.14. below it, nothing whatever is raised: measured by recording
    /// every event of every name raised while one starts, on 3.13 and on 3.14
    MultiprocessingSpawn {
        /// the interpreter's `major.minor`, as it reported itself
        interpreter: String,
    },
}

impl fmt::Display for Blindspot {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self::MultiprocessingSpawn { interpreter } = self;
        write!(
            out,
            "this program imported `multiprocessing`, and python {interpreter} \
             raises no event at all when it starts a child with the `spawn` or \
             `forkserver` start method — so bpd cannot see one, and silence \
             here does not mean there was none. `_posixsubprocess.fork_exec` \
             became an audit event in 3.14, where this is visible. the `fork` \
             start method is visible on every version, and so is `subprocess`"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(verdict: Verdict) -> Spawn {
        Spawn {
            event: "_posixsubprocess.fork_exec".to_string(),
            executable: Some("/usr/bin/python3.14".to_string()),
            arguments: vec!["/usr/bin/python3.14".to_string(), "worker.py".to_string()],
            verdict,
        }
    }

    #[test]
    fn every_verdict_says_what_it_knows_and_names_the_command() {
        for verdict in [
            Verdict::ThisInterpreter,
            Verdict::AnotherInterpreter {
                named: "python3.13".to_string(),
            },
            Verdict::Perhaps {
                named: "python".to_string(),
            },
        ] {
            let said = started(verdict).to_string();
            assert!(
                said.contains("worker.py"),
                "a report that does not say which child it is about is one \
                 nobody can act on, and it said {said}"
            );
            assert!(
                said.contains("_posixsubprocess.fork_exec"),
                "the event is what a person looks up, and it said {said}"
            );
        }
    }

    #[test]
    fn an_uncertain_verdict_says_so_rather_than_claiming_a_python_child() {
        let perhaps = started(Verdict::Perhaps {
            named: "python".to_string(),
        });
        let said = perhaps.to_string();

        assert!(!perhaps.verdict.certain());
        assert!(
            said.contains("cannot tell"),
            "a guess presented as a finding is the failure this whole type \
             exists to prevent, and it said {said}"
        );
    }

    #[test]
    fn a_fork_is_reported_as_the_shared_process_it_is() {
        let forked = Spawn {
            event: "os.fork".to_string(),
            executable: None,
            arguments: Vec::new(),
            verdict: Verdict::ThisProcess,
        };

        assert!(forked.verdict.certain());
        assert_eq!(forked.command(), None);

        let said = forked.to_string();
        assert!(
            said.contains("control connection"),
            "a fork shares the debugger's own socket, and a report that did not \
             say so would leave that looking like a protocol bug later. it said \
             {said}"
        );
    }

    #[test]
    fn a_blind_spot_names_the_interpreter_the_start_method_and_the_release() {
        // the whole value of this message is that it is specific. "bpd might
        // not see every child" is a disclaimer nobody can act on
        let said = Blindspot::MultiprocessingSpawn {
            interpreter: "3.13".to_string(),
        }
        .to_string();

        assert!(said.contains("3.13"), "it said {said}");
        assert!(
            said.contains("spawn") && said.contains("forkserver"),
            "it said {said}"
        );
        assert!(said.contains("3.14"), "it said {said}");
        assert!(
            said.contains("silence here does not mean there was none"),
            "the point of the message is what silence stops meaning, and it \
             said {said}"
        );
        assert!(
            said.contains("`fork`") && said.contains("`subprocess`"),
            "a blind spot that did not say what is still visible reads as bpd \
             seeing nothing at all, and it said {said}"
        );
    }

    #[test]
    fn a_child_running_this_interpreter_is_as_certain_as_a_fork() {
        // the two certain verdicts are the ones read from a path rather than
        // from a name, and a front end that showed a name-based guess with the
        // same confidence would be inventing evidence
        assert!(started(Verdict::ThisInterpreter).verdict.certain());
        assert!(
            !started(Verdict::AnotherInterpreter {
                named: "python".to_string()
            })
            .verdict
            .certain()
        );
    }
}
