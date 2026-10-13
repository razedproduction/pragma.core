/// Ошибки L0-примитивов.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BaseError {
    /// Хэндл указывает на удалённый слот (поколение не совпадает) или на несуществующий индекс.
    #[error("stale or invalid handle (index {index}, generation {generation})")]
    StaleHandle {
        /// Индекс слота.
        index: u32,
        /// Поколение, записанное в хэндле.
        generation: u32,
    },
    /// Пул исчерпал адресное пространство индексов (`u32::MAX` слотов).
    #[error("handle pool capacity exhausted")]
    CapacityExhausted,
    /// Счётчик тиков переполнился.
    #[error("tick counter overflow")]
    TickOverflow,
}
