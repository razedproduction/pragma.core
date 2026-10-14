//! Property tests: the parallel primitives are equivalent to sequential code.
// Test code: a panic is a test failure; overflow is ruled out by the input sizes.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use pragma_jobs::{Grain, PoolConfig, Scope, ThreadPool};
use proptest::prelude::*;

fn pools() -> &'static [ThreadPool] {
    static POOLS: OnceLock<Vec<ThreadPool>> = OnceLock::new();
    POOLS.get_or_init(|| {
        [1, 2, 4]
            .into_iter()
            .map(|w| ThreadPool::new(PoolConfig::default().with_workers(w)).unwrap())
            .collect()
    })
}

fn grain() -> impl Strategy<Value = Grain> {
    prop_oneof![Just(Grain::Auto), (0_usize..64).prop_map(Grain::Fixed)]
}

/// A recursive tree of scopes: every node adds its value and spawns children.
fn nested<'scope>(
    s: Scope<'scope, '_>,
    pool: &'scope ThreadPool,
    depth: u32,
    fanout: u32,
    sum: &'scope AtomicU64,
) {
    sum.fetch_add(u64::from(depth) + 1, Ordering::Relaxed);
    if depth == 0 {
        return;
    }
    for i in 0..fanout {
        if i % 2 == 0 {
            // Even children are tasks of the same scope, odd ones open a new nested scope.
            s.spawn(move || nested(s, pool, depth - 1, fanout, sum));
        } else {
            pool.scope(|inner| nested(inner, pool, depth - 1, fanout, sum))
                .unwrap();
        }
    }
}

fn expected_nested(depth: u32, fanout: u32) -> u64 {
    let mut total = u64::from(depth) + 1;
    if depth > 0 {
        total += u64::from(fanout) * expected_nested(depth - 1, fanout);
    }
    total
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn parallel_for_equals_sequential(
        data in proptest::collection::vec(any::<u32>(), 0..2000),
        grain in grain(),
        pool_index in 0_usize..3,
    ) {
        let pool = &pools()[pool_index];
        let f = |x: &mut u32| *x = x.wrapping_mul(2_654_435_761).rotate_left(7) ^ 0xA5A5;
        let mut expected = data.clone();
        expected.iter_mut().for_each(f);
        let mut actual = data;
        pool.parallel_for(&mut actual, grain, f).unwrap();
        prop_assert_eq!(actual, expected);
    }

    #[test]
    fn parallel_reduce_equals_sequential_fold(
        data in proptest::collection::vec(-1.0e6_f64..1.0e6, 0..2000),
        grain in grain(),
    ) {
        // Reference: the same fold in chunk order on a single thread.
        let size = grain.chunk_size(data.len());
        let expected = data
            .chunks(size)
            .map(|c| c.iter().sum::<f64>())
            .reduce(|a, b| a + b);
        for pool in pools() {
            let actual = pool
                .parallel_reduce(&data, grain, |_, c| c.iter().sum::<f64>(), |a, b| a + b)
                .unwrap();
            prop_assert_eq!(actual.map(f64::to_bits), expected.map(f64::to_bits));
        }
    }

    #[test]
    fn nested_scopes_at_random_depth(depth in 0_u32..5, fanout in 1_u32..4, pool_index in 0_usize..3) {
        let pool = &pools()[pool_index];
        let sum = AtomicU64::new(0);
        pool.scope(|s| nested(s, pool, depth, fanout, &sum)).unwrap();
        prop_assert_eq!(sum.load(Ordering::Relaxed), expected_nested(depth, fanout));
    }
}
