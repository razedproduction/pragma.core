//! L3: async tasks of `pragma.core` (`docs/architecture.md` §3.4).
//!
//! [`Executor`] runs futures on the workers of a [`ThreadPool`](pragma_jobs::ThreadPool) with
//! `Background` priority (via `async-task`, with no second thread pool and no tokio,
//! DECISIONS #8), and hands synchronous I/O to a small bounded blocking pool
//! ([`Executor::unblock`]) so that it does not occupy frame workers (DECISIONS #23).
//!
//! - Dropping an [`AsyncHandle`] cancels the task; a panic in a future is [`AsyncError::Panicked`].
//! - The "async → frame" bridge is [`AsyncHandle::poll_ready`]: a non-blocking check at the start
//!   of a frame. A blocking [`AsyncHandle::wait`] from pool work (a worker, a graph frame) is forbidden
//!   and returns [`AsyncError::WouldBlock`].
//!
//! ```
//! use pragma_async::{AsyncConfig, AsyncError, Executor};
//! use pragma_jobs::{PoolConfig, ThreadPool};
//!
//! let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
//! let exec = Executor::new(&pool, AsyncConfig::default())?;
//!
//! // Background "loading": the synchronous read goes to the blocking pool, processing to a worker.
//! let io = exec.clone();
//! let mut level = exec.spawn_async(async move {
//!     let bytes = io.unblock(|| vec![1_u8, 2, 3]).await?; // std::fs::read would go here
//!     Ok::<usize, AsyncError>(bytes.len())
//! });
//!
//! // Game loop: at the start of a frame, check whether it is ready.
//! let loaded = loop {
//!     if let Some(result) = level.poll_ready() {
//!         break result??;
//!     }
//!     std::thread::yield_now(); // ...frame...
//! };
//! assert_eq!(loaded, 3);
//!
//! // A panic does not bring a worker down; dropping the handle cancels the task.
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
