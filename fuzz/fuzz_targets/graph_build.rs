//! Fuzz построителя графа: произвольное описание → либо ошибка, предсказанная оракулом,
//! либо корректный стабильный порядок; собранный граф исполняется на пуле и проверяется.
#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use pragma_jobs::{PoolConfig, ThreadPool};

// Тот же оракул, что и в property-тестах `pragma_graph`.
#[path = "../../crates/pragma_graph/tests/oracle/mod.rs"]
mod oracle;

fn pool() -> &'static ThreadPool {
    static POOL: OnceLock<ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        ThreadPool::new(PoolConfig::default().with_workers(2)).unwrap_or_else(|e| panic!("{e}"))
    })
}

fuzz_target!(|data: &[u8]| {
    // Префикс 0xFE — большой граф (лимиты MAX_TASKS / MAX_EDGES → TooLarge, а не abort).
    if let [0xFE, selector, ..] = data {
        oracle::check_large(*selector);
        return;
    }
    oracle::check(&oracle::Spec::from_bytes(data), Some(pool()), 2);
});
