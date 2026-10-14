/// Errors of the L0 primitives.
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
