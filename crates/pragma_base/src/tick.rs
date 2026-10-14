use crate::BaseError;

/// Tick number of the fixed simulation step.
///
/// The simulation works in ticks rather than `Instant::now()`; this is the basis of determinism and replays.
///
/// ```
/// use pragma_base::Tick;
///
/// let mut tick = Tick::ZERO;
/// for _ in 0..3 {
///     // ... run one fixed step of the simulation ...
///     tick = tick.next()?;
/// }
/// assert_eq!(tick, Tick::new(3));
/// assert!(Tick::new(u64::MAX).next().is_err()); // no silent wrap-around
/// # Ok::<(), pragma_base::BaseError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Tick(u64);

impl Tick {
    /// The zero tick.
    pub const ZERO: Self = Self(0);

    /// Creates a tick with the given number.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The tick number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next tick; an error on overflow instead of silently wrapping.
    pub fn next(self) -> Result<Self, BaseError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(BaseError::TickOverflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_is_an_error() {
        assert_eq!(Tick::new(u64::MAX).next(), Err(BaseError::TickOverflow));
    }
}
