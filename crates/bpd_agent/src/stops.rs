//! which threads are held, and which of them a request is about
//!
//! a stop holds **one thread**. every other thread in the process goes on
//! running, so more than one can be held at a time and each of them has to be
//! reachable independently. that is what this module is: a registry of held
//! threads, a mailbox per thread, and the routing that decides which mailbox a
//! request belongs in
//!
//! nothing here touches the interpreter. the routing runs on the connection's
//! reader thread, which has no GIL and must not take one — every answer is
//! computed on the python thread the question is about, because an expression
//! evaluated anywhere else would run the program's code on the wrong thread

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use bpd_protocol::message::{
    FrameId, FromAgent, FromEngine, Holding, Refusal, Stop, StopReason, Which,
};

use crate::attach;

/// what a held thread is asked to do next
#[derive(Debug)]
pub(crate) enum Command {
    /// answer this, and stay held
    Answer(FromEngine),
    /// stop being held
    Resume,
}

/// one held thread's queue of work
///
/// a mutex and a condition variable rather than a channel, because the held
/// thread waits on this with the GIL released and a `Receiver` cannot be shared
/// across that boundary
#[derive(Debug, Default)]
struct Mailbox {
    queue: Mutex<VecDeque<Command>>,
    ready: Condvar,
}

impl Mailbox {
    fn post(&self, command: Command) {
        self.queue
            .lock()
            .expect("a mailbox lock is only ever held to push or pop one command")
            .push_back(command);
        self.ready.notify_one();
    }

    /// the next command, waiting for one if there is none
    ///
    /// called with the GIL released, so the interpreter goes on running the
    /// rest of the program while this thread waits
    fn take(&self) -> Command {
        let mut queue = self
            .queue
            .lock()
            .expect("a mailbox lock is only ever held to push or pop one command");
        loop {
            if let Some(command) = queue.pop_front() {
                return command;
            }
            queue = self
                .ready
                .wait(queue)
                .expect("a mailbox lock is only ever held to push or pop one command");
        }
    }
}

#[derive(Debug)]
struct Entry {
    stop: u64,
    thread: u64,
    mailbox: Arc<Mailbox>,
}

#[derive(Debug)]
struct Registry {
    /// the last stop number handed out, so no two stops share one
    minted: u64,
    held: Vec<Entry>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    minted: 0,
    held: Vec::new(),
});

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY
        .lock()
        .expect("the stop registry is only ever held to look a thread up or add one")
}

impl Registry {
    fn stops(&self) -> Vec<u64> {
        self.held.iter().map(|entry| entry.stop).collect()
    }

    fn threads(&self) -> Vec<u64> {
        self.held.iter().map(|entry| entry.thread).collect()
    }
}

/// a thread's place in the registry, for as long as it is held
#[derive(Debug)]
pub(crate) struct Ticket {
    /// the stop number, which is what a frame id carries
    pub(crate) stop: u64,
    mailbox: Arc<Mailbox>,
}

impl Ticket {
    /// the next thing the engine wants, waiting with the GIL released
    pub(crate) fn next(&self) -> Command {
        self.mailbox.take()
    }
}

/// register a thread as held and tell the engine, in that order
///
/// the registration comes first so that a request naming this stop cannot
/// arrive before there is anything to route it to
pub(crate) fn enter(thread: u64, reason: StopReason, holding: Vec<Holding>) -> Ticket {
    let mailbox = Arc::new(Mailbox::default());
    let stop = {
        let mut registry = registry();
        registry.minted += 1;
        let stop = registry.minted;
        registry.held.push(Entry {
            stop,
            thread,
            mailbox: Arc::clone(&mailbox),
        });
        stop
    };

    attach::send(&FromAgent::Stopped {
        stop: Stop {
            stop,
            thread,
            reason,
            holding,
        },
    });
    Ticket { stop, mailbox }
}

/// the threads that are held right now
pub(crate) fn held_threads() -> Vec<u64> {
    registry().threads()
}

/// the stop holding a thread, if bpd is holding it
pub(crate) fn held_for(thread: u64) -> Option<u64> {
    registry()
        .held
        .iter()
        .find(|entry| entry.thread == thread)
        .map(|entry| entry.stop)
}

/// what a request is addressed to
#[derive(Debug, Clone, Copy)]
enum Address {
    /// any held thread will do, because the answer is about the process
    Any(&'static str),
    /// the thread one stop is holding
    Stop(u64),
    /// the thread the stop a frame id belongs to is holding
    Frame(FrameId),
}

/// hand a request to the thread it is about, or refuse it saying why there is
/// no such thread
pub(crate) fn route(request: FromEngine) {
    let address = match &request {
        FromEngine::Resume { which } => {
            resume(which);
            return;
        }
        FromEngine::SetBreakpoints { .. } => Address::Any("the breakpoints to resolve"),
        FromEngine::Threads { .. } => Address::Any("what the threads are doing"),
        FromEngine::Stack { stop, .. } | FromEngine::StopTheWorld { stop, .. } => {
            Address::Stop(*stop)
        }
        FromEngine::Variables { frame, .. }
        | FromEngine::Evaluate { frame, .. }
        | FromEngine::SetVariable { frame, .. } => Address::Frame(*frame),
        // `FromEngine` is non-exhaustive, so a newer engine could ask for
        // something this build cannot do. carrying on regardless would leave a
        // request unanswered and a debugger waiting for it
        other => attach::lost(&format!(
            "the debugger asked for {other:?}, which this agent does not understand"
        )),
    };
    deliver(address, request);
}

fn deliver(address: Address, request: FromEngine) {
    let registry = registry();
    let found = match address {
        // the lowest-numbered held stop, so which thread answers a
        // process-wide question is decided rather than raced
        Address::Any(_) => registry.held.iter().min_by_key(|entry| entry.stop),
        Address::Stop(stop) => registry.held.iter().find(|entry| entry.stop == stop),
        Address::Frame(frame) => registry.held.iter().find(|entry| entry.stop == frame.stop),
    };

    match found {
        Some(entry) => {
            let mailbox = Arc::clone(&entry.mailbox);
            drop(registry);
            mailbox.post(Command::Answer(request));
        }
        None => {
            let reason = match address {
                Address::Any(wanted) => Refusal::NothingHeld {
                    wanted: wanted.to_string(),
                },
                Address::Stop(stop) => Refusal::NoSuchStop {
                    stop,
                    held: registry.stops(),
                },
                Address::Frame(frame) => Refusal::StaleFrame {
                    frame,
                    held: registry.stops(),
                },
            };
            drop(registry);
            attach::send(&FromAgent::Refused { reason });
        }
    }
}

/// let held threads go
///
/// all or nothing when threads are named: a resume that let some of them go and
/// refused the rest would leave the client's idea of what is held different
/// from the agent's, with nothing saying which is right
fn resume(which: &Which) {
    let mut registry = registry();

    let taken = match which {
        Which::All => std::mem::take(&mut registry.held),
        Which::Named { threads } => {
            let mut taken = Vec::with_capacity(threads.len());
            for thread in threads {
                let found = registry
                    .held
                    .iter()
                    .position(|entry| entry.thread == *thread);
                let Some(index) = found else {
                    registry.held.append(&mut taken);
                    registry.held.sort_by_key(|entry| entry.stop);
                    let held = registry.threads();
                    drop(registry);
                    attach::send(&FromAgent::Refused {
                        reason: Refusal::ThreadNotHeld {
                            thread: *thread,
                            held,
                        },
                    });
                    return;
                };
                taken.push(registry.held.remove(index));
            }
            taken
        }
    };

    let threads = taken.iter().map(|entry| entry.thread).collect();
    drop(registry);

    // acknowledged before anything is woken, so a thread that stops again
    // immediately cannot report it ahead of being told it was resumed
    attach::send(&FromAgent::Resumed { threads });
    for entry in taken {
        entry.mailbox.post(Command::Resume);
    }
}
