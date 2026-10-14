//! Fuzzing the graph builder: an arbitrary description → either the error predicted by the oracle
//! or a correct stable order; the built graph is run on a pool and checked.
#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use pragma_jobs::{PoolConfig, ThreadPool};

// The same oracle as in the `pragma_graph` property tests.
#[path = "../../crates/pragma_graph/tests/oracle/mod.rs"]
mod oracle;

fn pool() -> &'static ThreadPool {
    static POOL: OnceLock<ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        ThreadPool::new(PoolConfig::default().with_workers(2)).unwrap_or_else(|e| panic!("{e}"))
    })
}

fuzz_target!(|data: &[u8]| {
    // The 0xFE prefix means a large graph (MAX_TASKS / MAX_EDGES limits → TooLarge, not abort).
    if let [0xFE, selector, ..] = data {
        oracle::check_large(*selector);
        return;
    }
    oracle::check(&oracle::Spec::from_bytes(data), Some(pool()), 2);
});
