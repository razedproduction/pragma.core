//! Поведенческие тесты пула: жизненный цикл, паники, вложенность, приоритеты.
// Тестовый код: паника = провал теста, переполнение исключено размерами входа.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use pragma_jobs::{Grain, JobError, PoolConfig, Priority, ShutdownPolicy, ThreadPool};

fn pool(workers: usize) -> ThreadPool {
    ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap()
}

fn fib(pool: &ThreadPool, n: u64) -> u64 {
    if n < 2 {
        return n;
    }
    let (a, b) = pool.join(|| fib(pool, n - 1), || fib(pool, n - 2)).unwrap();
    a + b
}

#[test]
fn invalid_config_is_an_error() {
    assert!(ThreadPool::new(PoolConfig::default().with_workers(0)).is_err());
}

#[test]
fn threads_are_named() {
    let pool = ThreadPool::new(
        PoolConfig::default()
            .with_workers(2)
            .with_name_prefix("test-pool"),
    )
    .unwrap();
    let name = pool
        // Background: внешний поток в join() не исполняет фоновые задачи, значит, это воркер.
        .spawn(Priority::Background, || {
            std::thread::current().name().map(str::to_owned)
        })
        .join()
        .unwrap()
        .unwrap();
    assert!(name.starts_with("test-pool-"), "{name}");
    assert_eq!(pool.workers(), 2);
    assert!(!pool.is_worker_thread());
}

#[test]
fn spawn_returns_value() {
    let pool = pool(2);
    let handles: Vec<_> = (0..100_u64)
        .map(|i| pool.spawn(Priority::Normal, move || i * i))
        .collect();
    let sum: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(sum, (0..100_u64).map(|i| i * i).sum());
}

#[test]
fn panic_becomes_error_and_worker_survives() {
    let pool = pool(1);
    let err = pool
        .spawn(Priority::Frame, || -> u32 { panic!("boom") })
        .join()
        .unwrap_err();
    assert_eq!(
        err,
        JobError::Panicked {
            job: "spawn".into(),
            message: "boom".into()
        }
    );
    // Единственный воркер жив и продолжает работать.
    assert_eq!(pool.spawn(Priority::Frame, || 7).join().unwrap(), 7);
}

#[test]
fn nested_join_on_one_worker_does_not_deadlock() {
    let pool = pool(1);
    assert_eq!(fib(&pool, 20), 6765);
    // То же изнутри задачи единственного воркера.
    let pool = Arc::new(pool);
    let inner = Arc::clone(&pool);
    let handle = pool.spawn(Priority::Normal, move || fib(&inner, 18));
    assert_eq!(handle.join().unwrap(), 2584);
}

#[test]
fn join_reports_which_side_panicked() {
    let pool = pool(2);
    let err = pool.join(|| 1, || -> i32 { panic!("right") }).unwrap_err();
    assert!(
        matches!(err, JobError::Panicked { ref job, .. } if job == "join b"),
        "{err}"
    );
    let err = pool.join(|| -> i32 { panic!("left") }, || 2).unwrap_err();
    assert!(
        matches!(err, JobError::Panicked { ref job, .. } if job == "join a"),
        "{err}"
    );
}

#[test]
fn scope_borrows_and_nests() {
    let pool = &pool(3);
    let mut grid = vec![vec![0_u32; 64]; 16];
    pool.scope(|s| {
        for (y, row) in grid.iter_mut().enumerate() {
            s.spawn(move || {
                pool.scope(|inner| {
                    for (x, cell) in row.iter_mut().enumerate() {
                        inner.spawn(move || *cell = u32::try_from(y * 64 + x).unwrap());
                    }
                })
                .unwrap();
            });
        }
    })
    .unwrap();
    for (y, row) in grid.iter().enumerate() {
        for (x, cell) in row.iter().enumerate() {
            assert_eq!(*cell as usize, y * 64 + x);
        }
    }
}

#[test]
fn scope_tasks_can_spawn_siblings() {
    let pool = pool(2);
    let count = &AtomicUsize::new(0);
    pool.scope(|s| {
        for _ in 0..10 {
            s.spawn(move || {
                count.fetch_add(1, Ordering::SeqCst);
                s.spawn(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                });
            });
        }
    })
    .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 20);
}

#[test]
fn scope_panic_waits_for_other_tasks() {
    let pool = pool(2);
    let finished = AtomicUsize::new(0);
    let err = pool
        .scope(|s| {
            s.spawn(|| panic!("task failed"));
            for _ in 0..8 {
                s.spawn(|| {
                    std::thread::sleep(Duration::from_millis(2));
                    finished.fetch_add(1, Ordering::SeqCst);
                });
            }
        })
        .unwrap_err();
    assert!(matches!(err, JobError::Panicked { .. }));
    assert_eq!(finished.load(Ordering::SeqCst), 8);
}

#[test]
fn parallel_for_and_chunks() {
    let pool = pool(4);
    let mut v: Vec<u64> = (0..10_000).collect();
    pool.parallel_for(&mut v, Grain::Auto, |x| *x *= 2).unwrap();
    assert!(v.iter().enumerate().all(|(i, &x)| x == 2 * i as u64));

    let mut offsets = vec![0_usize; 1000];
    pool.parallel_for_chunks(&mut offsets, Grain::Fixed(7), |start, chunk| {
        for (i, x) in chunk.iter_mut().enumerate() {
            *x = start + i;
        }
    })
    .unwrap();
    assert!(offsets.iter().enumerate().all(|(i, &x)| x == i));

    let mut empty: [u8; 0] = [];
    pool.parallel_for(&mut empty, Grain::Auto, |_| panic!("no elements"))
        .unwrap();
}

#[test]
fn parallel_for_panic_is_reported() {
    let pool = pool(2);
    let mut v = vec![0_u32; 1000];
    let err = pool
        .parallel_for(&mut v, Grain::Fixed(10), |x| {
            if *x == 0 {
                panic!("bad element");
            }
        })
        .unwrap_err();
    assert!(matches!(err, JobError::Panicked { ref job, .. } if job == "parallel_for chunk"));
}

#[test]
fn reduce_is_bitwise_deterministic_across_pools() {
    let data: Vec<f64> = (0..100_000).map(|i| 1.0 / f64::from(i + 1)).collect();
    let sums: Vec<f64> = [1, 2, 4, 7]
        .into_iter()
        .map(|w| {
            pool(w)
                .parallel_reduce(
                    &data,
                    Grain::Auto,
                    |_, c| c.iter().sum::<f64>(),
                    |a, b| a + b,
                )
                .unwrap()
                .unwrap()
        })
        .collect();
    assert!(
        sums.windows(2).all(|w| w[0].to_bits() == w[1].to_bits()),
        "{sums:?}"
    );
    assert_eq!(
        pool(2)
            .parallel_reduce(&[] as &[u8], Grain::Auto, |_, _| 0, |a, b| a + b)
            .unwrap(),
        None
    );
}

#[test]
fn background_respects_worker_limit() {
    let pool = ThreadPool::new(
        PoolConfig::default()
            .with_workers(4)
            .with_background_limit(Some(2)),
    )
    .unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (0..24)
        .map(|_| {
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            pool.spawn(Priority::Background, move || {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(2));
                active.fetch_sub(1, Ordering::SeqCst);
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let peak = peak.load(Ordering::SeqCst);
    assert!(
        (1..=2).contains(&peak),
        "peak background concurrency {peak}"
    );
}

#[test]
fn frame_work_runs_while_background_is_saturated() {
    let pool = ThreadPool::new(
        PoolConfig::default()
            .with_workers(2)
            .with_background_limit(Some(1)),
    )
    .unwrap();
    let release = Arc::new(AtomicUsize::new(0));
    let bg: Vec<_> = (0..4)
        .map(|_| {
            let release = Arc::clone(&release);
            pool.spawn(Priority::Background, move || {
                while release.load(Ordering::SeqCst) == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        })
        .collect();
    // Фон занимает не больше одного воркера, второй свободен для кадра.
    let frame = pool.spawn(Priority::Frame, || 42);
    assert_eq!(frame.join().unwrap(), 42);
    release.store(1, Ordering::SeqCst);
    for h in bg {
        h.join().unwrap();
    }
}

#[test]
fn background_join_inside_background_with_limit_one() {
    let pool = Arc::new(
        ThreadPool::new(
            PoolConfig::default()
                .with_workers(1)
                .with_background_limit(Some(1)),
        )
        .unwrap(),
    );
    let inner = Arc::clone(&pool);
    let outer = pool.spawn(Priority::Background, move || {
        inner.spawn(Priority::Background, || 5).join().unwrap() + 1
    });
    assert_eq!(outer.join().unwrap(), 6);
}

#[test]
fn drop_drains_queue_by_default() {
    let done = Arc::new(AtomicUsize::new(0));
    {
        let pool = pool(2);
        for _ in 0..100 {
            let done = Arc::clone(&done);
            drop(pool.spawn(Priority::Normal, move || {
                done.fetch_add(1, Ordering::SeqCst);
            }));
        }
    }
    assert_eq!(done.load(Ordering::SeqCst), 100);
}

#[test]
fn drop_with_cancel_policy_cancels_pending_jobs() {
    let pool = ThreadPool::new(
        PoolConfig::default()
            .with_workers(1)
            .with_shutdown(ShutdownPolicy::Cancel),
    )
    .unwrap();
    let gate = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let blocker = {
        let gate = Arc::clone(&gate);
        let started = Arc::clone(&started);
        pool.spawn(Priority::Normal, move || {
            started.store(1, Ordering::SeqCst);
            while gate.load(Ordering::SeqCst) == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    // Ждём, пока единственный воркер займётся блокирующей задачей.
    while started.load(Ordering::SeqCst) == 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    let sentinel = Arc::new(());
    let pending: Vec<_> = (0..10)
        .map(|_| {
            let sentinel = Arc::clone(&sentinel);
            pool.spawn(Priority::Normal, move || drop(sentinel))
        })
        .collect();
    let opener = std::thread::spawn({
        let gate = Arc::clone(&gate);
        move || {
            std::thread::sleep(Duration::from_millis(20));
            gate.store(1, Ordering::SeqCst);
        }
    });
    drop(pool);
    opener.join().unwrap();
    blocker.join().unwrap();
    // Все задачи либо выполнены, либо отменены; ни одна не утекла.
    assert_eq!(Arc::strong_count(&sentinel), 1);
    for h in pending {
        match h.join() {
            Ok(()) | Err(JobError::Cancelled { .. }) => {}
            Err(other) => panic!("unexpected {other}"),
        }
    }
}

#[test]
fn external_waiter_helps_when_workers_are_busy() {
    let pool = pool(1);
    let gate = Arc::new(AtomicUsize::new(0));
    let busy = {
        let gate = Arc::clone(&gate);
        pool.spawn(Priority::Normal, move || {
            while gate.load(Ordering::SeqCst) == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    // Воркер занят; главный поток сам исполнит обе ветви.
    let (a, b) = pool.join(|| 1, || 2).unwrap();
    assert_eq!(a + b, 3);
    let mut v = vec![1_u32; 256];
    pool.parallel_for(&mut v, Grain::Fixed(16), |x| *x += 1)
        .unwrap();
    assert!(v.iter().all(|&x| x == 2));
    gate.store(1, Ordering::SeqCst);
    busy.join().unwrap();
}

#[test]
fn external_context_is_reused_across_pools() {
    // Контекст внешнего слота кэшируется в потоке (DECISIONS #18): переключение между пулами,
    // вложенный вход в другой пул и удаление пула не должны ни терять слоты, ни путать пулы.
    let a = pool(2);
    for round in 0..50_u32 {
        let b = pool(1);
        assert_eq!(a.join(|| round, || 1).unwrap(), (round, 1));
        assert_eq!(b.join(|| 2, || round).unwrap(), (2, round));
        let nested = a
            .scope(|_| b.join(|| fib(&b, 10), || fib(&a, 10)).unwrap())
            .unwrap();
        assert_eq!(nested, (55, 55));
        drop(b);
        let mut v = vec![0_u32; 300];
        a.parallel_for(&mut v, Grain::Fixed(7), |x| *x += round)
            .unwrap();
        assert!(v.iter().all(|&x| x == round));
    }
    // Все 4 внешних слота по-прежнему доступны: 4 внешних потока ждут одновременно.
    let a = &a;
    std::thread::scope(|ts| {
        for _ in 0..4 {
            ts.spawn(move || assert_eq!(fib(a, 15), 610));
        }
    });
}

#[test]
fn pool_dropped_inside_its_own_job_does_not_hang() {
    let pool = Arc::new(pool(2));
    let inner = Arc::clone(&pool);
    let handle = pool.spawn(Priority::Normal, move || {
        std::thread::sleep(Duration::from_millis(5));
        drop(inner);
        1
    });
    drop(pool);
    // Последняя ссылка уходит внутри задачи; Drop не должен join-ить собственный поток.
    assert_eq!(handle.join().unwrap(), 1);
}

/// Сбросы помеченного значения: задача, отклонённая пулом, должна быть сброшена.
struct DropFlag(Arc<AtomicUsize>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn spawner_runs_detached_jobs() {
    let pool = pool(2);
    let spawner = pool.spawner();
    assert!(spawner.is_alive());
    let counter = Arc::new(AtomicUsize::new(0));
    for priority in [Priority::Frame, Priority::Normal, Priority::Background] {
        for _ in 0..100 {
            let c = Arc::clone(&counter);
            spawner
                .spawn_detached(priority, move || {
                    c.fetch_add(1, Ordering::SeqCst);
                })
                .unwrap();
        }
    }
    drop(pool); // `Drain`: всё принятое доработает
    assert_eq!(counter.load(Ordering::SeqCst), 300);
}

#[test]
fn spawner_rejects_after_shutdown_and_drops_job() {
    let pool = pool(1);
    let spawner = pool.spawner();
    drop(pool);
    assert!(!spawner.is_alive());
    let drops = Arc::new(AtomicUsize::new(0));
    let flag = DropFlag(Arc::clone(&drops));
    let result = spawner.spawn_detached(Priority::Background, move || drop(flag));
    assert!(matches!(result, Err(JobError::Cancelled { .. })));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn spawner_detached_panic_keeps_worker_alive() {
    let pool = pool(1);
    let spawner = pool.spawner();
    spawner
        .spawn_detached(Priority::Normal, || panic!("detached boom"))
        .unwrap();
    assert_eq!(pool.spawn(Priority::Normal, || 1).join(), Ok(1));
}

#[test]
fn spawner_knows_pool_context() {
    let pool = Arc::new(pool(1));
    let spawner = pool.spawner();
    assert!(!spawner.in_pool_context());
    let inner = spawner.clone();
    assert_eq!(
        pool.spawn(Priority::Normal, move || inner.in_pool_context())
            .join(),
        Ok(true)
    );
    assert!(pool.scope(|_| spawner.in_pool_context()).unwrap());
}

#[test]
fn spawner_racing_with_shutdown_never_loses_jobs() {
    // Каждая задача либо исполнена, либо сброшена — ни одна не застревает в очереди
    // остановленного пула.
    for _ in 0..200 {
        let pool = pool(2);
        let spawner = pool.spawner();
        let accounted = Arc::new(AtomicUsize::new(0));
        let pusher = {
            let accounted = Arc::clone(&accounted);
            std::thread::spawn(move || {
                for _ in 0..50 {
                    let flag = DropFlag(Arc::clone(&accounted));
                    let _ = spawner.spawn_detached(Priority::Background, move || drop(flag));
                }
            })
        };
        drop(pool);
        pusher.join().unwrap();
        assert_eq!(accounted.load(Ordering::SeqCst), 50);
    }
}
