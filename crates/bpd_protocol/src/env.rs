//! how the engine tells the agent where to connect
//!
//! the agent is entered through `python -c`, which leaves no room to pass
//! anything structured, so the launch parameters arrive in the environment.
//! they are defined here rather than in either side, because two copies of a
//! variable name is one copy too many — a rename that misses one produces an
//! agent that starts and then cannot find its engine
//!
//! the agent removes all three before user code runs. a program that could
//! notice the debugger by reading its environment is a program the debugger
//! changed

/// the loopback endpoint the agent connects back to
pub const ENDPOINT: &str = "BPD_ENDPOINT";

/// this session's token, hex encoded
pub const TOKEN: &str = "BPD_TOKEN";

/// the program the agent should run
///
/// what it holds depends on [`FORM`]: a path, a module name, or the source of a
/// command
pub const TARGET: &str = "BPD_TARGET";

/// which of the three ways the interpreter can be entered [`TARGET`] is
pub const FORM: &str = "BPD_FORM";

/// what `PYTHONPATH` held before the agent's directory was put in front of it
///
/// the agent is imported by putting its staged directory on `PYTHONPATH`, and
/// the agent takes that back off before user code runs. it needs the original
/// spelling to do it: **absent** is not the same as set and empty, and a
/// program that inspects `os.environ` can tell the two apart
///
/// set only when the launcher inherited one, so absent here means absent there
pub const PYTHON_PATH: &str = "BPD_PYTHONPATH";

/// where a child that was **`exec`'d** connects back to
///
/// the same endpoint the session's own agent was given. it is a separate name
/// because it lives in the environment for a different span: [`ENDPOINT`] is
/// taken back out before any user code runs, and this one is **put in** when
/// child debugging is asked for and stays there, because the child that reads
/// it is a fresh interpreter that inherits nothing else
pub const CHILD_ENDPOINT: &str = "BPD_CHILD_ENDPOINT";

/// the token an `exec`'d child presents, hex encoded
///
/// **not [`TOKEN`]**, and that is the whole reason it exists. the session token
/// is removed from the environment before the program runs; this one has to
/// stay there for as long as the program can start a child, where anything that
/// can read this process's environment can read it. a session token left in the
/// environment would let any of them write frames into the session bpd is
/// already holding — so a child is given a token whose only power is to open a
/// session of its own
pub const CHILD_TOKEN: &str = "BPD_CHILD_TOKEN";

/// the staged directory an `exec`'d child imports the agent from
///
/// the `sitecustomize` that enters a child holds nothing but the four lines
/// that find the agent, and it is in a directory of its own — so the agent's
/// own directory has to be named somewhere, and this is it. it is put on
/// `sys.path` by the child, for the one import, and taken off again by the
/// agent before the child stops
pub const CHILD_AGENT: &str = "BPD_CHILD_AGENT";

/// the staged directory holding the `sitecustomize` an `exec`'d child is
/// entered through
///
/// launcher to agent only: it is **never** in the environment a program can
/// read under that name. what child debugging puts in the environment is this
/// directory appended to `PYTHONPATH`, which is the only spelling an
/// interpreter that has not started yet will act on
pub const SITECUSTOMIZE: &str = "BPD_SITECUSTOMIZE";

/// every variable the launcher sets, so neither side can forget one
///
/// [`CHILD_ENDPOINT`] and [`CHILD_AGENT`] are here without the launcher setting
/// them, and that is deliberate rather than an oversight: this is the list the
/// agent **clears**, and a stale pair inherited from an outer `bpd` would
/// otherwise send this program's children to an engine that is not this one
pub const ALL: &[&str] = &[
    ENDPOINT,
    TOKEN,
    TARGET,
    FORM,
    PYTHON_PATH,
    CHILD_ENDPOINT,
    CHILD_TOKEN,
    CHILD_AGENT,
    SITECUSTOMIZE,
];

/// the three names child debugging puts into a debuggee's environment
///
/// the whole of what an `exec`'d child is reached through, and the whole of
/// what a program can read about it beyond `PYTHONPATH`. it is a list rather
/// than three uses of three constants because the parity mirrors in
/// `crates/bpd/tests/launch_parity.rs` are written against exactly this set: a
/// fourth name added here without a reason beside it fails there
pub const CHILD: &[&str] = &[CHILD_ENDPOINT, CHILD_TOKEN, CHILD_AGENT];

/// how the interpreter is asked to enter the program
///
/// the three forms are not variations of one another: `sys.argv[0]`,
/// `sys.path[0]` and `__main__` differ between them, and a launcher that treats
/// one as a special case of another gets at least one of them wrong. what each
/// produces is recorded in `crates/bpd_test/tests/launch_forms.rs`
///
/// it travels in the environment rather than over the control connection
/// because the agent has to know it before it connects to anything: the
/// interpreter is entered through `-c`, which leaves no room for anything
/// structured
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// `python <path>`
    Script,
    /// `python -m <module>`
    Module,
    /// `python -c <source>`
    Command,
}

impl Form {
    /// every form, so neither side can answer for a subset of them
    pub const ALL: &'static [Self] = &[Self::Script, Self::Module, Self::Command];

    /// how this form is spelled in the environment
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::Module => "module",
            Self::Command => "command",
        }
    }

    /// the form that variable named, or nothing
    ///
    /// nothing can only mean the engine and the agent disagree, which the
    /// exact-match handshake already rules out — so the caller reports it as
    /// the contradiction it is rather than choosing a form
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|form| form.as_str() == text)
    }
}

#[cfg(test)]
mod tests {
    use super::Form;

    #[test]
    fn every_form_survives_the_environment() {
        for form in Form::ALL {
            assert_eq!(Form::parse(form.as_str()), Some(*form));
        }
    }

    #[test]
    fn a_form_nobody_wrote_is_not_invented() {
        assert_eq!(Form::parse("Script"), None);
        assert_eq!(Form::parse(""), None);
    }
}
