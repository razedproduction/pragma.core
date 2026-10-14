use std::sync::{Arc, Weak};

use crate::pool::{Job, Shared};
use crate::{JobError, Priority, ThreadPool};

const DETACHED_JOB: &str = "spawn_detached";

/// A lightweight reference to the pool for tasks without a handle: the basis of executors on top of
/// the pool (`pragma_async` schedules the `Runnable`s of its futures through it).
///
/// Cheap to clone and **does not keep the pool alive**: once the pool begins shutting down
/// ([`ThreadPool`] is dropped), tasks are not accepted and are dropped without running.
///
/// ```
/// use std::sync::mpsc;
/// use pragma_jobs::{PoolConfig, Priority, ThreadPool};
///
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(1))?;
/// let spawner = pool.spawner();
/// let (tx, rx) = mpsc::channel();
/// spawner.spawn_detached(Priority::Normal, move || tx.send(7).unwrap_or(()))?;
/// assert_eq!(rx.recv()?, 7);
///
/// drop(pool);
/// assert!(spawner.spawn_detached(Priority::Normal, || ()).is_err());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct Spawner {
    shared: Weak<Shared>,
}

impl std::fmt::Debug for Spawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spawner")
            .field("alive", &self.is_alive())
            .finish()
    }
}

impl Spawner {
    /// Queues a `'static` task with priority `priority`, without a handle to the result.
    ///
    /// A panic of the task is caught by the worker and logged, and the worker keeps running;
    /// if the result or the panic matters to the caller, the task reports them itself.
    ///
    /// If the pool is already shutting down or has stopped, the task is dropped **without running**
    /// (its destructor runs immediately) and [`JobError::Cancelled`] is returned. A task
    /// accepted in a race with shutdown may also be dropped without running, just like
    /// queued tasks under [`ShutdownPolicy::Cancel`](crate::ShutdownPolicy::Cancel).
    pub fn spawn_detached<F>(&self, priority: Priority, f: F) -> Result<(), JobError>
    where
        F: FnOnce() + Send + 'static,
    {
        let Some(shared) = self.shared.upgrade() else {
            drop(f);
            return Err(JobError::cancelled(DETACHED_JOB));
        };
        let job = Job::Boxed(Box::new(move || {
            crate::labeled("detached", DETACHED_JOB, f)
        }));
        if shared.push_detached(job, priority) {
            Ok(())
        } else {
            Err(JobError::cancelled(DETACHED_JOB))
        }
    }

    /// Whether the pool still accepts tasks (has not begun shutting down).
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.shared.upgrade().is_some_and(|s| !s.is_shutdown())
    }

    /// Whether the current thread is doing this pool's work: it is one of its workers or an external
    /// thread inside `scope`/`join`/[`JobHandle::join`](crate::JobHandle::join) (for example the main
    /// thread during a graph frame). A blocking wait here would delay the frame or hang the pool.
    ///
    /// ```
    /// use pragma_jobs::{PoolConfig, ThreadPool};
    ///
    /// let pool = ThreadPool::new(PoolConfig::default().with_workers(1))?;
    /// let spawner = pool.spawner();
    /// assert!(!spawner.in_pool_context()); // plain main thread: blocking is fine
    /// let (inside, _) = pool.join(|| spawner.in_pool_context(), || ())?;
    /// assert!(inside); // inside `join`: do not block
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn in_pool_context(&self) -> bool {
        self.shared.upgrade().is_some_and(|s| s.in_context())
    }
}

impl ThreadPool {
    /// A lightweight reference to the pool for tasks without a handle (see [`Spawner`]).
    #[must_use]
    pub fn spawner(&self) -> Spawner {
        Spawner {
            shared: Arc::downgrade(&self.shared),
        }
    }
}
