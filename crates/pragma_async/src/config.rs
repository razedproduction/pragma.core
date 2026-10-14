use crate::AsyncError;

/// Upper bound on blocking threads, a guard against typos in the config.
const MAX_BLOCKING_THREADS: usize = 256;

/// Executor configuration: the size of the blocking pool and its queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncConfig {
    blocking_threads: usize,
    blocking_capacity: usize,
    name_prefix: String,
    stack_size: Option<usize>,
}

impl Default for AsyncConfig {
    /// 4 blocking threads (created on demand), up to 1024 queued blocking tasks.
    fn default() -> Self {
        Self {
            blocking_threads: 4,
            blocking_capacity: 1024,
            name_prefix: String::from("pragma-blocking"),
            stack_size: None,
        }
    }
}

impl AsyncConfig {
    /// Maximum number of blocking pool threads. Threads are created on demand and live until the
    /// executor is dropped.
    #[must_use]
    pub fn with_blocking_threads(mut self, threads: usize) -> Self {
        self.blocking_threads = threads;
        self
    }

    /// How many blocking tasks may be queued at once (waiting and running).
    /// Beyond the limit, [`Executor::unblock`](crate::Executor::unblock) immediately returns
    /// [`AsyncError::Overloaded`]: the queue does not grow without bound, and the caller does not wait.
    #[must_use]
    pub fn with_blocking_capacity(mut self, capacity: usize) -> Self {
        self.blocking_capacity = capacity;
        self
    }

    /// Blocking thread name prefix: `<prefix>-<index>`.
    #[must_use]
    pub fn with_name_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.name_prefix = prefix.into();
        self
    }

    /// Blocking thread stack size in bytes (`None`: the OS default).
    #[must_use]
    pub fn with_stack_size(mut self, bytes: Option<usize>) -> Self {
        self.stack_size = bytes;
        self
    }

    /// Maximum number of blocking pool threads.
    #[must_use]
    pub fn blocking_threads(&self) -> usize {
        self.blocking_threads
    }

    /// The limit on queued blocking tasks.
    #[must_use]
    pub fn blocking_capacity(&self) -> usize {
        self.blocking_capacity
    }

    /// Blocking thread name prefix.
    #[must_use]
    pub fn name_prefix(&self) -> &str {
        &self.name_prefix
    }

    /// Blocking thread stack size.
    #[must_use]
    pub fn stack_size(&self) -> Option<usize> {
        self.stack_size
    }

    /// Validates the configuration.
    pub fn validate(self) -> Result<Self, AsyncError> {
        if self.blocking_threads == 0 {
            return Err(AsyncError::InvalidConfig {
                reason: "blocking_threads must be >= 1",
            });
        }
        if self.blocking_threads > MAX_BLOCKING_THREADS {
            return Err(AsyncError::InvalidConfig {
                reason: "blocking_threads must be <= 256",
            });
        }
        if self.blocking_capacity < self.blocking_threads {
            return Err(AsyncError::InvalidConfig {
                reason: "blocking_capacity must be >= blocking_threads",
            });
        }
        if self.stack_size == Some(0) {
            return Err(AsyncError::InvalidConfig {
                reason: "stack_size must be > 0",
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
        assert!(AsyncConfig::default().validate().is_ok());
    }

    #[test]
    fn bounds() {
        let cfg = AsyncConfig::default();
        assert!(cfg.clone().with_blocking_threads(0).validate().is_err());
        assert!(cfg.clone().with_blocking_threads(257).validate().is_err());
        assert!(
            cfg.clone()
                .with_blocking_threads(8)
                .with_blocking_capacity(4)
                .validate()
                .is_err()
        );
        assert!(cfg.clone().with_stack_size(Some(0)).validate().is_err());
        assert!(
            cfg.with_blocking_threads(2)
                .with_blocking_capacity(2)
                .validate()
                .is_ok()
        );
    }
}
