//! loom models of the `pragma_sync` atomic protocols.
//!
//! Run: `RUSTFLAGS="--cfg loom" cargo test -p pragma_sync --release --lib --tests`.
//! Each model explores all permitted thread interleavings; loom reports a hang (a lost
//! wakeup) as a deadlock.
#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::Mutex;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::thread;
use pragma_sync::__private::{ArrivalCounter, Parker, StackJob, join_on_stack};
use pragma_sync::{AtomicCounter, CountLatch};

#[test]
fn stack_join_owner_never_misses_thief() {
    // The `join_on_stack` latch protocol: the owner helps, then announces sleep and parks;
    // the thief runs the job and wakes the owner if it announced sleep. A deadlock means a lost
    // wakeup; once the owner returns the frame is gone, and the thief no longer touches it.
    loom::model(|| {
        let parker = Parker::new();
        let unparker = parker.unparker();
        let queue: Arc<Mutex<Option<StackJob>>> = Arc::new(Mutex::new(None));
        let thief = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || {
                loop {
                    if let Some(job) = queue.lock().unwrap().take() {
                        if job.run() == Some(0) {
                            unparker.unpark();
                        }
                        return;
                    }
                    thread::yield_now();
                }
            })
        };
        let data = 41_usize;
        let ((), b) = join_on_stack(
            0,
            || data + 1,
            |job| *queue.lock().unwrap() = Some(job),
            |latch| {
                if latch.prepare_park() {
                    parker.park();
                }
            },
        );
        assert_eq!(b.and_then(Result::ok), Some(42));
        thief.join().unwrap();
    });
}

#[test]
fn latch_releases_waiter() {
    loom::model(|| {
        let latch = Arc::new(CountLatch::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let latch = Arc::clone(&latch);
                thread::spawn(move || latch.count_down())
            })
            .collect();
        latch.wait();
        assert!(latch.is_released());
        for t in threads {
            t.join().unwrap();
        }
    });
}

#[test]
fn parker_never_loses_wakeup() {
    loom::model(|| {
        let parker = Parker::new();
        let unparker = parker.unparker();
        let flag = Arc::new(AtomicUsize::new(0));
        let t = {
            let flag = Arc::clone(&flag);
            thread::spawn(move || {
                flag.store(1, Ordering::Release);
                unparker.unpark();
            })
        };
        // The classic wait-for-condition loop: an unpark after the store cannot be lost.
        while flag.load(Ordering::Acquire) == 0 {
            parker.park();
        }
        t.join().unwrap();
    });
}

#[test]
fn parker_timeout_path_never_loses_wakeup() {
    loom::model(|| {
        let parker = Parker::new();
        let unparker = parker.unparker();
        let flag = Arc::new(AtomicUsize::new(0));
        let t = {
            let flag = Arc::clone(&flag);
            thread::spawn(move || {
                flag.store(1, Ordering::Release);
                unparker.unpark();
            })
        };
        // In loom the timeout never fires, so the model checks that the `park_timeout`
        // path does not lose a wakeup either.
        while flag.load(Ordering::Acquire) == 0 {
            parker.park_timeout(std::time::Duration::from_millis(1));
        }
        t.join().unwrap();
    });
}

#[test]
fn counter_wait_sees_last_decrement() {
    loom::model(|| {
        let counter = Arc::new(AtomicCounter::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let counter = Arc::clone(&counter);
                thread::spawn(move || {
                    counter.decrement();
                })
            })
            .collect();
        counter.wait();
        assert!(counter.is_zero());
        for t in threads {
            t.join().unwrap();
        }
    });
}

#[test]
fn last_arrival_sees_every_predecessor() {
    // A frame graph: two predecessor tasks write their data (Relaxed, with no synchronization of
    // their own) and arrive; the last to arrive "launches the successor" and must see both
    // writes. Exactly one of the two is the last.
    loom::model(|| {
        let arrival = Arc::new(ArrivalCounter::new(2));
        let data: Arc<[AtomicUsize; 2]> = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let threads: Vec<_> = (0..2)
            .map(|i| {
                let (arrival, data) = (Arc::clone(&arrival), Arc::clone(&data));
                thread::spawn(move || {
                    data[i].store(i + 1, Ordering::Relaxed);
                    if arrival.arrive() {
                        assert_eq!(data[0].load(Ordering::Relaxed), 1);
                        assert_eq!(data[1].load(Ordering::Relaxed), 2);
                        true
                    } else {
                        false
                    }
                })
            })
            .collect();
        let last: usize = threads
            .into_iter()
            .map(|t| usize::from(t.join().unwrap()))
            .sum();
        assert_eq!(last, 1);
        assert_eq!(arrival.remaining(), 0);
    });
}

#[test]
fn counter_increment_while_waiting() {
    loom::model(|| {
        let counter = Arc::new(AtomicCounter::new(1));
        let t = {
            let counter = Arc::clone(&counter);
            thread::spawn(move || {
                // The task spawns a child before finishing, so the counter does not hit zero prematurely.
                counter.increment().unwrap();
                counter.decrement();
                counter.decrement();
            })
        };
        counter.wait();
        assert_eq!(counter.get(), 0);
        t.join().unwrap();
    });
}
