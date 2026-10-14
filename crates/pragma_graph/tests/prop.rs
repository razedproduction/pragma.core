//! Graph property tests: arbitrary descriptions → either the error predicted by the oracle or a
//! correct stable order, and execution respects the edges (`tests/oracle/mod.rs`).
// Test code: a panic is a test failure.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod oracle;

use std::sync::OnceLock;

use pragma_jobs::{PoolConfig, ThreadPool};
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn random_descriptions_match_oracle(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        oracle::check(&oracle::Spec::from_bytes(&bytes), None, 0);
    }

    #[test]
    fn random_graphs_execute_in_order(
        bytes in proptest::collection::vec(any::<u8>(), 0..64),
        pool_index in 0_usize..3,
    ) {
        // The `ordered` bit is always set: this way most descriptions build and run.
        let mut bytes = bytes;
        if let Some(first) = bytes.first_mut() {
            *first |= 1;
        }
        oracle::check(&oracle::Spec::from_bytes(&bytes), Some(&pools()[pool_index]), 3);
    }
}

proptest! {
    // Each case builds thousands of tasks: few cases, and all three shapes occur.
    #![proptest_config(ProptestConfig::with_cases(12))]

    #[test]
    fn large_graphs_are_too_large_not_abort(selector in any::<u8>()) {
        oracle::check_large(selector);
    }
}

#[test]
fn decoder_covers_every_outcome() {
    // Random bytes by themselves almost always produce an error; make sure the decoder
    // produces both successful graphs and every kind of error.
    use pragma_graph::GraphError;
    let mut seen = [false; 5];
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    for _ in 0..20_000 {
        let bytes: Vec<u8> = (0..48)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect();
        let spec = oracle::Spec::from_bytes(&bytes);
        let mut g = if spec.ordered {
            pragma_graph::FrameGraph::builder().ordered()
        } else {
            pragma_graph::FrameGraph::builder()
        };
        for t in &spec.tasks {
            let deps = t.deps.clone();
            g.task(
                t.name.clone(),
                move |mut c| {
                    for (before, other) in deps {
                        c = if before {
                            c.before(other)
                        } else {
                            c.after(other)
                        };
                    }
                    c
                },
                |_| (),
            );
        }
        let slot = match g.build() {
            Ok(_) => 0,
            Err(GraphError::DuplicateTask { .. }) => 1,
            Err(GraphError::UnknownDependency { .. }) => 2,
            Err(GraphError::Cycle { .. }) => 3,
            Err(_) => 4,
        };
        if let Some(s) = seen.get_mut(slot) {
            *s = true;
        }
    }
    // Without resources there are no conflicts in this build, so slot 4 is optional.
    assert!(seen[..4].iter().all(|&s| s), "{seen:?}");
}
