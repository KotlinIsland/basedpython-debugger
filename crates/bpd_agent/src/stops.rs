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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use bpd_core::{FrameId, Holding, Refusal, Reported, StepKind, StopReason, Which};
use bpd_protocol::message::{FromAgent, FromEngine};

use crate::cells::ForkCell;
use crate::{attach, pause};

/// what a held thread is asked to do next
#[derive(Debug)]
pub(crate) enum Command {
    /// answer this, and stay held
    Answer(FromEngine),
    /// stop being held
    Resume,
    /// stop being held, and be held again where this step lands
    Step(StepKind),
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

/// the last stop number handed out, so no two stops share one
///
/// outside the table it numbers, and an atomic, because a **forked child keeps
/// it**. the child's copy of the table names threads the fork did not copy and
/// is given up with the rest of the session, but the counter is inherited
/// memory and is left exactly where the fork found it
///
/// that is not a claim that two processes cannot land on the same number.
/// counting on from the same place, they can and will. what inheriting removes
/// is the collision resetting to one guarantees: a child that started again at
/// one would reissue, immediately, numbers its parent had **already reported**.
/// that a number can still name a stop in two sessions is why a
/// [`bpd_core::Stop`] is named by the session it arrived on, and why
/// `bpd_core::Addressed::of` refuses rather than picks when a number names two
static MINTED: AtomicU64 = AtomicU64::new(0);

/// the threads held right now
///
/// in a [`ForkCell`] because a program thread can be inside [`enter`] holding
/// it while another thread of the same process calls `os.fork()`. what keeps
/// the two apart today is the GIL on a gil build and `os.fork()`'s
/// stop-the-world on a free-threaded one — neither of which is a property of
/// this agent, and the second of which is an interpreter internal nothing
/// promises. so the child's copy is replaced rather than relied on
#[derive(Debug)]
struct Registry {
    held: Vec<Entry>,
}

static REGISTRY: ForkCell<Registry> = ForkCell::new(nothing_held);

/// what the registry holds before the first stop, and in a forked child
const fn nothing_held() -> Registry {
    Registry { held: Vec::new() }
}

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY
        .get()
        .lock()
        .expect("the stop registry is only ever held to look a thread up or add one")
}

/// give up the held-thread table this process was forked holding
///
/// the entries name threads the fork did not copy, so none of them is a thread
/// this process can answer for. the table is **replaced** rather than emptied
/// because emptying it means locking it, and a thread that was holding it at
/// the instant of the fork is one of the threads that did not survive
///
/// [`MINTED`] is deliberately not touched — see its own note
#[cfg(unix)]
pub(crate) fn abandon() {
    REGISTRY.abandon();
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
    // minted outside the table's lock, which changes nothing about uniqueness:
    // the counter is what makes a number unique, and the table is only where
    // the thread holding it is looked up. what it buys is that the number
    // survives a fork while the table does not
    let stop = MINTED.fetch_add(1, Ordering::Relaxed) + 1;
    registry().held.push(Entry {
        stop,
        thread,
        mailbox: Arc::clone(&mailbox),
    });

    attach::send(&FromAgent::Stopped {
        stop: Reported {
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
        // a step is a resume with instrumentation, so it leaves the registry
        // the way a resume does rather than being answered inside the stop
        FromEngine::Step { stop, kind } => {
            step(*stop, *kind);
            return;
        }
        // the one request that is about a program with nothing held. it is
        // armed on a thread of the agent's own, because there is no thread of
        // the debuggee's waiting to be asked
        FromEngine::Pause => {
            pause::request();
            return;
        }
        FromEngine::SetBreakpoints { .. } => Address::Any("the breakpoints to resolve"),
        FromEngine::SetExceptionBreakpoints { .. } => {
            Address::Any("the exception breakpoints to set")
        }
        FromEngine::Threads { .. } => Address::Any("what the threads are doing"),
        // about the process rather than about one held thread: it replaces the
        // code of a file, and which held thread answers makes no difference to
        // what it finds or what it writes
        FromEngine::ReplaceCode { .. } => Address::Any("the code to replace"),
        FromEngine::Stack { stop, .. } | FromEngine::StopTheWorld { stop, .. } => {
            Address::Stop(*stop)
        }
        FromEngine::Variables { frame, .. }
        | FromEngine::TemplateContext { frame, .. }
        | FromEngine::Evaluate { frame, .. }
        | FromEngine::Source { frame, .. }
        | FromEngine::SetVariable { frame, .. }
        | FromEngine::SetNextStatement { frame, .. }
        | FromEngine::RestartFrame { frame } => Address::Frame(*frame),
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

/// let one held thread go, with a step armed on it
///
/// acknowledged as a resume of that thread, before it is woken, for the same
/// reason a resume is: the landing is a stop of its own and must not arrive
/// ahead of the acknowledgement that the thread was let go at all
fn step(stop: u64, kind: StepKind) {
    let mut registry = registry();
    let Some(index) = registry.held.iter().position(|entry| entry.stop == stop) else {
        let held = registry.stops();
        drop(registry);
        attach::send(&FromAgent::Refused {
            reason: Refusal::NoSuchStop { stop, held },
        });
        return;
    };

    let entry = registry.held.remove(index);
    drop(registry);
    attach::send(&FromAgent::Resumed {
        threads: vec![entry.thread],
    });
    entry.mailbox.post(Command::Step(kind));
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
