//! the session's shared state, in cells a forked child can replace
//!
//! a fork keeps **only the calling thread**, so a lock another thread held at
//! the instant of the fork is one the child's copy would wait on for ever. on a
//! gil build that is narrow: `os.fork()` holds the GIL, and so does every
//! program thread that reaches [`crate::stops::enter`] or
//! [`crate::attach::send`]. on a free-threaded build there is no GIL to
//! serialise them — a thread can be inside either of those while another calls
//! `os.fork()` — and free-threaded builds are a first-class target, so the
//! argument that holds on one build is not the argument
//!
//! leaning on `os.fork()`'s stop-the-world instead would be guessing about
//! interpreter internals, which is the thing this project refuses in the one
//! place a wrong guess is a debugger that hangs
//!
//! so the state a forked child has to be able to use again does not live in a
//! `static Mutex<T>` directly. it lives behind an atomic pointer to one, and a
//! child **replaces the whole cell** with a single store rather than taking
//! anything. the shape is the one `crate::events` already uses to publish its
//! armed set: an owned box behind an `AtomicPtr`, read without a lock by the
//! one process that cannot afford to take one
//!
//! ## the abandoned cell leaks, deliberately
//!
//! [`ForkCell::abandon`] drops the pointer and does not free what it pointed
//! at. two reasons, either of which is enough:
//!
//! - the `Mutex` can be **locked**, by a thread the fork did not copy.
//!   destroying a locked mutex is undefined on any platform whose mutex has
//!   something to destroy, and the values inside it would be dropped by a
//!   thread that never took it
//! - what the cells hold owns descriptor **numbers**
//!   [`crate::attach::detach`] has already closed. dropping a `TcpStream` for
//!   one of those would `close(2)` a number this process has since recycled,
//!   which is the debugger closing a file the program opened
//!
//! what leaks is one box per fork, in a process that has just been made and is
//! about to run a program that would have allocated it anyway. taking it back
//! would cost a correctness argument nobody can make

use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};

/// a `static Mutex<T>` that a forked child can give up without locking it
///
/// `T: Send` is on the type rather than on the methods on purpose. the cell
/// hands out `&'static Mutex<T>` from any thread, which is only sound when
/// `Mutex<T>` is `Sync` — and `AtomicPtr` is `Sync` whatever it points at, so
/// without this bound the auto-derived `Sync` would be a lie for a `T` that is
/// not `Send`
pub(crate) struct ForkCell<T: Send + 'static> {
    /// the cell in use, or null before the first one and after a fork gave one
    /// up
    live: AtomicPtr<Mutex<T>>,
    /// what a cell holds when it is new, which is also what one holds in a
    /// forked child
    fresh: fn() -> T,
}

impl<T: Send + 'static> ForkCell<T> {
    /// a cell that has not been used yet
    pub(crate) const fn new(fresh: fn() -> T) -> Self {
        Self {
            live: AtomicPtr::new(ptr::null_mut()),
            fresh,
        }
    }

    /// the cell as it is now, making one if this process has none
    ///
    /// the reference is `'static` because no box this ever hands out is freed:
    /// the live one lives for the life of the process, and an abandoned one
    /// leaks for the reasons in the module note
    pub(crate) fn get(&'static self) -> &'static Mutex<T> {
        let live = self.live.load(Ordering::Acquire);
        if live.is_null() {
            return self.install();
        }
        Self::borrow(live)
    }

    /// make the first cell of this process, or lose the race to make it
    ///
    /// cold because it runs twice in an ordinary session — once for the writing
    /// end and once for the stop registry — against a lock that is taken on
    /// every event the debuggee reports
    #[cold]
    fn install(&'static self) -> &'static Mutex<T> {
        let candidate = Box::into_raw(Box::new(Mutex::new((self.fresh)())));
        match self.live.compare_exchange(
            ptr::null_mut(),
            candidate,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Self::borrow(candidate),
            Err(installed) => {
                // SAFETY: this box came from `Box::into_raw` on the line above
                // and the compare-exchange failed, so it was never stored and
                // no other thread has ever been able to reach it. this is the
                // one box the cell frees, and it frees it before anybody could
                // have borrowed it
                #[expect(
                    unsafe_code,
                    reason = "the loser of an install race owns a box nothing \
                              else can name, and dropping it is what keeps the \
                              race from leaking on every call — see above"
                )]
                drop(unsafe { Box::from_raw(candidate) });
                Self::borrow(installed)
            }
        }
    }

    /// give up the cell this process was forked holding
    ///
    /// **takes no lock**, which is the whole point: it runs in
    /// `os.register_at_fork(after_in_child=…)`, where a lock another thread
    /// held at the instant of the fork would never be released. the next
    /// [`Self::get`] makes a fresh one
    ///
    /// what it gave up is not freed — see the module note
    #[cfg(unix)]
    pub(crate) fn abandon(&'static self) {
        self.live.store(ptr::null_mut(), Ordering::Release);
    }

    /// SAFETY: every non-null value `live` holds came from `Box::into_raw` in
    /// [`Self::install`], and nothing ever frees a box after storing it. so a
    /// pointer read out of `live` names a live `Mutex<T>` for the rest of the
    /// process, which is what `'static` claims
    #[expect(
        unsafe_code,
        reason = "an owned box behind an atomic pointer is what makes a cell \
                  replaceable from a fork handler without a lock — see above"
    )]
    fn borrow(cell: *mut Mutex<T>) -> &'static Mutex<T> {
        unsafe { &*cell }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::ForkCell;

    fn empty() -> Vec<u32> {
        Vec::new()
    }

    static CELL: ForkCell<Vec<u32>> = ForkCell::new(empty);
    static LOCKED: ForkCell<Vec<u32>> = ForkCell::new(empty);

    #[test]
    fn a_cell_is_the_same_one_every_time_until_it_is_abandoned() {
        let first = CELL.get();
        first
            .lock()
            .expect("nothing has poisoned a cell of its own")
            .push(1);
        assert!(std::ptr::eq(first, CELL.get()));
        assert_eq!(
            *CELL
                .get()
                .lock()
                .expect("nothing has poisoned a cell of its own"),
            vec![1]
        );

        CELL.abandon();

        let replacement = CELL.get();
        assert!(
            !std::ptr::eq(first, replacement),
            "abandoning a cell has to replace it. a child that kept the one it \
             inherited would be reading its parent's state"
        );
        assert!(
            replacement
                .lock()
                .expect("a fresh cell has never been locked by anyone")
                .is_empty(),
            "the replacement starts as a new cell does, not as the one it \
             replaced"
        );
    }

    /// the case the whole type exists for: the cell is **locked** when it is
    /// given up
    ///
    /// in a forked child the thread holding it did not survive, so the lock is
    /// held for ever. this holds it from a thread that is still running, which
    /// is the same thing from the abandoning thread's point of view: if
    /// `abandon` or the `get` after it touched the old cell at all, this would
    /// never return
    #[test]
    fn a_cell_that_is_locked_when_it_is_abandoned_is_replaced_anyway() {
        let held = LOCKED.get();
        let guard = held.lock().expect("nothing has poisoned a cell of its own");

        LOCKED.abandon();

        let replacement = LOCKED.get();
        assert!(!std::ptr::eq(held, replacement));
        assert!(
            replacement
                .lock()
                .expect("a fresh cell has never been locked by anyone")
                .is_empty()
        );

        // the abandoned cell is leaked rather than freed, so the guard taken
        // before it was given up is still a live borrow of live memory
        assert!(guard.is_empty());
    }
}
