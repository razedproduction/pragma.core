use std::any::Any;

/// Errors of the pool lifecycle (creation, configuration).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum JobsError {
    /// Invalid pool configuration.
    #[error("invalid pool config: {reason}")]
    #[non_exhaustive]
    InvalidConfig {
        /// What exactly is wrong.
        reason: &'static str,
    },
    /// The OS refused to create a worker thread.
    #[error("failed to spawn worker thread `{name}`")]
    #[non_exhaustive]
    SpawnFailed {
        /// Thread name.
        name: String,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },
}

/// Task execution errors: returned from `join`/`scope`/`parallel_*` and [`JobHandle::join`].
///
/// Kept separate from [`JobsError`] (DECISIONS.md #13): a task outcome can be cloned, compared
/// and sent between threads, while a pool creation error carries a `std::io::Error`.
///
/// [`JobHandle::join`]: crate::JobHandle::join
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum JobError {
    /// The task panicked; the panic was caught and the worker keeps running.
    #[error("job `{job}` panicked: {message}")]
    #[non_exhaustive]
    Panicked {
        /// Which task (for example `spawn`, `join b`, `parallel_for chunk`).
        job: String,
        /// The panic message, if it was a string.
        message: String,
    },
    /// The task was dropped without running (the pool was stopped with [`ShutdownPolicy::Cancel`]).
    ///
    /// [`ShutdownPolicy::Cancel`]: crate::ShutdownPolicy::Cancel
    #[error("job `{job}` was cancelled before it ran")]
    #[non_exhaustive]
    Cancelled {
        /// Which task.
        job: String,
    },
}

impl JobError {
    pub(crate) fn panicked(job: &str, payload: &(dyn Any + Send)) -> Self {
        let message = if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            String::from("<non-string panic payload>")
        };
        Self::Panicked {
            job: job.to_owned(),
            message,
        }
    }

    pub(crate) fn cancelled(job: &str) -> Self {
        Self::Cancelled {
            job: job.to_owned(),
        }
    }
}
