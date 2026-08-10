//! the threads of the debuggee, held and running

/// where a thread was when it was sampled
///
/// the innermost python frame, which for a thread inside a C call is the frame
/// that made the call rather than the call itself — the interpreter has no
/// frame for one, and inventing a location for it would be the debugger
/// describing something it cannot see
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Where {
    /// the `co_filename` of the code it is running
    pub file: String,
    /// the line it is on, as `f_lineno` reports it
    pub line: u32,
    /// `co_qualname`
    pub function: String,
}

impl std::fmt::Display for Where {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}:{} in {}",
            self.file, self.line, self.function
        )
    }
}

/// whether a thread was seen to get anywhere between two samples
///
/// this is the general half of the lock problem, and it is deliberately not
/// called a diagnosis. cpython exposes no owner for a lock, so bpd cannot say
/// "thread 7 is waiting for a lock thread 3 holds". what it can say is that
/// thread 7 was in the same place twice, a stated interval apart, which is the
/// symptom the user is actually looking at when they think bpd has hung
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Progress {
    /// bpd is holding this thread, so it did not move because bpd stopped it
    Held,
    /// it was somewhere else in the second sample
    Moved,
    /// it was in the same place in both samples
    ///
    /// not proof of anything on its own: a thread blocked in `sock.recv` and a
    /// thread piled up behind a lock the held thread took look identical from
    /// here. it is where to look, not what is wrong
    Still,
}

/// one thread of the debuggee, as of a sample
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadState {
    /// the interpreter's identity for it
    pub thread: u64,
    /// the stop holding it, when bpd is holding it
    pub held: Option<u64>,
    /// where it was, or `None` when it has no python frame of the program's
    ///
    /// the agent's own bootstrap frame is not a location: it is the `-c` the
    /// interpreter was entered through, and reporting it would put a frame of
    /// bpd's in front of a user
    pub at: Option<Where>,
    /// whether it was seen to get anywhere
    pub progress: Progress,
}

/// which held threads a resume is about
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "which", rename_all = "snake_case")]
pub enum Which {
    /// every thread that is held right now
    ///
    /// resolved by the agent at the moment the request arrives, so a thread
    /// that stopped while this was in flight is included. that is deliberate:
    /// the alternative is a client that asked for everything and got a program
    /// with one thread still held and nothing saying so
    All,
    /// exactly these, by the interpreter's thread identity
    ///
    /// naming a thread that is not held is refused, not ignored
    Named {
        /// the threads to let go
        threads: Vec<u64>,
    },
}
