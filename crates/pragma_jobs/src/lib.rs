//! L2: work-stealing job system `pragma.core`.
//!
//! Пул рабочих потоков ([`ThreadPool`]) с локальными LIFO-очередями, глобальными очередями
//! по приоритетам и кражей задач (`docs/architecture.md` §3.1–3.2). Любое ожидание
//! (`join`, `scope`, [`JobHandle::join`]) исполняет чужие задачи, пока ждёт своих
//! (help-while-wait), поэтому вложенный параллелизм не дедлочит даже на одном воркере.
//! Паника в задаче перехватывается и возвращается как [`JobError::Panicked`].
//!
//! ```
//! use pragma_jobs::{Grain, PoolConfig, Priority, ThreadPool};
//!
//! let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
//!
//! // fork-join
//! let (a, b) = pool.join(|| 1 + 1, || 2 + 2)?;
//! assert_eq!((a, b), (2, 4));
//!
//! // задачи заимствуют данные со стека
//! let mut data = vec![1_u32; 1000];
//! pool.scope(|s| {
//!     for chunk in data.chunks_mut(100) {
//!         s.spawn(move || chunk.iter_mut().for_each(|x| *x *= 2));
//!     }
//! })?;
//! assert!(data.iter().all(|&x| x == 2));
//!
//! pool.parallel_for(&mut data, Grain::Auto, |x| *x += 1)?;
//! let sum = pool.parallel_reduce(&data, Grain::Fixed(64), |_, c| c.iter().sum::<u32>(), |a, b| a + b)?;
//! assert_eq!(sum, Some(3000));
//!
//! // 'static-задача с хэндлом; паника не роняет процесс
//! let handle = pool.spawn(Priority::Background, || -> u32 { panic!("bake failed") });
//! assert!(handle.join().is_err());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![forbid(unsafe_code)]

mod config;
mod error;
mod handle;
mod pool;
mod priority;
mod scope;
mod spawner;

pub use config::{PoolConfig, ShutdownPolicy};
pub use error::{JobError, JobsError};
pub use handle::JobHandle;
pub use pool::ThreadPool;
pub use priority::{Grain, Priority};
pub use scope::Scope;
pub use spawner::Spawner;
