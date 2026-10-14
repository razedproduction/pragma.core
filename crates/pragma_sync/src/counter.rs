use std::sync::PoisonError;
use std::time::Duration;

use crossbeam_utils::Backoff;

use crate::SyncError;
use crate::primitives::{AtomicUsize, Condvar, Mutex, Ordering};

/// How often a waiter in [`AtomicCounter::wait_while_helping`] rechecks whether work has appeared
/// while it sleeps on the counter.
const HELP_RECHECK: Duration = Duration::from_micros(500);

/// A counter of unfinished tasks that can be waited on until it reaches zero.
///
/// Instead of a handle per task: a group of tasks increments the counter on creation and decrements
/// it on completion, and the waiter waits for a single "counter reached zero" event.
///
/// ```
/// use pragma_sync::AtomicCounter;
/// use std::sync::Arc;
///
/// let pending = Arc::new(AtomicCounter::new(0));
/// let threads: Vec<_> = (0..4)
///     .map(|_| {
///         pending.increment()?; // before the task starts, never inside it
///         let pending = Arc::clone(&pending);
///         Ok(std::thread::spawn(move || {
///             // ... the task's work ...
///             pending.decrement();
///         }))
///     })
///     .collect::<Result<_, pragma_sync::SyncError>>()?;
/// pending.wait();
/// assert!(pending.is_zero());
/// # for t in threads { t.join().unwrap(); }
/// # Ok::<(), pragma_sync::SyncError>(())
/// ```
///
/// Wakeup protocol: the waiter registers itself in `waiters` (an RMW) under the mutex and
/// rechecks the counter; after reaching zero, the decrementer also reads `waiters` via an RMW and
/// takes the mutex only if it is non-zero. RMWs on `waiters` are totally ordered, so at least
/// one side sees the other's action (loom model in `tests/loom.rs`). Without waiters, reaching
/// zero does not touch the mutex.
#[derive(Debug)]
pub struct AtomicCounter {
    count: AtomicUsize,
    waiters: AtomicUsize,
    lock: Mutex<()>,
    cvar: Condvar,
}

impl Default for AtomicCounter {
    fn default() -> Self {
        Self::new(0)
    }
}

impl AtomicCounter {
    /// A counter with the initial value `count`.
    #[must_use]
    pub fn new(count: usize) -> Self {
        Self {
            count: AtomicUsize::new(count),
            waiters: AtomicUsize::new(0),
            lock: Mutex::new(()),
            cvar: Condvar::new(),
        }
    }

    /// The current value.
    #[must_use]
    pub fn get(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Whether the counter is zero (all tasks have finished).
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.get() == 0
    }

    /// Increments the counter by one; an error instead of wrapping at `usize::MAX`.
    pub fn increment(&self) -> Result<(), SyncError> {
        let mut current = self.count.load(Ordering::Relaxed);
        // A CAS loop instead of `fetch_update`: that one is deprecated in 1.99, and `try_update` is not in the MSRV.
        loop {
            let next = current.checked_add(1).ok_or(SyncError::CounterOverflow)?;
            match self.count.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    /// Decrements the counter; returns `true` if it reached zero (and then wakes the waiters).
    /// On a zero counter does nothing and returns `false`.
    pub fn decrement(&self) -> bool {
        let mut current = self.count.load(Ordering::Acquire);
        let reached_zero = loop {
            let Some(next) = current.checked_sub(1) else {
                return false;
            };
            match self.count.compare_exchange_weak(
                current,
                next,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => break next == 0,
                Err(actual) => current = actual,
            }
        };
        // An RMW, not a load: reading via `fetch_add(0)` always sees the latest value in the
        // modification order of `waiters`, so the protocol stays correct even without the global
        // SeqCst order (loom models SeqCst as AcqRel and catches a lost wakeup in the load variant).
        if reached_zero && self.waiters.fetch_add(0, Ordering::SeqCst) > 0 {
            // The waiter holds the mutex from checking the counter until falling asleep in `wait`,
            // so a notify under the mutex cannot slip past it.
            let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
            self.cvar.notify_all();
        }
        reached_zero
    }

    /// Blocks the thread until the counter reaches zero.
    pub fn wait(&self) {
        self.wait_inner(None);
    }

    /// Waits for zero no longer than `timeout`; returns `true` if the counter is zero by then.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        self.wait_inner(Some(timeout))
    }

    /// Waits for zero, calling `help` in the meantime (help-while-wait).
    ///
    /// `help` must run one unit of someone else's work and return `true`, or return `false`
    /// if there is no work. Without work, the thread first spins with [`Backoff`], then sleeps on the
    /// counter in short intervals, asking `help` again between them.
    ///
    /// ```
    /// use pragma_sync::AtomicCounter;
    /// use std::cell::RefCell;
    ///
    /// // Work the waiter can run itself instead of sleeping.
    /// let queue = RefCell::new(vec![1, 2, 3]);
    /// let pending = AtomicCounter::new(3);
    /// let mut sum = 0;
    /// pending.wait_while_helping(|| match queue.borrow_mut().pop() {
    ///     Some(job) => {
    ///         sum += job;
    ///         pending.decrement();
    ///         true
    ///     }
    ///     None => false,
    /// });
    /// assert_eq!(sum, 6);
    /// ```
    pub fn wait_while_helping(&self, mut help: impl FnMut() -> bool) {
        let backoff = Backoff::new();
        while !self.is_zero() {
            if help() {
                backoff.reset();
            } else if backoff.is_completed() {
                self.wait_timeout(HELP_RECHECK);
            } else {
                backoff.snooze();
            }
        }
    }

    fn wait_inner(&self, timeout: Option<Duration>) -> bool {
        if self.is_zero() {
            return true;
        }
        let mut guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.waiters.fetch_add(1, Ordering::SeqCst);
        match timeout {
            None => {
                while self.count.load(Ordering::SeqCst) != 0 {
                    guard = self
                        .cvar
                        .wait(guard)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
            Some(timeout) => {
                if self.count.load(Ordering::SeqCst) != 0 {
                    guard = self
                        .cvar
                        .wait_timeout(guard, timeout)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            }
        }
        self.waiters.fetch_sub(1, Ordering::SeqCst);
        drop(guard);
        self.is_zero()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn increment_decrement_roundtrip() {
        let c = AtomicCounter::new(0);
        c.increment().unwrap();
        c.increment().unwrap();
        assert_eq!(c.get(), 2);
        assert!(!c.decrement());
        assert!(c.decrement());
        assert!(!c.decrement(), "decrement on zero is a no-op");
        assert!(c.is_zero());
    }

    #[test]
    fn overflow_is_an_error() {
        let c = AtomicCounter::new(usize::MAX);
        assert_eq!(c.increment(), Err(SyncError::CounterOverflow));
    }

    #[test]
    fn wait_wakes_on_zero() {
        const N: usize = 32;
        let c = Arc::new(AtomicCounter::new(N));
        let threads: Vec<_> = (0..N)
            .map(|_| {
                let c = Arc::clone(&c);
                std::thread::spawn(move || {
                    c.decrement();
                })
            })
            .collect();
        c.wait();
        assert!(c.is_zero());
        for t in threads {
            t.join().unwrap();
        }
    }

    #[test]
    fn helper_runs_until_zero() {
        let c = AtomicCounter::new(3);
        let mut calls = 0;
        c.wait_while_helping(|| {
            calls += 1;
            c.decrement();
            true
        });
        assert_eq!(calls, 3);
    }

    #[test]
    fn wait_timeout_reports_state() {
        let c = AtomicCounter::new(1);
        assert!(!c.wait_timeout(Duration::from_millis(1)));
        c.decrement();
        assert!(c.wait_timeout(Duration::from_millis(1)));
    }
}
