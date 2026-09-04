//! a child process the debuggee started, and what `bpd` can tell about it
//!
//! the debuggee is one process, and a program that starts another one has moved
//! the work `bpd` was pointed at somewhere `bpd` is not. django's `runserver`
//! is the case that made this worth reporting: the reloader starts a child and
//! waits on its exit code, so the process holding the agent never renders a
//! template
//!
//! this reports. whether the child is **taken up** as a session of its own is a
//! setting of the debuggee's, and the report carries it — see
//! [`Spawn::taking_up`], which is deliberately a statement about what will be
//! attempted rather than about what the child has already done
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

use std::ffi::CStr;
use std::fmt;

/// the audit events a child process is recognised through
///
/// they live here rather than in the agent that installs the hook because two
/// things need the same answer: the agent, which compares an event name against
/// this on every audit event a program raises, and
/// `a_program_that_watches_its_own_audit_events_sees_exactly_the_ones_it_would_have`,
/// which has to know the fixture actually reached one of them or it compared
/// two runs of a program and proved nothing
///
/// that test restated the list as a single event, and picked
/// `_posixsubprocess.fork_exec`. which of these an ordinary `subprocess.run`
/// raises is the interpreter's choice on the day — cpython uses `posix_spawn`
/// where it can — so the restatement was true on one machine and false on the
/// next, and the test refused a run in which nothing was wrong. a list that is
/// written twice is a list that disagrees with itself eventually
/// the audit events that make a process, on 3.14 and later
///
/// `subprocess.Popen` is deliberately absent: the event beneath it fires for
/// the same child, so watching both reports every ordinary subprocess twice.
/// `os.system` is absent for a different reason — it hands a whole command line
/// to a shell, and what a shell does with one is not knowable from the vector
#[cfg(not(windows))]
pub const MAKING_A_PROCESS: &[&CStr] = &[
    c"_posixsubprocess.fork_exec",
    c"os.posix_spawn",
    c"os.exec",
    c"os.fork",
];

/// the same on 3.13, where `_posixsubprocess.fork_exec` raises nothing
///
/// `subprocess.Popen` takes its place, because there it is the only event a
/// `subprocess` child raises at all. `import` is watched for one reason and one
/// only — the agent's `announce_blindspot`, which needs to know when `multiprocessing`
/// arrives
#[cfg(not(windows))]
pub const MAKING_A_PROCESS_BEFORE_314: &[&CStr] = &[
    c"subprocess.Popen",
    c"os.posix_spawn",
    c"os.exec",
    c"os.fork",
    c"import",
];

/// the events that make a process on windows, on every supported release
///
/// the list is per platform because the events are: nothing on windows raises
/// `_posixsubprocess.fork_exec`, and nothing on posix raises
/// `_winapi.CreateProcess`. there is no `os.fork` here and there cannot be one,
/// and `subprocess.Popen` is absent for the reason it is on posix — windows
/// raises it beside `_winapi.CreateProcess` for the same child
///
/// it does not change with the release, because `_winapi.CreateProcess` has
/// been an audit event since PEP 578 landed in 3.8 — long before this project's
/// minimum. `multiprocessing`'s spawn method goes through it here, so the 3.13
/// blind spot below is a posix one
#[cfg(windows)]
pub const MAKING_A_PROCESS: &[&CStr] = &[c"_winapi.CreateProcess", c"os.exec"];

/// the same on 3.13, which on windows is the same list
#[cfg(windows)]
pub const MAKING_A_PROCESS_BEFORE_314: &[&CStr] = MAKING_A_PROCESS;

/// the events to watch on an interpreter of this version
///
/// the split is at 3.14, where `_posixsubprocess.fork_exec` became an audit
/// event. below it that event is silent, so `subprocess.Popen` is watched in
/// its place — which covers `subprocess` and does **not** cover
/// `multiprocessing`, because `multiprocessing` never goes near `subprocess`.
/// the agent turns that into a [`Blindspot`] it announces rather than a silence
#[must_use]
pub fn making_a_process(major: u8, minor: u8) -> &'static [&'static CStr] {
    if (major, minor) < (3, 14) {
        MAKING_A_PROCESS_BEFORE_314
    } else {
        MAKING_A_PROCESS
    }
}

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
    /// absent for an `os.fork`, where the child runs this very process
    pub executable: Option<String>,

    /// the argument vector, as the program gave it
    ///
    /// empty for an `os.fork`, and for an event that carries no vector
    pub arguments: Vec<String>,

    /// what `bpd` can tell about the child being python, and on what evidence
    pub verdict: Verdict,

    /// whether `bpd` will try to take this child up as a session of its own
    ///
    /// **what was asked for, and not what happened.** the report is written in
    /// the parent, at the moment of the `fork` or the `exec`, and at that moment
    /// the child has done nothing: a forked one has not reconnected yet, and an
    /// `exec`'d one is not an interpreter yet. so the only thing knowable here
    /// is whether this debuggee was told to debug its children, which is what
    /// this carries
    ///
    /// a child can still fail to arrive — a fork whose connection is refused, an
    /// interpreter the staged agent will not import into, a command that turns
    /// out not to run python at all. a child that got as far as the agent says
    /// so on its **own** stderr and then runs as it would have without `bpd`;
    /// one started with `-E`, `-I` or `-S` never gets that far and says nothing,
    /// which is why the sentence names that case rather than promising a line
    /// that is not always written
    ///
    /// the session that joins is what says one really did arrive
    pub taking_up: bool,
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
    /// it is python by construction, and for the length of the `fork` call it
    /// is more than that: it inherits the agent's `sys.monitoring` state and
    /// the descriptors of the debugger's own control connection, neither of
    /// which `bpd` gave it
    ///
    /// it gives this session's descriptors up before `os.fork()` returns to it,
    /// on every path. what it does with the monitoring state is
    /// [`Spawn::taking_up`]'s to say: it either takes the tool off itself and
    /// runs as a bare process would, or keeps it, opens a connection of its own
    /// and holds where the fork returned
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

/// what every report of a child `bpd` is taking up says it was told to do
///
/// one wording, used by all four verdicts, because the thing that decides them
/// all is one setting. a front end looking for the claim in the sentence has one
/// phrase to look for
const ASKED: &str = "bpd was asked to debug this program's children";

/// what settles a claim this report cannot settle itself
///
/// the report is written in the parent, at the moment the child is asked for, so
/// everything after that moment is the **child's** to do: reach the engine,
/// import the agent, hold. saying so here is what keeps the sentence a statement
/// about an attempt rather than about an outcome nobody can know yet
///
/// a macro rather than a constant because both tails below open with it and are
/// built from it — a phrase every taking-up report shares cannot then drift out
/// of one of them, and a front end looking for that half has one thing to find
macro_rules! joining_settles_it {
    () => {
        "the session that joins is what says it arrived"
    };
}

/// what a **forked** child does when it cannot open a session of its own
///
/// unconditional, and it can be: the handler that would say so is inherited
/// memory, so there is no way for a forked child to fail silently
const AND_THEN: &str = concat!(
    joining_settles_it!(),
    ", and a child that cannot reach the debugger says so on its own stderr and \
     runs as it would have without bpd"
);

/// the same for an `exec`'d child, where it is **not** unconditional
///
/// the channel is `PYTHONPATH` and a `sitecustomize`, so a child that reaches
/// the agent at all can say what went wrong — and one started with `-E`, `-I` or
/// `-S` reads neither and never gets that far. that is a silence, so it is named
/// rather than papered over with the wording the fork gets: a person who expected
/// a session and got none needs to know which of the two they are looking at
const OR_NOT_AT_ALL: &str = concat!(
    joining_settles_it!(),
    ". a child that reaches the agent and cannot be debugged says so on its own \
     stderr and runs as it would have without bpd, and one started with `-E`, \
     `-I` or `-S` reads neither `PYTHONPATH` nor `site` and so never reaches \
     the agent at all"
);

/// what an `exec`'d child that is taken up does, which is not what a fork does
///
/// a fork inherits memory and holds where it forked; an `exec` is a fresh
/// interpreter entered from `site`, before `__main__` exists, so there is no
/// line to name and the report must not invent one
const AT_STARTUP: &str = "the child is entered at its own interpreter's startup \
                          and held before it runs a line of its program";

/// the sentence every front end says about a spawn
///
/// written once, here, because the CLI, the DAP adapter and the MCP server all
/// have to say it and three wordings of the same fact is three descriptions of
/// the same program
///
/// it says what `bpd` **will attempt** rather than what the child has done, and
/// that is the whole of why [`Spawn::taking_up`] is on the type rather than
/// being decided by each front end: the two claims a reader has to put together
/// are this one and the session that joins, and a report that stated an outcome
/// here would be contradicting the join a line later
impl fmt::Display for Spawn {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.verdict, self.taking_up) {
            (Verdict::ThisProcess, false) => write!(
                out,
                "the program forked. the child was a copy of this process, agent \
                 and all, and gave the debugger up before it ran a line — it \
                 holds none of the agent's monitoring state and neither \
                 descriptor of this session's control connection. bpd is not \
                 debugging it as a session of its own"
            ),
            (Verdict::ThisProcess, true) => write!(
                out,
                "the program forked. the child was a copy of this process, agent \
                 and all, and gives this session up before it runs a line — it \
                 holds neither descriptor of this session's control connection. \
                 {ASKED}, so it keeps the agent's monitoring state, opens a \
                 connection of its own and holds where the fork returned to it. \
                 {AND_THEN}"
            ),
            (Verdict::ThisInterpreter, false) => write!(
                out,
                "the program started a python child, running the interpreter \
                 this program is running, and bpd is not debugging it"
            ),
            (Verdict::ThisInterpreter, true) => write!(
                out,
                "the program started a python child, running the interpreter \
                 this program is running. {ASKED}, so {AT_STARTUP}. \
                 {OR_NOT_AT_ALL}"
            ),
            (Verdict::AnotherInterpreter { named }, false) => write!(
                out,
                "the program started a child whose program is called `{named}`, \
                 which is a python interpreter's name and is not the one this \
                 program is running. bpd is not debugging it"
            ),
            (Verdict::AnotherInterpreter { named }, true) => write!(
                out,
                "the program started a child whose program is called `{named}`, \
                 which is a python interpreter's name and is not the one this \
                 program is running. {ASKED}, so {AT_STARTUP} — for as long as \
                 it is the same release of python, since the staged agent is \
                 not abi3 and cannot be imported by another. {OR_NOT_AT_ALL}"
            ),
            (Verdict::Perhaps { named }, false) => write!(
                out,
                "the program started a child whose command names `{named}`, and \
                 bpd cannot tell from the command whether the child will run \
                 python. if it does, bpd is not debugging it"
            ),
            (Verdict::Perhaps { named }, true) => write!(
                out,
                "the program started a child whose command names `{named}`, and \
                 bpd cannot tell from the command whether the child will run \
                 python. {ASKED}, so if it does, {AT_STARTUP}. \
                 {OR_NOT_AT_ALL}"
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
            taking_up: false,
        }
    }

    /// one report of every kind, both ways round
    fn every_report() -> Vec<Spawn> {
        let verdicts = [
            Verdict::ThisProcess,
            Verdict::ThisInterpreter,
            Verdict::AnotherInterpreter {
                named: "python3.13".to_string(),
            },
            Verdict::Perhaps {
                named: "python".to_string(),
            },
        ];
        verdicts
            .into_iter()
            .flat_map(|verdict| {
                [false, true].map(|taking_up| Spawn {
                    taking_up,
                    ..started(verdict.clone())
                })
            })
            .collect()
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
    fn no_report_says_bpd_is_not_debugging_a_child_it_is_taking_up() {
        // the defect this field exists for. the report is made in the parent and
        // a session that joins is made later, and a run where the first says the
        // child is not being debugged and the second says it is leaves a reader
        // deciding which half of the debugger's own output to believe
        for child in every_report() {
            let said = child.to_string();
            if child.taking_up {
                assert!(
                    !said.contains("not debugging"),
                    "bpd is taking this child up and the report said {said}"
                );
                assert!(
                    said.contains(ASKED),
                    "a report of a child being taken up has to say so in the \
                     words every front end looks for, and it said {said}"
                );
                assert!(
                    said.contains(joining_settles_it!()),
                    "the report is written before the child has done anything, \
                     so it has to name what settles it, and it said {said}"
                );
            } else {
                assert!(
                    said.contains("not debugging"),
                    "nothing is taking this child up, and a report that did not \
                     say so leaves a person waiting for a stop that cannot \
                     come. it said {said}"
                );
                assert!(
                    !said.contains(ASKED),
                    "child debugging is off and the report said {said}"
                );
            }
        }
    }

    #[test]
    fn a_report_of_a_child_being_taken_up_claims_an_attempt_and_not_an_outcome() {
        // the honest limit. `bpd` knows what it was asked for at the moment of
        // the fork or the exec, and it does not know that the child arrived —
        // so the sentence names the two things that settle it rather than
        // asserting one of them
        for tail in [AND_THEN, OR_NOT_AT_ALL] {
            assert!(
                tail.contains("says so on its own stderr"),
                "a child that could not be debugged says so where it can, and a \
                 report that did not mention it would leave that line looking \
                 like the program's: {tail}"
            );
        }

        // and the two are not interchangeable. a forked child cannot fail
        // silently — the handler that would say so is inherited memory — where
        // an `exec`'d one started with `-E`, `-I` or `-S` reads neither
        // `PYTHONPATH` nor `site`, arrives nowhere, and says nothing at all
        let forked = Spawn {
            event: "os.fork".to_string(),
            executable: None,
            arguments: Vec::new(),
            verdict: Verdict::ThisProcess,
            taking_up: true,
        };
        assert!(!forked.to_string().contains("-S"), "{forked}");

        let execd = Spawn {
            taking_up: true,
            ..started(Verdict::ThisInterpreter)
        };
        let said = execd.to_string();
        assert!(
            said.contains("`-E`, `-I` or `-S`"),
            "a child that runs undebugged **and says nothing** is the one \
             outcome a person cannot diagnose from the output, so the report \
             names it. it said {said}"
        );
    }

    #[test]
    fn a_fork_is_reported_as_the_shared_process_it_is() {
        let forked = Spawn {
            event: "os.fork".to_string(),
            executable: None,
            arguments: Vec::new(),
            verdict: Verdict::ThisProcess,
            taking_up: false,
        };

        assert!(forked.verdict.certain());
        assert_eq!(forked.command(), None);

        // both ways round. a fork inherits the debugger's own socket whether or
        // not the child is taken up, so a report that did not say what became of
        // it leaves a reader with no way to tell either case from the one where
        // two processes really are writing into one
        for taking_up in [false, true] {
            let said = Spawn {
                taking_up,
                ..forked.clone()
            }
            .to_string();
            assert!(
                said.contains("control connection"),
                "a fork with taking_up {taking_up} said {said}"
            );
        }
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
