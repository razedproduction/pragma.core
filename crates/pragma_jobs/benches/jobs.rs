//! Benchmarks of the job system against `rayon` on pools of the same size.
//!
//! `cargo bench -p pragma_jobs` (report: `target/criterion/report/index.html`).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss
)]

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use pragma_jobs::{Grain, PoolConfig, Priority, ThreadPool};

fn workers() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get())
}

fn pragma_pool() -> ThreadPool {
    ThreadPool::new(PoolConfig::default().with_workers(workers())).unwrap()
}

fn rayon_pool() -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(workers())
        .build()
        .unwrap()
}

fn pragma_fib(pool: &ThreadPool, n: u32) -> u64 {
    if n < 2 {
        return u64::from(n);
    }
    let (a, b) = pool
        .join(|| pragma_fib(pool, n - 1), || pragma_fib(pool, n - 2))
        .unwrap();
    a + b
}

fn rayon_fib(n: u32) -> u64 {
    if n < 2 {
        return u64::from(n);
    }
    let (a, b) = rayon::join(|| rayon_fib(n - 1), || rayon_fib(n - 2));
    a + b
}

fn bench_join(c: &mut Criterion) {
    let pragma = pragma_pool();
    let rayon = rayon_pool();
    let mut g = c.benchmark_group("join");
    g.bench_function("pragma/single", |b| {
        b.iter(|| pragma.join(|| black_box(1), || black_box(2)).unwrap());
    });
    g.bench_function("rayon/single", |b| {
        b.iter(|| rayon.install(|| rayon::join(|| black_box(1), || black_box(2))));
    });
    // Recursive fork-join: ~11k nested joins. In pragma the main thread takes part in the
    // work itself; in rayon the call goes through install.
    g.bench_function("pragma/fib20", |b| {
        b.iter(|| black_box(pragma_fib(&pragma, 20)));
    });
    g.bench_function("rayon/fib20", |b| {
        b.iter(|| rayon.install(|| black_box(rayon_fib(20))));
    });
    g.finish();
}

fn bench_spawn(c: &mut Criterion) {
    let pragma = pragma_pool();
    let rayon = rayon_pool();
    let mut g = c.benchmark_group("spawn");
    g.throughput(Throughput::Elements(1000));
    g.bench_function("pragma/scope_1000", |b| {
        b.iter(|| {
            pragma
                .scope(|s| {
                    for i in 0..1000_u32 {
                        s.spawn(move || {
                            black_box(i);
                        });
                    }
                })
                .unwrap();
        });
    });
    g.bench_function("rayon/scope_1000", |b| {
        b.iter(|| {
            rayon.scope(|s| {
                for i in 0..1000_u32 {
                    s.spawn(move |_| {
                        black_box(i);
                    });
                }
            });
        });
    });
    g.bench_function("pragma/spawn_join_handle", |b| {
        b.iter(|| {
            pragma
                .spawn(Priority::Frame, || black_box(1))
                .join()
                .unwrap()
        });
    });
    g.finish();
}

fn bench_parallel_for(c: &mut Criterion) {
    use rayon::prelude::*;

    const N: usize = 1_000_000;
    let pragma = pragma_pool();
    let rayon = rayon_pool();
    let mut data: Vec<f32> = (0..N).map(|i| i as f32).collect();
    let mut g = c.benchmark_group("parallel_for");
    g.throughput(Throughput::Elements(N as u64));
    let update = |x: &mut f32| *x = (*x * 1.000_1 + 0.5).sqrt();
    g.bench_with_input(BenchmarkId::new("pragma", N), &N, |b, _| {
        b.iter(|| pragma.parallel_for(&mut data, Grain::Auto, update).unwrap());
    });
    g.bench_with_input(BenchmarkId::new("rayon", N), &N, |b, _| {
        b.iter(|| rayon.install(|| data.par_iter_mut().for_each(update)));
    });
    g.bench_with_input(BenchmarkId::new("sequential", N), &N, |b, _| {
        b.iter(|| data.iter_mut().for_each(update));
    });
    g.bench_with_input(BenchmarkId::new("pragma_reduce", N), &N, |b, _| {
        b.iter(|| {
            pragma
                .parallel_reduce(
                    &data,
                    Grain::Auto,
                    |_, c| c.iter().sum::<f32>(),
                    |a, b| a + b,
                )
                .unwrap()
        });
    });
    g.bench_with_input(BenchmarkId::new("rayon_reduce", N), &N, |b, _| {
        b.iter(|| rayon.install(|| data.par_iter().sum::<f32>()));
    });
    g.finish();
}

criterion_group!(benches, bench_join, bench_spawn, bench_parallel_for);
criterion_main!(benches);
