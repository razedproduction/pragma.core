use crate::AsyncError;

/// Верхняя граница blocking-потоков — защита от опечаток в конфиге.
const MAX_BLOCKING_THREADS: usize = 256;

/// Конфигурация исполнителя: размер blocking-пула и его очереди.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncConfig {
    blocking_threads: usize,
    blocking_capacity: usize,
    name_prefix: String,
    stack_size: Option<usize>,
}

impl Default for AsyncConfig {
    /// 4 blocking-потока (создаются по требованию), до 1024 поставленных blocking-задач.
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
    /// Максимум потоков blocking-пула. Потоки создаются по требованию и живут до сброса
    /// исполнителя.
    #[must_use]
    pub fn with_blocking_threads(mut self, threads: usize) -> Self {
        self.blocking_threads = threads;
        self
    }

    /// Сколько blocking-задач может быть поставлено одновременно (в очереди и в работе).
    /// Сверх лимита [`Executor::unblock`](crate::Executor::unblock) сразу возвращает
    /// [`AsyncError::Overloaded`] — очередь не растёт без границы, вызывающий не ждёт.
    #[must_use]
    pub fn with_blocking_capacity(mut self, capacity: usize) -> Self {
        self.blocking_capacity = capacity;
        self
    }

    /// Префикс имени blocking-потоков: `<prefix>-<index>`.
    #[must_use]
    pub fn with_name_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.name_prefix = prefix.into();
        self
    }

    /// Размер стека blocking-потока в байтах (`None` — по умолчанию ОС).
    #[must_use]
    pub fn with_stack_size(mut self, bytes: Option<usize>) -> Self {
        self.stack_size = bytes;
        self
    }

    /// Максимум потоков blocking-пула.
    #[must_use]
    pub fn blocking_threads(&self) -> usize {
        self.blocking_threads
    }

    /// Лимит поставленных blocking-задач.
    #[must_use]
    pub fn blocking_capacity(&self) -> usize {
        self.blocking_capacity
    }

    /// Префикс имени blocking-потоков.
    #[must_use]
    pub fn name_prefix(&self) -> &str {
        &self.name_prefix
    }

    /// Размер стека blocking-потока.
    #[must_use]
    pub fn stack_size(&self) -> Option<usize> {
        self.stack_size
    }

    /// Проверяет конфигурацию.
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
