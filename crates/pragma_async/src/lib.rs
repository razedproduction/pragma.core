//! L3: async-задачи `pragma.core`.
//!
//! Статус: каркас (веха M3 в `docs/roadmap.md`). Executor на `async-task`
//! для долгих операций (I/O, загрузка ассетов, стриминг), которые не должны
//! занимать воркеры кадра: отдельный приоритет и ограничение параллелизма.
//!
//! ```
//! use pragma_async::AsyncError;
//!
//! assert_eq!(AsyncError::Cancelled.to_string(), "task was cancelled");
//! ```
#![forbid(unsafe_code)]

/// Ошибки async-задач.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AsyncError {
    /// Задача отменена до завершения (хэндл сброшен или рантайм остановлен).
    #[error("task was cancelled")]
    Cancelled,
    /// Задача запаниковала; паника перехвачена и не уронила воркер.
    #[error("task panicked: {message}")]
    Panicked {
        /// Сообщение паники, если оно было строкой.
        message: String,
    },
}
