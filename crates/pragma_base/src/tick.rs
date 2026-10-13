use crate::BaseError;

/// Номер тика фиксированного шага симуляции.
///
/// Симуляция оперирует тиками, а не `Instant::now()` — это основа детерминизма и реплеев.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Tick(u64);

impl Tick {
    /// Нулевой тик.
    pub const ZERO: Self = Self(0);

    /// Создаёт тик с заданным номером.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Номер тика.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Следующий тик; ошибка при переполнении вместо тихого заворачивания.
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
