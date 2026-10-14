//! M5: growing a `HandlePool` without memory yields `BaseError::OutOfMemory`, not `abort`;
//! `remove` and insertion into a freed slot do not allocate at all.
//!
//! A separate test binary with a failing global allocator (DECISIONS.md #37).
// Test code: a panic is a test failure.
#![allow(clippy::unwrap_used, clippy::panic, clippy::arithmetic_side_effects)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use pragma_base::{BaseError, HandlePool};

/// The system allocator, refusing every allocation on a thread that has `FAIL` raised.
struct Failing;

thread_local! {
    // Const initialization: accessing it from the allocator does not allocate by itself.
    static FAIL: Cell<bool> = const { Cell::new(false) };
}

fn failing() -> bool {
    FAIL.try_with(Cell::get).unwrap_or(false)
}

// SAFETY: the methods delegate to `System` with the same arguments; failure means returning null,
// which the `GlobalAlloc::alloc`/`realloc` contract explicitly allows (the caller must handle null).
unsafe impl GlobalAlloc for Failing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if failing() {
            return std::ptr::null_mut();
        }
        // SAFETY: the `alloc` contract is passed through from the caller unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if failing() {
            return std::ptr::null_mut();
        }
        // SAFETY: `ptr` was allocated by this same allocator (i.e. `System`); the contract is passed through as is.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was allocated by `System` through this allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Failing = Failing;

/// Runs `f` while the allocator refuses on this thread.
fn without_memory<R>(f: impl FnOnce() -> R) -> R {
    FAIL.with(|fail| fail.set(true));
    let result = f();
    FAIL.with(|fail| fail.set(false));
    result
}

#[test]
fn growth_without_memory_is_an_error_and_pool_stays_usable() {
    let mut pool: HandlePool<u64> = HandlePool::new();
    assert!(matches!(
        without_memory(|| pool.insert(1)),
        Err(BaseError::OutOfMemory { slots: 0, .. })
    ));
    assert!(pool.is_empty());

    // With memory available the pool grows as usual.
    let handles: Vec<_> = (0..100).map(|v| pool.insert(v).unwrap()).collect();

    // Freeing and reusing a slot does not allocate.
    let reused = without_memory(|| {
        let value = pool.remove(handles[10]).unwrap();
        let reused = pool.insert(value + 1000).unwrap();
        (value, reused)
    });
    assert_eq!(reused.0, 10);
    assert_eq!(pool.get(reused.1), Some(&1010));

    // Sooner or later growth needs an allocation: an error carrying the current size, no `abort`.
    let error = without_memory(|| (0..1_000_000_u64).find_map(|v| pool.insert(v).err()));
    let Some(BaseError::OutOfMemory { slots, .. }) = error else {
        panic!("expected OutOfMemory, got {error:?}");
    };
    assert_eq!(slots, pool.len());

    // After the failure the pool is intact: all live handles are in place and growth works again.
    assert_eq!(pool.get(handles[0]), Some(&0));
    assert!(pool.insert(7).is_ok());
}
