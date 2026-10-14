use std::num::NonZeroUsize;
use std::time::Duration;

use crate::JobsError;
use crate::sys::ThreadPriority;

/// Порог watchdog по умолчанию (только debug-сборки, см. [`PoolConfig::with_watchdog`]).
pub const DEFAULT_WATCHDOG: Duration = Duration::from_secs(5);

/// Верхняя граница числа воркеров — защита от опечаток в конфиге.
const MAX_WORKERS: usize = 512;

/// Что делать с невыполненными задачами при `Drop` пула.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ShutdownPolicy {
    /// Воркеры дорабатывают все очереди, затем завершаются.
    #[default]
    Drain,
    /// Воркеры завершаются после текущей задачи; оставшиеся задачи сбрасываются,
    /// их хэндлы возвращают [`JobError::Cancelled`](crate::JobError::Cancelled).
    Cancel,
}

/// Конфигурация пула потоков.
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
}

impl Default for PoolConfig {
    /// Число воркеров = логические ядра − 1 (главный поток тоже исполняет задачи), минимум 1.
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

    /// Префикс имени потоков (видно в профайлере и отладчике): `<prefix>-<index>`.
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

    /// Сколько воркеров одновременно могут исполнять задачи
    /// [`Priority::Background`](crate::Priority::Background)
    /// (`None` — половина воркеров с округлением вверх).
    #[must_use]
    pub fn with_background_limit(mut self, limit: Option<usize>) -> Self {
        self.background_limit = limit;
        self
    }

    /// Политика завершения при `Drop` пула.
    #[must_use]
    pub fn with_shutdown(mut self, policy: ShutdownPolicy) -> Self {
        self.shutdown = policy;
        self
    }

    /// Watchdog зависших задач (**только debug-сборки**): задача с меткой
    /// ([`labeled`](crate::labeled) — задачи графа, `spawn`, async), исполняющаяся на потоке
    /// пула дольше `threshold`, даёт одно предупреждение в лог (`tracing::warn!`, target
    /// `pragma_jobs::watchdog`) с именем задачи и потока. `None` — выключен.
    ///
    /// По умолчанию [`DEFAULT_WATCHDOG`]. В release-сборке настройка игнорируется: ни потока
    /// watchdog, ни учёта задач нет.
    #[must_use]
    pub fn with_watchdog(mut self, threshold: Option<Duration>) -> Self {
        self.watchdog = threshold;
        self
    }

    /// Приоритет рабочих потоков ОС (`None` — не менять). Если ОС отказала (например,
    /// повышение на Linux без `CAP_SYS_NICE`), воркер пишет предупреждение в лог и работает
    /// с прежним приоритетом.
    #[must_use]
    pub fn with_thread_priority(mut self, priority: Option<ThreadPriority>) -> Self {
        self.thread_priority = priority;
        self
    }

    /// Привязка воркеров к логическим ядрам: воркер `i` закрепляется за ядром
    /// `cores[i % cores.len()]` (`None` — без привязки, решает ОС). Ошибка привязки —
    /// предупреждение в лог, воркер работает без неё.
    #[must_use]
    pub fn with_affinity(mut self, cores: Option<Vec<usize>>) -> Self {
        self.affinity = cores;
        self
    }

    /// Порог watchdog (действует только в debug-сборках).
    #[must_use]
    pub fn watchdog(&self) -> Option<Duration> {
        self.watchdog
    }

    /// Приоритет рабочих потоков.
    #[must_use]
    pub fn thread_priority(&self) -> Option<ThreadPriority> {
        self.thread_priority
    }

    /// Ядро для воркера `index`, если задана привязка.
    #[must_use]
    pub fn core_for(&self, index: usize) -> Option<usize> {
        let cores = self.affinity.as_deref()?;
        cores.get(index.checked_rem(cores.len())?).copied()
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

    /// Лимит воркеров для фоновых задач с учётом значения по умолчанию.
    #[must_use]
    pub fn background_limit(&self) -> usize {
        self.background_limit
            .unwrap_or_else(|| self.workers.div_ceil(2))
            .max(1)
    }

    /// Политика завершения.
    #[must_use]
    pub fn shutdown(&self) -> ShutdownPolicy {
        self.shutdown
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
