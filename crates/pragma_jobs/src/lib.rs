//! L2: the work-stealing job system of `pragma.core`.
//!
//! A pool of worker threads ([`ThreadPool`]) with local LIFO queues, global per-priority
//! queues and work stealing (`docs/architecture.md` §3.1–3.2). Any wait
//! (`join`, `scope`, [`JobHandle::join`]) runs other tasks while waiting for its own
//! (help-while-wait), so nested parallelism does not deadlock even on a single worker.
//! A panic in a task is caught and returned as [`JobError::Panicked`].
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
//! // tasks borrow data from the stack
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
//! // a 'static task with a handle; a panic does not bring the process down
//! let handle = pool.spawn(Priority::Background, || -> u32 { panic!("bake failed") });
//! assert!(handle.join().is_err());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Observability (`docs/architecture.md` §5)
//!
//! - [`ThreadPool::stats`]: pool metrics ([`PoolStats`]) without stopping the workers.
//! - [`current_task`]: the label of the task running on this thread (for the panic hook and
//!   crash reports); labels are set by [`labeled`]. Labels are given to graph tasks (`graph`),
//!   [`ThreadPool::spawn`] (`spawn`), [`Scope::spawn`] and `parallel_*` tasks (`scope`),
//!   [`Spawner::spawn_detached`] (`detached`) and `pragma_async` async tasks (`async`,
//!   `unblock`). The second branch of `join` gets no label: it is a hot path, and its name
//!   would say nothing anyway; the outer label names the culprit.
//! - A watchdog for stuck tasks in debug builds ([`PoolConfig::with_watchdog`]).
//! - `tracing`: `scope`/`parallel_for`/`parallel_reduce` spans at `TRACE` level; levels
//!   are compiled out with the `tracing` features (`max_level_*`, `release_max_level_*`).
//! - Feature `puffin`: the same regions in the `puffin` frame profiler.
//! - [`sys`]: thread names, priorities and core pinning (the `pragma_sys` crate).
#![forbid(unsafe_code)]

mod config;
mod error;
mod handle;
mod pool;
mod priority;
mod scope;
mod spawner;
mod stats;
#[cfg(debug_assertions)]
mod watchdog;

pub use config::{DEFAULT_IDLE_SPIN, DEFAULT_WATCHDOG, PoolConfig, ShutdownPolicy};
pub use error::{JobError, JobsError};
pub use handle::JobHandle;
pub use pool::ThreadPool;
pub use pragma_sync::{TaskLabel, current_label as current_task};
pub use priority::{Grain, Priority};
pub use scope::Scope;
pub use spawner::Spawner;
pub use stats::{PoolStats, ThreadStats};

/// The platform layer: thread names, priorities and core pinning (a re-export of
/// `pragma_sys`; the `unsafe` FFI lives there, DECISIONS #27).
pub use pragma_sys as sys;

/// Runs `f` with the task label `kind`/`name` on the current thread.
///
/// The label is visible to [`current_task`] (including from the panic hook); this is how the
/// application names the culprit of a crash. In debug builds, a task on a pool thread is also
/// registered with the watchdog ([`PoolConfig::with_watchdog`]). In release it is two TLS writes, no allocations.
///
/// ```
/// use pragma_jobs::{current_task, labeled};
///
/// let name = labeled("asset", "level.bin", || current_task(|t| t.map(|t| t.name().to_owned())));
/// assert_eq!(name.as_deref(), Some("level.bin"));
/// ```
///
/// A panic hook that names the task (no allocations before formatting the report):
///
/// ```no_run
/// std::panic::set_hook(Box::new(|info| {
///     pragma_jobs::current_task(|task| match task {
///         Some(task) => eprintln!("panic in {task}: {info}"),
///         None => eprintln!("panic outside pool tasks: {info}"),
///     });
/// }));
/// ```
pub fn labeled<R>(kind: &'static str, name: &str, f: impl FnOnce() -> R) -> R {
    #[cfg(debug_assertions)]
    let _watch = pool::watch(kind, name);
    pragma_sync::__private::run_labeled(kind, name, f)
}

/// The priority of the pool task the current thread is running; `None` if the thread is not
/// running a pool task right now (for example the main thread in a `scope` body or outside the pool).
///
/// ```
/// use pragma_jobs::{PoolConfig, Priority, ThreadPool, current_priority};
///
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(1))?;
/// assert_eq!(current_priority(), None);
/// let inside = pool.spawn(Priority::Normal, current_priority).join()?;
/// assert_eq!(inside, Some(Priority::Normal));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[must_use]
pub fn current_priority() -> Option<Priority> {
    pool::current_priority()
}
