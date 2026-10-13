/// Приоритет задачи (`docs/architecture.md` §3.2). Порядок `Ord`: `Frame` — самый срочный.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[non_exhaustive]
pub enum Priority {
    /// Работа текущего кадра (граф, `parallel_for`): берётся первой.
    Frame,
    /// Задачи без дедлайна кадра: после всех `Frame`.
    #[default]
    Normal,
    /// Запекание, декомпрессия, стриминг: отдельная глобальная очередь и лимит
    /// одновременно занятых воркеров ([`PoolConfig::with_background_limit`]).
    ///
    /// [`PoolConfig::with_background_limit`]: crate::PoolConfig::with_background_limit
    Background,
}

/// Способ разбиения данных на задачи в `parallel_*`.
///
/// Разбиение зависит **только** от длины данных и `Grain`, но не от числа воркеров, поэтому
/// `parallel_reduce` даёт побитово одинаковый результат на любом пуле (`architecture.md` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Grain {
    /// Около [`Grain::AUTO_CHUNKS`] чанков, но не меньше [`Grain::AUTO_MIN_CHUNK`] элементов в чанке.
    #[default]
    Auto,
    /// Фиксированный размер чанка в элементах (`0` трактуется как `1`).
    Fixed(usize),
}

impl Grain {
    /// Целевое число чанков для [`Grain::Auto`].
    pub const AUTO_CHUNKS: usize = 128;
    /// Минимальный размер чанка для [`Grain::Auto`]: мельче — накладные расходы дороже работы.
    pub const AUTO_MIN_CHUNK: usize = 64;

    /// Размер чанка для `len` элементов (всегда `>= 1`).
    #[must_use]
    pub fn chunk_size(self, len: usize) -> usize {
        match self {
            Self::Auto => len.div_ceil(Self::AUTO_CHUNKS).max(Self::AUTO_MIN_CHUNK),
            Self::Fixed(n) => n.max(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_size_is_positive_and_worker_independent() {
        assert_eq!(Grain::Auto.chunk_size(0), Grain::AUTO_MIN_CHUNK);
        assert_eq!(Grain::Auto.chunk_size(1_000_000), 7813);
        assert_eq!(Grain::Fixed(0).chunk_size(10), 1);
        assert_eq!(Grain::Fixed(16).chunk_size(10), 16);
    }

    #[test]
    fn frame_is_most_urgent() {
        assert!(Priority::Frame < Priority::Normal);
        assert!(Priority::Normal < Priority::Background);
    }
}
