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

/// every variable the launcher sets, so neither side can forget one
pub const ALL: &[&str] = &[ENDPOINT, TOKEN, TARGET, FORM, PYTHON_PATH];

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
