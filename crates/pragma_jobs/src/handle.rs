use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, PoisonError};

use pragma_sync::AtomicCounter;

use crate::pool::{Job, Shared, enter, help_once};
use crate::{JobError, Priority, ThreadPool};

const SPAWN_JOB: &str = "spawn";

struct JobState<T> {
    done: AtomicCounter,
    result: Mutex<Option<Result<T, JobError>>>,
}

/// A handle to a task started via [`ThreadPool::spawn`].
///
/// Dropping the handle does not cancel the task: it runs to completion and the result is discarded.
///
/// ```
/// use pragma_jobs::{JobError, PoolConfig, Priority, ThreadPool};
///
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
/// let mesh = pool.spawn_named("bake navmesh", Priority::Background, || vec![0_u8; 64]);
///
/// // ... the frame goes on; poll without blocking ...
/// let _ready = mesh.is_finished();
///
/// assert_eq!(mesh.join()?.len(), 64);
///
/// let broken = pool.spawn_named("bake lightmap", Priority::Background, || -> u8 {
///     panic!("no lights")
/// });
/// match broken.join() {
///     Err(JobError::Panicked { job, message, .. }) => {
///         assert_eq!(job, "bake lightmap");
///         assert_eq!(message, "no lights");
///     }
///     other => panic!("unexpected {other:?}"),
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[must_use = "without join() the task's result and panic are lost"]
pub struct JobHandle<T> {
    state: Arc<JobState<T>>,
    shared: Arc<Shared>,
    name: &'static str,
}

impl<T> std::fmt::Debug for JobHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobHandle")
            .field("finished", &self.is_finished())
            .finish()
    }
}

impl<T> JobHandle<T> {
    /// Whether the task has finished (successfully, by panic or by cancellation).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.state.done.is_zero()
    }

    /// Waits for the result. While waiting it runs other pool tasks (help-while-wait), so
    /// a call from a task of the same pool does not deadlock even on a single worker.
    pub fn join(self) -> Result<T, JobError> {
        let shared = &self.shared;
        enter(shared, || {
            self.state.done.wait_while_helping(|| help_once(shared));
        });
        self.state
            .result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| Err(JobError::cancelled(self.name)))
    }
}

/// The executor side: writes the result; dropping it without a result means cancellation.
struct Completer<T> {
    state: Arc<JobState<T>>,
    name: &'static str,
}

impl<T> Completer<T> {
    fn complete(self, result: Result<T, JobError>) {
        *self
            .state
            .result
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(result);
        // The completion signal is sent in `Drop`.
    }
}

impl<T> Drop for Completer<T> {
    fn drop(&mut self) {
        {
            let mut slot = self
                .state
                .result
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if slot.is_none() {
                *slot = Some(Err(JobError::cancelled(self.name)));
            }
        }
        self.state.done.decrement();
    }
}

impl ThreadPool {
    /// Starts a `'static` task with priority `priority` and returns a handle to the result.
    ///
    /// For work that borrows local data use [`scope`](Self::scope) instead.
    ///
    /// ```
    /// use pragma_jobs::{PoolConfig, Priority, ThreadPool};
    ///
    /// let pool = ThreadPool::new(PoolConfig::default().with_workers(2))?;
    /// let path = String::from("level1.bin");
    /// let handle = pool.spawn(Priority::Background, move || path.len());
    /// assert_eq!(handle.join()?, 10);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn spawn<F, T>(&self, priority: Priority, f: F) -> JobHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_named(SPAWN_JOB, priority, f)
    }

    /// Like [`spawn`](Self::spawn), but with a task name: it goes into the task label
    /// ([`current_task`](crate::current_task), watchdog) and into [`JobError::Panicked`].
    ///
    /// ```
    /// use pragma_jobs::{PoolConfig, Priority, ThreadPool, current_task};
    ///
    /// let pool = ThreadPool::new(PoolConfig::default().with_workers(1))?;
    /// let handle = pool.spawn_named("decompress", Priority::Normal, || {
    ///     current_task(|task| task.map(|t| (t.kind(), t.name().to_owned())))
    /// });
    /// assert_eq!(handle.join()?, Some(("spawn", String::from("decompress"))));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn spawn_named<F, T>(&self, name: &'static str, priority: Priority, f: F) -> JobHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let state = Arc::new(JobState {
            done: AtomicCounter::new(1),
            result: Mutex::new(None),
        });
        let completer = Completer {
            state: Arc::clone(&state),
            name,
        };
        let job = Job::Boxed(Box::new(move || {
            let labeled = || crate::labeled("spawn", name, f);
            let result = panic::catch_unwind(AssertUnwindSafe(labeled))
                .map_err(|payload| JobError::panicked(name, &*payload));
            completer.complete(result);
        }));
        self.shared.push(job, priority);
        JobHandle {
            state,
            shared: Arc::clone(&self.shared),
            name,
        }
    }
}
