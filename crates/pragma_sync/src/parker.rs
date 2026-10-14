use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::PoisonError;
use std::time::Duration;

use crate::primitives::{Arc, AtomicUsize, Condvar, Mutex, Ordering};

const EMPTY: usize = 0;
const PARKED: usize = 1;
const NOTIFIED: usize = 2;

#[derive(Debug)]
struct Inner {
    state: AtomicUsize,
    lock: Mutex<()>,
    cvar: Condvar,
}

/// Thread parking: the owner sleeps in [`park`](Self::park) until it is woken by
/// [`Unparker::unpark`].
///
/// A wakeup is never lost: `unpark` before `park` leaves a "token", and the next `park`
/// returns immediately. There is a single token: several `unpark` calls in a row do not accumulate.
/// `Parker` is not `Sync`: only the owner thread may park, anyone may wake it.
#[derive(Debug)]
pub struct Parker {
    inner: Arc<Inner>,
    _not_sync: PhantomData<Cell<()>>,
}

/// The waker for a [`Parker`]; cheap to clone and to pass between threads.
#[derive(Debug, Clone)]
pub struct Unparker {
    inner: Arc<Inner>,
}

impl Default for Parker {
    fn default() -> Self {
        Self::new()
    }
}

impl Parker {
    /// A new parker without a token.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: AtomicUsize::new(EMPTY),
                lock: Mutex::new(()),
                cvar: Condvar::new(),
            }),
            _not_sync: PhantomData,
        }
    }

    /// The waker for this parker.
    #[must_use]
    pub fn unparker(&self) -> Unparker {
        Unparker {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Sleeps until `unpark`; if a token is already present, takes it and returns immediately.
    pub fn park(&self) {
        self.park_inner(None);
    }

    /// Like [`park`](Self::park), but no longer than `timeout` (a safety net against bugs in the
    /// caller's wakeup protocol). Spurious wakeups are possible.
    pub fn park_timeout(&self, timeout: Duration) {
        self.park_inner(Some(timeout));
    }

    fn park_inner(&self, timeout: Option<Duration>) {
        let inner = &*self.inner;
        // Fast path: the token is already there.
        if inner
            .state
            .compare_exchange(NOTIFIED, EMPTY, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return;
        }
        let mut guard = inner.lock.lock().unwrap_or_else(PoisonError::into_inner);
        match inner
            .state
            .compare_exchange(EMPTY, PARKED, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => {}
            Err(_) => {
                // The token arrived between the fast path and taking the mutex (the only other
                // possible state, because only the owner can park).
                inner.state.store(EMPTY, Ordering::SeqCst);
                return;
            }
        }
        match timeout {
            None => loop {
                guard = inner
                    .cvar
                    .wait(guard)
                    .unwrap_or_else(PoisonError::into_inner);
                if inner
                    .state
                    .compare_exchange(NOTIFIED, EMPTY, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    return;
                }
                // A spurious wakeup: keep sleeping.
            },
            Some(timeout) => {
                let (_guard, _timed_out) = inner
                    .cvar
                    .wait_timeout(guard, timeout)
                    .unwrap_or_else(PoisonError::into_inner);
                // Both a timeout and a notification bring the state to EMPTY: the token is consumed.
                inner.state.store(EMPTY, Ordering::SeqCst);
            }
        }
    }
}

impl Unparker {
    /// Wakes the parker's owner (or leaves a token if it is not asleep yet).
    pub fn unpark(&self) {
        let inner = &*self.inner;
        match inner.state.swap(NOTIFIED, Ordering::SeqCst) {
            PARKED => {}
            // EMPTY or already NOTIFIED: the owner is not asleep, the token is enough.
            _ => return,
        }
        // Taking the mutex guarantees the owner is already inside `wait` and not between
        // the transition to PARKED and falling asleep; otherwise the notify would be lost.
        drop(inner.lock.lock().unwrap_or_else(PoisonError::into_inner));
        inner.cvar.notify_one();
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn unpark_before_park_is_not_lost() {
        let p = Parker::new();
        p.unparker().unpark();
        p.park(); // returned immediately thanks to the token
    }

    #[test]
    fn tokens_do_not_accumulate() {
        let p = Parker::new();
        let u = p.unparker();
        u.unpark();
        u.unpark();
        p.park();
        let start = Instant::now();
        p.park_timeout(Duration::from_millis(20));
        assert!(start.elapsed() >= Duration::from_millis(10));
    }

    #[test]
    fn cross_thread_wakeup() {
        let p = Parker::new();
        let u = p.unparker();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            u.unpark();
        });
        p.park();
        t.join().unwrap();
    }
}
