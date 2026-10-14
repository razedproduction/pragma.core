use std::sync::PoisonError;

use crate::primitives::{AtomicUsize, Condvar, Mutex, Ordering};

/// A count-down latch: `wait` blocks until the counter reaches zero.
///
/// The base primitive for `join`/`scope`: the waiting thread waits for N tasks to finish.
/// The fast path (`is_released`) is a single atomic load without locking.
///
/// Unlike [`AtomicCounter`](crate::AtomicCounter), the count is fixed at creation and only goes
/// down: once open, the latch stays open.
///
/// ```
/// use pragma_sync::CountLatch;
/// use std::sync::Arc;
///
/// // The main thread waits until both loaders have finished.
/// let loaded = Arc::new(CountLatch::new(2));
/// let loaders: Vec<_> = (0..2)
///     .map(|_| {
///         let loaded = Arc::clone(&loaded);
///         std::thread::spawn(move || {
///             // ... load something ...
///             loaded.count_down();
///         })
///     })
///     .collect();
/// loaded.wait();
/// # for l in loaders { l.join().unwrap(); }
/// assert_eq!(loaded.remaining(), 0);
/// loaded.count_down(); // no underflow on an open latch
/// assert!(loaded.is_released());
/// ```
#[derive(Debug)]
pub struct CountLatch {
    remaining: AtomicUsize,
    lock: Mutex<()>,
    cvar: Condvar,
}

impl CountLatch {
    /// A latch waiting for `count` calls to [`count_down`](Self::count_down).
    #[must_use]
    pub fn new(count: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(count),
            lock: Mutex::new(()),
            cvar: Condvar::new(),
        }
    }

    /// How many `count_down` calls are still expected.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.remaining.load(Ordering::Acquire)
    }

    /// Whether the latch is open (the counter is zero).
    #[must_use]
    pub fn is_released(&self) -> bool {
        self.remaining() == 0
    }

    /// Decrements the counter; on reaching zero wakes all waiters.
    /// A call on an already open latch does nothing (no underflow).
    pub fn count_down(&self) {
        let mut current = self.remaining.load(Ordering::Acquire);
        // A CAS loop instead of `fetch_update`/`try_update`: works the same on the MSRV and newer toolchains.
        let reached_zero = loop {
            let Some(next) = current.checked_sub(1) else {
                return;
            };
            match self.remaining.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break next == 0,
                Err(actual) => current = actual,
            }
        };
        if reached_zero {
            // Taking the mutex rules out the "checked the counter -> fell asleep -> missed notify" race.
            let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
            self.cvar.notify_all();
        }
    }

    /// Blocks the current thread until the latch opens.
    pub fn wait(&self) {
        if self.is_released() {
            return;
        }
        let mut guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        while !self.is_released() {
            guard = self
                .cvar
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn zero_latch_is_released() {
        let latch = CountLatch::new(0);
        latch.wait();
        latch.count_down();
        assert_eq!(latch.remaining(), 0);
    }

    #[test]
    fn many_threads_release_once() {
        const N: usize = 64;
        let latch = Arc::new(CountLatch::new(N));
        let handles: Vec<_> = (0..N)
            .map(|_| {
                let latch = Arc::clone(&latch);
                std::thread::spawn(move || latch.count_down())
            })
            .collect();
        latch.wait();
        for h in handles {
            h.join().unwrap();
        }
        assert!(latch.is_released());
    }
}
