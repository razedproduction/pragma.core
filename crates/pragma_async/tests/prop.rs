//! Много async-задач со случайной отменой и паниками: каждая неотменённая задача даёт
//! ожидаемый исход, каждый future сброшен ровно один раз, ничего не виснет.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

mod common;

use common::{DropCounter, wait_until};
use futures_lite::future;
use pragma_async::{AsyncConfig, AsyncError, AsyncHandle, Executor};
use pragma_jobs::{PoolConfig, ThreadPool};
use proptest::prelude::*;

#[derive(Debug, Clone, Copy)]
enum Outcome {
    Value,
    Panic,
    /// Никогда не завершается: такую задачу обязательно отменяют.
    Pending,
}

#[derive(Debug, Clone, Copy)]
enum Cancel {
    No,
    Immediately,
    AfterYield,
}

#[derive(Debug, Clone, Copy)]
struct Spec {
    outcome: Outcome,
    yields: u8,
    blocking: bool,
    cancel: Cancel,
}

fn spec() -> impl Strategy<Value = Spec> {
    (
        prop_oneof![
            Just(Outcome::Value),
            Just(Outcome::Panic),
            Just(Outcome::Pending)
        ],
        0_u8..4,
        any::<bool>(),
        prop_oneof![
            Just(Cancel::No),
            Just(Cancel::Immediately),
            Just(Cancel::AfterYield)
        ],
    )
        .prop_map(|(outcome, yields, blocking, cancel)| {
            let cancel = match (outcome, cancel) {
                (Outcome::Pending, Cancel::No) => Cancel::Immediately,
                (_, c) => c,
            };
            Spec {
                outcome,
                yields,
                blocking,
                cancel,
            }
        })
}

fn run_specs(workers: usize, specs: &[Spec]) {
    let pool = ThreadPool::new(PoolConfig::default().with_workers(workers)).unwrap();
    let exec = Executor::new(
        &pool,
        AsyncConfig::default()
            .with_blocking_threads(2)
            .with_blocking_capacity(specs.len().max(2)),
    )
    .unwrap();
    let drops = DropCounter::default();
    let mut live: Vec<(usize, AsyncHandle<usize>)> = Vec::new();
    let mut late_cancel = Vec::new();
    for (i, s) in specs.iter().copied().enumerate() {
        let token = drops.token();
        let io = exec.clone();
        let handle = exec.spawn_async(async move {
            let _token = token;
            for _ in 0..s.yields {
                future::yield_now().await;
            }
            let base = if s.blocking {
                io.unblock(move || i).await.unwrap_or(i)
            } else {
                i
            };
            match s.outcome {
                Outcome::Value => base,
                Outcome::Panic => panic!("task {i}"),
                Outcome::Pending => future::pending().await,
            }
        });
        match s.cancel {
            Cancel::No => live.push((i, handle)),
            Cancel::Immediately => drop(handle),
            Cancel::AfterYield => late_cancel.push(handle),
        }
    }
    std::thread::yield_now();
    drop(late_cancel);
    for (i, handle) in live {
        let spec = specs[i];
        match (spec.outcome, handle.wait()) {
            (Outcome::Value, Ok(v)) => assert_eq!(v, i),
            (Outcome::Panic, Err(AsyncError::Panicked { message, .. })) => {
                assert_eq!(message, format!("task {i}"));
            }
            (outcome, other) => panic!("task {i} ({outcome:?}): unexpected {other:?}"),
        }
    }
    wait_until("all futures dropped", || drops.get() == specs.len());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn random_cancel_and_panics(workers in 1_usize..4, specs in prop::collection::vec(spec(), 0..48)) {
        run_specs(workers, &specs);
    }
}

/// Длинный стресс: `cargo test -p pragma_async --release -- --ignored`.
#[test]
#[ignore = "long stress; run in nightly CI"]
fn stress_many_tasks_random_cancel_and_panics() {
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let cores = std::thread::available_parallelism().map_or(2, usize::from);
    for round in 0..40_usize {
        let workers = 1 + round % (2 * cores);
        let specs: Vec<Spec> = (0..5_000)
            .map(|_| {
                let r = next();
                let outcome = match r % 10 {
                    0 => Outcome::Panic,
                    1 => Outcome::Pending,
                    _ => Outcome::Value,
                };
                let cancel = match ((r >> 8) % 4, outcome) {
                    (_, Outcome::Pending) | (0, _) => Cancel::Immediately,
                    (1, _) => Cancel::AfterYield,
                    _ => Cancel::No,
                };
                Spec {
                    outcome,
                    yields: u8::try_from((r >> 16) % 4).unwrap(),
                    blocking: (r >> 24) % 8 == 0,
                    cancel,
                }
            })
            .collect();
        run_specs(workers, &specs);
    }
}
