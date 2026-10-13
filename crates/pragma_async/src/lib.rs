//! L3: async-задачи `pragma.core` (`docs/architecture.md` §3.4).
//!
//! [`Executor`] исполняет future на воркерах [`ThreadPool`](pragma_jobs::ThreadPool) с
//! приоритетом `Background` (через `async-task`, без второго пула потоков и без tokio —
//! DECISIONS #8), а синхронный I/O отдаёт в небольшой ограниченный blocking-пул
//! ([`Executor::unblock`]), чтобы тот не занимал воркеры кадра (DECISIONS #23).
//!
//! - Сброс [`AsyncHandle`] отменяет задачу; паника в future — [`AsyncError::Panicked`].
//! - Мост «async → кадр» — [`AsyncHandle::poll_ready`]: неблокирующая проверка в начале
//!   кадра. Блокирующее [`AsyncHandle::wait`] из работы пула (воркер, кадр графа) запрещено
//!   и возвращает [`AsyncError::WouldBlock`].
//!
//! ```
//! use pragma_async::{AsyncConfig, AsyncError, Executor};
//! use pragma_jobs::{PoolConfig, ThreadPool};
//!
//! let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
//! let exec = Executor::new(&pool, AsyncConfig::default())?;
//!
//! // Фоновая «загрузка»: синхронное чтение — в blocking-пуле, обработка — на воркере.
//! let io = exec.clone();
//! let mut level = exec.spawn_async(async move {
//!     let bytes = io.unblock(|| vec![1_u8, 2, 3]).await?; // здесь был бы std::fs::read
//!     Ok::<usize, AsyncError>(bytes.len())
//! });
//!
//! // Игровой цикл: в начале кадра проверяем, готово ли.
//! let loaded = loop {
//!     if let Some(result) = level.poll_ready() {
//!         break result??;
//!     }
//!     std::thread::yield_now(); // ...кадр...
//! };
//! assert_eq!(loaded, 3);
//!
//! // Паника не роняет воркер, сброс хэндла отменяет задачу.
//! let failed = exec.spawn_async(async { panic!("corrupt asset") }).wait();
//! assert!(matches!(failed, Err(AsyncError::Panicked { .. })));
//! drop(exec.spawn_async(futures_lite::future::pending::<()>()));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![forbid(unsafe_code)]

mod blocking;
mod config;
mod error;
mod executor;
mod handle;

pub use config::AsyncConfig;
pub use error::AsyncError;
pub use executor::Executor;
pub use handle::AsyncHandle;
