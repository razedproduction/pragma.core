use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;

use async_task::Runnable;
use futures_lite::FutureExt;
use pragma_jobs::{Priority, Spawner, ThreadPool};

use crate::blocking::BlockingPool;
use crate::{AsyncConfig, AsyncError, AsyncHandle};

const SPAWN_ASYNC: &str = "spawn_async";
const UNBLOCK: &str = "unblock";

/// An async task executor on top of a [`ThreadPool`] (`docs/architecture.md` §3.4).
///
/// - [`spawn_async`](Self::spawn_async): the future runs on the pool workers with priority
///   [`Priority::Background`], within the background worker limit, without displacing the frame.
/// - [`unblock`](Self::unblock): a synchronous (blocking) closure, for example reading a file,
///   runs in a separate bounded blocking pool and does not occupy the pool workers.
///
/// Cheap to clone (`Arc`); a clone can be captured in a future to spawn subtasks.
/// The executor does not keep the pool alive: once the pool stops, unfinished tasks are cancelled
/// ([`AsyncError::Cancelled`]). Dropping the last clone cancels blocking tasks not yet taken and
/// waits for the running ones (synchronous I/O cannot be interrupted).
#[derive(Clone)]
pub struct Executor {
    inner: Arc<Inner>,
}

struct Inner {
    spawner: Spawner,
    blocking: BlockingPool,
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor")
            .field("pool", &self.inner.spawner)
            .field("blocking_threads", &self.inner.blocking.threads())
            .finish()
    }
}

impl Executor {
    /// Creates an executor over `pool`. Blocking pool threads are created on demand.
    pub fn new(pool: &ThreadPool, config: AsyncConfig) -> Result<Self, AsyncError> {
        let config = config.validate()?;
        Ok(Self {
            inner: Arc::new(Inner {
                spawner: pool.spawner(),
                blocking: BlockingPool::new(config),
            }),
        })
    }

    /// Spawns a future on the pool workers with priority [`Priority::Background`].
    ///
    /// A panic in the future is caught and returned as [`AsyncError::Panicked`]; the worker
    /// keeps running. Dropping the handle cancels the task. If the pool has stopped, the handle returns
    /// [`AsyncError::Cancelled`].
    pub fn spawn_async<F>(&self, future: F) -> AsyncHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.spawn_async_named(SPAWN_ASYNC, future)
    }

    /// Like [`spawn_async`](Self::spawn_async), but with a task name: every `poll` of the future
    /// runs with the label `async`/`name` ([`pragma_jobs::current_task`], for the panic hook and the
    /// watchdog), and the name also goes into [`AsyncError::Panicked`].
    pub fn spawn_async_named<F>(&self, name: &'static str, future: F) -> AsyncHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let caught = AssertUnwindSafe(future).catch_unwind();
        let future = async move {
            caught
                .await
                .map_err(|payload| AsyncError::panicked(name, &*payload))
        };
        let spawner = self.inner.spawner.clone();
        let schedule = move |runnable: Runnable| {
            // A refusal (the pool has stopped) drops the `Runnable`: `async-task` cancels the task and
            // wakes the waiter, so the error needs no handling here.
            let _ = spawner.spawn_detached(Priority::Background, move || {
                pragma_jobs::labeled("async", name, || runnable.run());
            });
        };
        let (runnable, task) = async_task::spawn(future, schedule);
        runnable.schedule();
        AsyncHandle::running(task.fallible(), self.inner.spawner.clone())
    }

    /// Runs a synchronous closure in the blocking pool (file I/O, decompression).
    ///
    /// The call does not block: if `blocking_capacity` tasks are already queued, the handle immediately
    /// holds [`AsyncError::Overloaded`]. It works both from a future (`.await` on the handle) and
    /// from a frame (`poll_ready`). Dropping the handle before the start cancels the task; a started one
    /// runs to completion and its result is discarded. A panic is [`AsyncError::Panicked`].
    pub fn unblock<F, T>(&self, f: F) -> AsyncHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let spawner = self.inner.spawner.clone();
        let slot = match self.inner.blocking.reserve() {
            Ok(slot) => slot,
            Err(error) => return AsyncHandle::ready(Err(error), spawner),
        };
        let future = async move {
            let _slot = slot;
            let labeled = || pragma_jobs::labeled("unblock", UNBLOCK, f);
            panic::catch_unwind(AssertUnwindSafe(labeled))
                .map_err(|payload| AsyncError::panicked(UNBLOCK, &*payload))
        };
        match self
            .inner
            .blocking
            .submit(|schedule| async_task::spawn(future, schedule))
        {
            Ok(task) => AsyncHandle::running(task.fallible(), spawner),
            Err(error) => AsyncHandle::ready(Err(error), spawner),
        }
    }

    /// Whether the pool running the tasks still accepts work.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.inner.spawner.is_alive()
    }

    /// How many blocking pool threads are running right now (at most `blocking_threads`).
    #[must_use]
    pub fn blocking_threads(&self) -> usize {
        self.inner.blocking.threads()
    }
}
