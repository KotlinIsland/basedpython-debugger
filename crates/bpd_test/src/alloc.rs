//! deterministic allocation accounting, for performance assertions that do not
//! depend on a clock
//!
//! wall-clock on a shared runner varies by more than the effects worth
//! catching, so a timing gate in CI is a gate that teaches people to ignore it.
//! allocation counts do not vary: a hot path that allocated zero times
//! yesterday and once today has regressed, on any machine, every time
//!
//! counting is per thread, so tests measuring different things in parallel do
//! not see each other's allocations
//!
//! to use it, the **test binary** installs the allocator:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOCATOR: bpd_test::alloc::Counting = bpd_test::alloc::Counting;
//! ```
//!
//! without that line [`measure`] reports zero for everything, so
//! [`Allocations::assert_measured`] exists to catch a test that silently
//! measured nothing

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    // `const` initialisers keep thread local access from allocating, which
    // would make the allocator re-enter itself on first touch
    static COUNT: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
    static ENABLED: Cell<bool> = const { Cell::new(false) };
}

/// what happened on the heap while a closure ran
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocations {
    /// how many times the allocator was asked for memory, counting a
    /// reallocation as one — a `Vec` that grows twice reports two
    pub count: u64,
    /// how many bytes those requests asked for
    pub bytes: u64,
}

impl Allocations {
    /// fail when nothing was recorded at all
    ///
    /// a zero count is the assertion most of these tests want to make, and it
    /// is also exactly what a test binary that forgot to install the allocator
    /// reports. this distinguishes the two: run something that must allocate,
    /// and check the harness noticed
    pub fn assert_measured(self) {
        assert!(
            self.count > 0,
            "no allocations were recorded, which usually means the test binary \
             did not install `bpd_test::alloc::Counting` as its \
             `#[global_allocator]`"
        );
    }
}

/// the allocator that does the counting
///
/// it delegates every operation to the system allocator and only observes
pub struct Counting;

#[expect(
    unsafe_code,
    reason = "implementing GlobalAlloc is unsafe by definition. the body of \
              every method is a delegation to `System`, with a counter bump \
              that cannot allocate"
)]
// SAFETY: every method forwards to `System` with the same arguments it was
// given, so the allocator contract is whatever `System` guarantees. `record`
// touches only `Cell<u64>` thread locals with `const` initialisers, which
// neither allocate nor run destructors, so it cannot re-enter the allocator
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }
}

fn record(size: usize) {
    // `try_with` rather than `with`: during thread teardown the thread local is
    // gone, and an allocation then is not something a test is measuring
    let enabled = ENABLED.try_with(Cell::get).unwrap_or(false);
    if !enabled {
        return;
    }
    COUNT.try_with(|count| count.set(count.get() + 1)).ok();
    BYTES
        .try_with(|bytes| bytes.set(bytes.get() + size as u64))
        .ok();
}

/// run `body`, reporting what it allocated on this thread
///
/// # panics
///
/// if called from inside another `measure` on the same thread. nested
/// measurements would attribute the inner allocations to both, and a count
/// that is quietly wrong is worse than one that is missing
pub fn measure<T>(body: impl FnOnce() -> T) -> (T, Allocations) {
    assert!(
        !ENABLED.with(Cell::get),
        "measure is already running on this thread"
    );

    COUNT.with(|count| count.set(0));
    BYTES.with(|bytes| bytes.set(0));
    ENABLED.with(|enabled| enabled.set(true));

    let produced = body();

    ENABLED.with(|enabled| enabled.set(false));
    let allocations = Allocations {
        count: COUNT.with(Cell::get),
        bytes: BYTES.with(Cell::get),
    };

    (produced, allocations)
}
