use std::any::Any;

/// Errors of async tasks and the executor.
///
/// `Clone`/`PartialEq`, like `JobError`: task outcomes are sent between threads and compared.
/// The enum and its struct variants are `#[non_exhaustive]`: match with `..` and a wildcard arm.
///
/// ```
/// use pragma_async::{AsyncConfig, AsyncError, Executor};
/// use pragma_jobs::{PoolConfig, ThreadPool};
///
/// let pool = ThreadPool::new(PoolConfig::default().with_workers(1))?;
/// let exec = Executor::new(&pool, AsyncConfig::default())?;
/// drop(pool);
/// assert!(!exec.is_alive());
///
/// // The pool has stopped: the task is cancelled instead of being queued forever.
/// match exec.spawn_async(async { 42 }).wait() {
///     Err(AsyncError::Cancelled) => {}
///     Err(AsyncError::Panicked { message, .. }) => eprintln!("task failed: {message}"),
///     other => panic!("unexpected {other:?}"),
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AsyncError {
    /// The task was cancelled before completion: its handle was dropped, or the pool or executor stopped.
    #[error("task was cancelled")]
    Cancelled,
    /// The task panicked; the panic was caught and did not bring the worker down.
    #[error("task `{task}` panicked: {message}")]
    #[non_exhaustive]
    Panicked {
        /// Which task: `spawn_async` or `unblock`.
        task: &'static str,
        /// The panic message, if it was a string.
        message: String,
    },
    /// The blocking pool is full: the task was not accepted, and the caller did not wait.
    #[error("blocking pool is full ({capacity} pending tasks)")]
    #[non_exhaustive]
    Overloaded {
        /// The limit on simultaneously queued blocking tasks.
        capacity: usize,
    },
    /// The OS refused to create a blocking pool thread, and there are no free threads.
    #[error("failed to spawn blocking thread `{thread}`: {message}")]
    #[non_exhaustive]
    SpawnFailed {
        /// Thread name.
        thread: String,
        /// The OS error text (`std::io::Error` is not `Clone`).
        message: String,
    },
    /// A blocking wait was called from pool work (a worker or a graph frame): it would delay
    /// the frame or hang the pool. Use `poll_ready` or `.await`.
    #[error("blocking wait inside pool work is not allowed; use poll_ready() or .await")]
    WouldBlock,
    /// The result has already been taken from the handle.
    #[error("task result was already taken")]
    Consumed,
    /// Invalid executor configuration.
    #[error("invalid async config: {reason}")]
    #[non_exhaustive]
    InvalidConfig {
        /// What exactly is wrong.
        reason: &'static str,
    },
}

impl AsyncError {
    pub(crate) fn panicked(task: &'static str, payload: &(dyn Any + Send)) -> Self {
        let message = if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            String::from("<non-string panic payload>")
        };
        Self::Panicked { task, message }
    }
}
