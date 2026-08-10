//! an exception of the debugged program, as the agent read it off the object

/// an exception the interpreter raised, as the agent read it off the object
///
/// read by walking the exception and its traceback rather than by calling
/// `traceback.format_exception`: the agent must not import a module to describe
/// a failure, because the import would run inside a monitoring callback and
/// because a debuggee that imports `traceback` it would not otherwise have
/// imported is a debuggee the debugger changed
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PythonError {
    /// the exception's type, qualified by its module unless it is a builtin
    pub kind: String,
    /// `str(exception)`
    pub message: String,
    /// the frames the exception carries, outermost first
    pub traceback: Vec<TracebackFrame>,
}

impl std::fmt::Display for PythonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(formatter, "{}", self.kind)
        } else {
            write!(formatter, "{}: {}", self.kind, self.message)
        }
    }
}

/// one frame of a traceback
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TracebackFrame {
    /// the `co_filename` of the code that was running
    pub file: String,
    /// the line it was on
    pub line: u32,
    /// `co_qualname`
    pub function: String,
}
