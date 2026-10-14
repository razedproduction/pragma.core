//! Executor behavior: results, panics, cancellation at every stage, the frame bridge,
//! the blocking pool (limit, overflow, cancellation), pool and executor shutdown.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

mod common;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::Poll;
use std::time::{Duration, Instant};

use common::{DropCounter, wait_until};
use futures_lite::future;
use pragma_async::{AsyncConfig, AsyncError, AsyncHandle, Executor};
use pragma_jobs::{PoolConfig, Priority, ThreadPool};

fn setup(workers: usize, config: AsyncConfig) -> (ThreadPool, Executor) {
    let pool = ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap();
    let exec = Executor::new(&pool, config).unwrap();
    (pool, exec)
}

/// Occupies the pool's only worker until `release` lets go.
fn occupy_worker(pool: &ThreadPool) -> (mpsc::Sender<()>, pragma_jobs::JobHandle<()>) {
    let (release, wait) = mpsc::channel::<()>();
    let (started_tx, started) = mpsc::channel();
    let job = pool.spawn(Priority::Normal, move || {
        started_tx.send(()).unwrap();
        let _ = wait.recv();
    });
    started.recv().unwrap();
    (release, job)
}

#[test]
fn spawn_async_returns_value() {
    for workers in [1, 4] {
        let (_pool, exec) = setup(workers, AsyncConfig::default());
        let handles: Vec<AsyncHandle<u64>> = (0..64_u64)
            .map(|i| {
                exec.spawn_async(async move {
                    future::yield_now().await;
                    i * i
                })
            })
            .collect();
        let sum: u64 = handles.into_iter().map(|h| h.wait().unwrap()).sum();
        assert_eq!(sum, (0..64_u64).map(|i| i * i).sum());
    }
}

#[test]
fn async_tasks_run_on_pool_workers() {
    let (pool, exec) = setup(2, AsyncConfig::default());
    let pool = Arc::new(pool);
    let probe = Arc::clone(&pool);
    let on_worker = exec.spawn_async(async move { probe.is_worker_thread() });
    assert!(on_worker.wait().unwrap());
}

#[test]
fn panic_becomes_error_and_worker_survives() {
    let (_pool, exec) = setup(1, AsyncConfig::default());
    for _ in 0..3 {
        let err = exec
            .spawn_async(async {
                future::yield_now().await;
                panic!("boom");
            })
            .wait();
        assert!(
            matches!(&err, Err(AsyncError::Panicked { task: "spawn_async", message, .. }) if message == "boom"),
            "{err:?}"
        );
        // The same single worker keeps running tasks.
        assert_eq!(exec.spawn_async(async { 5 }).wait(), Ok(5));
    }
}

#[test]
fn cancel_before_start_drops_future_without_polling() {
    let (pool, exec) = setup(1, AsyncConfig::default());
    let (release, job) = occupy_worker(&pool);
    let drops = DropCounter::default();
    let polled = Arc::new(AtomicBool::new(false));
    let token = drops.token();
    let flag = Arc::clone(&polled);
    let handle = exec.spawn_async(async move {
        let _token = token;
        flag.store(true, Ordering::SeqCst);
    });
    drop(handle);
    release.send(()).unwrap();
    job.join().unwrap();
    wait_until("future dropped", || drops.get() == 1);
    assert!(!polled.load(Ordering::SeqCst));
}

#[test]
fn cancel_while_idle_drops_future() {
    let (_pool, exec) = setup(2, AsyncConfig::default());
    let drops = DropCounter::default();
    let polled = Arc::new(AtomicBool::new(false));
    let token = drops.token();
    let flag = Arc::clone(&polled);
    let handle = exec.spawn_async(async move {
        let _token = token;
        flag.store(true, Ordering::SeqCst);
        future::pending::<()>().await;
    });
    wait_until("first poll", || polled.load(Ordering::SeqCst));
    assert_eq!(drops.get(), 0);
    drop(handle);
    wait_until("future dropped", || drops.get() == 1);
}

#[test]
fn cancel_during_poll_drops_future_after_poll() {
    let (_pool, exec) = setup(2, AsyncConfig::default());
    let drops = DropCounter::default();
    let (in_poll_tx, in_poll) = mpsc::channel();
    let (go, go_rx) = mpsc::channel::<()>();
    let token = drops.token();
    let handle = exec.spawn_async(async move {
        let _token = token;
        let mut first = true;
        future::poll_fn(move |_| {
            if first {
                first = false;
                in_poll_tx.send(()).unwrap();
                go_rx.recv().unwrap();
            }
            Poll::<()>::Pending
        })
        .await;
    });
    in_poll.recv().unwrap();
    drop(handle); // the future is inside poll on a worker right now
    assert_eq!(drops.get(), 0);
    go.send(()).unwrap();
    wait_until("future dropped", || drops.get() == 1);
}

#[test]
fn drop_after_completion_is_harmless() {
    let (_pool, exec) = setup(2, AsyncConfig::default());
    let drops = DropCounter::default();
    let token = drops.token();
    let mut handle = exec.spawn_async(async move {
        let _token = token;
        7
    });
    wait_until("finished", || handle.is_finished());
    assert_eq!(handle.poll_ready(), Some(Ok(7)));
    assert_eq!(handle.poll_ready(), Some(Err(AsyncError::Consumed)));
    drop(handle);
    assert_eq!(drops.get(), 1);
}

#[test]
fn unread_result_is_dropped_with_handle() {
    let (_pool, exec) = setup(2, AsyncConfig::default());
    let drops = DropCounter::default();
    let token = drops.token();
    let handle = exec.spawn_async(async move { token });
    wait_until("finished", || handle.is_finished());
    assert_eq!(drops.get(), 0, "the result is still held by the task");
    drop(handle);
    assert_eq!(drops.get(), 1);
}

#[test]
fn detach_lets_task_finish() {
    let (_pool, exec) = setup(2, AsyncConfig::default());
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    exec.spawn_async(async move {
        future::yield_now().await;
        flag.store(true, Ordering::SeqCst);
    })
    .detach();
    wait_until("detached task finished", || done.load(Ordering::SeqCst));
}

#[test]
fn handles_compose_with_await() {
    let (_pool, exec) = setup(2, AsyncConfig::default());
    let inner = exec.clone();
    let outer = exec.spawn_async(async move {
        let a = inner.spawn_async(async { 20 });
        let b = inner.unblock(|| 22);
        Ok::<_, AsyncError>(a.await? + b.await?)
    });
    assert_eq!(outer.wait().unwrap(), Ok(42));
}

#[test]
fn wait_inside_pool_work_is_rejected() {
    let (pool, exec) = setup(1, AsyncConfig::default());
    // From a worker.
    let handle = exec.spawn_async(future::pending::<()>());
    let job = pool.spawn(Priority::Normal, move || handle.wait());
    assert_eq!(job.join().unwrap(), Err(AsyncError::WouldBlock));
    // From the main thread during a `scope` (as in a graph frame).
    let handle = exec.spawn_async(async { 1 });
    let result = pool.scope(|_| handle.wait()).unwrap();
    assert_eq!(result, Err(AsyncError::WouldBlock));
}

#[test]
fn poll_ready_never_blocks_the_frame() {
    let (_pool, exec) = setup(1, AsyncConfig::default());
    let (go, go_rx) = mpsc::channel::<()>();
    let mut handle = exec.unblock(move || go_rx.recv().is_ok());
    let start = Instant::now();
    for _ in 0..100 {
        assert_eq!(handle.poll_ready(), None);
    }
    assert!(start.elapsed() < Duration::from_secs(1));
    go.send(()).unwrap();
    wait_until("ready", || handle.is_finished());
    assert_eq!(handle.poll_ready(), Some(Ok(true)));
}

#[test]
fn pool_shutdown_cancels_waiting_tasks_without_hang() {
    let (pool, exec) = setup(1, AsyncConfig::default());
    let (go, go_rx) = mpsc::channel::<()>();
    let (started_tx, started) = mpsc::channel();
    let io = exec.clone();
    let drops = DropCounter::default();
    let token = drops.token();
    let handle = exec.spawn_async(async move {
        let _token = token;
        started_tx.send(()).unwrap();
        io.unblock(move || go_rx.recv().is_ok()).await
    });
    started.recv().unwrap();
    drop(pool);
    assert!(!exec.is_alive());
    // The blocking task finishes after the pool has stopped and wakes the async task: the pool will
    // not accept it anymore, the task is cancelled, and the waiter does not hang.
    go.send(()).unwrap();
    assert_eq!(handle.wait(), Err(AsyncError::Cancelled));
    assert_eq!(drops.get(), 1);
    assert_eq!(
        exec.spawn_async(async { 1 }).wait(),
        Err(AsyncError::Cancelled)
    );
}

#[test]
fn unblock_panic_becomes_error() {
    let (_pool, exec) = setup(1, AsyncConfig::default());
    let err = exec.unblock(|| -> u8 { panic!("disk on fire") }).wait();
    assert!(
        matches!(&err, Err(AsyncError::Panicked { task: "unblock", message, .. }) if message == "disk on fire"),
        "{err:?}"
    );
    assert_eq!(exec.unblock(|| 3).wait(), Ok(3));
}

#[test]
fn blocking_pool_respects_thread_limit() {
    let config = AsyncConfig::default()
        .with_blocking_threads(2)
        .with_blocking_capacity(64);
    let (_pool, exec) = setup(2, config);
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let threads = Arc::new(Mutex::new(HashSet::new()));
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let (running, peak, threads) = (
                Arc::clone(&running),
                Arc::clone(&peak),
                Arc::clone(&threads),
            );
            exec.unblock(move || {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                threads.lock().unwrap().insert(std::thread::current().id());
                std::thread::sleep(Duration::from_millis(10));
                running.fetch_sub(1, Ordering::SeqCst);
            })
        })
        .collect();
    for h in handles {
        h.wait().unwrap();
    }
    assert!(peak.load(Ordering::SeqCst) <= 2);
    assert!(threads.lock().unwrap().len() <= 2);
    assert!(exec.blocking_threads() <= 2);
}

#[test]
fn blocking_overflow_is_rejected_immediately() {
    let config = AsyncConfig::default()
        .with_blocking_threads(1)
        .with_blocking_capacity(2);
    let (_pool, exec) = setup(1, config);
    let (go, go_rx) = mpsc::channel::<()>();
    let go_rx = Arc::new(Mutex::new(go_rx));
    let busy: Vec<_> = (0..2)
        .map(|_| {
            let rx = Arc::clone(&go_rx);
            exec.unblock(move || rx.lock().unwrap().recv().is_ok())
        })
        .collect();
    let start = Instant::now();
    let mut rejected = exec.unblock(|| ());
    assert!(matches!(
        rejected.poll_ready(),
        Some(Err(AsyncError::Overloaded { capacity: 2, .. }))
    ));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "overflow does not wait"
    );
    go.send(()).unwrap();
    go.send(()).unwrap();
    for h in busy {
        assert_eq!(h.wait(), Ok(true));
    }
    // The slots have been released.
    assert_eq!(exec.unblock(|| 1).wait(), Ok(1));
}

#[test]
fn blocking_work_does_not_occupy_pool_workers() {
    let config = AsyncConfig::default().with_blocking_threads(1);
    let (pool, exec) = setup(1, config);
    let (go, go_rx) = mpsc::channel::<()>();
    let io = exec.unblock(move || go_rx.recv().is_ok());
    // While the blocking thread is busy, the pool's only worker is free for the frame and async work.
    assert_eq!(pool.join(|| 1, || 2).unwrap(), (1, 2));
    assert_eq!(pool.spawn(Priority::Normal, || 3).join(), Ok(3));
    assert_eq!(exec.spawn_async(async { 4 }).wait(), Ok(4));
    go.send(()).unwrap();
    assert_eq!(io.wait(), Ok(true));
}

#[test]
fn blocking_cancel_before_start_skips_closure() {
    let config = AsyncConfig::default().with_blocking_threads(1);
    let (_pool, exec) = setup(1, config);
    let (go, go_rx) = mpsc::channel::<()>();
    let first = exec.unblock(move || go_rx.recv().is_ok());
    let drops = DropCounter::default();
    let ran = Arc::new(AtomicBool::new(false));
    let token = drops.token();
    let flag = Arc::clone(&ran);
    let second = exec.unblock(move || {
        let _token = token;
        flag.store(true, Ordering::SeqCst);
    });
    drop(second);
    go.send(()).unwrap();
    assert_eq!(first.wait(), Ok(true));
    wait_until("closure dropped", || drops.get() == 1);
    assert!(!ran.load(Ordering::SeqCst));
    // The cancelled task returned its slot.
    assert_eq!(exec.unblock(|| 2).wait(), Ok(2));
}

#[test]
fn executor_drop_cancels_queued_blocking_tasks() {
    let config = AsyncConfig::default().with_blocking_threads(1);
    let (_pool, exec) = setup(1, config);
    let (go, go_rx) = mpsc::channel::<()>();
    let (started_tx, started) = mpsc::channel();
    let first = exec.unblock(move || {
        started_tx.send(()).unwrap();
        go_rx.recv().is_ok()
    });
    let drops = DropCounter::default();
    let token = drops.token();
    let queued = exec.unblock(move || drop(token));
    started.recv().unwrap();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        go.send(()).unwrap();
    });
    drop(exec); // waits for the running task, cancels the queued ones
    releaser.join().unwrap();
    assert_eq!(first.wait(), Ok(true));
    assert_eq!(queued.wait(), Err(AsyncError::Cancelled));
    assert_eq!(drops.get(), 1);
}

#[test]
fn invalid_config_is_an_error() {
    let pool = ThreadPool::new(PoolConfig::default().with_workers(1)).unwrap();
    let err = Executor::new(&pool, AsyncConfig::default().with_blocking_threads(0)).unwrap_err();
    assert!(matches!(err, AsyncError::InvalidConfig { .. }));
}
