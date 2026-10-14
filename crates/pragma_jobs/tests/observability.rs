//! Pool observability (M4): `PoolStats` metrics, task labels, worker platform settings.
// Test code: a panic is a test failure; overflow is ruled out by the input sizes.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::time::Duration;

use pragma_jobs::sys::{self, ThreadPriority};
use pragma_jobs::{
    Grain, PoolConfig, Priority, ThreadPool, current_priority, current_task, labeled,
};

fn pool(workers: usize) -> ThreadPool {
    ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap()
}

/// The current thread's label chain: `kind:name`, from the innermost to the outermost.
fn chain() -> Vec<String> {
    current_task(|task| {
        task.map(|t| {
            t.chain()
                .map(|l| format!("{}:{}", l.kind(), l.name()))
                .collect()
        })
        .unwrap_or_default()
    })
}

#[test]
fn executed_sums_match_started_jobs() {
    let pool = pool(3);
    let before = pool.stats();
    for _ in 0..100 {
        pool.spawn(Priority::Normal, || ()).join().unwrap();
    }
    pool.scope(|s| {
        for _ in 0..50 {
            s.spawn(|| ());
        }
    })
    .unwrap();
    let mut data = vec![0_u32; 100];
    pool.parallel_for(&mut data, Grain::Fixed(10), |x| *x += 1)
        .unwrap();
    for _ in 0..20 {
        pool.join(|| (), || ()).unwrap();
    }
    let after = pool.stats();
    // 100 spawns + 50 scope tasks + 10 chunks + 20 second `join` branches.
    assert_eq!(after.executed() - before.executed(), 180);
    let per_thread: u64 =
        after.workers().iter().map(|w| w.executed()).sum::<u64>() + after.external().executed();
    assert_eq!(per_thread, after.executed());
    assert!(after.stolen() <= after.executed());
    assert_eq!(after.queued(), 0);
}

#[test]
fn idle_workers_report_parking() {
    let pool = pool(2);
    // The park timeout is 50 ms: by then every worker has fallen asleep and woken up at least once.
    std::thread::sleep(Duration::from_millis(150));
    for worker in pool.stats().workers() {
        assert!(worker.parks() >= 1, "{worker:?}");
        assert!(worker.parked() > Duration::ZERO);
        assert!(worker.idle() >= worker.parked());
    }
}

#[test]
fn snapshot_does_not_wait_for_busy_workers() {
    let pool = pool(2);
    let started = Arc::new(Barrier::new(3));
    let (release, gate) = mpsc::channel::<()>();
    let gate = Arc::new(std::sync::Mutex::new(gate));
    let busy: Vec<_> = (0..2)
        .map(|_| {
            let started = Arc::clone(&started);
            let gate = Arc::clone(&gate);
            pool.spawn(Priority::Normal, move || {
                started.wait();
                let _ = gate.lock().unwrap().recv();
            })
        })
        .collect();
    started.wait();
    // Both workers are busy: new tasks sit in the global queue, and the snapshot sees them.
    let queued: Vec<_> = (0..5)
        .map(|_| pool.spawn(Priority::Normal, || ()))
        .collect();
    let stats = pool.stats();
    assert_eq!(stats.global_queued(Priority::Normal), 5);
    assert_eq!(stats.global_queued(Priority::Frame), 0);
    assert_eq!(stats.queued(), 5);
    for _ in 0..2 {
        release.send(()).unwrap();
    }
    for h in busy.into_iter().chain(queued) {
        h.join().unwrap();
    }
}

#[test]
fn tasks_carry_labels() {
    let pool = pool(2);
    assert!(chain().is_empty());
    let spawned = pool
        .spawn_named("bake-navmesh", Priority::Background, || {
            assert_eq!(current_priority(), Some(Priority::Background));
            labeled("asset", "level.bin", chain)
        })
        .join()
        .unwrap();
    assert_eq!(spawned, ["asset:level.bin", "spawn:bake-navmesh"]);

    let default = pool.spawn(Priority::Normal, chain).join().unwrap();
    assert_eq!(default, ["spawn:spawn"]);

    let mut seen = vec![Vec::new(); 4];
    pool.parallel_for(&mut seen, Grain::Fixed(1), |slot| *slot = chain())
        .unwrap();
    assert!(
        seen.iter()
            .all(|c| c.first().map(String::as_str) == Some("scope:parallel_for chunk"))
    );

    let (tx, rx) = mpsc::channel();
    pool.spawner()
        .spawn_detached(Priority::Normal, move || tx.send(chain()).unwrap())
        .unwrap();
    assert_eq!(rx.recv().unwrap(), ["detached:spawn_detached"]);
    assert_eq!(current_priority(), None);
}

#[test]
fn spawn_named_reports_name_in_errors() {
    let pool = pool(1);
    let error = pool
        .spawn_named("decompress", Priority::Normal, || -> u32 {
            panic!("bad block")
        })
        .join()
        .unwrap_err();
    assert_eq!(error.to_string(), "job `decompress` panicked: bad block");
}

/// Runs `f` specifically on a worker: `JobHandle::join` would help and take the task from the queue itself.
fn on_worker<T: Send + 'static>(pool: &ThreadPool, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    pool.spawner()
        .spawn_detached(Priority::Normal, move || tx.send(f()).unwrap())
        .unwrap();
    rx.recv().unwrap()
}

#[test]
fn worker_index_is_known_on_workers_only() {
    let pool = Arc::new(pool(2));
    assert_eq!(pool.current_worker_index(), None);
    let probe = Arc::clone(&pool);
    let index = on_worker(&pool, move || probe.current_worker_index());
    assert!(matches!(index, Some(0 | 1)), "{index:?}");
    let p = &*pool;
    let inside = pool.scope(|_| p.current_worker_index()).unwrap();
    assert_eq!(inside, None, "main thread in a scope is not a worker");
}

#[test]
fn worker_threads_get_configured_priority() {
    let pool = ThreadPool::new(
        PoolConfig::default()
            .with_workers(1)
            .with_thread_priority(Some(ThreadPriority::Low)),
    )
    .unwrap();
    let priority = on_worker(&pool, sys::current_thread_priority);
    assert_eq!(priority, Ok(ThreadPriority::Low));
}

#[test]
fn worker_threads_can_be_pinned() {
    let pool = ThreadPool::new(
        PoolConfig::default()
            .with_workers(1)
            .with_affinity(Some(vec![0])),
    )
    .unwrap();
    assert_eq!(on_worker(&pool, sys::current_core), Some(0));
}
