//! Стоимость планирования async-задач в пул (долг M3 → M4b, DECISIONS #31).
//!
//! Каждое планирование `Runnable` кладёт в пул `Box` с замыканием (`Spawner::spawn_detached`).
//! Бенч сравнивает полную цену перепланирования (`yield_now` → пробуждение → пул → `poll`) с
//! ценой самого `Box`. `cargo bench -p pragma_async`.
#![allow(missing_docs, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use pragma_async::{AsyncConfig, Executor};
use pragma_jobs::{PoolConfig, ThreadPool};

const YIELDS: u64 = 1000;

fn bench_schedule(c: &mut Criterion) {
    let pool = ThreadPool::new(PoolConfig::default()).unwrap();
    let exec = Executor::new(&pool, AsyncConfig::default()).unwrap();
    let mut g = c.benchmark_group("async");

    // Одна задача перепланирует себя `YIELDS` раз: на каждый раз — один `Box` в пуле.
    g.throughput(Throughput::Elements(YIELDS));
    g.bench_function("reschedule_x1000", |b| {
        b.iter(|| {
            exec.spawn_async(async {
                for _ in 0..YIELDS {
                    futures_lite::future::yield_now().await;
                }
            })
            .wait()
            .unwrap();
        });
    });

    // 1000 коротких задач: создание задачи `async-task` + одно планирование каждой.
    g.bench_function("spawn_x1000", |b| {
        b.iter(|| {
            let handles: Vec<_> = (0..YIELDS)
                .map(|i| exec.spawn_async(async move { black_box(i) }))
                .collect();
            for h in handles {
                black_box(h.wait().unwrap());
            }
        });
    });

    // Цена самого `Box<dyn FnOnce>` (аллокация + вызов + освобождение) — та часть
    // перепланирования, которую можно было бы убрать.
    g.bench_function("box_dyn_fnonce_x1000", |b| {
        b.iter(|| {
            for i in 0..YIELDS {
                let job: Box<dyn FnOnce() + Send> = Box::new(move || {
                    black_box(i);
                });
                black_box(job)();
            }
        });
    });
    g.finish();
}

criterion_group!(benches, bench_schedule);
criterion_main!(benches);
