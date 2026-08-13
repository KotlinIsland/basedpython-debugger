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
///
/// not every frame in a stack is a frame the interpreter has. a django template
/// is not compiled to python, so the only way a template line can appear at all
/// is for `bpd` to **synthesise** a frame for it — and a synthesised frame that
/// looked like a python frame would be the debugger inventing something the
/// program does not have. [`Frame::kind`] is how a client tells them apart, and
/// everything that is true of only one of them lives in there rather than in a
/// field the other has to fill in with something
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Frame {
    /// how to ask about this frame, for as long as this stop lasts
    pub id: FrameId,
    /// the file the frame is running code from
    ///
    /// `co_filename` for a python frame, and the template's own path for a
    /// template frame
    pub file: String,
    /// the line of that file the frame is on now
    pub line: u32,
    /// what the source map said about the two fields above, when one did
    ///
    /// `None` is the ordinary case: nothing mapped this location and it is the
    /// interpreter's own. for a basedpython build it is what says **which of
    /// the two files** the fields above name — a `.by` line with the generated
    /// location beside it, or generated python with the map's own reason why no
    /// `.by` line is behind it
    ///
    /// it is on a frame and not on a [`crate::StopReason`] because the stop's
    /// location is always this frame's: a breakpoint, a step, a pause, a raise
    /// and a fork all report the code object that is running, which is frame
    /// zero. one field that a client reads once beats seven that have to agree
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping: Option<crate::source_map::Mapping>,
    /// what the frame is, and what only that kind of frame has
    ///
    /// flattened on the wire, so a frame is one object carrying a `kind`
    /// discriminator beside the fields that kind has — rather than a `kind`
    /// holding a `kind`
    #[serde(flatten)]
    pub kind: FrameKind,
}

impl Frame {
    /// what to call this frame in one word
    ///
    /// `co_qualname` for a python frame, and the django node class for a
    /// template frame. it exists so a front end that has one place to put a
    /// name does not have to decide what a template frame's is, and it is
    /// **not** a substitute for reading [`Frame::kind`] — the two frames are
    /// different things and a client that shows only this cannot tell
    pub fn name(&self) -> &str {
        match &self.kind {
            FrameKind::Python { function, .. } => function,
            FrameKind::Template { node, .. } => node,
        }
    }

    /// the python frame this one is, or the one that renders it
    ///
    /// a template frame is synthesised over the `Node.render_annotated` frame
    /// that is really running, and that frame is where python can be evaluated
    /// and where python scopes can be read
    pub const fn python(&self) -> FrameId {
        match &self.kind {
            FrameKind::Python { .. } => self.id,
            FrameKind::Template { python, .. } => *python,
        }
    }
}

/// what a frame is
///
/// deliberately closed. a client that absorbed an unknown kind into a catch-all
/// arm would be rendering a synthesised frame as a real one, which is the whole
/// thing this enum exists to prevent
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrameKind {
    /// a frame the interpreter really has
    Python {
        /// `co_qualname`
        function: String,
        /// `co_firstlineno`, which separates two code objects with the same name
        first_line: u32,
    },

    /// a frame `bpd` synthesised from a django template node
    ///
    /// the interpreter has no frame for it. django walks a tree of `Node`
    /// objects and calls `Node.render_annotated` once per node, so what is
    /// really running is that method — and the template line it is running is
    /// only knowable from the node it was handed
    Template {
        /// the django node class the tag compiled to — `VariableNode`, `IfNode`
        node: String,
        /// the `Node.render_annotated` frame underneath this one
        ///
        /// python is evaluated there, and its scopes are read there. it is
        /// always the frame one deeper than this one, and it is carried rather
        /// than left to be worked out
        python: FrameId,
    },
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
