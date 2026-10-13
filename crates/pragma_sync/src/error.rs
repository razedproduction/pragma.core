/// Ошибки примитивов синхронизации.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SyncError {
    /// Счётчик [`AtomicCounter`](crate::AtomicCounter) достиг `usize::MAX`.
    #[error("atomic counter overflow")]
    CounterOverflow,
}
