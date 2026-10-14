use std::num::NonZeroUsize;
use std::time::Duration;

use crate::JobsError;
use crate::sys::ThreadPriority;

/// Default number of `yield` rounds before parking (see [`PoolConfig::with_idle_spin`]).
pub const DEFAULT_IDLE_SPIN: u32 = 32;

/// Default watchdog threshold (debug builds only, see [`PoolConfig::with_watchdog`]).
pub const DEFAULT_WATCHDOG: Duration = Duration::from_secs(5);

/// Upper bound on the number of workers, a guard against typos in the config.
const MAX_WORKERS: usize = 512;

/// What to do with pending tasks when the pool is dropped.
///
/// ```
/// use pragma_jobs::{JobError, PoolConfig, Priority, ShutdownPolicy, ThreadPool};
///
/// let pool = ThreadPool::new(
///     PoolConfig::default().with_workers(1).with_shutdown(ShutdownPolicy::Cancel),
/// )?;
/// let (started_tx, started_rx) = std::sync::mpsc::channel();
/// let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
/// let busy = pool.spawn(Priority::Normal, move || {
///     started_tx.send(()).ok();
///     release_rx.recv().ok(); // hold the only worker
/// });
/// started_rx.recv()?;
/// let queued = pool.spawn(Priority::Normal, || 42);
///
/// // Drop the pool while its only worker is busy; release the worker once shutdown has begun.
/// let spawner = pool.spawner();
/// let dropping = std::thread::spawn(move || drop(pool));
/// while spawner.is_alive() {
///     std::thread::yield_now();
/// }
/// release_tx.send(())?;
/// dropping.join().map_err(|_| "drop panicked")?;
///
/// // The running task finished, the queued one was dropped without running.
/// assert!(busy.join().is_ok());
/// assert!(matches!(queued.join(), Err(JobError::Cancelled { .. })));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ShutdownPolicy {
    /// Workers drain all queues, then exit.
    #[default]
    Drain,
    /// Workers exit after their current task; the remaining tasks are dropped and
    /// their handles return [`JobError::Cancelled`](crate::JobError::Cancelled).
    Cancel,
}

/// Thread pool configuration.
///
/// A builder: start from [`Default`] (cores − 1 workers, at least 1) and override what the engine
/// needs. Errors are reported by [`validate`](Self::validate), which
/// [`ThreadPool::new`](crate::ThreadPool::new) calls for you.
///
/// ```
/// use std::time::Duration;
/// use pragma_jobs::{PoolConfig, ShutdownPolicy, ThreadPool};
/// use pragma_jobs::sys::ThreadPriority;
///
/// let config = PoolConfig::default()
///     .with_workers(4)
///     .with_name_prefix("game-worker")
///     .with_background_limit(Some(1)) // streaming never takes more than one worker
///     .with_shutdown(ShutdownPolicy::Cancel)
///     .with_watchdog(Some(Duration::from_millis(500)))
///     .with_thread_priority(Some(ThreadPriority::Normal));
/// assert_eq!(config.background_limit(), 1);
///
/// let pool = ThreadPool::new(config)?;
/// assert_eq!(pool.workers(), 4);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    workers: usize,
    name_prefix: String,
    stack_size: Option<usize>,
    background_limit: Option<usize>,
    shutdown: ShutdownPolicy,
    watchdog: Option<Duration>,
    thread_priority: Option<ThreadPriority>,
    affinity: Option<Vec<usize>>,
    idle_spin: u32,
}

impl Default for PoolConfig {
    /// Number of workers = logical cores − 1 (the main thread runs tasks too), at least 1.
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
        Self {
            workers: cores.saturating_sub(1).max(1),
            name_prefix: String::from("pragma-worker"),
            stack_size: None,
            background_limit: None,
            shutdown: ShutdownPolicy::Drain,
            watchdog: Some(DEFAULT_WATCHDOG),
            thread_priority: None,
            affinity: None,
            idle_spin: DEFAULT_IDLE_SPIN,
        }
    }
}

impl PoolConfig {
    /// Sets the number of worker threads.
    #[must_use]
    pub fn with_workers(mut self, workers: usize) -> Self {
        self.workers = workers;
        self
    }

    /// Thread name prefix (visible in the profiler and the debugger): `<prefix>-<index>`.
    #[must_use]
    pub fn with_name_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.name_prefix = prefix.into();
        self
    }

    /// Worker thread stack size in bytes (`None`: the OS default).
    #[must_use]
    pub fn with_stack_size(mut self, bytes: Option<usize>) -> Self {
        self.stack_size = bytes;
        self
    }

    /// How many workers may run [`Priority::Background`](crate::Priority::Background)
    /// tasks at the same time
    /// (`None`: half of the workers, rounded up).
    #[must_use]
    pub fn with_background_limit(mut self, limit: Option<usize>) -> Self {
        self.background_limit = limit;
        self
    }

    /// Shutdown policy when the pool is dropped.
    #[must_use]
    pub fn with_shutdown(mut self, policy: ShutdownPolicy) -> Self {
        self.shutdown = policy;
        self
    }

    /// Watchdog for stuck tasks (**debug builds only**): a labeled task
    /// ([`labeled`](crate::labeled): graph tasks, `spawn`, async) running on a pool thread
    /// longer than `threshold` produces one warning in the log (`tracing::warn!`, target
    /// `pragma_jobs::watchdog`) with the task and thread names. `None` disables it.
    ///
    /// Defaults to [`DEFAULT_WATCHDOG`]. In a release build the setting is ignored: there is neither
    /// a watchdog thread nor task bookkeeping.
    #[must_use]
    pub fn with_watchdog(mut self, threshold: Option<Duration>) -> Self {
        self.watchdog = threshold;
        self
    }

    /// OS priority of the worker threads (`None`: leave unchanged). If the OS refuses (for example
    /// raising it on Linux without `CAP_SYS_NICE`), the worker logs a warning and runs
    /// with its previous priority.
    #[must_use]
    pub fn with_thread_priority(mut self, priority: Option<ThreadPriority>) -> Self {
        self.thread_priority = priority;
        self
    }

    /// Pins workers to logical cores: worker `i` is pinned to core
    /// `cores[i % cores.len()]` (`None`: no pinning, the OS decides). A pinning error is
    /// a warning in the log; the worker runs without it.
    ///
    /// ```
    /// use pragma_jobs::PoolConfig;
    ///
    /// // Leave core 0 to the main thread, spread 4 workers over cores 1..=3.
    /// let config = PoolConfig::default().with_workers(4).with_affinity(Some(vec![1, 2, 3]));
    /// assert_eq!(config.core_for(0), Some(1));
    /// assert_eq!(config.core_for(3), Some(1)); // wraps around
    /// ```
    #[must_use]
    pub fn with_affinity(mut self, cores: Option<Vec<usize>>) -> Self {
        self.affinity = cores;
        self
    }

    /// How many `yield` rounds an idle worker searches for work before going to sleep (after a short
    /// spin). More rounds pick up the next wave of work (a frame, a nested `join`) faster, but keep
    /// the CPU busy doing nothing for longer; `0` parks right after the spin. The default
    /// [`DEFAULT_IDLE_SPIN`] was tuned with benchmarks (`docs/perf.md`).
    #[must_use]
    pub fn with_idle_spin(mut self, rounds: u32) -> Self {
        self.idle_spin = rounds;
        self
    }

    /// `yield` rounds before parking.
    #[must_use]
    pub fn idle_spin(&self) -> u32 {
        self.idle_spin
    }

    /// The watchdog threshold (effective in debug builds only).
    #[must_use]
    pub fn watchdog(&self) -> Option<Duration> {
        self.watchdog
    }

    /// Priority of the worker threads.
    #[must_use]
    pub fn thread_priority(&self) -> Option<ThreadPriority> {
        self.thread_priority
    }

    /// The core for worker `index`, if pinning is configured.
    #[must_use]
    pub fn core_for(&self, index: usize) -> Option<usize> {
        let cores = self.affinity.as_deref()?;
        cores.get(index.checked_rem(cores.len())?).copied()
    }

    /// Number of worker threads.
    #[must_use]
    pub fn workers(&self) -> usize {
        self.workers
    }

    /// Thread name prefix.
    #[must_use]
    pub fn name_prefix(&self) -> &str {
        &self.name_prefix
    }

    /// Worker thread stack size.
    #[must_use]
    pub fn stack_size(&self) -> Option<usize> {
        self.stack_size
    }

    /// The worker limit for background tasks, with the default applied.
    #[must_use]
    pub fn background_limit(&self) -> usize {
        self.background_limit
            .unwrap_or_else(|| self.workers.div_ceil(2))
            .max(1)
    }

    /// The shutdown policy.
    #[must_use]
    pub fn shutdown(&self) -> ShutdownPolicy {
        self.shutdown
    }

    /// Validates the configuration before threads are started.
    ///
    /// Rules: `1..=512` workers, a non-zero stack size, a background limit in `1..=workers`, a
    /// non-zero watchdog threshold, a non-empty affinity list.
    ///
    /// ```
    /// use pragma_jobs::{JobsError, PoolConfig};
    ///
    /// let err = PoolConfig::default()
    ///     .with_workers(2)
    ///     .with_background_limit(Some(3))
    ///     .validate()
    ///     .unwrap_err();
    /// assert!(matches!(err, JobsError::InvalidConfig { .. }));
    /// assert!(PoolConfig::default().validate().is_ok());
    /// ```
    pub fn validate(self) -> Result<Self, JobsError> {
        if self.workers == 0 {
            return Err(JobsError::InvalidConfig {
                reason: "workers must be >= 1",
            });
        }
        if self.workers > MAX_WORKERS {
            return Err(JobsError::InvalidConfig {
                reason: "workers must be <= 512",
            });
        }
        if self.stack_size == Some(0) {
            return Err(JobsError::InvalidConfig {
                reason: "stack_size must be > 0",
            });
        }
        if let Some(limit) = self.background_limit
            && (limit == 0 || limit > self.workers)
        {
            return Err(JobsError::InvalidConfig {
                reason: "background_limit must be in 1..=workers",
            });
        }
        if self.watchdog == Some(Duration::ZERO) {
            return Err(JobsError::InvalidConfig {
                reason: "watchdog threshold must be > 0",
            });
        }
        if self.affinity.as_ref().is_some_and(Vec::is_empty) {
            return Err(JobsError::InvalidConfig {
                reason: "affinity core list must not be empty",
            });
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        assert!(PoolConfig::default().validate().is_ok());
    }

    #[test]
    fn zero_workers_rejected() {
        assert!(PoolConfig::default().with_workers(0).validate().is_err());
    }

    #[test]
    fn background_limit_bounds() {
        let cfg = PoolConfig::default().with_workers(4);
        assert_eq!(cfg.background_limit(), 2);
        assert!(
            cfg.clone()
                .with_background_limit(Some(0))
                .validate()
                .is_err()
        );
        assert!(
            cfg.clone()
                .with_background_limit(Some(5))
                .validate()
                .is_err()
        );
        assert!(cfg.with_background_limit(Some(4)).validate().is_ok());
        assert_eq!(PoolConfig::default().with_workers(1).background_limit(), 1);
    }

    #[test]
    fn watchdog_and_affinity_validation() {
        let cfg = PoolConfig::default();
        assert_eq!(cfg.watchdog(), Some(DEFAULT_WATCHDOG));
        assert!(
            cfg.clone()
                .with_watchdog(Some(Duration::ZERO))
                .validate()
                .is_err()
        );
        assert!(cfg.clone().with_watchdog(None).validate().is_ok());
        assert!(cfg.clone().with_affinity(Some(vec![])).validate().is_err());
        let pinned = cfg.with_affinity(Some(vec![2, 5]));
        assert_eq!(pinned.core_for(0), Some(2));
        assert_eq!(pinned.core_for(3), Some(5));
        assert_eq!(PoolConfig::default().core_for(0), None);
    }
}
