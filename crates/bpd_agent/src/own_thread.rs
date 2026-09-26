//! a thread of the agent's own, joined all the way to the kernel
//!
//! the agent takes its threads off the process before a fork, because cpython
//! counts the process's operating system threads at `os.fork()` and a program
//! can record the warning that count raises — see [`crate::forks`]. `join` is
//! what was meant to say a thread is gone, and **on linux it says so early**.
//! `pthread_join` returns when the kernel clears the thread's id at exit, and
//! the kernel takes the thread out of the count cpython reads from
//! `/proc/self/stat` a little later, in the same exit. measured in a container
//! with two cpus: 53 of 20000 joins were still counted the moment `join`
//! returned, 158 of 20000 with the cpus busy, and the count took up to 900
//! reads to settle. ci found it as a debuggee on a loaded runner recording a
//! `DeprecationWarning` for its own fork that a bare run does not
//!
//! so on linux a join also waits for `/proc/self/task/<tid>` to be gone. the
//! kernel lowers the count before it unhashes the thread, under one lock, so a
//! thread whose directory has gone is one cpython no longer counts. a `/proc`
//! that is not mounted answers the same way, and that is right rather than
//! lucky: cpython reads its count from `/proc` too, and without it counts only
//! the threads `threading` knows about, which an agent thread never is
//!
//! it is linux only because linux is where it was measured. the same fork
//! suite passes on every macos job, and the count macos gives cpython comes
//! from `task_threads` rather than from anything a join could run ahead of

use std::io;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread::JoinHandle;

/// a running thread of the agent's, and what it hands back when it returns
pub(crate) struct OwnThread<T> {
    handle: JoinHandle<T>,
    /// the kernel's id for it, stored by the thread before anything else
    #[cfg(target_os = "linux")]
    kernel_id: Arc<AtomicI32>,
}

/// start `work` on a thread of the agent's own, named `name`
///
/// # errors
///
/// when the operating system would not start a thread
pub(crate) fn spawn<T, F>(name: &str, work: F) -> io::Result<OwnThread<T>>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    #[cfg(target_os = "linux")]
    let kernel_id = Arc::new(AtomicI32::new(0));
    #[cfg(target_os = "linux")]
    let recorded = Arc::clone(&kernel_id);

    let handle = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            #[cfg(target_os = "linux")]
            recorded.store(
                rustix::thread::gettid().as_raw_nonzero().get(),
                Ordering::SeqCst,
            );
            work()
        })?;

    Ok(OwnThread {
        handle,
        #[cfg(target_os = "linux")]
        kernel_id,
    })
}

impl<T> OwnThread<T> {
    /// wait for the thread to return, and for the kernel to have let it go
    ///
    /// `Err` is the thread having panicked, exactly as [`JoinHandle::join`]
    /// says it
    pub(crate) fn join(self) -> std::thread::Result<T> {
        let returned = self.handle.join();
        #[cfg(target_os = "linux")]
        gone(self.kernel_id.load(Ordering::SeqCst));
        returned
    }
}

/// wait until the kernel no longer has a thread by this id in this process
#[cfg(target_os = "linux")]
fn gone(kernel_id: i32) {
    assert!(
        kernel_id > 0,
        "a joined thread stored its kernel id as the first thing it did"
    );
    let task = format!("/proc/self/task/{kernel_id}");
    loop {
        match std::fs::symlink_metadata(&task) {
            Ok(_) => std::thread::yield_now(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => crate::attach::fatal(&format!(
                "{task} could not be read ({error}), so whether a thread the \
                 agent joined is still counted cannot be known, and a fork now \
                 could change what the program records"
            )),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_joined_thread_is_no_longer_one_the_kernel_has() {
        // two thousand rounds, because the window is a fraction of a percent
        // of joins wide: a plain `join` in place of this one fails here with
        // near certainty on a machine with any load at all
        for _ in 0..2000 {
            let thread = spawn("bpd-test", || {
                rustix::thread::gettid().as_raw_nonzero().get()
            })
            .expect("a thread starts");
            let kernel_id = thread.join().expect("the thread returned");
            assert!(
                !std::path::Path::new(&format!("/proc/self/task/{kernel_id}")).exists(),
                "thread {kernel_id} was joined and the kernel still has it"
            );
        }
    }
}
