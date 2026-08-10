//! frames of a held thread's stack, and the scopes a frame really has

/// which frame of the stopped thread's stack something is asked about
///
/// an id is minted at a stop and names the stop it was minted at, so a client
/// that holds one across a resume finds out rather than reading a frame that is
/// no longer the one it meant. DAP's opaque handle cannot do that: it looks the
/// same before and after, and the debugger has to guess which the client meant
///
/// `depth` counts from the frame that stopped, so `0` is always where the
/// program is now
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct FrameId {
    /// which stop this id belongs to, counting from one
    pub stop: u64,
    /// how far down the stack, with the frame that stopped at zero
    pub depth: u32,
}

impl std::fmt::Display for FrameId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "frame {} of stop {}", self.depth, self.stop)
    }
}

/// one frame of the stopped thread's stack
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Frame {
    /// how to ask about this frame, for as long as this stop lasts
    pub id: FrameId,
    /// the `co_filename` of the code it is running
    pub file: String,
    /// the line it is on now, as `f_lineno` reports it
    pub line: u32,
    /// `co_qualname`
    pub function: String,
    /// `co_firstlineno`, which separates two code objects with the same name
    pub first_line: u32,
}

/// where a name lives, which is not a detail a debugger may round off
///
/// python resolves a name in a function by which of these it is, decided at
/// compile time. merging them into one "variables" mapping — which is what
/// `f_locals` itself does — means a report that cannot distinguish a captured
/// variable from a global of the same name, and "a variable read from the wrong
/// scope" is the thing this project exists not to do
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// the frame's own locals — `co_varnames`
    ///
    /// for a module or a class body there are none of these in the code object,
    /// and the frame's namespace mapping is the local scope instead
    Local,

    /// locals of this frame that a nested function captures — `co_cellvars`
    ///
    /// an argument that a closure captures is in this scope **and** in
    /// [`Scope::Local`], because cpython says it is both
    Cell,

    /// variables this frame captures from an enclosing one — `co_freevars`
    ///
    /// the value lives in the enclosing frame's cell. it is not a local of this
    /// frame and it is not a global
    Free,

    /// the module namespace the frame's code was compiled into — `f_globals`
    Global,
}

impl std::fmt::Display for Scope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Local => "local",
            Self::Cell => "cell",
            Self::Free => "free",
            Self::Global => "global",
        })
    }
}
