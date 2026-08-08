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
pub const TARGET: &str = "BPD_TARGET";

/// every variable the launcher sets, so neither side can forget one
pub const ALL: &[&str] = &[ENDPOINT, TOKEN, TARGET];
