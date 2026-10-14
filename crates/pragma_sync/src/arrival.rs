use crate::primitives::{AtomicUsize, Ordering};

/// A "wait for N arrivals" counter: the last one to arrive learns that work may proceed.
///
/// The basis of dataflow execution of the frame graph: a task has as many arrivals as it has
/// predecessors; the predecessor that arrives last launches it. An arrival is an RMW with
/// `AcqRel`, so the last arriver sees everything the others did before their arrival
/// (loom model in `tests/loom.rs`).
///
/// Unlike [`AtomicCounter`](crate::AtomicCounter), it cannot be waited on, and it need not be:
/// nobody blocks, there is no mutex and no `Condvar`, just one atomic.
#[derive(Debug)]
pub struct ArrivalCounter {
    remaining: AtomicUsize,
}

impl Default for ArrivalCounter {
    fn default() -> Self {
        Self::new(0)
    }
}

impl ArrivalCounter {
    /// A counter for `expected` arrivals.
    #[must_use]
    pub fn new(expected: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(expected),
        }
    }

    /// Prepares the counter for a new round. `&mut` guarantees there are no arrivals of the previous
    /// round at this moment; the new round is published to other threads by whoever hands them work.
    pub fn reset(&mut self, expected: usize) {
        self.remaining.store(expected, Ordering::Relaxed);
    }

    /// Registers an arrival; `true` means it is the last one. An extra arrival (at zero) is a caller
    /// bug: it changes nothing and returns `false` instead of wrapping around.
    pub fn arrive(&self) -> bool {
        let mut current = self.remaining.load(Ordering::Relaxed);
        loop {
            let Some(next) = current.checked_sub(1) else {
                debug_assert!(false, "ArrivalCounter: more arrivals than expected");
                return false;
            };
            match self.remaining.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return next == 0,
                Err(actual) => current = actual,
            }
        }
    }

    /// How many arrivals are still expected.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.remaining.load(Ordering::Acquire)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize as StdAtomic;

    #[test]
    fn last_arrival_is_reported_once() {
        let mut c = ArrivalCounter::new(3);
        assert!(!c.arrive());
        assert!(!c.arrive());
        assert!(c.arrive());
        assert_eq!(c.remaining(), 0);
        c.reset(1);
        assert!(c.arrive());
    }

    #[test]
    fn exactly_one_thread_sees_the_last_arrival() {
        const N: usize = 16;
        for _ in 0..100 {
            let c = Arc::new(ArrivalCounter::new(N));
            let winners = Arc::new(StdAtomic::new(0));
            let threads: Vec<_> = (0..N)
                .map(|_| {
                    let (c, winners) = (Arc::clone(&c), Arc::clone(&winners));
                    std::thread::spawn(move || {
                        if c.arrive() {
                            winners.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
            assert_eq!(winners.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }
}
