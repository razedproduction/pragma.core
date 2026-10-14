/// Errors of the synchronization primitives.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SyncError {
    /// The [`AtomicCounter`](crate::AtomicCounter) reached `usize::MAX`.
    #[error("atomic counter overflow")]
    CounterOverflow,
}
