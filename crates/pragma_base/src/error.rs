/// Errors of the L0 primitives.
///
/// The enum and its struct variants are `#[non_exhaustive]`: match with `..` and a wildcard arm.
///
/// ```
/// use pragma_base::{BaseError, HandlePool};
///
/// let mut pool = HandlePool::new();
/// let h = pool.insert(1_u8)?;
/// pool.remove(h)?;
/// match pool.remove(h) {
///     Err(BaseError::StaleHandle { index, .. }) => assert_eq!(index, h.index()),
///     Err(other) => return Err(other),
///     Ok(_) => unreachable!("the handle was already removed"),
/// }
/// # Ok::<(), BaseError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BaseError {
    /// The handle points to a removed slot (generation mismatch) or to a nonexistent index.
    #[error("stale or invalid handle (index {index}, generation {generation})")]
    #[non_exhaustive]
    StaleHandle {
        /// Slot index.
        index: u32,
        /// Generation recorded in the handle.
        generation: u32,
    },
    /// The pool has exhausted its index space (`u32::MAX` slots).
    #[error("handle pool capacity exhausted")]
    CapacityExhausted,
    /// The allocator could not provide memory for a new pool slot (instead of `abort`).
    #[error("out of memory growing handle pool beyond {slots} slots")]
    #[non_exhaustive]
    OutOfMemory {
        /// Number of pool slots at the moment of failure.
        slots: usize,
    },
    /// The tick counter overflowed.
    #[error("tick counter overflow")]
    TickOverflow,
}
