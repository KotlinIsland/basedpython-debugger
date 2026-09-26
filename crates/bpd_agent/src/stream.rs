//! the stream of trace records, and the thread that writes it
//!
//! the ui runtime of basedpython-ui announces every record it appends, on the
//! thread that appended it, and a client that is watching wants each one as
//! it happens. what a client must never get is the program waiting for it:
//! the announcement is made on the ui thread with the GIL held, and a socket
//! write there blocks the whole ui the moment the engine's receive buffer is
//! full — which it is whenever a client resumes the program and then thinks
//! for a while, because the engine reads the connection only inside a request.
//! measured before this module existed: a ui that recomposed while an agent
//! was idle froze for exactly as long as the agent was idle
//!
//! so the hook does not write. it renders the record — bounded, storage-only,
//! as [`crate::ui_trace`] reads the ring at a stop — and hands it to a queue,
//! and a thread of the agent's own takes records off the queue and writes
//! them. the queue's lock is held for one push or one pop and never across
//! the write, so the ui thread waits for nothing longer than a pop
//!
//! ## the queue is bounded, and the bound is said
//!
//! a ui that writes records faster than the engine reads them would otherwise
//! grow the queue without limit inside the program's own memory. it holds
//! [`CAPACITY`] records; when it is full the **oldest** is dropped and counted,
//! and the count rides the next record that gets through as
//! `dropped_before` — so a client reading a stream whose count is above zero
//! knows there is a gap before that record, and one reading zero knows there
//! is none. oldest rather than newest, because the newest is the one a client
//! watching the ui recompose is waiting for, and the ring read at a stop is
//! where the oldest can still be found
//!
//! ## it is not on the process while it forks
//!
//! for the reason the control connection's reader is not — see
//! [`crate::attach`]: cpython counts the process's operating system threads at
//! `os.fork()` and a program can put the resulting warning in its own data. so
//! [`stand_down`] joins the writer before a fork and [`resume_writing`] starts
//! it again in the parent, and what has been queued and not written stays in
//! the queue for the next writer. a forked child starts with the watch **off**
//! and with a queue and a writer of its own, replaced without a lock the way
//! every other cell of the session is — [`abandon`], from
//! [`crate::attach::detach`] — because a child that inherited its parent's
//! watch would forward its own ui's records onto a session that asked about
//! the parent's
//!
//! ## a stop waits for the stream to catch up
//!
//! a thread reporting a stop writes the report itself, on the connection the
//! writer shares. left alone, a record queued a moment before the stop could
//! land on the socket *after* it, and a client that read the ring at the stop
//! and then saw the record arrive would read the ui as having moved while it
//! was held. so a stopping thread waits — [`flush`], with the GIL released —
//! until everything queued before its stop has been written or counted, and
//! then reports the stop: a record written before a stop arrives before it,
//! which is the order the records happened in. a stopping thread is about to
//! wait for the engine anyway, so this costs the program nothing it was not
//! already paying
//!
//! ## what does wait, and is said to
//!
//! the writer polls for room on the connection before it writes, so an engine
//! that is not reading leaves it waiting in `poll` — where a stand-down reaches
//! it at once — rather than inside a write. two cases remain, and both are
//! about a frame rather than the program: a write that is in flight when a
//! fork begins is finished before the writer can be joined, because a
//! length-prefixed frame abandoned half-written would desynchronise the whole
//! connection, so a fork begun at that instant waits for the engine's next
//! read; and at the program's end what is still queued is written before the
//! end is reported — [`finish`] — because a record dropped on the way out
//! would be one nothing counted. that holds the **exit** for the engine's next
//! read, and nothing of the program, which is over by then

use std::collections::VecDeque;
use std::io;
#[cfg(unix)]
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use bpd_core::TraceRecord;
use bpd_protocol::message::FromAgent;

use crate::attach;
use crate::cells::ForkCell;
use crate::own_thread::{self, OwnThread};

/// how many records wait to be written before the oldest is dropped
///
/// a run record renders to a few hundred bytes, so this is a fraction of a
/// megabyte of the program's memory at most — and it is what a client that
/// reads the stream promptly never fills
pub(crate) const CAPACITY: usize = 1024;

/// whether announced records are forwarded
///
/// read by the audit hook on every trace event, so it is an atomic and
/// nothing else: the hook runs on the program's thread with the GIL held, and
/// a lock there would put the whole ui behind whoever holds it. a forked child
/// starts with it off — see [`abandon`]
static WATCHING: AtomicBool = AtomicBool::new(false);

/// whether announced records are being forwarded now
pub(crate) fn watching() -> bool {
    WATCHING.load(Ordering::Relaxed)
}

/// the records waiting to be written, oldest first, and what fell off the
/// front of them
///
/// the rule is the whole of the type: a push onto a full queue drops the
/// oldest and counts it, and a take hands out the oldest with the count that
/// accrued before it, which starts again at zero. the count belongs to the
/// next record taken and not to the one being pushed, because it is a gap in
/// the stream a reader sees **before** the record that follows it
pub(crate) struct Outbound {
    queue: VecDeque<TraceRecord>,
    /// records dropped since the last take
    dropped: u64,
    /// every record ever pushed
    pushed: u64,
    /// every record that has left the queue for good: written, or dropped
    ///
    /// records leave in the order they were pushed, one at a time, so a
    /// thread that noted `pushed` and waits for `delivered` to reach it has
    /// waited for exactly what was queued before it looked
    delivered: u64,
    /// the writer is to return the moment it can
    ///
    /// here rather than in [`Writer`] because it is the writer's condition
    /// variable that says so, and that waits on this lock
    stopping: bool,
    /// woken by a push and by a stand-down
    ///
    /// in the cell rather than beside it, so that a forked child's replacement
    /// queue comes with a condition variable of its own — one that has never
    /// been associated with the mutex the child abandoned
    wake: Arc<Condvar>,
    /// woken whenever `delivered` grows — what [`flush`] waits on
    drained: Arc<Condvar>,
}

impl Outbound {
    fn new() -> Self {
        Self {
            queue: VecDeque::with_capacity(CAPACITY),
            dropped: 0,
            pushed: 0,
            delivered: 0,
            stopping: false,
            wake: Arc::new(Condvar::new()),
            drained: Arc::new(Condvar::new()),
        }
    }

    /// queue a record, dropping the oldest when there is no room
    pub(crate) fn push(&mut self, record: TraceRecord) {
        if self.queue.len() >= CAPACITY {
            self.queue.pop_front();
            self.dropped += 1;
            self.delivered += 1;
            self.drained.notify_all();
        }
        self.queue.push_back(record);
        self.pushed += 1;
        self.wake.notify_one();
    }

    /// the oldest record, with how many were dropped before it
    ///
    /// `None` when nothing is queued; a count that has accrued stays for the
    /// next record, which is the one it precedes
    pub(crate) fn take(&mut self) -> Option<(TraceRecord, u64)> {
        let record = self.queue.pop_front()?;
        Some((record, std::mem::take(&mut self.dropped)))
    }

    /// the record last taken has been written
    pub(crate) fn written(&mut self) {
        self.delivered += 1;
        self.drained.notify_all();
    }
}

/// the queue, in a cell a forked child replaces without locking it
///
/// a program thread can be inside [`queue`] holding it while another thread of
/// the same process calls `os.fork()` — there is no GIL on a free-threaded
/// build to keep the two apart
static OUTBOUND: ForkCell<Outbound> = ForkCell::new(Outbound::new);

/// what the writer thread holds while it runs
///
/// the wakeup is the reading half of a pair [`stand_down`] writes a byte into,
/// and the connection is a handle of its own on the control socket, polled for
/// room and never written: [`attach::send`] is the one writer of frames
struct Writing {
    #[cfg(unix)]
    connection: TcpStream,
    #[cfg(unix)]
    wakeup: UnixStream,
}

/// who is writing the stream, and what is keeping them off it
///
/// the mirror of the reader's own state in [`crate::attach`], for the same
/// transitions: a thread that holds the connection, a fork that has it stood
/// down, and the count that puts it back when the last concurrent fork is
/// through. **nothing under this lock needs the interpreter**
struct Writer {
    /// whether a writer is to be on the process at all
    ///
    /// set by the first watch and never by a fork, so a fork that finds it set
    /// and the thread stood down knows to start one again
    wanted: bool,
    /// the thread that holds the connection's handle, which hands it back when
    /// it stands down
    running: Option<OwnThread<Writing>>,
    /// the handle while no thread holds it
    idle: Option<Writing>,
    /// how many forks are between their `before` handler and their
    /// `after_in_parent` one
    #[cfg(unix)]
    forking: usize,
    /// the writing half of the wakeup pair, or nothing before the first writer
    #[cfg(unix)]
    waker: Option<UnixStream>,
}

const fn no_writer() -> Writer {
    Writer {
        wanted: false,
        running: None,
        idle: None,
        #[cfg(unix)]
        forking: 0,
        #[cfg(unix)]
        waker: None,
    }
}

/// in a [`ForkCell`] for the reason the reader's state is: a fork can land
/// while a concurrent fork holds it
static WRITER: ForkCell<Writer> = ForkCell::new(no_writer);

/// every descriptor the stream opened in the debuggee
///
/// the connection's handle and the two halves of the wakeup pair, kept by
/// number for the reason [`crate::attach`] keeps its own: a forked child has to
/// close all three and can reach none of them. `-1` before the first writer
#[cfg(unix)]
static DESCRIPTORS: [AtomicI32; 3] = [AtomicI32::new(-1), AtomicI32::new(-1), AtomicI32::new(-1)];

fn lock<T>(mutex: &'static Mutex<T>) -> MutexGuard<'static, T> {
    mutex.lock().expect(
        "nothing panics while holding a stream lock: every path through one is a push, a take, or a field read",
    )
}

fn outbound() -> MutexGuard<'static, Outbound> {
    lock(OUTBOUND.get())
}

fn writer() -> MutexGuard<'static, Writer> {
    lock(WRITER.get())
}

/// start or stop forwarding announced records, and say what is set
///
/// turning it on puts the writer on the process, if none is; turning it off
/// leaves the writer idle rather than joining it, because a watch that is
/// switched on and off around every step should not cost a thread each time.
/// nothing here needs the runtime module to have been imported: watching is
/// an interest in records to come
///
/// # errors
///
/// when the writer's handle on the connection or its wakeup could not be
/// opened, or the thread could not be started. the flag is left as it was, so
/// what is set is what the answer would have said
pub(crate) fn watch(on: bool) -> io::Result<bool> {
    if on {
        ensure_writer()?;
    }
    WATCHING.store(on, Ordering::Relaxed);
    Ok(WATCHING.load(Ordering::Relaxed))
}

/// hand a rendered record to the writer
///
/// called from the audit hook, on the thread that appended the record, with
/// the GIL held. it takes the queue's lock for the length of one push
pub(crate) fn queue(record: TraceRecord) {
    outbound().push(record);
}

/// a writer is to be on the process, and is, unless a fork has it off
fn ensure_writer() -> io::Result<()> {
    let mut writer = writer();
    writer.wanted = true;
    if writer.running.is_some() {
        return Ok(());
    }
    #[cfg(unix)]
    if writer.forking > 0 {
        // the fork's `after_in_parent` handler starts it, because a thread
        // started inside the window is one cpython counts
        return Ok(());
    }
    start_writing(&mut writer)
}

/// hand the connection's handle to a thread of its own
///
/// the caller holds the lock for the whole transition, as the reader's own
/// start does
fn start_writing(writer: &mut Writer) -> io::Result<()> {
    let writing = match writer.idle.take() {
        Some(writing) => writing,
        #[cfg(unix)]
        None => Writing::open(writer)?,
        #[cfg(not(unix))]
        None => Writing::open()?,
    };
    let handle = own_thread::spawn("bpd-stream", move || write_records(writing))?;
    writer.running = Some(handle);
    Ok(())
}

impl Writing {
    /// open the handle and the wakeup pair, and record their numbers
    ///
    /// everything is opened before anything is kept, so a failure part way
    /// leaves nothing behind for a later attempt to leak
    #[cfg(unix)]
    fn open(writer: &mut Writer) -> io::Result<Self> {
        let (waker, wakeup) = UnixStream::pair()?;
        wakeup.set_nonblocking(true)?;
        let connection = attach::clone_connection()?;
        {
            use std::os::fd::AsRawFd as _;
            DESCRIPTORS[0].store(connection.as_raw_fd(), Ordering::Relaxed);
            DESCRIPTORS[1].store(waker.as_raw_fd(), Ordering::Relaxed);
            DESCRIPTORS[2].store(wakeup.as_raw_fd(), Ordering::Relaxed);
        }
        writer.waker = Some(waker);
        Ok(Self { connection, wakeup })
    }

    /// what a writer holds where there is no fork to stand down for
    ///
    /// nothing: there is no wakeup to reach it through and no room to poll
    /// for, so it writes through [`attach::send`] alone and a stop of it —
    /// which only [`finish`] asks for — waits for the write it is inside
    #[cfg(not(unix))]
    const fn open() -> io::Result<Self> {
        Ok(Self {})
    }

    /// wait until the connection can take a frame, or until this thread is to
    /// go
    #[cfg(unix)]
    fn awaited_room(&self) -> attach::Next {
        use std::os::fd::AsFd as _;

        attach::awaited(
            &self.wakeup,
            self.connection.as_fd(),
            rustix::event::PollFlags::OUT,
        )
    }
}

/// write records for as long as this thread holds the connection's handle
///
/// it hands the handle back rather than closing it, for the reason the reader
/// does: a stand-down for a fork, and the connection outlives this thread
fn write_records(writing: Writing) -> Writing {
    loop {
        {
            let mut outbound = outbound();
            while outbound.queue.is_empty() && !outbound.stopping {
                let wake = Arc::clone(&outbound.wake);
                outbound = wake.wait(outbound).expect(
                    "nothing panics while holding a stream lock: every path through one is a push, a take, or a field read",
                );
            }
            if outbound.stopping {
                return writing;
            }
        }

        // room first, then the record: a record taken and then not written
        // would have to go back in front of a queue that may have filled
        // meanwhile. only this thread takes, so what the wait above found is
        // still there
        #[cfg(unix)]
        if matches!(writing.awaited_room(), attach::Next::StandDown) {
            return writing;
        }

        let Some((record, dropped_before)) = outbound().take() else {
            unreachable!(
                "the writer of the trace stream is the only thread that takes from the \
                 queue, and it was woken for a record that is not there"
            );
        };
        attach::send(&FromAgent::Recomposed {
            record,
            dropped_before,
        });
        outbound().written();
    }
}

/// wait until everything queued so far has been written or counted
///
/// called by a thread about to report a stop, with the GIL released, so that
/// a record written before the stop reaches the engine before it — see the
/// module note. it returns at once when no writer is wanted: nothing waits on
/// a writer that is not there to catch up, and records are queued only while
/// one is
pub(crate) fn flush() {
    if attach::detached() {
        return;
    }
    let wanted = writer().wanted;
    if !wanted {
        return;
    }

    let mut outbound = outbound();
    let target = outbound.pushed;
    while outbound.delivered < target {
        let drained = Arc::clone(&outbound.drained);
        outbound = drained.wait(outbound).expect(
            "nothing panics while holding a stream lock: every path through one is a push, a take, or a field read",
        );
    }
}

/// stop the writer and take the handle back
///
/// the caller holds the writer's lock. `stopping` is set under the queue's
/// lock and the wakeup written after it, so a writer waiting on either is
/// reached; one inside a write finishes the frame first — see the module note
fn stop(writer: &mut Writer, handle: OwnThread<Writing>) {
    {
        let mut outbound = outbound();
        outbound.stopping = true;
        outbound.wake.notify_all();
    }

    #[cfg(unix)]
    {
        use std::io::Write as _;
        let waker = writer
            .waker
            .as_mut()
            .unwrap_or_else(|| unreachable!("a writer is started with its wakeup pair beside it"));
        if let Err(error) = waker.write_all(&[0]) {
            attach::fatal(&format!(
                "the writer of the trace stream could not be told to stand down: \
                 {error}. it cannot be joined, and a fork with it still on the \
                 process would change what the program records"
            ));
        }
    }

    match handle.join() {
        Ok(writing) => {
            #[cfg(unix)]
            let writing = {
                let mut writing = writing;
                attach::drain_wakeup(&mut writing.wakeup);
                writing
            };
            writer.idle = Some(writing);
        }
        // a panic in the agent is a broken invariant, and this is the one
        // place it would otherwise be swallowed
        Err(_) => attach::fatal(
            "the writer of the trace stream panicked. records the ui runtime \
             wrote would go unsent and uncounted, and the program is not being \
             left to run undebugged",
        ),
    }
    outbound().stopping = false;
}

/// take the writer off the process, because it is about to fork
///
/// called from `os.register_at_fork(before=…)`, after the reader's own
/// stand-down, on the thread that is forking and with the GIL held — nothing
/// here needs the interpreter
#[cfg(unix)]
pub(crate) fn stand_down() {
    if attach::detached() {
        return;
    }

    let mut writer = writer();
    writer.forking += 1;
    let Some(handle) = writer.running.take() else {
        // never wanted, or another fork is already in flight and has taken
        // it off
        return;
    };
    stop(&mut writer, handle);
}

/// put the writer back, now that the fork is over
///
/// called from `os.register_at_fork(after_in_parent=…)`, in the process that
/// did the forking. a watch turned on inside the window is started here too:
/// `wanted` is what says so
#[cfg(unix)]
pub(crate) fn resume_writing() {
    if attach::detached() {
        return;
    }

    let mut writer = writer();
    assert!(
        writer.forking > 0,
        "every fork's `after_in_parent` handler follows its own `before` one"
    );
    writer.forking -= 1;
    if writer.forking > 0 || !writer.wanted {
        return;
    }

    if let Err(error) = start_writing(&mut writer) {
        attach::fatal(&format!(
            "the writer of the trace stream could not be started again after a \
             fork: {error}. records the ui runtime writes would go unsent and \
             uncounted, and the program is not being left to run undebugged"
        ));
    }
}

/// give up the stream this process was forked holding
///
/// the watch goes off, the three descriptors are closed by number, and the
/// queue and the writer's state are replaced rather than emptied — with an
/// atomic store each, and no lock taken, for the reasons [`crate::cells`]
/// gives. what the parent had queued is the parent's: a record of its ui
/// forwarded from here would be a record the child's session never saw written
#[cfg(unix)]
pub(crate) fn abandon() {
    WATCHING.store(false, Ordering::SeqCst);
    attach::close_by_number(&DESCRIPTORS);
    OUTBOUND.abandon();
    WRITER.abandon();
}

/// the program is over: write what is still queued, on this thread
///
/// the watch goes off first — a record announced by a thread still running
/// during finalization would be queued for a writer that is gone — and then
/// the writer is stopped, so that one thread writes the rest, in order, and
/// nothing restarts it: `wanted` is cleared. a record still queued when the
/// connection closes would be one that was neither sent nor counted, and the
/// count is the whole promise of the stream. this is the one place the stream
/// waits on the engine from the program's own thread, and what it holds is the
/// exit
pub(crate) fn finish() {
    if attach::detached() {
        return;
    }
    WATCHING.store(false, Ordering::Relaxed);

    {
        let mut writer = writer();
        writer.wanted = false;
        if let Some(handle) = writer.running.take() {
            stop(&mut writer, handle);
        }
    }

    loop {
        // taken in a statement of its own, so the queue's guard is gone before
        // the write: a `while let` over the take would hold it through the
        // body, and the write's own bookkeeping takes the same lock
        let taken = outbound().take();
        let Some((record, dropped_before)) = taken else {
            return;
        };
        attach::send(&FromAgent::Recomposed {
            record,
            dropped_before,
        });
        outbound().written();
    }
}

#[cfg(test)]
mod tests {
    use super::{CAPACITY, Outbound};
    use bpd_core::TraceRecord;

    fn frame(number: u64) -> TraceRecord {
        TraceRecord::Frame {
            runtime: 0,
            frame: number,
            runs: 1,
            skips: 0,
            compose_ns: 0,
            commit_ns: 0,
        }
    }

    fn frame_of(record: &TraceRecord) -> u64 {
        match record {
            TraceRecord::Frame { frame, .. } => *frame,
            other => panic!("the queue was handed frame records, and this is {other:?}"),
        }
    }

    #[test]
    fn a_full_queue_drops_the_oldest_and_counts_it_onto_the_next_record_taken() {
        let mut outbound = Outbound::new();
        let pushed = u64::try_from(CAPACITY).expect("the capacity is a small number") + 3;
        for number in 0..pushed {
            outbound.push(frame(number));
        }
        assert_eq!(outbound.queue.len(), CAPACITY, "the bound holds");

        assert_eq!(outbound.pushed, pushed);
        assert_eq!(
            outbound.delivered, 3,
            "a dropped record has left the queue for good, and counts as delivered"
        );

        // the three oldest went, and the count rides the oldest that is left
        let (record, dropped_before) = outbound.take().expect("the queue is full");
        assert_eq!(frame_of(&record), 3);
        assert_eq!(dropped_before, 3);
        outbound.written();
        assert_eq!(outbound.delivered, 4);

        // and it rides nothing else: the gap is before that record alone
        let (record, dropped_before) = outbound.take().expect("the queue holds more");
        assert_eq!(frame_of(&record), 4);
        assert_eq!(dropped_before, 0);
        outbound.written();

        // everything after is in the order it was pushed, none missing
        let mut next = 5;
        while let Some((record, dropped_before)) = outbound.take() {
            assert_eq!(frame_of(&record), next);
            assert_eq!(dropped_before, 0);
            outbound.written();
            next += 1;
        }
        assert_eq!(
            next, pushed,
            "the newest record pushed is the last one taken"
        );
        assert_eq!(
            outbound.delivered, outbound.pushed,
            "every record pushed was written or dropped, which is what a flush waits for"
        );
    }

    #[test]
    fn a_queue_that_never_filled_hands_every_record_out_with_no_gap() {
        let mut outbound = Outbound::new();
        assert!(outbound.take().is_none(), "nothing was pushed");
        for number in 0..7 {
            outbound.push(frame(number));
        }
        for number in 0..7 {
            let (record, dropped_before) = outbound.take().expect("seven were pushed");
            assert_eq!(frame_of(&record), number);
            assert_eq!(dropped_before, 0);
        }
        assert!(outbound.take().is_none());
    }
}
