//! The Miri suite (M5): a pool in a minimal configuration (1 worker + an external thread) goes
//! through all the `unsafe` in `pragma_sync` that `pragma_jobs` relies on: `join_on_stack`
//! (owner and thief), `StackSlot`/`scope` lifetime erasure, the task label frame.
//!
//! `cargo +nightly miri test -p pragma_jobs --test miri` (CI, Linux). Sizes are small: Miri
//! is thousands of times slower than native code. A regular `cargo test` runs the same suite.
// Test code: a panic is a test failure.
#![allow(clippy::unwrap_used, clippy::panic, clippy::arithmetic_side_effects)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

use pragma_jobs::{Grain, JobError, PoolConfig, Priority, ThreadPool, current_task, labeled};

/// 1 worker, no watchdog (Miri does not need a thread with timers) and a short spin.
fn pool() -> ThreadPool {
    ThreadPool::new(
        PoolConfig::default()
            .with_workers(1)
            .with_watchdog(None)
            .with_idle_spin(1),
    )
    .unwrap()
}

fn fib(pool: &ThreadPool, n: u32) -> u64 {
    if n < 2 {
        return u64::from(n);
    }
    let (a, b) = pool.join(|| fib(pool, n - 1), || fib(pool, n - 2)).unwrap();
    a + b
}

#[test]
fn nested_join_from_external_thread() {
    let pool = pool();
    assert_eq!(fib(&pool, 7), 13);
}

#[test]
fn nested_join_on_worker_while_external_thread_helps() {
    // A task on the worker does a join while the main thread waits on the handle and helps; it may
    // steal the second branch (the thief path in `join_on_stack`).
    let pool = Arc::new(pool());
    let on_worker = Arc::clone(&pool);
    let handle = pool.spawn(Priority::Normal, move || fib(&on_worker, 6));
    assert_eq!(handle.join().unwrap(), 8);
    // Two external threads at once (each with its own external slot).
    std::thread::scope(|s| {
        let other = s.spawn(|| fib(&pool, 5));
        assert_eq!(fib(&pool, 5), 5);
        assert_eq!(other.join().unwrap(), 5);
    });
}

#[test]
fn join_propagates_panic_from_either_branch() {
    std::panic::set_hook(Box::new(|_| {}));
    let pool = pool();
    let left = pool.join(|| panic!("left"), || 2);
    let right = pool.join(|| 1, || panic!("right"));
    let _ = std::panic::take_hook();
    assert!(matches!(left, Err(JobError::Panicked { .. })));
    assert!(matches!(right, Err(JobError::Panicked { .. })));
    // The pool is alive.
    assert_eq!(pool.join(|| 1, || 2).unwrap(), (1, 2));
}

#[test]
fn scope_borrows_stack_data() {
    let pool = pool();
    let mut data = [0_u32; 8];
    let counter = AtomicUsize::new(0);
    pool.scope(|s| {
        for (i, x) in data.iter_mut().enumerate() {
            let counter = &counter;
            s.spawn(move || {
                *x = u32::try_from(i).unwrap() * 2;
                counter.fetch_add(1, Ordering::Relaxed);
            });
        }
    })
    .unwrap();
    assert_eq!(counter.load(Ordering::Relaxed), 8);
    assert_eq!(data, [0, 2, 4, 6, 8, 10, 12, 14]);
}

#[test]
fn parallel_for_and_reduce() {
    let pool = pool();
    let mut v = [1_u32; 16];
    pool.parallel_for(&mut v, Grain::Fixed(3), |x| *x += 1)
        .unwrap();
    assert!(v.iter().all(|&x| x == 2));
    let sum = pool
        .parallel_reduce(
            &v,
            Grain::Fixed(5),
            |_, c| c.iter().sum::<u32>(),
            |a, b| a + b,
        )
        .unwrap();
    assert_eq!(sum, Some(32));
}

#[test]
fn spawn_detached_runs_and_handle_joins() {
    let pool = pool();
    let (tx, rx) = mpsc::channel();
    pool.spawner()
        .spawn_detached(Priority::Background, move || tx.send(7_u32).unwrap())
        .unwrap();
    assert_eq!(rx.recv().unwrap(), 7);
    let h = pool.spawn_named("answer", Priority::Normal, || 42);
    assert_eq!(h.join().unwrap(), 42);
}

#[test]
fn labels_name_the_running_task() {
    let pool = pool();
    let read = || current_task(|t| t.map(|t| t.name().to_owned()));
    assert_eq!(labeled("test", "outer", read).as_deref(), Some("outer"));
    let nested = labeled("test", "outer", || {
        labeled("test", "inner", || {
            current_task(|t| t.map(|t| t.chain().map(|l| l.name().to_owned()).collect::<Vec<_>>()))
        })
    });
    assert_eq!(nested, Some(vec!["inner".to_owned(), "outer".to_owned()]));
    let h = pool.spawn_named("job", Priority::Normal, read);
    assert_eq!(h.join().unwrap().as_deref(), Some("job"));
    assert_eq!(read(), None);
}

#[test]
fn drop_with_pending_work_does_not_leak() {
    let pool = pool();
    let handles: Vec<_> = (0..4)
        .map(|i| pool.spawn(Priority::Normal, move || i))
        .collect();
    drop(handles);
    drop(pool);
}
