//! Накладные расходы кадра графа: 50 задач над 10 ресурсами (как в `tests/alloc.rs`).
//!
//! `cargo bench -p pragma_graph`. Эталон — те же системы, вызванные по очереди на одном потоке
//! в стабильном порядке графа: разница — цена планирования, а не работы.
// Бенч: паника = провал прогона.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use pragma_graph::{FrameGraph, FrameGraphBuilder, GraphError, Resources, TaskContext};
use pragma_jobs::{PoolConfig, ThreadPool};

struct R<const N: usize>(Vec<u64>);

fn work(s: u64, dst: &mut [u64]) {
    for (i, x) in dst.iter_mut().enumerate() {
        *x = x.wrapping_add(s).wrapping_add(i as u64) % 1_000_003;
    }
}

fn task<const W: usize, const RD: usize>(g: &mut FrameGraphBuilder, name: String) {
    g.task(
        name,
        |t| t.writes::<R<W>>().reads::<R<RD>>(),
        |ctx: &TaskContext<'_>| -> Result<(), GraphError> {
            let s = ctx.read::<R<RD>>()?.0.iter().sum();
            work(s, &mut ctx.write::<R<W>>()?.0);
            Ok(())
        },
    );
}

/// Две независимые цепочки ресурсов (0–4 и 5–9): граф даёт параллелизм 2 и больше.
fn graph() -> FrameGraph {
    let mut g = FrameGraph::builder().ordered();
    for i in 0..5 {
        task::<0, 1>(&mut g, format!("a{i}"));
        task::<1, 2>(&mut g, format!("b{i}"));
        task::<2, 3>(&mut g, format!("c{i}"));
        task::<3, 4>(&mut g, format!("d{i}"));
        task::<4, 0>(&mut g, format!("e{i}"));
        task::<5, 6>(&mut g, format!("f{i}"));
        task::<6, 7>(&mut g, format!("g{i}"));
        task::<7, 8>(&mut g, format!("h{i}"));
        task::<8, 9>(&mut g, format!("i{i}"));
        task::<9, 5>(&mut g, format!("j{i}"));
    }
    g.build().unwrap()
}

fn resources(len: usize) -> Resources {
    let mut r = Resources::new();
    r.insert(R::<0>(vec![1; len]));
    r.insert(R::<1>(vec![1; len]));
    r.insert(R::<2>(vec![1; len]));
    r.insert(R::<3>(vec![1; len]));
    r.insert(R::<4>(vec![1; len]));
    r.insert(R::<5>(vec![1; len]));
    r.insert(R::<6>(vec![1; len]));
    r.insert(R::<7>(vec![1; len]));
    r.insert(R::<8>(vec![1; len]));
    r.insert(R::<9>(vec![1; len]));
    r
}

fn bench(c: &mut Criterion) {
    let pool = ThreadPool::new(PoolConfig::default()).unwrap();
    let mut group = c.benchmark_group("frame_graph_50");
    for len in [16_usize, 4096] {
        let mut graph = graph();
        let mut res = resources(len);
        group.bench_function(format!("graph/{len}"), |b| {
            b.iter(|| graph.run(&pool, &mut res).unwrap());
        });

        // Эталон: те же 50 вызовов последовательно, без планировщика.
        let mut data: Vec<Vec<u64>> = (0..10).map(|_| vec![1; len]).collect();
        group.bench_function(format!("sequential/{len}"), |b| {
            b.iter(|| {
                for _ in 0..5 {
                    for (w, r) in [(0, 1), (1, 2), (2, 3), (3, 4), (4, 0)] {
                        for base in [0, 5] {
                            let s = black_box(data[base + r].iter().sum());
                            work(s, &mut data[base + w]);
                        }
                    }
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
