//! Stress tests (long-running): `cargo test -p pragma_jobs --release --test stress -- --ignored`.
//! In CI: the nightly workflow `.github/workflows/nightly.yml`.
// Test code: a panic is a test failure; overflow is ruled out by the input sizes.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use pragma_jobs::{Grain, JobError, PoolConfig, Priority, ThreadPool};

fn worker_counts() -> Vec<usize> {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    let mut counts = vec![1, 2, cores, 2 * cores];
    counts.dedup();
    counts
}

/// Soak run length multiplier (`PRAGMA_SOAK_ROUNDS`, nightly CI); defaults to 1.
fn soak_rounds() -> u64 {
    std::env::var("PRAGMA_SOAK_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1)
}

/// Seed of the random panics (`PRAGMA_SOAK_SEED`): nightly CI changes it every night.
fn soak_seed() -> u64 {
    let seed = std::env::var("PRAGMA_SOAK_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if seed != 0 {
        eprintln!("PRAGMA_SOAK_SEED={seed}");
    }
    seed
}

/// A cheap deterministic generator, so that panics are "random" yet reproducible.
fn hash(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x
}

#[test]
#[ignore = "long: 10^7 tasks"]
fn ten_million_tiny_tasks() {
    const TASKS: u64 = 10_000_000;
    const PER_SCOPE: u64 = 10_000;
    for workers in worker_counts() {
        let pool = ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap();
        for _ in 0..soak_rounds() {
            let sum = AtomicU64::new(0);
            for batch in 0..TASKS / PER_SCOPE {
                pool.scope(|s| {
                    for i in 0..PER_SCOPE {
                        let sum = &sum;
                        s.spawn(move || {
                            sum.fetch_add(batch * PER_SCOPE + i, Ordering::Relaxed);
                        });
                    }
                })
                .unwrap();
            }
            assert_eq!(
                sum.load(Ordering::Relaxed),
                TASKS * (TASKS - 1) / 2,
                "workers={workers}"
            );
        }
    }
}

#[test]
#[ignore = "long: random panics in nested join/scope"]
fn random_panics_never_hang_or_kill_workers() {
    // Thousands of expected panics: keep the output clean.
    std::panic::set_hook(Box::new(|_| {}));
    let rounds = 2_000 * soak_rounds();
    let seed = soak_seed();
    for workers in worker_counts() {
        let pool = ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap();
        let panics = AtomicUsize::new(0);
        let oks = AtomicUsize::new(0);
        for round in 0..rounds {
            let r = pool.scope(|s| {
                for i in 0..64_u64 {
                    let pool = &pool;
                    s.spawn(move || {
                        let h = hash(seed ^ (round * 64 + i));
                        let r = pool.join(
                            || {
                                assert!(!h.is_multiple_of(7), "left");
                                h
                            },
                            || {
                                assert!(!h.is_multiple_of(11), "right");
                                h
                            },
                        );
                        if r.is_err() {
                            // A panic in one branch does not prevent handling the other's result.
                        }
                    });
                }
            });
            match r {
                Ok(()) => oks.fetch_add(1, Ordering::Relaxed),
                Err(JobError::Panicked { .. }) => panics.fetch_add(1, Ordering::Relaxed),
                Err(other) => panic!("unexpected {other}"),
            };
            let handle = pool.spawn(Priority::Background, move || {
                assert!(!hash(seed ^ round).is_multiple_of(3), "background");
                round
            });
            match handle.join() {
                Ok(v) => assert_eq!(v, round),
                Err(JobError::Panicked { .. }) => {}
                Err(other) => panic!("unexpected {other}"),
            }
        }
        // The pool still works.
        let mut v = vec![1_u64; 100_000];
        pool.parallel_for(&mut v, Grain::Auto, |x| *x += 1).unwrap();
        assert!(v.iter().all(|&x| x == 2));
        assert_eq!(
            oks.load(Ordering::Relaxed) + panics.load(Ordering::Relaxed),
            usize::try_from(rounds).unwrap()
        );
    }
}

#[test]
#[ignore = "long: repeated pool creation and destruction"]
fn create_drop_cycles_do_not_leak_or_hang() {
    for i in 0..500 * soak_rounds() {
        let workers = usize::try_from(1 + i % 8).unwrap();
        let pool = ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap();
        let handles: Vec<_> = (0..32)
            .map(|j| pool.spawn(Priority::Normal, move || j * 2))
            .collect();
        if i % 2 == 0 {
            for (j, h) in handles.into_iter().enumerate() {
                assert_eq!(h.join().unwrap(), j * 2);
            }
        }
        drop(pool);
    }
}
