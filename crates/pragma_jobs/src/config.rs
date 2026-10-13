use std::num::NonZeroUsize;

use crate::JobsError;

/// Верхняя граница числа воркеров — защита от опечаток в конфиге.
const MAX_WORKERS: usize = 512;

/// Конфигурация пула потоков.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    workers: usize,
    name_prefix: String,
    stack_size: Option<usize>,
}

impl Default for PoolConfig {
    /// Число воркеров = логические ядра − 1 (главный поток тоже исполняет задачи), минимум 1.
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
        Self {
            workers: cores.saturating_sub(1).max(1),
            name_prefix: String::from("pragma-worker"),
            stack_size: None,
        }
    }
}

impl PoolConfig {
    /// Задать число рабочих потоков.
    #[must_use]
    pub fn with_workers(mut self, workers: usize) -> Self {
        self.workers = workers;
        self
    }

    /// Префикс имени потоков (видно в профайлере и отладчике).
    #[must_use]
    pub fn with_name_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.name_prefix = prefix.into();
        self
    }

    /// Размер стека рабочего потока в байтах (`None` — по умолчанию ОС).
    #[must_use]
    pub fn with_stack_size(mut self, bytes: Option<usize>) -> Self {
        self.stack_size = bytes;
        self
    }

    /// Число рабочих потоков.
    #[must_use]
    pub fn workers(&self) -> usize {
        self.workers
    }

    /// Префикс имени потоков.
    #[must_use]
    pub fn name_prefix(&self) -> &str {
        &self.name_prefix
    }

    /// Размер стека рабочего потока.
    #[must_use]
    pub fn stack_size(&self) -> Option<usize> {
        self.stack_size
    }

    /// Проверяет конфигурацию до запуска потоков.
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
}
