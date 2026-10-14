/// Errors of the synchronization primitives.
///
/// ```
/// use pragma_sync::{AtomicCounter, SyncError};
///
/// let full = AtomicCounter::new(usize::MAX);
/// assert_eq!(full.increment(), Err(SyncError::CounterOverflow));
/// assert_eq!(full.get(), usize::MAX); // the value is left as it was
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SyncError {
    /// The [`AtomicCounter`](crate::AtomicCounter) reached `usize::MAX`.
    #[error("atomic counter overflow")]
    CounterOverflow,
}
